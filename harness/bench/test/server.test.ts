import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";

async function up() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", turn: async () => "ok", platform: undefined });
  await bench.start();
  const { port, close } = await serve(bench, 0);
  const base = `http://127.0.0.1:${port}`;
  return { bench, base, close: async () => { await close(); await bench.stop(); } };
}

test("healthz reports ok", async () => {
  const { base, close } = await up();
  const r = await fetch(`${base}/healthz`);
  assert.equal(r.status, 200);
  assert.equal((await r.json()).ok, true);
  await close();
});

test("POST /sessions creates a main session; GET /sessions/{id}/children and /abort work", async () => {
  const { base, close } = await up();
  const created = await (await fetch(`${base}/sessions`, { method: "POST" })).json();
  const children = await fetch(`${base}/sessions/${created.id}/children`);
  assert.equal(children.status, 200);
  assert.deepEqual(await children.json(), []);
  const abort = await fetch(`${base}/sessions/${created.id}/abort`, { method: "POST" });
  assert.equal(abort.status, 204);
  await close();
});

test("send then messages returns {messages, rows, total}", async () => {
  const { base, close } = await up();
  const created = await (await fetch(`${base}/sessions`, { method: "POST" })).json();
  const sent = await fetch(`${base}/sessions/${created.id}/send`, { method: "POST", body: JSON.stringify({ text: "hi" }) });
  assert.equal(sent.status, 200);
  await new Promise((res) => setTimeout(res, 60));
  const m = await (await fetch(`${base}/sessions/${created.id}/messages`)).json();
  assert.ok(Array.isArray(m.messages) && Array.isArray(m.rows) && typeof m.total === "number");
  await close();
});

test("send to a closed session maps to HTTP 409", async () => {
  const { bench, base, close } = await up();
  const created = await (await fetch(`${base}/sessions`, { method: "POST" })).json();
  bench.sessions.update(created.id, { state: "closed" });
  const r = await fetch(`${base}/sessions/${created.id}/send`, { method: "POST", body: JSON.stringify({ text: "hi" }) });
  assert.equal(r.status, 409);
  await close();
});

test("btw routes return the 'btw is gone' error", async () => {
  const { base, close } = await up();
  const created = await (await fetch(`${base}/sessions`, { method: "POST" })).json();
  const post = await fetch(`${base}/sessions/${created.id}/btw`, { method: "POST", body: JSON.stringify({ question: "?" }) });
  assert.equal((await post.json()).error, "btw is gone; the bench runs the sys-1 engine");
  const get = await fetch(`${base}/sessions/${created.id}/btw`);
  assert.equal((await get.json()).error, "btw is gone; the bench runs the sys-1 engine");
  await close();
});

test("workspace session open, send, and workspaceMessages", async () => {
  const { base, close } = await up();
  const opened = await (await fetch(`${base}/workspaces/api/session`, { method: "POST" })).json();
  await fetch(`${base}/sessions/${opened.id}/send`, { method: "POST", body: JSON.stringify({ text: "ship it" }) });
  await new Promise((res) => setTimeout(res, 60));
  const wm = await (await fetch(`${base}/workspaces/api/messages`)).json();
  assert.ok(Array.isArray(wm.rows));
  assert.ok(wm.rows.some((r: { kind: string }) => r.kind === "turn.end"));
  await close();
});
