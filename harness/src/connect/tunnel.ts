import net from "node:net";
import WebSocket from "ws";
import type { Session } from "./bench";

/** Above this many bytes queued toward the gateway, stop reading the local socket until it drains. */
const HIGH_WATER = 1 << 20;

/**
 * `kl-connect bench`, in the main process: a 127.0.0.1 port whose every accepted TCP connection
 * gets its own single-use session token and its own gateway WebSocket — nothing is multiplexed,
 * so a token is spent exactly once. BenchClient talks plain HTTP/WS to `base` and never learns
 * the tunnel exists, the same as it did with HARNESS_BENCH.
 *
 * `insecureLoopback` exists for tests only: it admits a `ws://127.0.0.1` gateway. Anything else
 * that is not `wss:` is refused before the token is sent.
 */
export async function openTunnel(
  mint: () => Promise<Session>,
  onError: (e: Error) => void,
  opts: { insecureLoopback?: boolean } = {},
): Promise<{ base: string; close(): void }> {
  const open = new Set<net.Socket>();
  // Paused from accept: a client that writes while the bench wakes keeps its bytes queued.
  const server = net.createServer({ pauseOnConnect: true }, (sock) => {
    open.add(sock);
    sock.on("close", () => open.delete(sock));
    sock.on("error", () => undefined); // close follows
    void (async () => {
      let s: Session;
      try {
        s = await mint();
        const u = new URL(s.gateway);
        if (u.protocol !== "wss:" && !(opts.insecureLoopback && u.protocol === "ws:" && u.hostname === "127.0.0.1")) {
          throw new Error(`gateway must be wss, got ${u.protocol}`);
        }
      } catch (e) {
        onError(e as Error);
        return void sock.destroy();
      }
      if (sock.destroyed) return;
      const ws = new WebSocket(s.gateway, { headers: { authorization: `Bearer ${s.token}` } });
      ws.on("open", () => {
        if (sock.destroyed) return void ws.terminate();
        sock.on("data", (d) => {
          ws.send(d, { binary: true }, () => {
            if (ws.bufferedAmount < HIGH_WATER && !sock.destroyed) sock.resume();
          });
          if (ws.bufferedAmount >= HIGH_WATER) sock.pause();
        });
        sock.on("end", () => ws.close());
        sock.resume();
      });
      ws.on("message", (d) => {
        if (!sock.write(d as Buffer)) {
          ws.pause();
          sock.once("drain", () => ws.resume());
        }
      });
      ws.on("close", () => sock.end());
      ws.on("error", () => sock.destroy()); // the message may name the gateway, never the token
      sock.on("close", () => ws.readyState === WebSocket.CLOSED || ws.terminate());
    })();
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const { port } = server.address() as net.AddressInfo;
  return {
    base: `http://127.0.0.1:${port}`,
    close() {
      server.close();
      for (const s of open) s.destroy();
    },
  };
}
