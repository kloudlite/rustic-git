import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { FAKE } from "./fake-pi.ts";
import { BenchClient } from "../../src/bench-client.ts";

const BIN = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "src", "main.ts");
import { until } from "./wait.ts";

/** The real harness-bench bin with a fake pi, resolved once it says where it listens. */
function run(dir: string, port: number): Promise<{ c: ChildProcess; port: number }> {
  const c = spawn(BIN, ["--host", "127.0.0.1", "--port", String(port), "--dir", dir, "--model", "fake/m"], {
    env: { ...process.env, HARNESS_PI_BIN: FAKE, KL_BENCH_IDLE_SECS: "", TERMINATION_LOG: path.join(dir, "term") },
  });
  let out = "";
  return new Promise((resolve, reject) => {
    c.stdout!.on("data", (d) => {
      out += d;
      const m = /listening on [\d.]+:(\d+)/.exec(out);
      if (m) resolve({ c, port: Number(m[1]) });
    });
    c.on("exit", (code) => reject(new Error(`harness-bench exited ${code}`)));
  });
}
const stop = (c: ChildProcess) => new Promise<void>((r) => (c.exitCode !== null ? r() : (c.once("exit", () => r()), c.kill("SIGTERM"))));

test("rpc streams as pi:event, offline refuses and reads the cache, a restarted bench resyncs and streams again", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-cl-"));
  const cacheFile = path.join(os.tmpdir(), `bench-cl-cache-${process.pid}.json`);
  let b = await run(path.join(dir, "bench"), 0);
  const port = b.port;
  const seen: Record<string, unknown>[] = [];
  const c = new BenchClient(`http://127.0.0.1:${port}`, (e) => seen.push(e), cacheFile);
  try {
    c.start();
    await until(() => c.connected() && seen.some((e) => e.type === "bench" && e.connected === true), 5_000, "the client to connect");
    const sid = (await c.rest<{ id: string }[]>("GET", "/sessions"))[0].id;
    await c.rpc(sid, { type: "prompt", message: "hi" });
    await until(() => seen.some((e) => e.pi === sid && e.type === "agent_end"), 5_000, "agent_end");
    assert.deepEqual(seen.filter((e) => e.pi === sid && e.type !== "response").map((e) => e.type), ["agent_start", "message_update", "agent_end"]);
    assert.equal((await c.messages(sid)).length, 2);

    // Only the session's socket drops, /events stays up: the pending rpc is answered.
    const pending = c.rpc(sid, { type: "get_state" });
    setImmediate(() => (c as unknown as { sockets: Map<string, { terminate(): void }> }).sockets.get(sid)!.terminate());
    const t0 = Date.now();
    const r = await pending;
    assert.equal(r.success, false);
    assert.match(String(r.error), /bench connection closed/);
    assert.ok(Date.now() - t0 < 1000, "answered promptly");
    assert.equal(c.connected(), true, "/events is still up");
    assert.equal((await c.rpc(sid, { type: "get_state" })).success, true, "a fresh socket opens on the next rpc");

    await stop(b.c);
    await until(() => !c.connected() && seen.some((e) => e.type === "bench" && e.connected === false), 5_000, "the client to see the bench go");
    await assert.rejects(c.rpc(sid, { type: "prompt", message: "lost?" }), /not connected to the bench; nothing was sent/);
    assert.equal((await c.messages(sid)).length, 2, "offline reads come from the cache");
    assert.ok(c.cached().sessions.length >= 1, "offline list comes from the cache");
    const onDisk = fs.readFileSync(cacheFile, "utf8");
    assert.ok(!/token|secret|authorization/i.test(onDisk), "no credential in the cache");

    const before = seen.length;
    b = await run(path.join(dir, "bench"), port);
    await until(() => c.connected() && seen.slice(before).some((e) => e.type === "bench:resync"), 15_000, "reconnect and resync");
    await c.rpc(sid, { type: "prompt", message: "again" });
    await until(() => seen.slice(before).some((e) => e.pi === sid && e.type === "agent_end"), 5_000, "agent_end after reconnect");
    assert.deepEqual(seen.slice(before).filter((e) => e.pi === sid && e.type !== "response").map((e) => e.type), ["agent_start", "message_update", "agent_end"], "new events arrive after reconnect");
  } finally {
    c.close();
    await stop(b.c);
    fs.rmSync(dir, { recursive: true, force: true });
    fs.rmSync(cacheFile, { force: true });
  }
});

test("a second device that never sent anything streams another device's live turn", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-cl2-"));
  const b = await run(path.join(dir, "bench"), 0);
  const base = `http://127.0.0.1:${b.port}`;
  const seenA: Record<string, unknown>[] = [];
  const seenB: Record<string, unknown>[] = [];
  const a = new BenchClient(base, (e) => seenA.push(e), path.join(dir, "a.json"));
  const bb = new BenchClient(base, (e) => seenB.push(e), path.join(dir, "b.json"));
  try {
    a.start();
    bb.start();
    await until(() => a.connected() && bb.connected(), 5_000, "both clients to connect");
    const sid = (await a.rest<{ id: string }[]>("GET", "/sessions"))[0].id;
    await a.rpc(sid, { type: "prompt", message: "hi" });
    await until(() => seenB.some((e) => e.pi === sid && e.type === "agent_end"), 5_000, "B to see A's agent_end");
    assert.deepEqual(seenB.filter((e) => e.pi === sid && e.type !== "response").map((e) => e.type).filter((t) => t !== "sessions"), ["agent_start", "message_update", "agent_end"]);
    await until(() => seenA.some((e) => e.pi === sid && e.type === "agent_end"), 5_000, "A's agent_end");
    assert.deepEqual(seenA.filter((e) => e.pi === sid && e.type !== "response").map((e) => e.type), ["agent_start", "message_update", "agent_end"], "A, holding the session socket, sees each event once");
  } finally {
    a.close();
    bb.close();
    await stop(b.c);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a tunnel nonce rides every REST request and WebSocket upgrade; the cache is keyed by cacheKey", async () => {
  const http = await import("node:http");
  const seen: (string | undefined)[] = [];
  const srv = http.createServer((q, r) => (seen.push(q.headers["x-kl-tunnel"] as string | undefined), r.end("[]")));
  srv.on("upgrade", (q, s) => (seen.push(q.headers["x-kl-tunnel"] as string | undefined), s.destroy()));
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const base = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;
  const cacheFile = path.join(os.tmpdir(), `bench-cl-nonce-${process.pid}.json`);
  fs.writeFileSync(cacheFile, JSON.stringify({ base: "me@api", sessions: ["kept"], exchanges: [], messages: {} }));
  const c = new BenchClient(base, () => undefined, cacheFile, "me@api", "n0nce");
  try {
    assert.deepEqual(c.cached().sessions, ["kept"]);
    await c.rest("GET", "/sessions");
    c.start();
    await until(() => seen.length >= 2, 5_000, "the upgrade");
    assert.deepEqual(seen.slice(0, 2), ["n0nce", "n0nce"]);
  } finally {
    c.close();
    srv.close();
    fs.rmSync(cacheFile, { force: true });
  }
});

/**
 * The desktop's `/events` socket was reaped by the Cloudflare edge after ~100 s of no
 * client→server traffic (`bins/gateway/src/tunnel.rs:23`): 79–136 s in the gateway logs, ~25 times
 * an hour, on a pod that was never touched. The client now pings, so the edge sees traffic.
 */
test("the client pings its events socket, and stops when it closes", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-ping-"));
  const cacheFile = path.join(dir, "cache.json");
  const b = await run(path.join(dir, "bench"), 0);
  const c = new BenchClient(`http://127.0.0.1:${b.port}`, () => {}, cacheFile);
  try {
    c.start();
    await until(() => c.connected(), 5_000, "the client to connect");
    // The keep-alive timer is armed for as long as the socket is up...
    const timer = (c as unknown as { ping?: NodeJS.Timeout }).ping;
    assert.ok(timer, "a ping interval is running while /events is open");
    // ...and the bench answers a ping, which is what the edge needs to see.
    const ev = (c as unknown as { events: { ping(): void; once(e: string, f: () => void): void } }).events;
    await new Promise<void>((r) => (ev.once("pong", () => r()), ev.ping()));
    // Closing the client stops it: nothing is left pinging a socket that is gone.
    c.close();
    assert.equal((c as unknown as { ping?: NodeJS.Timeout }).ping, undefined);
  } finally {
    c.close();
    await stop(b.c);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a stalled REST body is bounded by the request deadline", async () => {
  const http = await import("node:http");
  const srv = http.createServer((_q, r) => {
    r.writeHead(200, { "content-type": "application/json" });
    r.write("[");
  });
  await new Promise<void>((resolve) => srv.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;
  const c = new BenchClient(base, () => undefined, path.join(os.tmpdir(), `bench-timeout-${process.pid}.json`), base, undefined, 40);
  const started = Date.now();
  try {
    await assert.rejects(c.rest("GET", "/stalled"), /bench request timed out after 40ms/);
    assert.ok(Date.now() - started < 1000, "a body that never ends must not hold the caller");
  } finally {
    c.close();
    await new Promise<void>((resolve) => srv.close(() => resolve()));
  }
});

test("structured operation errors preserve code, status, and revisions", async () => {
  const http = await import("node:http");
  const srv = http.createServer((_req, res) => {
    res.writeHead(409, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { code: "stale_revision", message: "revision changed", expectedRevision: 3, actualRevision: 4 } }));
  });
  await new Promise<void>((resolve) => srv.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;
  const client = new BenchClient(base, () => undefined, path.join(os.tmpdir(), `bench-errors-${process.pid}.json`));
  try {
    await assert.rejects(
      client.cancelOperation("op-1", 3, { authorization: "Bearer person", "x-kl-owner": "alice", "x-kl-login": "alice" }),
      (error: Error & { code?: string; expectedRevision?: number; actualRevision?: number; status?: number }) => error.message === "revision changed" && error.code === "stale_revision" && error.expectedRevision === 3 && error.actualRevision === 4 && error.status === 409,
    );
  } finally {
    client.close();
    await new Promise<void>((resolve) => srv.close(() => resolve()));
  }
});
