import { test, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { Platform, PlatformError } from "../src/platform.ts";

const hits: { method?: string; url?: string; auth?: string; body: string }[] = [];
const srv = http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    hits.push({ method: req.method, url: req.url, auth: req.headers.authorization, body });
    const [, , , ws, verb] = req.url!.split("?")[0].split("/");
    if (ws === "ws1" && verb === "tools") return res.end(JSON.stringify({ address: "10.0.0.5:7788" }));
    if (ws === "ws1" && !verb && req.method === "GET") return res.end(JSON.stringify({ id: "ws1", name: "alpha" }));
    if (ws === "ws1" && verb === "clone") { res.statusCode = 202; return res.end(JSON.stringify({ id: `${ws}-c1` })); }
    if (ws === "ws1-c1" && req.method === "DELETE") { res.statusCode = 204; return res.end(); }
    res.statusCode = 409; res.end(JSON.stringify({ error: "workspaces: 3 of 3 in use" }));
  });
});
await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
after(() => srv.close());
const api = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;

test("tools, clone and remove hit the right routes with the token", async () => {
  const p = new Platform(api, "tok", "team1");
  assert.equal(await p.tools("ws1"), "10.0.0.5:7788");
  assert.equal(await p.name("ws1"), "alpha");
  assert.equal(await p.clone("ws1", "sub-1"), "ws1-c1");
  await p.remove("ws1-c1");
  assert.deepEqual(hits.map((h) => `${h.method} ${h.url}`), [
    "GET /v1/workspaces/ws1/tools?team=team1",
    "GET /v1/workspaces/ws1?team=team1",
    "POST /v1/workspaces/ws1/clone?team=team1",
    "DELETE /v1/workspaces/ws1-c1?team=team1",
  ]);
  assert.ok(hits.every((h) => h.auth === "Bearer tok"));
  assert.equal(JSON.parse(hits[2].body).name, "sub-1");
});

test("a refused call is a PlatformError carrying status and body", async () => {
  const p = new Platform(api, "tok");
  await assert.rejects(p.clone("nope", "x"), (e: PlatformError) => e instanceof PlatformError && e.status === 409 && /3 of 3/.test(e.body));
});
