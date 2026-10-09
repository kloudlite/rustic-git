//! What the pod's browser TUI (ttyd -> cli.tsx) spawns: a dumb pipe
//! between this process's stdio and the bench daemon's socket (./daemon). A relay and not the
//! backend itself because the daemon outlives every connection: one agent per session key, so one
//! writer per session file, and a turn keeps running when this pipe goes away. The daemon may still
//! be booting after a pod restart, so connecting retries for KL_HOST_WAIT_MS (default 15 s).
import { connect } from "node:net";
import { sockPath } from "./daemon.ts";

const wait = Number(process.env.KL_HOST_WAIT_MS ?? 15000);
const sock = sockPath();
const start = Date.now();

function attempt() {
  const conn = connect(sock);
  conn.once("connect", () => {
    process.stdin.pipe(conn);
    conn.pipe(process.stdout);
    conn.on("close", () => process.exit(0));
    conn.on("error", () => process.exit(0));
    process.stdin.on("end", () => conn.end());
  });
  conn.once("error", () => {
    conn.destroy();
    if (Date.now() - start >= wait) {
      process.stderr.write("kl-host: the bench backend is not running\n");
      process.exit(1);
    }
    setTimeout(attempt, 200);
  });
}
attempt();
