import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";
import { until } from "./wait.ts";

// The bin itself, run as the pod runs it: through its shebang, not `node main.ts`.
const BIN = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "src", "main.ts");
const run = (args: string[], env: Record<string, string> = {}) => {
  // The binary refuses to start without these; these tests never reach the engine, so dummy values suffice.
  const c = spawn(BIN, args, { env: { ...process.env, KL_BENCH_IDLE_SECS: "", TYPESAFE_API_KEY: "test", JEVHARN_API_KEY: "test", ...env } });
  let out = "";
  c.stdout!.on("data", (d) => (out += d));
  c.stderr!.on("data", (d) => (out += d));
  const line = (re: RegExp) => until(() => re.test(out), 10_000, `output matching ${re}`).then(() => out);
  return { c, out: () => out, line };
};
const exited = (c: ChildProcess) => new Promise<number | null>((r) => (c.exitCode !== null ? r(c.exitCode) : c.on("exit", r)));
const portOf = (out: string) => Number(/listening on [\d.]+:(\d+)/.exec(out)![1]);
/** Whatever a failed assert left running is killed, and the folder goes. */
const cleanup = (dir: string, cs: (ChildProcess | undefined)[]) => {
  for (const c of cs) if (c && c.exitCode === null && c.signalCode === null) c.kill("SIGKILL");
  fs.rmSync(dir, { recursive: true, force: true });
};
const scratch = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-main-"));
  const term = path.join(dir, "termination-log");
  fs.writeFileSync(term, "");
  return { dir, term };
};

test("a second writer exits 75 naming the holder; a reader beside it is served and answers --ping", async () => {
  const { dir, term } = scratch();
  const a = run(["--dir", dir, "--port", "0"], { NODE_NAME: "node-a", TERMINATION_LOG: term });
  let r: ReturnType<typeof run> | undefined;
  try {
  await a.line(/listening on 0\.0\.0\.0:\d+ \(running\)/);

  const b = run(["--dir", dir, "--port", "0"], { TERMINATION_LOG: term });
  assert.equal(await exited(b.c), 75);
  assert.match(b.out(), /locked by node-a pid \d+/);
  assert.match(fs.readFileSync(term, "utf8"), /^node-a pid \d+$/);

  r = run(["--dir", dir, "--port", "0", "--read-only"], { TERMINATION_LOG: term });
  const port = portOf(await r.line(/\(read-only\)/));
  const rows = await (await fetch(`http://127.0.0.1:${port}/sessions`)).json();
  assert.deepEqual(rows, [], "a fresh bench boots with no session until one is created");
  // The first --ping pays a cold Node start; time the second, as a probe after the first would see it.
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 0);
  const t0 = performance.now();
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 0);
  const pingMs = performance.now() - t0;
  console.log(`--ping took ${pingMs.toFixed(0)} ms`);
  assert.ok(pingMs < 2500, `--ping must beat the probe's 3 s timeout, took ${pingMs.toFixed(0)} ms`);
  r.c.kill("SIGTERM");
  assert.equal(await exited(r.c), 0);
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 1, "nothing listening is not ready");

  a.c.kill("SIGTERM");
  assert.equal(await exited(a.c), 0);
  } finally {
    cleanup(dir, [a.c, r?.c]);
  }
});

test("with nobody connected and nothing running it marks .idle and keeps serving, and --ping says so", async () => {
  const { dir, term } = scratch();
  const a = run(["--dir", dir, "--port", "0"], { KL_BENCH_IDLE_SECS: "1", TERMINATION_LOG: term });
  try {
  const port = portOf(await a.line(/\(running\)/));
  const mark = path.join(dir, ".idle");
  await until(() => fs.existsSync(mark), 10_000, "the idle mark");
  assert.equal(a.c.exitCode, null, "idling never exits: kubelet would only restart the container");
  assert.equal((await (await fetch(`http://127.0.0.1:${port}/healthz`)).json()).idle, fs.readFileSync(mark, "utf8"));
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 2, "idle is unready, which is the agent's cue");

  // A client takes it out of idle, and the probe passes again.
  const w = new WebSocket(`ws://127.0.0.1:${port}/events`);
  await new Promise((r) => w.once("open", r));
  await until(() => !fs.existsSync(mark), 5_000, "the mark to go");
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 0);
  w.close();

  a.c.kill("SIGTERM");
  assert.equal(await exited(a.c), 0);
  assert.equal(fs.readFileSync(term, "utf8"), "", "a signal is not a termination message");
  } finally {
    cleanup(dir, [a.c]);
  }
});

test("--idle-secs 0 never signals idle, and a stale mark from the last pod goes at startup", async () => {
  const { dir, term } = scratch();
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, ".idle"), "2020-01-01T00:00:00.000Z");
  const a = run(["--dir", dir, "--port", "0", "--idle-secs", "0"], { TERMINATION_LOG: term });
  try {
    const port = portOf(await a.line(/\(running\)/));
    const mark = path.join(dir, ".idle");
    await until(() => !fs.existsSync(mark), 5_000, "the previous pod's mark to go");
    // Well past several idle beats: with 0 read as "sleep at once" the mark would be back by now.
    await new Promise((r) => setTimeout(r, 4_000));
    assert.equal(fs.existsSync(mark), false, "0 is a bench that never sleeps");
    assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 0, "and it stays ready");

    a.c.kill("SIGTERM");
    assert.equal(await exited(a.c), 0);
  } finally {
    cleanup(dir, [a.c]);
  }
});

test("a connected WebSocket holds an idle bench out of idle", async () => {
  const { dir, term } = scratch();
  const a = run(["--dir", dir, "--port", "0", "--idle-secs", "1"], { TERMINATION_LOG: term });
  try {
  const w = new WebSocket(`ws://127.0.0.1:${portOf(await a.line(/\(running\)/))}/events`);
  await new Promise((r) => w.once("open", r));
  await new Promise((r) => setTimeout(r, 7_000)); // past several idle beats with the mark written if the socket did not count
  assert.equal(a.c.exitCode, null, "still running with a client connected");
  assert.equal(fs.existsSync(path.join(dir, ".idle")), false);
  w.close();
  a.c.kill("SIGTERM");
  assert.equal(await exited(a.c), 0);
  } finally {
    cleanup(dir, [a.c]);
  }
});
