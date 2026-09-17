import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import WebSocket from "ws";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

async function up() {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-srv-")), readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const srv = await serve(bench, 0);
    const base = `http://127.0.0.1:${srv.port}`;
    return { bench, srv, base, ws: (p: string) => new WebSocket(`ws://127.0.0.1:${srv.port}${p}`), down: async () => (await bench.stop(), await srv.close()) };
  } catch (e) {
    await bench.stop();
    throw e;
  }
}
const opened = (w: WebSocket) => new Promise((r) => w.once("open", r));
const frames = (w: WebSocket, out: Record<string, unknown>[]) => w.on("message", (d) => out.push(JSON.parse(d.toString())));
const health = async (base: string) => (await fetch(base + "/healthz")).json();

test("two clients on one session see the same events in the same order; responses go to their sender", async () => {
  const t = await up();
  try {
    const a = t.ws("/sessions/s-1/rpc"), b = t.ws("/sessions/s-1/rpc");
    await Promise.all([opened(a), opened(b)]);
    const fa: Record<string, unknown>[] = [], fb: Record<string, unknown>[] = [];
    frames(a, fa);
    frames(b, fb);
    a.send(JSON.stringify({ id: "1", type: "prompt", message: "hi" }));
    b.send(JSON.stringify({ id: "1", type: "get_state" }));
    await until(() => fa.some((x) => x.type === "agent_end") && fb.some((x) => x.type === "agent_end") && fb.some((x) => x.command === "get_state"), 5_000, "both clients' frames");
    const events = (f: Record<string, unknown>[]) => f.filter((x) => x.type !== "response").map((x) => x.type);
    assert.deepEqual(events(fa), events(fb));
    assert.deepEqual(events(fa), ["agent_start", "message_update", "agent_end"]);
    assert.deepEqual(fa.filter((x) => x.type === "response").map((x) => x.command), ["prompt"]);
    assert.deepEqual(fb.filter((x) => x.type === "response").map((x) => x.command), ["get_state"]);
    assert.ok(fa.every((x) => x.type !== "response" || x.id === "1"));
    a.close(); b.close();
  } finally {
    await t.down();
  }
});

test("REST: list, create, messages, archive, exchanges by both views, delete", async () => {
  const t = await up();
  try {
    const quiet = await health(t.base);
    assert.equal(quiet.clients, 0);
    assert.equal(typeof quiet.idleSince, "number", "no client and nothing running is idle");
    assert.equal(quiet.idle, new Date(quiet.idleSince).toISOString(), "healthz carries the moment --ping reads");
    const ev = t.ws("/events");
    await opened(ev);
    const held = await health(t.base);
    assert.equal(held.clients, 1);
    assert.equal(held.idleSince, null, "a connected client holds the bench up");
    assert.equal(held.idle, undefined, "and no idle moment for the probe to fail on");
    const seen: Record<string, unknown>[] = [];
    frames(ev, seen);
    const j = async (method: string, p: string, body?: unknown) => {
      const r = await fetch(t.base + p, { method, headers: body ? { "content-type": "application/json" } : {}, body: body ? JSON.stringify(body) : undefined });
      return { status: r.status, body: r.status === 204 ? null : await r.json() };
    };
    assert.equal((await j("POST", "/sessions")).status, 201);
    assert.deepEqual((await j("GET", "/sessions")).body.map((s: { id: string }) => s.id), ["s-1", "s-2"]);
    await t.bench.rpc("s-1", { type: "prompt", message: "exchange" });
    await until(() => t.bench.exchanges.bySession("s-1").length > 0 && !t.bench.busy() && seen.some((e) => e.type === "exchange"), 5_000, "the exchange and agent_end");
    assert.equal((await j("GET", "/sessions/s-1/messages?after=1&limit=5")).body.total, 2);
    assert.equal((await j("GET", "/exchanges?session=s-1")).body[0].workspace, "api");
    assert.equal((await j("GET", "/exchanges?workspace=api")).body[0].session, "s-1");
    assert.equal((await j("GET", "/exchanges")).status, 400);
    assert.equal((await j("POST", "/sessions/s-2/archive")).body.archived, true);
    assert.equal((await j("POST", "/sessions/s-1/archive")).status, 409);
    assert.equal((await j("DELETE", "/sessions/nope", { stop: true })).status, 404);
    assert.equal((await j("DELETE", "/sessions/s-2", { stop: true })).status, 204);
    await until(() => seen.some((e) => e.type === "sessions"), 5_000, "a sessions frame");
    ev.close();
  } finally {
    await t.down();
  }
});

test("hostile paths and bodies: bad escapes, traversal ids and oversized bodies are refused without killing the server", async () => {
  const t = await up();
  try {
    assert.equal((await fetch(t.base + "/sessions/%E0/messages")).status, 400);
    assert.equal((await fetch(t.base + "/sessions/..%2F..%2Fx/btw")).status, 400);
    assert.equal((await fetch(t.base + "/workspaces/..%2Fx/messages")).status, 400);
    assert.equal((await fetch(t.base + "/sessions/nope/btw")).status, 404);
    const bad = t.ws("/sessions/%E0/rpc");
    await new Promise((r) => bad.once("error", r));
    const walk = t.ws("/sessions/..%2Fx/rpc");
    await new Promise((r) => walk.once("error", r));
    const small = await serve(t.bench, 0, "127.0.0.1", undefined, 16);
    try {
      const r = await fetch(`http://127.0.0.1:${small.port}/import`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ items: [], loose: [], pad: "x".repeat(1000) }) }).catch((e) => e);
      assert.ok(r instanceof Error || r.status === 413, `413 or a closed connection, got ${r.status}`);
    } finally {
      await small.close();
    }
    const w = t.ws("/events");
    await opened(w);
    assert.equal((await health(t.base)).clients, 1);
    w.close();
    await new Promise((r) => w.once("close", r));
    let h: { clients: number; ok: boolean } = { clients: -1, ok: false };
    await until(async () => (h = await health(t.base)).clients === 0, 5_000, "a closed socket to stop counting");
    assert.equal(h.ok, true);
  } finally {
    await t.down();
  }
});

test("a bad archive or restore id and a malformed import answer 404/400 and leave the folder writable", async () => {
  const t = await up();
  try {
    const post = (p: string, body?: unknown) => fetch(t.base + p, { method: "POST", headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
    assert.equal((await post("/sessions/s-99/archive")).status, 404);
    assert.equal((await post("/sessions/s-99/restore")).status, 404);
    assert.equal((await post("/import", { items: [null], loose: [] })).status, 400);
    assert.equal((await post("/import", { items: "x" })).status, 400);
    assert.equal((await health(t.base)).writable, true);
  } finally {
    await t.down();
  }
});

test("import refuses a row with a field outside the allow-list, and writes nothing", async () => {
  const t = await up();
  try {
    const row = { id: "s-7", name: "old", seq: 7, created: 1, lastActive: 1, archived: false };
    const post = (r: unknown) => fetch(t.base + "/import", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ items: [{ row: r, name: "s-7.jsonl", content: "{}\n" }], loose: [] }) });
    for (const bad of [{ ...row, seq: "x" }, { ...row, seq: -1 }, { ...row, id: "../x" }, { ...row, kind: "root" }, { ...row, kind: "workspace", workspace: "Not_A_Label", target: "api" }, { ...row, archived: "yes" }, { ...row, lastActive: "now" }, { ...row, evil: 1 }]) {
      const r = await post(bad);
      assert.equal(r.status, 400, JSON.stringify(bad));
    }
    assert.equal(t.bench.sessions.get("s-7"), undefined);
    assert.equal((await health(t.base)).writable, true);
    assert.equal((await post(row)).status, 200, "the same row, well-formed, is taken");
  } finally {
    await t.down();
  }
});

test("btw is refused on a workspace thread", async () => {
  const t = await up();
  try {
    await t.bench.openWorkspace("api");
    const r = await fetch(t.base + "/sessions/w-api/btw", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ question: "q" }) });
    assert.equal(r.status, 400);
    assert.equal(((await r.json()) as { error: string }).error, "btw is only for bench sessions");
  } finally {
    await t.down();
  }
});

test("a WebSocket frame over the body cap closes the socket", async () => {
  const t = await up();
  const small = await serve(t.bench, 0, "127.0.0.1", undefined, 1024);
  try {
    const w = new WebSocket(`ws://127.0.0.1:${small.port}/sessions/s-1/rpc`);
    await opened(w);
    const code = new Promise((r) => w.once("close", r));
    w.send("x".repeat(4096));
    assert.equal(await code, 1009);
  } finally {
    await small.close();
    await t.down();
  }
});

test("a /pty upgrade with a scope that is neither the bench nor a workspace id is refused", async () => {
  const t = await up();
  try {
    const w = new WebSocket(`ws://127.0.0.1:${t.srv.port}/pty?scope=${encodeURIComponent("../x")}`);
    const e = await new Promise<Error>((r) => w.once("error", r));
    assert.match(e.message, /400/);
  } finally {
    await t.down();
  }
});

test("GET /sessions/{id}/tools answers what that session can call, and 404s an id that is not one", async () => {
  const t = await up();
  try {
    const created = await (await fetch(t.base + "/sessions", { method: "POST" })).json() as { id: string };
    const r = await fetch(`${t.base}/sessions/${created.id}/tools`);
    assert.equal(r.status, 200);
    const { tools, toolsAddress, builtinTools } = await r.json() as { tools: string[]; toolsAddress?: string; builtinTools: boolean };
    // Where they RUN is the half the names cannot show: its own workspace container's tool server,
    // on loopback, with pi's own builtins off.
    assert.equal(toolsAddress, "127.0.0.1:7788");
    assert.equal(builtinTools, false);
    // The seven are there, but they are the tool server's, run in the bench's OWN workspace — the
    // built-ins, which would have run in the bench container, are what `--no-builtin-tools` removed.
    for (const own of ["bash", "read", "write"]) assert.ok(tools.includes(own), `${own}: ${tools.join(",")}`);
    assert.ok(tools.includes("ask"), tools.join(","));
    assert.equal((await fetch(t.base + "/sessions/nope/tools")).status, 404);
  } finally {
    await t.down();
  }
});

/**
 * A window has to name what will answer before any session row has loaded. Without this the bench
 * thread's composer said "no model" while the bench had one all along (owner, 2026-09-17).
 */
test("the bench's default model is on /healthz and /bootstrap", async () => {
  const t = await up();
  try {
    assert.equal((await health(t.base)).model, "fake/m");
    const boot = await (await fetch(t.base + "/bootstrap")).json();
    assert.equal(boot.model, "fake/m");
    assert.ok(Array.isArray(boot.sessions));
  } finally {
    await t.down();
  }
});
