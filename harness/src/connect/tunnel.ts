import crypto from "node:crypto";
import net from "node:net";
import WebSocket from "ws";
import type { Session } from "./bench";

/** Above this many bytes queued toward the gateway, stop reading the local socket until it drains. */
const HIGH_WATER = 1 << 20;
const MAX_HEAD = 16 * 1024;
const HEAD_MS = 5000;

/**
 * `kl-connect bench`, in the main process: a 127.0.0.1 port whose every accepted TCP connection
 * gets its own single-use session token and its own gateway WebSocket — nothing is multiplexed,
 * so a token is spent exactly once.
 *
 * Loopback is not a fence: another OS user or a DNS-rebinding browser page can reach the port.
 * So the first request head must carry `Host: 127.0.0.1:<port>` exactly (defeats rebinding) and
 * `x-kl-tunnel: <nonce>`, a per-launch secret held only by the in-process BenchClient; anything
 * else is a 403 before a token is minted. The head is then forwarded to the gateway unchanged.
 * ponytail: only the first request on a connection is checked — keep-alive reuse is the same
 * caller, and a new connection is checked again.
 *
 * `insecureLoopback` exists for tests only: it admits a `ws://127.0.0.1` gateway. Anything else
 * that is not `wss:` is refused before the token is sent.
 */
export async function openTunnel(
  mint: () => Promise<Session>,
  onError: (e: Error) => void,
  opts: { insecureLoopback?: boolean } = {},
): Promise<{ base: string; nonce: string; close(): void }> {
  const nonce = crypto.randomBytes(32).toString("base64url");
  const want = Buffer.from(nonce);
  const open = new Set<net.Socket>();
  let port = 0;

  const authorized = (head: string) => {
    const lines = head.split("\r\n").slice(1);
    const values = (name: string) =>
      lines.filter((l) => l.slice(0, l.indexOf(":")).trim().toLowerCase() === name).map((l) => l.slice(l.indexOf(":") + 1).trim());
    const host = values("host");
    const tok = values("x-kl-tunnel");
    if (host.length !== 1 || host[0] !== `127.0.0.1:${port}` || tok.length !== 1) return false;
    const got = Buffer.from(tok[0]);
    return got.length === want.length && crypto.timingSafeEqual(got, want);
  };

  /** Resolves with every byte read up to and past the end of the first request head, or undefined. */
  const readHead = (sock: net.Socket) =>
    new Promise<Buffer | undefined>((resolve) => {
      let buf = Buffer.alloc(0);
      const done = (v: Buffer | undefined) => {
        clearTimeout(timer);
        sock.off("data", onData);
        sock.off("close", onClose);
        sock.pause();
        resolve(v);
      };
      const onData = (d: Buffer) => {
        buf = Buffer.concat([buf, d]);
        const end = buf.indexOf("\r\n\r\n");
        if (end >= 0 && end + 4 <= MAX_HEAD) done(buf);
        else if (buf.length > MAX_HEAD) done(undefined);
      };
      const onClose = () => done(undefined);
      const timer = setTimeout(() => done(undefined), HEAD_MS);
      sock.on("data", onData);
      sock.on("close", onClose);
      sock.resume();
    });

  const server = net.createServer({ pauseOnConnect: true }, (sock) => {
    open.add(sock);
    sock.on("close", () => open.delete(sock));
    sock.on("error", () => undefined); // close follows
    void (async () => {
      const head = await readHead(sock);
      if (!head) return void sock.destroy();
      if (!authorized(head.subarray(0, head.indexOf("\r\n\r\n")).toString("latin1"))) {
        return void sock.end("HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
      }
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
      let wsPaused = false;
      sock.on("drain", () => {
        if (wsPaused) (wsPaused = false), ws.resume();
      });
      ws.on("open", () => {
        if (sock.destroyed) return void ws.terminate();
        ws.send(head, { binary: true });
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
        if (!sock.write(d as Buffer) && !wsPaused) (wsPaused = true), ws.pause();
      });
      ws.on("close", () => sock.end());
      // ws errors carry no request headers, so the message never names the token.
      ws.on("error", (e) => (onError(new Error(`gateway: ${e.message}`)), sock.destroy()));
      sock.on("close", () => ws.readyState === WebSocket.CLOSED || ws.terminate());
    })();
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  port = (server.address() as net.AddressInfo).port;
  return {
    base: `http://127.0.0.1:${port}`,
    nonce,
    close() {
      server.close();
      for (const s of open) s.destroy();
    },
  };
}
