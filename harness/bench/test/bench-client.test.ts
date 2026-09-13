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
const settle = (ms = 250) => new Promise((r) => setTimeout(r, ms));

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
    await settle(500);
    assert.equal(c.connected(), true);
    assert.ok(seen.some((e) => e.type === "bench" && e.connected === true));
    const sid = (await c.rest<{ id: string }[]>("GET", "/sessions"))[0].id;
    await c.rpc(sid, { type: "prompt", message: "hi" });
    await settle();
    assert.deepEqual(seen.filter((e) => e.pi === sid && e.type !== "response").map((e) => e.type), ["agent_start", "message_update", "agent_end"]);
    assert.equal((await c.messages(sid)).length, 2);

    await stop(b.c);
    await settle();
    assert.equal(c.connected(), false);
    assert.ok(seen.some((e) => e.type === "bench" && e.connected === false));
    await assert.rejects(c.rpc(sid, { type: "prompt", message: "lost?" }), /not connected to the bench; nothing was sent/);
    assert.equal((await c.messages(sid)).length, 2, "offline reads come from the cache");
    assert.ok(c.cached().sessions.length >= 1, "offline list comes from the cache");
    const onDisk = fs.readFileSync(cacheFile, "utf8");
    assert.ok(!/token|secret|authorization/i.test(onDisk), "no credential in the cache");

    const before = seen.length;
    b = await run(path.join(dir, "bench"), port);
    await settle(2500);
    assert.equal(c.connected(), true, "reconnected");
    assert.ok(seen.slice(before).some((e) => e.type === "bench:resync"), "resynced");
    await c.rpc(sid, { type: "prompt", message: "again" });
    await settle();
    assert.deepEqual(seen.slice(before).filter((e) => e.pi === sid && e.type !== "response").map((e) => e.type), ["agent_start", "message_update", "agent_end"], "new events arrive after reconnect");
  } finally {
    c.close();
    await stop(b.c);
    fs.rmSync(dir, { recursive: true, force: true });
    fs.rmSync(cacheFile, { force: true });
  }
});
