import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { WebSocketServer } from "ws";
import WebSocket from "ws";
import { IGNORED, relative, spliceWatch, watchFrame } from "../src/watch.ts";

/**
 * The desktop follows a workspace's files instead of re-reading them. The tool server already
 * watches (`crates/ide/src/watches.rs`); the bench starts one watch on the root and splices its
 * events, relativised and filtered, because the pod filters nothing and speaks absolute paths.
 */
const ROOT = "/home/kl/workspaces/api";

test("an event is named the way every view above names it", () => {
  assert.equal(relative(ROOT, `${ROOT}/src/main.ts`), "src/main.ts");
  assert.equal(relative(`${ROOT}/`, `${ROOT}/a.ts`), "a.ts", "a trailing slash on the root is not part of a name");
  assert.equal(relative(ROOT, ROOT), undefined, "the root itself is not a row");
  assert.equal(relative(ROOT, "/etc/passwd"), undefined, "nothing outside the workspace crosses");
  assert.equal(relative(ROOT, 7), undefined);
});

test("what a build writes never reaches the desktop", () => {
  for (const dir of IGNORED) assert.equal(relative(ROOT, `${ROOT}/${dir}/x`), undefined, dir);
  assert.equal(relative(ROOT, `${ROOT}/src/node_modules_helper.ts`), "src/node_modules_helper.ts", "a NAME that starts with an ignored one is a real file");
  assert.equal(relative(ROOT, `${ROOT}/a/node_modules/b/c.js`), undefined, "ignored anywhere in the path, not only at the top");
});

test("the stream's own notices become one reason to read everything again", () => {
  assert.deepEqual(watchFrame(ROOT, JSON.stringify({ path: `${ROOT}/a.ts`, kind: "modify" })), { path: "a.ts", kind: "modify" });
  assert.deepEqual(watchFrame(ROOT, JSON.stringify({ dropped_events: 12 })), { resync: true }, "events the pod dropped cannot be patched in");
  assert.deepEqual(watchFrame(ROOT, JSON.stringify({ state: "stopped" })), { resync: true });
  assert.equal(watchFrame(ROOT, "not json"), undefined);
  assert.equal(watchFrame(ROOT, JSON.stringify({ path: `${ROOT}/.cache/x`, kind: "create" })), undefined);
});

test("the bench starts one watch on the root and splices what it says", async () => {
  const asked: { url: string; body?: string }[] = [];
  const srv = http.createServer((req, res) => {
    let raw = "";
    req.on("data", (d) => (raw += d));
    req.on("end", () => {
      asked.push({ url: req.url ?? "", body: raw || undefined });
      res.setHeader("content-type", "application/json");
      if (req.url === "/healthz") return res.end(JSON.stringify({ ok: true, root: ROOT }));
      if (req.url === "/tools/watch") return res.end(JSON.stringify({ id: "w-1" }));
      res.end(JSON.stringify({ ok: true }));
    });
  });
  const wss = new WebSocketServer({ server: srv, path: "/stream/watch/w-1" });
  wss.on("connection", (up) => {
    up.send(JSON.stringify({ path: `${ROOT}/src/new.ts`, kind: "create" }));
    up.send(JSON.stringify({ path: `${ROOT}/node_modules/dep/index.js`, kind: "modify" }));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const address = `127.0.0.1:${(srv.address() as { port: number }).port}`;

  // The desktop's end of the splice, as the bench's upgrade handler hands it over.
  const client = new WebSocketServer({ port: 0 });
  const seen: unknown[] = [];
  const down = new Promise<void>((done) => {
    client.on("connection", (w) => {
      void spliceWatch(w, address);
      w.on("close", () => done());
    });
  });
  const desktop = new WebSocket(`ws://127.0.0.1:${(client.address() as { port: number }).port}`);
  desktop.on("message", (d: Buffer) => seen.push(JSON.parse(d.toString())));
  await new Promise((r) => setTimeout(r, 300));

  assert.deepEqual(seen, [{ path: "src/new.ts", kind: "create" }], "only what a view would draw, named relative to the workspace");
  assert.equal(asked.find((a) => a.url === "/tools/watch")?.body, JSON.stringify({ paths: ["."] }), "one recursive watch on the workspace root");

  desktop.close();
  await down;
  await new Promise((r) => setTimeout(r, 100));
  assert.ok(
    asked.some((a) => a.url === "/tools/watch_stop"),
    "a watch whose desktop is gone is stopped: a pod allows 32",
  );
  client.close();
  wss.close();
  await new Promise((r) => srv.close(r));
});
