import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { BenchClient } from "../../src/bench-client.ts";

const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

test("the desktop operations bridge uses fixed authenticated routes while keeping the tunnel nonce private", async () => {
  const seen: { method?: string; url?: string; headers: http.IncomingHttpHeaders; body: unknown }[] = [];
  const server = http.createServer(async (req, res) => {
    let raw = "";
    for await (const chunk of req) raw += chunk;
    seen.push({ method: req.method, url: req.url, headers: req.headers, body: raw ? JSON.parse(raw) : undefined });
    res.writeHead(200, { "content-type": "application/json" });
    res.end(req.url?.endsWith("/events?after=cursor%2F1&limit=25") ? JSON.stringify({ events: [], hasMore: false }) : JSON.stringify({ operationId: "op-1" }));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
  const cache = path.join(os.tmpdir(), `operations-bridge-${process.pid}.json`);
  const client = new BenchClient(base, () => undefined, cache, "alice@api", "private-nonce");
  const auth = { authorization: "Bearer person-token", "x-kl-owner": "acme", "x-kl-login": "alice" };
  try {
    await client.operationSnapshot("op-1", auth);
    await client.operationEvents("op-1", "cursor/1", 25, auth);
    await client.cancelOperation("op-1", 7, auth);
    await client.recordOperationDecision("op-1", "decision-1", { stepId: "step-1", expectedRevision: 7, outcome: "granted" }, auth);
    await client.provideOperationInput("op-1", "decision-1", 8, { answer: "because" }, auth);

    assert.deepEqual(seen.map(({ method, url, body }) => ({ method, url, body })), [
      { method: "GET", url: "/operations/op-1", body: undefined },
      { method: "GET", url: "/operations/op-1/events?after=cursor%2F1&limit=25", body: undefined },
      { method: "POST", url: "/operations/op-1/cancel", body: { expectedRevision: 7 } },
      { method: "POST", url: "/operations/op-1/decisions/decision-1", body: { stepId: "step-1", expectedRevision: 7, outcome: "granted" } },
      { method: "POST", url: "/operations/op-1/input", body: { decisionId: "decision-1", expectedRevision: 8, inputs: { answer: "because" } } },
    ]);
    for (const request of seen) {
      assert.equal(request.headers.authorization, "Bearer person-token");
      assert.equal(request.headers["x-kl-owner"], "acme");
      assert.equal(request.headers["x-kl-login"], "alice");
      assert.equal(request.headers["x-kl-tunnel"], "private-nonce");
    }
  } finally {
    client.close();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    fs.rmSync(cache, { force: true });
  }
});

test("preload exposes only named operation calls and main gates them on ready auth", () => {
  const preload = fs.readFileSync(path.join(ROOT, "src", "preload.ts"), "utf8");
  const main = fs.readFileSync(path.join(ROOT, "src", "main.ts"), "utf8");
  assert.match(preload, /operations:\s*\{/);
  for (const method of ["snapshot", "events", "cancel", "decision", "input"]) assert.match(preload, new RegExp(`${method}:`));
  assert.doesNotMatch(preload, /operations:[\s\S]*authorization/i);
  assert.match(main, /async function operation/);
  assert.match(main, /s\.phase !== "ready" \|\| !c/);
  assert.match(main, /stillValid\(c\)/);
  const allowlist = /export const BENCH_ROUTES = new RegExp\(([\s\S]*?)\);/.exec(main)?.[1] ?? "";
  assert.doesNotMatch(allowlist, /operations/);
});
