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
    // Where they RUN is the half the names cannot show, and for a bench session the answer is
    // NOWHERE: no tool server and no builtins (spec §3.1). It used to be handed its own pod's
    // loopback, which is what had the model asking for the bench itself to be started.
    assert.equal(toolsAddress, undefined);
    assert.equal(builtinTools, false);
    for (const none of ["bash", "read", "write", "process", "kl_repo_clone"]) assert.ok(!tools.includes(none), `${none}: ${tools.join(",")}`);
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

/**
 * Nothing pinged on `/events`, so the Cloudflare edge reaped the socket after ~100 s of no
 * client→server traffic (`bins/gateway/src/tunnel.rs:23`) — 79–136 s in the gateway logs, ~25
 * times an hour, on a pod that was never restarted. The heartbeat runs both ways: the client pings
 * to keep the edge open, and the server terminates a socket that stops answering rather than
 * writing events into one nobody reads.
 */
test("the events socket is kept alive, and a half-open one is terminated", async () => {
  const t = await up();
  try {
    const w = t.ws("/events");
    await opened(w);
    // The server answers a client ping, which is what keeps the edge from reaping the socket.
    const ponged = new Promise<void>((r) => w.once("pong", () => r()));
    w.ping();
    await ponged;

    // A socket that never answers is swept; one that does is kept. The sweep is on a 30 s timer, so
    // the behaviour is exercised directly rather than by waiting for it.
    // A socket that ANSWERS survives every round: two sweeps, still open.
    t.srv.sweepOnce();
    await new Promise((r) => setTimeout(r, 50));
    t.srv.sweepOnce();
    assert.equal(w.readyState, w.OPEN, "a socket that pongs is kept");

    // One that stops answering is terminated rather than written into forever. A real half-open
    // socket still LOOKS writable, which is the whole problem; here the pong simply never arrives,
    // so two rounds pass with the flag unset.
    w.removeAllListeners("ping");
    w.on("ping", () => {}); // swallowed: no pong goes back, as from a dead peer
    t.srv.sweepOnce(); // marks it unanswered
    t.srv.sweepOnce(); // and it missed the round: terminate
    await until(() => w.readyState === w.CLOSED || w.readyState === w.CLOSING, 5_000, "the half-open socket to be closed");
  } finally {
    await t.down();
  }
});

/** The pool keeps a connection warm for 15 s; Node's 5 s default dropped it every ~6 s. */
test("the server outlives the client's idle", async () => {
  const t = await up();
  try {
    assert.ok(t.srv.server.keepAliveTimeout >= 65_000, `keepAliveTimeout ${t.srv.server.keepAliveTimeout}`);
    assert.ok(t.srv.server.headersTimeout > t.srv.server.keepAliveTimeout, "headers must outlast keep-alive");
  } finally {
    await t.down();
  }
});

/**
 * D2, regressed (api-test-report Round 2). The desktop's socket calls `bench.rpc()` directly, which
 * had no mid-turn handling, so a plain `prompt` 0.8 s after another was refused by pi and dropped —
 * on a WARM child too, and this socket is the only prompt path there is.
 */
test("two prompts close together over the socket both land", async () => {
  const t = await up();
  try {
    const a = t.ws("/sessions/s-1/rpc");
    await opened(a);
    const frames: Record<string, unknown>[] = [];
    a.on("message", (d) => frames.push(JSON.parse(d.toString())));

    // The first turn does not end (the stand-in answers "…hang" with silence), so the second
    // prompt arrives while a turn is genuinely in flight — the case that was dropped. A fast
    // stand-in would finish before the gap and prove nothing.
    a.send(JSON.stringify({ id: "1", type: "prompt", message: "ONE hang" }));
    await until(() => frames.some((f) => f.type === "agent_start"), 5_000, "the first turn to start");
    await new Promise((r) => setTimeout(r, 800));
    a.send(JSON.stringify({ id: "2", type: "prompt", message: "TWO" }));

    await until(() => frames.filter((f) => f.type === "response" && f.id === "2").length > 0, 8_000, "the second prompt to be answered");
    const second = frames.find((f) => f.type === "response" && f.id === "2")!;
    assert.notEqual(second.success, false, `the second prompt was refused: ${JSON.stringify(second)}`);
    // A person's own line mid-turn is a STEER, first (spec §4): it reaches the turn that is
    // running, so a second line is read in the work it is about rather than after it.
    assert.equal(second.command, "steer", "a person's line reaches the turn it was typed into");
    a.close();
  } finally {
    await t.down();
  }
});

/**
 * The operation control envelope (`{error:{code,message}}`) is `/operations/*`'s own shape.
 * `operationControlError` matches on `.code` alone, so an ordinary route whose handler throws an
 * error that happens to carry `code: "not_found"` (a session or workspace lookup, say) answered
 * with that object envelope instead of the bench's own `{error: "<message>"}` — the bug this gate
 * closes.
 */
test("a non-operation route's thrown error keeps the bench's own string envelope, not the operation object one", async () => {
  const t = await up();
  try {
    const original = t.bench.models;
    t.bench.models = async () => { throw Object.assign(new Error("nope"), { code: "not_found" }); };
    try {
      const r = await fetch(t.base + "/models");
      const body = await r.json();
      assert.equal(typeof body.error, "string", JSON.stringify(body));
      assert.equal(body.error, "nope");
    } finally {
      t.bench.models = original;
    }
  } finally {
    await t.down();
  }
});

/**
 * The mirror: a `/operations/*` request whose operation source throws the very same shape of
 * error still answers the operation control's own object envelope — `isOperationRoute` must let
 * this one through, never blanket-suppress it.
 */
test("an operations route's thrown error still answers the operation object envelope", async () => {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-opsrv-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0, "127.0.0.1", undefined, undefined, {
    operationSource: {
      inspect: async () => { throw Object.assign(new Error("no such operation"), { code: "not_found" }); },
      events: async () => ({ events: [], hasMore: false }),
      cancel: async () => ({}),
      recordDecision: async () => ({}),
      provideInput: async () => ({}),
    },
    operationAuthorizer: async (request) => (request.authorization === "Bearer person" ? { actorId: "alice", tenantId: "alice", tokenKind: "person" } : undefined),
  });
  try {
    const r = await fetch(`http://127.0.0.1:${srv.port}/operations/op-1`, { headers: { authorization: "Bearer person", "x-kl-owner": "alice", "x-kl-login": "alice" } });
    const body = await r.json();
    assert.equal(r.status, 404);
    assert.equal(body.error.code, "operation_not_found");
  } finally {
    await bench.stop();
    await srv.close();
  }
});
