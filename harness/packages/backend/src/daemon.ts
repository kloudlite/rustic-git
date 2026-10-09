//! The bench's one agent backend, run under runit (bench/sv/kl-host). Every client — kl-tui over ssh
//! (login-shell `kl-host` -> relay.ts) and the browser TUI (ttyd -> cli.tsx -> relay.ts) — is a
//! connection to it on a unix socket, so a turn outlives the client that started it (a quit, an ssh
//! drop) and there is exactly one agent, and one writer of each pi session file, per key. A
//! connection speaks ./wire through one `serve`; closing it disposes that client's views only
//! (local.ts keeps a running agent alive and settles an idle one). Console output stays on
//! stdout/stderr, which runit sends to the container log: frames only travel on sockets.
//! The socket is 0600 and lives in the user's home: the bench user is the only one who can reach it.
import { mkdirSync, rmSync, chmodSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { createServer, type Server } from "node:net";
import type { Backend } from "./index.ts";
import { serve } from "./serve.ts";
import { Peer } from "./wire.ts";

export const sockPath = () => process.env.KL_HOST_SOCK ?? join(homedir(), ".kl", "host.sock");

export function host(backend: Backend, sock: string): Promise<Server> {
  rmSync(sock, { force: true }); // a crashed daemon leaves its socket file behind
  const server = createServer((conn) => {
    const peer = new Peer((line) => void conn.write(line));
    const s = serve(backend, peer);
    conn.on("data", (c: Buffer) => peer.feed(c));
    conn.on("close", () => {
      peer.close();
      void s.dispose();
    });
    conn.on("error", () => {});
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(sock, () => {
      chmodSync(sock, 0o600);
      resolve(server);
    });
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
