//! The bench's one agent backend, run under runit (bench/sv/kl-host). Every client — kl-tui over ssh
//! (login-shell `kl-host` -> relay.ts) and the browser TUI (ttyd -> cli.tsx -> relay.ts) on the unix
//! socket, the laptop kl-tui through the gateway on TCP 7791 — is a connection to it, so a turn outlives the client that started it (a quit, an ssh
//! drop) and there is exactly one agent, and one writer of each pi session file, per key. A
//! connection speaks ./wire through one `serve`; closing it disposes that client's views only
//! (local.ts keeps a running agent alive and settles an idle one). Console output stays on
//! stdout/stderr, which runit sends to the container log: frames only travel on sockets.
//! The socket is 0600 in the user's home; the TCP port is fenced by the pod's NetworkPolicy to the
//! gateway, whose token is the lock.
import { mkdirSync, rmSync, chmodSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { createServer, type Server, type Socket } from "node:net";
import type { Backend } from "./index.ts";
import { serve } from "./serve.ts";
import { Peer } from "./wire.ts";

export const sockPath = () => process.env.KL_HOST_SOCK ?? join(homedir(), ".kl", "host.sock");

/** One client connection, whatever it arrived on: one `serve`, its views disposed when it closes. */
export function attach(backend: Backend) {
  return (conn: Socket) => {
    const peer = new Peer((line) => void conn.write(line));
    const s = serve(backend, peer);
    conn.setNoDelay?.(true);
    conn.on("data", (c: Buffer) => peer.feed(c));
    conn.on("close", () => {
      peer.close();
      void s.dispose();
    });
    conn.on("error", () => {});
  };
}

export function host(backend: Backend, sock: string): Promise<Server> {
  rmSync(sock, { force: true }); // a crashed daemon leaves its socket file behind
  const server = createServer(attach(backend));
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(sock, () => {
      chmodSync(sock, 0o600);
      resolve(server);
    });
  });
}

/** The laptop kl-tui's door: the gateway's `/tui/{bench}` pumps here. No auth of its own — the
 * gateway token is the lock and the pod's NetworkPolicy admits only the gateway to this port
 * (`crates/workspaces/src/k8s/bench.rs` `BENCH_TUI_PORT`). */
export function listenTcp(backend: Backend, port: number, hostname = "0.0.0.0"): Promise<Server> {
  const server = createServer(attach(backend));
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, hostname, () => resolve(server));
  });
}

if (import.meta.main) {
  // one stray rejection must not take every session on the bench down with it
  process.on("unhandledRejection", (e) => console.error("unhandled rejection", e));
  process.chdir(homedir());
  mkdirSync(join(homedir(), ".kl"), { recursive: true });
  const { LocalBackend } = await import("./local.ts");
  const b = new LocalBackend();
  await host(b, sockPath());
  // 7791 = BENCH_TUI_PORT (crates/workspaces/src/k8s/bench.rs); env only so a laptop run can move it
  await listenTcp(b, Number(process.env.KL_HOST_TUI_PORT ?? 7791));
  // The idle clock (bench/sessions/main.ts) cannot see a turn whose client left; this file is how
  // it does. Rewritten on a beat so a dead daemon's last count goes stale instead of pinning the pod.
  const busyFile = join(homedir(), ".kl", "host.busy");
  const beat = () => {
    try {
      writeFileSync(busyFile, String(b.busyCount));
    } catch (e) {
      console.error("host.busy", e);
    }
  };
  beat();
  setInterval(beat, 5000);
  await b.resumeAsks();
}
