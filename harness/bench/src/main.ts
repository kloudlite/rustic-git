#!/usr/bin/env node
/**
 * harness-bench: one person's bench on one folder, as the platform's pod runs it
 * (constraints decisions 11 and 12). The pod runs it with no flags and
 * KL_BENCH_IDLE_SECS in its env; its readiness probe is `harness-bench --ping`;
 * the agent reads exit 75 as FolderLocked and a Succeeded pod as asleep.
 * Children inherit this process's env, so KL_TEAM reaches pi's extensions as is.
 */
import fs from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { Bench } from "./bench.ts";
import { Idle } from "./idle.ts";
import { FolderLocked, takeLock, type Lock } from "./lock.ts";
import { serve } from "./server.ts";

const { values: a } = parseArgs({
  options: {
    dir: { type: "string", default: "/bench" },
    // The pod IP: the gateway dials it, and the platform's NetworkPolicy admits only the gateway.
    host: { type: "string", default: "0.0.0.0" },
    port: { type: "string", default: "7789" },
    model: { type: "string", default: process.env.KL_MODEL ?? process.env.HARNESS_PI_MODEL ?? "deepseek/deepseek-v4-flash" },
    "read-only": { type: "boolean", default: false },
    wait: { type: "boolean", default: false },
    ping: { type: "boolean", default: false },
    // The platform's benchIdleSecs, stamped into the pod; 0 (a laptop) never sleeps.
    "idle-secs": { type: "string", default: process.env.KL_BENCH_IDLE_SECS || "0" },
  },
});

if (a.ping) {
  const ok = await fetch(`http://127.0.0.1:${a.port}/healthz`, { signal: AbortSignal.timeout(3000) }).then((r) => r.ok, () => false);
  process.exit(ok ? 0 : 1);
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

let lock: Lock | undefined;
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

const bench = new Bench({ dir, readOnly, model: a.model });
await bench.start();
if (!readOnly) {
  fs.writeFileSync(path.join(dir, ".health"), ""); // the probe appends; start each process from empty
  bench.writable.probe();
}
const idle = new Idle(() => bench.busy());
const srv = await serve(bench, Number(a.port), a.host, idle);
console.log(`harness-bench listening on ${a.host}:${srv.port} (${readOnly ? "read-only" : "running"}) dir=${dir}`);

const beat = readOnly ? undefined : setInterval(() => bench.writable.probe(), 10_000).unref();
const sleep = setInterval(() => {
  const since = idle.state().idleSince;
  if (idleMs > 0 && since !== null && Date.now() - since >= idleMs) void shutdown("idle");
}, 5_000).unref();

let leaving = false;
async function shutdown(why?: string) {
  if (leaving) return;
  leaving = true;
  clearInterval(beat);
  clearInterval(sleep);
  if (why) {
    terminationLog(why);
    console.error(`harness-bench: ${why}: no client and nothing running for ${a["idle-secs"]} s`);
  }
  bench.stop();
  await srv.close();
  lock?.release();
  process.exit(0);
}
process.on("SIGTERM", () => void shutdown());
process.on("SIGINT", () => void shutdown());
