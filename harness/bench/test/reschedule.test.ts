// The node-death drill from task-17-brief.md, automated: SIGKILL the bench (no
// clean shutdown, the flock child and its process tree left to die on their
// own) and start a second instance on the same folder with a different
// NODE_NAME. Every session must reopen with the same ids and files, the
// running task must read "lost", the live process must read lost:true with a
// numeric ended, and the new instance must hold the lock.
import { test } from "node:test";
import assert from "node:assert/strict";
import { execSync } from "node:child_process";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

const BIN = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "src", "main.ts");
const run = (args: string[], env: Record<string, string> = {}) => {
  const c = spawn(BIN, args, { env: { ...process.env, HARNESS_PI_BIN: FAKE, KL_BENCH_IDLE_SECS: "", ...env } });
  let out = "";
  c.stdout!.on("data", (d) => (out += d));
  c.stderr!.on("data", (d) => (out += d));
  return { c, out: () => out };
};
const line = (getOut: () => string, re: RegExp) =>
  until(() => re.test(getOut()), 10_000, `output matching ${re}`).then(() => getOut());
const exited = (c: ChildProcess) => new Promise<number | null>((r) => (c.exitCode !== null ? r(c.exitCode) : c.on("exit", r)));
const portOf = (out: string) => Number(/listening on [\d.]+:(\d+)/.exec(out)![1]);
const j = async (base: string, method: string, path: string) => {
  const res = await fetch(base + path, { method });
  return { status: res.status, body: res.status === 204 ? undefined : await res.json() };
};

test("a killed bench reschedules: sessions reopen, the running task and process read lost, the new instance holds the lock", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-reschedule-"));
  const term = path.join(dir, "termination-log");
  fs.writeFileSync(term, "");

  const a = run(["--dir", dir, "--port", "0"], { NODE_NAME: "node-a", TERMINATION_LOG: term });
  let b: ReturnType<typeof run> | undefined;
  let c: ReturnType<typeof run> | undefined;
  const sockets: WebSocket[] = [];
  try {
  const aOut = await line(a.out, /listening on 0\.0\.0\.0:\d+ \(running\)/);
  const aPort = portOf(aOut);
  const aBase = `http://127.0.0.1:${aPort}`;

  // s-1 (the bench's default session) picks up a task that never finishes.
  const wsTask = new WebSocket(`ws://127.0.0.1:${aPort}/sessions/s-1/rpc`);
  sockets.push(wsTask);
  await new Promise((r) => wsTask.once("open", r));
  wsTask.send(JSON.stringify({ id: "1", type: "prompt", message: "task" }));

  // A second session picks up a live background process.
  const s2 = (await j(aBase, "POST", "/sessions")).body as { id: string };
  const wsProc = new WebSocket(`ws://127.0.0.1:${aPort}/sessions/${s2.id}/rpc`);
  sockets.push(wsProc);
  const procDone = new Promise<void>((r) => wsProc.on("message", (d) => JSON.parse(d.toString()).type === "agent_end" && r()));
  await new Promise((r) => wsProc.once("open", r));
  wsProc.send(JSON.stringify({ id: "1", type: "prompt", message: "proc" }));
  await procDone;

  await until(async () => (await j(aBase, "GET", "/tasks")).body.some((t: { id: string; state: string }) => t.id === "t1" && t.state === "running"), 5_000, "task t1 running");
  let proc: { id: string; pid: number } | undefined;
  await until(async () => {
    const rows = (await j(aBase, "GET", "/procs")).body as { id: string; pid?: number; ended?: number }[];
    proc = rows.find((r) => r.id === "p1" && r.pid && r.ended === undefined) as { id: string; pid: number } | undefined;
    return proc;
  }, 5_000, "proc p1 alive");

  const sessionsBefore = (await j(aBase, "GET", "/sessions")).body as { id: string; file?: string }[];

  // Node loss, simulated: no SIGTERM, no clean shutdown. The bench's own
  // children (the two fake-pi stand-ins) and the leaf process the "proc"
  // prompt spawned die with it, same as a node dying under everything on it.
  const kids = execSync(`pgrep -P ${a.c.pid}`).toString().trim().split("\n").filter(Boolean).map(Number);
  a.c.kill("SIGKILL");
  for (const pid of kids) { try { process.kill(pid, "SIGKILL"); } catch { /* already gone */ } }
  try { process.kill(proc!.pid, "SIGKILL"); } catch { /* already gone */ }
  await exited(a.c);

  // A second instance, a different node, same folder: it must take the lock
  // at once (the flock child dies with the killed bench's stdin pipe) rather
  // than exit 75.
  b = run(["--dir", dir, "--port", "0"], { NODE_NAME: "node-b", TERMINATION_LOG: term });
  const bOut = await line(b.out, /listening on 0\.0\.0\.0:\d+ \(running\)/);
  const bPort = portOf(bOut);
  const bBase = `http://127.0.0.1:${bPort}`;

  const sessionsAfter = (await j(bBase, "GET", "/sessions")).body as { id: string; file?: string }[];
  assert.deepEqual(
    sessionsAfter.map((s) => [s.id, s.file]).sort(),
    sessionsBefore.map((s) => [s.id, s.file]).sort(),
    "every session reopens with the same id and file",
  );

  const tasks = (await j(bBase, "GET", "/tasks")).body as { id: string; state: string }[];
  assert.equal(tasks.find((t) => t.id === "t1")?.state, "lost", "the running task reads lost");

  // The PROCESS is not lost by a reschedule: it runs on a workspace's tool server, which the new
  // instance asks. Only a tool server that cannot be reached for three sweeps loses its rows.
  const procs = (await j(bBase, "GET", "/procs")).body as { id: string; lost?: boolean; ended?: number }[];
  const p1 = procs.find((p) => p.id === "p1");
  assert.ok(p1, "the row survives the move");
  assert.equal(p1?.ended, undefined, "and is not ended by the move alone");

  // The new instance holds the lock: a third would exit 75 naming it.
  c = run(["--dir", dir, "--port", "0"], { TERMINATION_LOG: term });
  assert.equal(await exited(c.c), 75);
  assert.match(c.out(), /locked by node-b pid \d+/);

  b.c.kill("SIGTERM");
  assert.equal(await exited(b.c), 0);
  } finally {
    for (const w of sockets) w.terminate();
    for (const x of [a.c, b?.c, c?.c]) if (x && x.exitCode === null && x.signalCode === null) x.kill("SIGKILL");
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
