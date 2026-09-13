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
