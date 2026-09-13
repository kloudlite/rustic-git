import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import WebSocket from "ws";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";

async function up() {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-srv-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  return { bench, srv, base, ws: (p: string) => new WebSocket(`ws://127.0.0.1:${srv.port}${p}`), down: async () => (bench.stop(), await srv.close()) };
}
const opened = (w: WebSocket) => new Promise((r) => w.once("open", r));
const frames = (w: WebSocket, out: Record<string, unknown>[]) => w.on("message", (d) => out.push(JSON.parse(d.toString())));
const settle = () => new Promise((r) => setTimeout(r, 200));

test("two clients on one session see the same events in the same order; responses go to their sender", async () => {
  const t = await up();
  const a = t.ws("/sessions/s-1/rpc"), b = t.ws("/sessions/s-1/rpc");
  await Promise.all([opened(a), opened(b)]);
  const fa: Record<string, unknown>[] = [], fb: Record<string, unknown>[] = [];
  frames(a, fa);
  frames(b, fb);
  a.send(JSON.stringify({ id: "1", type: "prompt", message: "hi" }));
  b.send(JSON.stringify({ id: "1", type: "get_state" }));
  await settle();
  const events = (f: Record<string, unknown>[]) => f.filter((x) => x.type !== "response").map((x) => x.type);
  assert.deepEqual(events(fa), events(fb));
  assert.deepEqual(events(fa), ["agent_start", "message_update", "agent_end"]);
  assert.deepEqual(fa.filter((x) => x.type === "response").map((x) => x.command), ["prompt"]);
  assert.deepEqual(fb.filter((x) => x.type === "response").map((x) => x.command), ["get_state"]);
  assert.ok(fa.every((x) => x.type !== "response" || x.id === "1"));
  a.close(); b.close();
  await t.down();
});

test("REST: list, create, messages, archive, exchanges by both views, delete", async () => {
  const t = await up();
  const quiet = await (await fetch(t.base + "/healthz")).json();
  assert.equal(quiet.clients, 0);
  assert.equal(typeof quiet.idleSince, "number", "no client and nothing running is idle");
  const ev = t.ws("/events");
  await opened(ev);
  const held = await (await fetch(t.base + "/healthz")).json();
  assert.equal(held.clients, 1);
  assert.equal(held.idleSince, null, "a connected client holds the bench up");
  const seen: Record<string, unknown>[] = [];
  frames(ev, seen);
  const j = async (method: string, p: string, body?: unknown) => {
    const r = await fetch(t.base + p, { method, headers: body ? { "content-type": "application/json" } : {}, body: body ? JSON.stringify(body) : undefined });
    return { status: r.status, body: r.status === 204 ? null : await r.json() };
  };
  assert.equal((await j("POST", "/sessions")).status, 201);
  assert.deepEqual((await j("GET", "/sessions")).body.map((s: { id: string }) => s.id), ["s-1", "s-2"]);
  await t.bench.rpc("s-1", { type: "prompt", message: "exchange" });
  await settle();
  assert.equal((await j("GET", "/sessions/s-1/messages?after=1&limit=5")).body.total, 2);
  assert.equal((await j("GET", "/exchanges?session=s-1")).body[0].workspace, "api");
  assert.equal((await j("GET", "/exchanges?workspace=api")).body[0].session, "s-1");
  assert.equal((await j("GET", "/exchanges")).status, 400);
  assert.equal((await j("POST", "/sessions/s-2/archive")).body.archived, true);
  assert.equal((await j("POST", "/sessions/s-1/archive")).status, 409);
  assert.equal((await j("DELETE", "/sessions/nope", { stop: true })).status, 404);
  assert.equal((await j("DELETE", "/sessions/s-2", { stop: true })).status, 204);
  assert.ok(seen.some((e) => e.type === "sessions") && seen.some((e) => e.type === "exchange"));
  ev.close();
  await t.down();
});
