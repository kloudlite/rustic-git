import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { toIde, fromIde, ToolServer, resolveFromApi } from "../../pi/workspace-tools.ts";
import kloudlite, { call } from "../../pi/kloudlite.ts";

test("pi's tools become the tool server's calls", () => {
  assert.deepEqual(toIde("edit", { path: "a.rs", edits: [{ oldText: "x", newText: "y" }] }), { tool: "edit", args: { path: "a.rs", edits: [{ old: "x", new: "y" }] } });
  assert.deepEqual(toIde("bash", { command: "make", timeout: 5 }), { tool: "exec", args: { cmd: "make", timeout_ms: 5000 } });
  assert.equal((toIde("bash", { command: "make", timeout: 3600 }).args as { timeout_ms: number }).timeout_ms, 600_000);
  assert.equal(toIde("grep", { pattern: "a.b", literal: true }).args.pattern, "a\\.b");
  assert.deepEqual(toIde("find", { pattern: "**/*.ts", path: "src" }), { tool: "glob", args: { pattern: "**/*.ts", cwd: "src" } });
  assert.deepEqual(toIde("ls", {}), { tool: "exec", args: { cmd: ["ls", "-1Ap", "--", "."], head: 500 } });
  assert.throws(() => toIde("kl_workspaces", {}), /no workspace tool kl_workspaces/);
});

test("an answer becomes text; a refusal is only its error", () => {
  assert.deepEqual(fromIde("bash", 200, { exit_code: 2, stdout: "out", stderr: "err", timed_out: false }), { content: [{ type: "text", text: "out\nerr\n[exit 2]" }], isError: true });
  assert.deepEqual(fromIde("read", 403, { error: "../x: outside the home" }), { content: [{ type: "text", text: "../x: outside the home" }], isError: true });
  assert.equal(fromIde("grep", 200, { matches: [{ path: "a.rs", line: 3, text: "fn a()" }], truncated: false }).content[0].text, "a.rs:3: fn a()");
  assert.equal(fromIde("grep", 200, { matches: [{ path: "a.rs", line: 3, text: "fn a()", context: "fn a() {\n  x()\n}" }] }).content[0].text, "a.rs:3: fn a()\nfn a() {\n  x()\n}");
  assert.equal(fromIde("find", 200, { paths: ["a", "b", "c"] }, 2).content[0].text, "a\nb");
  assert.equal(fromIde("edit", 200, { path: "/home/kl/workspaces/api/a.rs", applied: 1 }).isError, false);
});

test("a call goes to the looked-up address, a dead address is looked up once more, and a refusal names why", async () => {
  const seen: string[] = [];
  const srv = http.createServer((req, res) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      seen.push(`${req.url} ${b}`);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ exit_code: 0, stdout: "ide-kl", stderr: "" }));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const live = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  const answers = ["127.0.0.1:1", live]; // the first address is a pod that has gone
  let asked = 0;
  const s = new ToolServer("api", async () => answers[asked++]);
  const r = await s.call(toIde("bash", { command: "echo ide-$(id -un)" }));
  assert.equal(r.status, 200);
  assert.equal(asked, 2);
  assert.deepEqual(seen, ['/tools/exec {"cmd":"echo ide-$(id -un)","timeout_ms":120000}']);

  const stopped = new ToolServer("api", async () => { throw new Error("workspace api is stopped; start it to run tools"); });
  await assert.rejects(stopped.call(toIde("ls", {})), /is stopped; start it to run tools/);
  const gone = new ToolServer("api", async () => "127.0.0.1:1");
  await assert.rejects(gone.call(toIde("ls", {})), /workspace api did not answer at 127\.0\.0\.1:1/);
  srv.close();
});

test("with neither KL_TOOLS_ADDRESS nor KL_TEAM set, resolving refuses before any api call", async () => {
  const savedTeam = process.env.KL_TEAM;
  const savedAddr = process.env.KL_TOOLS_ADDRESS;
  delete process.env.KL_TEAM;
  delete process.env.KL_TOOLS_ADDRESS;
  try {
    await assert.rejects(resolveFromApi("api"), /KL_TEAM is not set/);
  } finally {
    if (savedTeam !== undefined) process.env.KL_TEAM = savedTeam;
    if (savedAddr !== undefined) process.env.KL_TOOLS_ADDRESS = savedAddr;
  }
});

test("a malformed address, from KL_TOOLS_ADDRESS or from /v1, is refused", async () => {
  const saved = process.env.KL_TOOLS_ADDRESS;
  process.env.KL_TOOLS_ADDRESS = "not-an-address";
  try {
    await assert.rejects(resolveFromApi("api"), /not a host:port address/);
  } finally {
    if (saved === undefined) delete process.env.KL_TOOLS_ADDRESS;
    else process.env.KL_TOOLS_ADDRESS = saved;
  }
});

test("a 409 from the tool server clears the cached address and is looked up once more", async () => {
  let hits = 0;
  const srv = http.createServer((req, res) => {
    hits++;
    if (hits === 1) {
      res.writeHead(409, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "workspace not ready" }));
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ exit_code: 0, stdout: "ok", stderr: "" }));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const addr = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  let asked = 0;
  const s = new ToolServer("api", async () => {
    asked++;
    return addr;
  });
  const r = await s.call(toIde("ls", {}));
  assert.equal(r.status, 200);
  assert.equal(asked, 2);
  srv.close();
});

test("a 409 from /v1 is re-asked once against a fake api, then the workspace's tool server is used", async () => {
  const toolSrv = http.createServer((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ exit_code: 0, stdout: "ok", stderr: "" }));
  });
  await new Promise<void>((r) => toolSrv.listen(0, "127.0.0.1", r));
  const toolAddr = `127.0.0.1:${(toolSrv.address() as { port: number }).port}`;

  let getCalls = 0;
  const api = http.createServer((_req, res) => {
    getCalls++;
    if (getCalls === 1) {
      res.writeHead(409, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "workspace api is starting" }));
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ address: toolAddr }));
  });
  await new Promise<void>((r) => api.listen(0, "127.0.0.1", r));
  const apiUrl = `http://127.0.0.1:${(api.address() as { port: number }).port}`;

  const cfgDir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-cfg-"));
  const restore = withToken(path.join(cfgDir, "token"), "t", apiUrl);
  const savedTeam = process.env.KL_TEAM;
  const savedAddr = process.env.KL_TOOLS_ADDRESS;
  process.env.KL_TEAM = "acme";
  delete process.env.KL_TOOLS_ADDRESS;
  try {
    const server = new ToolServer("api", resolveFromApi);
    const r = await server.call(toIde("ls", {}));
    assert.equal(r.status, 200);
    assert.equal(getCalls, 2);
  } finally {
    restore();
    if (savedTeam === undefined) delete process.env.KL_TEAM;
    else process.env.KL_TEAM = savedTeam;
    if (savedAddr !== undefined) process.env.KL_TOOLS_ADDRESS = savedAddr;
    toolSrv.close();
    api.close();
    fs.rmSync(cfgDir, { recursive: true, force: true });
  }
});

/** Points kloudlite.ts at a token file and an api; returns the undo. */
function withToken(file: string | undefined, tok: string | undefined, api: string) {
  const saved = { f: process.env.KL_TOOL_TOKEN_FILE, a: process.env.KL_API_URL };
  if (file && tok !== undefined) fs.writeFileSync(file, tok);
  if (file) process.env.KL_TOOL_TOKEN_FILE = file;
  else delete process.env.KL_TOOL_TOKEN_FILE;
  process.env.KL_API_URL = api;
  return () => {
    for (const [k, v] of [["KL_TOOL_TOKEN_FILE", saved.f], ["KL_API_URL", saved.a]] as const) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  };
}

async function fakeApi(handler: http.RequestListener) {
  const srv = http.createServer(handler);
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  return { srv, url: `http://127.0.0.1:${(srv.address() as { port: number }).port}` };
}

test("call re-reads the token file between calls", async () => {
  const seen: string[] = [];
  const { srv, url } = await fakeApi((req, res) => (seen.push(String(req.headers.authorization)), res.end("{}")));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-tok-"));
  const file = path.join(dir, "token");
  const restore = withToken(file, "one", url);
  try {
    await call("GET", "/v1/quota");
    fs.writeFileSync(file, "two\n");
    await call("GET", "/v1/quota");
    assert.deepEqual(seen, ["Bearer one", "Bearer two"]);
  } finally {
    restore();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a missing token file throws the sign-in sentence without a request", async () => {
  let asked = 0;
  const { srv, url } = await fakeApi((_req, res) => (asked++, res.end("{}")));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-tok-"));
  for (const restore of [withToken(path.join(dir, "absent"), undefined, url), withToken(path.join(dir, "empty"), "", url), withToken(undefined, undefined, url)]) {
    try {
      await assert.rejects(call("GET", "/v1/quota"), { message: "sign in on the Kloudlite desktop app" });
    } finally {
      restore();
    }
  }
  assert.equal(asked, 0);
  srv.close();
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a 401 maps to the sign-in sentence", async () => {
  const { srv, url } = await fakeApi((_req, res) => (res.writeHead(401), res.end("expired")));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-tok-"));
  const restore = withToken(path.join(dir, "token"), "t", url);
  try {
    assert.deepEqual(await call("GET", "/v1/quota"), { status: 401, data: "sign in on the Kloudlite desktop app (your desktop session ended or the bench was stopped)" });
  } finally {
    restore();
    srv.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("whoami never returns the token", async () => {
  const b64 = (o: object) => Buffer.from(JSON.stringify(o)).toString("base64url");
  const tok = `${b64({ alg: "HS256" })}.${b64({ sub: "ada", team: "acme", exp: 4102444800 })}.sig`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kl-tok-"));
  const restore = withToken(path.join(dir, "token"), tok, "http://127.0.0.1:1");
  const tools: Record<string, { execute: (...a: unknown[]) => Promise<{ content: { text: string }[] }> }> = {};
  kloudlite({ registerTool: (t: { name: string }) => (tools[t.name] = t as never), on: () => undefined } as never);
  try {
    const out = (await tools.kl_whoami.execute("c1", {}, undefined, undefined, undefined)).content[0].text;
    assert.deepEqual(JSON.parse(out), { username: "ada", team: "acme", expires_at: "2100-01-01T00:00:00.000Z" });
    assert.ok(!out.includes(tok) && !out.includes("sig"));
  } finally {
    restore();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
