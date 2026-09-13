import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";
import { FAKE } from "./fake-pi.ts";

// The bin itself, run as the pod runs it: through its shebang, not `node main.ts`.
const BIN = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "src", "main.ts");
const run = (args: string[], env: Record<string, string> = {}) => {
  const c = spawn(BIN, args, { env: { ...process.env, HARNESS_PI_BIN: FAKE, KL_BENCH_IDLE_SECS: "", ...env } });
  let out = "";
  c.stdout!.on("data", (d) => (out += d));
  c.stderr!.on("data", (d) => (out += d));
  const line = (re: RegExp) =>
    new Promise<string>((r) => {
      const t = setInterval(() => re.test(out) && (clearInterval(t), r(out)), 20);
    });
  return { c, out: () => out, line };
};
const exited = (c: ChildProcess) => new Promise<number | null>((r) => (c.exitCode !== null ? r(c.exitCode) : c.on("exit", r)));
const portOf = (out: string) => Number(/listening on [\d.]+:(\d+)/.exec(out)![1]);
const scratch = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-main-"));
  const term = path.join(dir, "termination-log");
  fs.writeFileSync(term, "");
  return { dir, term };
};

test("a second writer exits 75 naming the holder; a reader beside it is served and answers --ping", async () => {
  const { dir, term } = scratch();
  const a = run(["--dir", dir, "--port", "0"], { NODE_NAME: "node-a", TERMINATION_LOG: term });
  await a.line(/listening on 0\.0\.0\.0:\d+ \(running\)/);

  const b = run(["--dir", dir, "--port", "0"], { TERMINATION_LOG: term });
  assert.equal(await exited(b.c), 75);
  assert.match(b.out(), /locked by node-a pid \d+/);
  assert.match(fs.readFileSync(term, "utf8"), /^node-a pid \d+$/);

  const r = run(["--dir", dir, "--port", "0", "--read-only"], { TERMINATION_LOG: term });
  const port = portOf(await r.line(/\(read-only\)/));
  const rows = await (await fetch(`http://127.0.0.1:${port}/sessions`)).json();
  assert.equal(rows[0].id, "s-1");
  // The first --ping pays a cold Node start; time the second, as a probe after the first would see it.
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 0);
  const t0 = performance.now();
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 0);
  const pingMs = performance.now() - t0;
  console.log(`--ping took ${pingMs.toFixed(0)} ms`);
  assert.ok(pingMs < 1000, `--ping must beat the probe's 3 s timeout by a wide margin, took ${pingMs.toFixed(0)} ms`);
  r.c.kill("SIGTERM");
  assert.equal(await exited(r.c), 0);
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 1, "nothing listening is not ready");

  a.c.kill("SIGTERM");
  assert.equal(await exited(a.c), 0);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("with nobody connected and nothing running it exits 0 naming idle, and releases the lock", async () => {
  const { dir, term } = scratch();
  const a = run(["--dir", dir, "--port", "0"], { KL_BENCH_IDLE_SECS: "1", TERMINATION_LOG: term });
  const started = Date.now();
  assert.equal(await exited(a.c), 0);
  assert.ok(Date.now() - started < 15_000, "within the idle period plus two beats");
  assert.equal(fs.readFileSync(term, "utf8"), "idle");
  const b = run(["--dir", dir, "--port", "0"], { TERMINATION_LOG: term });
  await b.line(/\(running\)/);
  b.c.kill("SIGTERM");
  assert.equal(await exited(b.c), 0, "the next start takes the lock at once");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a connected WebSocket holds an idle bench up", async () => {
  const { dir, term } = scratch();
  const a = run(["--dir", dir, "--port", "0", "--idle-secs", "1"], { TERMINATION_LOG: term });
  const w = new WebSocket(`ws://127.0.0.1:${portOf(await a.line(/\(running\)/))}/events`);
  await new Promise((r) => w.once("open", r));
  await new Promise((r) => setTimeout(r, 7_000)); // past one idle beat with idleSince long stale if the socket did not count
  assert.equal(a.c.exitCode, null, "still running with a client connected");
  assert.equal(fs.readFileSync(term, "utf8"), "");
  w.close();
  a.c.kill("SIGTERM");
  assert.equal(await exited(a.c), 0);
  fs.rmSync(dir, { recursive: true, force: true });
});
