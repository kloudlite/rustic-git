#!/usr/bin/env node
/**
 * harness-bench: one person's bench on one folder, as the platform's pod runs it
 * (constraints decisions 11 and 12). The pod runs it with no flags and
 * KL_BENCH_IDLE_SECS in its env; its readiness probe is `harness-bench --ping`.
 * The agent reads exit 75 as FolderLocked and an unready-because-idle container
 * as asleep: the pod's restartPolicy is Always, so idling by exiting would only
 * be restarted — idleness held for --idle-secs is a file (`{dir}/.idle`, see
 * idle.ts) and the process keeps serving. Nothing but a signal exits 0.
 * Children inherit this process's env, so KL_TEAM reaches pi's extensions as is.
 *
 * Only node: builtins are imported statically: `--ping` is an exec readiness
 * probe with a 1 s default timeout, and type-stripping the bench, server, ws and
 * pi SDK under gVisor would flap the bench unready. The server path imports them.
 */
import fs from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";

const { values: a } = parseArgs({
  options: {
    // The bench lives inside its workspace's worktree, so its folder is snapshotted and replicated with it.
    dir: { type: "string", default: process.env.KL_WORKSPACE ? path.join(process.env.KL_WORKSPACE, ".bench") : "/bench" },
    // The pod IP: the gateway dials it, and the platform's NetworkPolicy admits only the gateway.
    host: { type: "string", default: "0.0.0.0" },
    port: { type: "string", default: "7789" },
    model: { type: "string", default: process.env.KL_MODEL ?? process.env.HARNESS_PI_MODEL ?? "deepseek/deepseek-v4-flash" },
    "read-only": { type: "boolean", default: false },
    wait: { type: "boolean", default: false },
    ping: { type: "boolean", default: false },
    // The platform's benchIdleSecs, stamped into the pod: how long idleness must hold before it is signalled.
    "idle-secs": { type: "string", default: process.env.KL_BENCH_IDLE_SECS || "300" },
  },
});

if (a.ping) {
  // 2 = serving but idle: the agent's cue to delete the pod. 1 = not serving at all.
  const h = await fetch(`http://127.0.0.1:${a.port}/healthz`, { signal: AbortSignal.timeout(900) }).then((r) => (r.ok ? (r.json() as Promise<{ idle?: string }>) : null), () => null);
  process.exit(h === null ? 1 : h.idle ? 2 : 0);
}

// Kubernetes keeps this file as the container's last message.
const terminationLog = (msg: string) => {
  try {
    fs.writeFileSync(process.env.TERMINATION_LOG ?? "/dev/termination-log", msg);
  } catch {
    /* not in a pod */
  }
};

const dir = path.resolve(a.dir);
const readOnly = a["read-only"];
const idleMs = Number(a["idle-secs"]) * 1000;
if (!Number.isFinite(idleMs) || idleMs < 0) {
  console.error(`harness-bench: --idle-secs must be a non-negative number, got ${a["idle-secs"]}`);
  process.exit(2);
}

const { FolderLocked, takeLock } = await import("./lock.ts");
let lock: { release(): void } | undefined;
if (!readOnly) {
  try {
    lock = await takeLock(dir, { wait: a.wait, onWaiting: (h) => console.error(`harness-bench: waiting on ${h} for ${dir}/.lock`) });
  } catch (e) {
    console.error(`harness-bench: ${(e as Error).message}`);
    if (!(e instanceof FolderLocked)) process.exit(1);
    terminationLog(e.holder);
    process.exit(75);
  }
}

// After `--ping` (1 s budget, must not load the SDK) and before the server loads `node:http`.
if (process.env.KLOUDLITE_OTLP_URL) {
  const { startTracing } = await import("./tracing.ts");
  startTracing(process.env.OTEL_SERVICE_NAME ?? "harness-bench", process.env.KLOUDLITE_OTLP_URL);
}

const [{ Bench }, { Idle }, { serve }, { loadOperationControl }] = await Promise.all([import("./bench.ts"), import("./idle.ts"), import("./server.ts"), import("./operations/production.ts")]);
const bench = new Bench({ dir, readOnly, model: a.model });
await bench.start();
if (!readOnly) {
  fs.writeFileSync(path.join(dir, ".health"), ""); // the probe appends; start each process from empty
  bench.writable.probe();
}
// `--idle-secs 0` is "never signal idle" — a laptop bench nobody wants put to sleep — not "sleep
// the instant the last client leaves". Unreachable through admin settings (range 60-86400), but
// `WS_BENCH_IDLE_SECS=0` reaches it.
const idle = new Idle(() => bench.busy(), readOnly ? undefined : dir, idleMs === 0 ? Infinity : idleMs);
const operationControl = await loadOperationControl({
  api: process.env.KL_API_URL,
  owner: process.env.KL_OWNER,
  team: process.env.KL_TEAM,
  bench: process.env.KL_BENCH ?? process.env.KL_WORKSPACE_ID,
  module: process.env.KL_OPERATION_CONTROL_MODULE,
  log: (message) => console.error(`harness-bench: ${message}`),
});
const srv = await serve(bench, Number(a.port), a.host, idle, undefined, operationControl);
console.log(`harness-bench listening on ${a.host}:${srv.port} (${readOnly ? "read-only" : "running"}) dir=${dir}`);

const beat = readOnly ? undefined : setInterval(() => bench.writable.probe(), 10_000).unref();
// Nothing else would notice the wait running out: idleness begins when the last client left.
const sleep = setInterval(() => idle.check(), 5_000).unref();

let leaving = false;
async function shutdown() {
  if (leaving) return;
  leaving = true;
  clearInterval(beat);
  clearInterval(sleep);
  // Whatever fails while closing, the lock goes and the exit stays 0: the agent reads non-zero as a crash.
  try {
    await bench.stop();
    await srv.close();
  } catch (e) {
    console.error(`harness-bench: while stopping: ${(e as Error).message}`);
  } finally {
    lock?.release();
    process.exit(0);
  }
}
process.on("SIGTERM", () => void shutdown());
process.on("SIGINT", () => void shutdown());
