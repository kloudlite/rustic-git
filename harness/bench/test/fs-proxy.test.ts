import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";

/**
 * The desktop renders a workspace from its tool server's `/fs/*` (spec §2): the bench proxies those
 * reads, and `/fs/file` answers BYTES with a content type and an ETag rather than JSON — so one
 * envelope carries either over the tunnel, and a file already held costs a 304 and no bytes.
 */
async function toolServer(handler: (req: http.IncomingMessage, res: http.ServerResponse) => void) {
  const srv = http.createServer(handler);
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  return { srv, address: `127.0.0.1:${(srv.address() as { port: number }).port}`, close: () => new Promise((r) => srv.close(r)) };
}

async function up(address: string) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-fs-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0, "127.0.0.1", undefined, undefined, { resolveTools: async () => address });
  return {
    base: `http://127.0.0.1:${srv.port}`,
    down: async () => (await srv.close(), await bench.stop(), fs.rmSync(dir, { recursive: true, force: true })),
  };
}

const WS = "ws-0123456789abcdef";

test("a text file comes back as text, with the tag to ask again with", async () => {
  const asked: { url: string; etag?: string }[] = [];
  const tools = await toolServer((req, res) => {
    asked.push({ url: req.url ?? "", etag: req.headers["if-none-match"] as string | undefined });
    if (req.headers["if-none-match"] === '"7-1"') return void res.writeHead(304, { etag: '"7-1"' }).end();
    res.writeHead(200, { "content-type": "text/plain; charset=utf-8", etag: '"7-1"' }).end("hello\n");
  });
  const t = await up(tools.address);
  try {
    const first = await (await fetch(`${t.base}/fs/file?scope=${WS}&path=src/index.ts`)).json();
    assert.equal(first.text, "hello\n");
    assert.equal(first.etag, '"7-1"');
    assert.equal(first.bytes, 6);
    assert.ok(!first.binary);
    // The path reaches the tool server; the scope does not — it is how the bench found it.
    assert.match(asked[0].url, /^\/fs\/file\?path=src%2Findex\.ts$/);
    assert.equal(asked[0].etag, undefined);

    // Asking again with the tag is a 304, and the envelope says so rather than carrying bytes.
    const again = await (await fetch(`${t.base}/fs/file?scope=${WS}&path=src/index.ts&etag=${encodeURIComponent('"7-1"')}`)).json();
    assert.deepEqual(again, { notModified: true, etag: '"7-1"' });
    assert.equal(asked[1].etag, '"7-1"');
  } finally {
    await t.down();
    await tools.close();
  }
});

test("a binary file is named and measured, never sent as text", async () => {
  const tools = await toolServer((_req, res) => {
    res.writeHead(200, { "content-type": "image/png", etag: '"9-2"' }).end(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0, 1, 2]));
  });
  const t = await up(tools.address);
  try {
    const got = await (await fetch(`${t.base}/fs/file?scope=${WS}&path=logo.png`)).json();
    assert.equal(got.binary, true);
    assert.equal(got.text, undefined);
    assert.equal(got.bytes, 7);
    assert.equal(got.mime, "image/png");
  } finally {
    await t.down();
    await tools.close();
  }
});

test("a file that cannot be read answers its own sentence, not an envelope", async () => {
  const tools = await toolServer((_req, res) => {
    res.writeHead(404, { "content-type": "application/json" }).end(JSON.stringify({ error: "src/gone.ts: not found" }));
  });
  const t = await up(tools.address);
  try {
    const r = await fetch(`${t.base}/fs/file?scope=${WS}&path=src/gone.ts`);
    assert.equal(r.status, 404);
    assert.deepEqual(await r.json(), { error: "src/gone.ts: not found" });
  } finally {
    await t.down();
    await tools.close();
  }
});

test("the other fs routes are passed through as the JSON they are", async () => {
  const tools = await toolServer((req, res) => {
    if ((req.url ?? "").startsWith("/fs/changes")) return void res.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({ repo: true, head: "9a9034f", branch: "main", changes: [{ path: "src/a.ts", index: ".", worktree: "M" }] }));
    res.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({ entries: [{ name: "src", kind: "dir", ignored: false, git: "" }] }));
  });
  const t = await up(tools.address);
  try {
    const tree = await (await fetch(`${t.base}/fs/tree?scope=${WS}`)).json();
    assert.deepEqual(tree.entries[0], { name: "src", kind: "dir", ignored: false, git: "" });
    const changes = await (await fetch(`${t.base}/fs/changes?scope=${WS}`)).json();
    assert.equal(changes.repo, true);
    assert.equal(changes.head, "9a9034f");
    assert.deepEqual(changes.changes[0], { path: "src/a.ts", index: ".", worktree: "M" });
  } finally {
    await t.down();
    await tools.close();
  }
});

/**
 * `/fs/log` is proxied like every other console read (`crates/ide/src/fs/mod.rs`, 8d0ed194): the
 * CHANGES tab's second half draws from it, and a person who had just committed saw an empty panel
 * before it existed (owner, 2026-09-18).
 */
test("the branch's commits come through with what each one touched", async () => {
  const seen: string[] = [];
  const tools = await toolServer((req, res) => {
    seen.push(req.url!);
    res.writeHead(200, { "content-type": "application/json", etag: '"log-1"' });
    res.end(
      JSON.stringify({
        repo: true,
        commits: [
          { hash: "a".repeat(40), short: "aaaaaaa", subject: "Add the service", author: "ada", at: "2026-09-18T04:00:00Z", files: [{ path: "src/main.go", status: "A" }] },
          { hash: "b".repeat(40), short: "bbbbbbb", subject: "Rename it", author: "ada", at: "2026-09-18T03:00:00Z", files: [{ path: "go.mod", status: "R", from: "gomod" }] },
        ],
      }),
    );
  });
  const b = await up(tools.address);
  try {
    const r = await (await fetch(`${b.base}/fs/log?scope=ws-0123456789abcdef&n=20`)).json() as { commits: { subject: string; files: { path: string; status: string; from?: string }[] }[] };
    assert.equal(seen[0], "/fs/log?n=20", "the count travels; the scope is the bench's own business");
    assert.deepEqual(r.commits.map((c) => c.subject), ["Add the service", "Rename it"]);
    assert.deepEqual(r.commits[1].files[0], { path: "go.mod", status: "R", from: "gomod" }, "a rename keeps where it came from");
  } finally {
    await b.down();
    await tools.close();
  }
});

/**
 * "Diff against main" (spec §4.5). It is an EXEC, so it does not go through the read-only `/fs/*`
 * pass-through: one route, one fixed argv, and a tree name checked before anything is dialled. A
 * general exec proxy here would be a shell for anything holding the bench's port.
 */
test("against-main runs one fixed read-only diff in the named tree, and refuses anything else", async () => {
  const sent: { url: string; body: string }[] = [];
  const tools = await toolServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      sent.push({ url: req.url ?? "", body: b });
      res.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({ exit_code: 0, stdout: "diff --git a/x b/x\n", stderr: "" }));
    });
  });
  const t = await up(tools.address);
  try {
    const got = await (await fetch(`${t.base}/fs/against-main?scope=${WS}&tree=audit-1`)).json();
    assert.deepEqual(got, { diff: "diff --git a/x b/x\n" });
    assert.equal(sent.length, 1);
    assert.equal(sent[0].url, "/tools/exec");
    assert.deepEqual(JSON.parse(sent[0].body).cmd, ["git", "diff", "main...HEAD"], "the argv is ours, never the client's");
    assert.equal(JSON.parse(sent[0].body).tree, "audit-1");
    assert.equal(JSON.parse(sent[0].body).detach, undefined, "a job, never a process left running");

    // A tree name that is a path, a scope that is not a workspace: refused before anything is dialled.
    for (const q of [`scope=${WS}&tree=../../etc`, `scope=${WS}&tree=`, `scope=bench&tree=x`, `scope=nope&tree=x`])
      assert.equal((await fetch(`${t.base}/fs/against-main?${q}`)).status, 400, q);
    assert.equal(sent.length, 1, "and nothing more reached the tool server");
  } finally {
    await t.down();
    await tools.close();
  }
});
