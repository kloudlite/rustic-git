import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import net from "node:net";
import type { AddressInfo } from "node:net";
import { WebSocketServer, type WebSocket as WS } from "ws";
import { openTunnel } from "../../src/connect/tunnel.ts";

// The stub gateway is plain ws on loopback; production refuses anything but wss.
const T = { insecureLoopback: true };
type Tunnel = Awaited<ReturnType<typeof openTunnel>>;
const portOf = (t: Tunnel) => Number(new URL(t.base).port);
const headFor = (t: Tunnel, host = `127.0.0.1:${portOf(t)}`, nonce: string | null = t.nonce) =>
  Buffer.from(`GET /x HTTP/1.1\r\nHost: ${host}\r\n${nonce === null ? "" : `x-kl-tunnel: ${nonce}\r\n`}\r\n`);

/** A stub gateway: /tunnel/bench-k echoes binary frames (or runs `onWs`) and records each upgrade. */
async function gateway(onWs?: (ws: WS) => void) {
  const bearers: string[] = [];
  const urls: string[] = [];
  const srv = http.createServer((_q, r) => r.writeHead(404).end());
  const wss = new WebSocketServer({ noServer: true });
  srv.on("upgrade", (req, sock, head) => {
    urls.push(req.url ?? "");
    if (req.url !== "/tunnel/bench-k") return sock.destroy();
    bearers.push(req.headers.authorization ?? "");
    wss.handleUpgrade(req, sock, head, (ws) => (onWs ? onWs(ws) : ws.on("message", (d, bin) => ws.send(d, { binary: bin }))));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const url = `ws://127.0.0.1:${(srv.address() as AddressInfo).port}/tunnel/bench-k`;
  return { url, bearers, urls, close: () => (wss.close(), srv.close()) };
}

/** Sends `payload`, resolves with everything read until `want` bytes or the socket closes. */
const exchange = (port: number, payload: Buffer, want = payload.length) =>
  new Promise<Buffer>((resolve, reject) => {
    const s = net.connect(port, "127.0.0.1", () => void s.write(payload));
    let got = Buffer.alloc(0);
    s.on("data", (d) => {
      got = Buffer.concat([got, d]);
      if (got.length >= want) (s.end(), resolve(got));
    });
    s.on("close", () => resolve(got));
    s.on("error", reject);
  });

const session = (url: string, token = "s") => async () => ({ id: "bench-k", token, gateway: url, expires_at: "2030" });

test("each TCP connection mints its own token and reaches the gateway with it", async () => {
  const g = await gateway();
  let n = 0;
  const t = await openTunnel(async () => ({ id: "bench-k", token: `s${++n}`, gateway: g.url, expires_at: "2030" }), (e) => assert.fail(e), T);
  try {
    for (const body of ["a", "b"]) {
      const p = Buffer.concat([headFor(t), Buffer.from(body)]);
      assert.equal((await exchange(portOf(t), p)).toString(), p.toString());
    }
    assert.equal(n, 2);
    assert.deepEqual(g.bearers, ["Bearer s1", "Bearer s2"]);
    assert.ok(g.urls.every((u) => !u.includes("s1") && !u.includes("s2")), "token never in the URL");
  } finally {
    t.close();
    g.close();
  }
});

test("bytes written while the mint is still waiting are delivered", async () => {
  const g = await gateway();
  const t = await openTunnel(async () => {
    await new Promise((r) => setTimeout(r, 100)); // a waking bench
    return { id: "bench-k", token: "s", gateway: g.url, expires_at: "2030" };
  }, (e) => assert.fail(e), T);
  try {
    const p = Buffer.concat([headFor(t), Buffer.from("early bytes")]);
    assert.equal((await exchange(portOf(t), p)).toString(), p.toString());
  } finally {
    t.close();
    g.close();
  }
});

test("a large payload survives the splice in both directions", async () => {
  const g = await gateway();
  const t = await openTunnel(session(g.url), (e) => assert.fail(e), T);
  try {
    const p = Buffer.concat([headFor(t), Buffer.alloc(8 * 1024 * 1024, 7)]);
    assert.ok((await exchange(portOf(t), p)).equals(p));
  } finally {
    t.close();
    g.close();
  }
});

test("it listens on 127.0.0.1 only, with a fresh 32-byte nonce", async () => {
  const t = await openTunnel(async () => { throw new Error("unused"); }, () => undefined);
  try {
    assert.match(t.base, /^http:\/\/127\.0\.0\.1:\d+$/);
    assert.equal(Buffer.from(t.nonce, "base64url").length, 32);
  } finally {
    t.close();
  }
});

for (const [name, head] of [
  ["no nonce header", (t: Tunnel) => headFor(t, undefined, null)],
  ["a wrong nonce", (t: Tunnel) => headFor(t, undefined, "A".repeat(43))],
  ["a wrong Host", (t: Tunnel) => headFor(t, "evil.test")],
] as const) {
  test(`${name} is a 403 and mints nothing`, async () => {
    let minted = 0;
    const t = await openTunnel(async () => (minted++, session("wss://x/tunnel/bench-k")()), () => undefined);
    try {
      assert.match((await exchange(portOf(t), head(t), 1 << 20)).toString(), /^HTTP\/1\.1 403/);
      assert.equal(minted, 0);
    } finally {
      t.close();
    }
  });
}

test("a head over 16 KiB is closed without minting", async () => {
  let minted = 0;
  const t = await openTunnel(async () => (minted++, session("wss://x")()), () => undefined);
  try {
    const got = await exchange(portOf(t), Buffer.from(`GET / HTTP/1.1\r\nx-pad: ${"a".repeat(17 * 1024)}`), 1 << 20);
    assert.equal(got.length, 0);
    assert.equal(minted, 0);
  } finally {
    t.close();
  }
});

test("a plain ws gateway is refused without the test-only option", async () => {
  const errors: string[] = [];
  const t = await openTunnel(session("ws://127.0.0.1:1/tunnel/bench-k", "secret-tok"), (e) => errors.push(e.message));
  try {
    await exchange(portOf(t), headFor(t), 1 << 20);
    assert.equal(errors.length, 1);
    assert.match(errors[0], /wss/);
    assert.ok(!errors[0].includes("secret-tok"));
  } finally {
    t.close();
  }
});

test("a gateway error reaches onError without the token", async () => {
  const dead = net.createServer();
  await new Promise<void>((r) => dead.listen(0, "127.0.0.1", r));
  const deadPort = (dead.address() as AddressInfo).port;
  await new Promise((r) => dead.close(r));
  const errors: string[] = [];
  const t = await openTunnel(session(`ws://127.0.0.1:${deadPort}/tunnel/bench-k`, "secret-tok"), (e) => errors.push(e.message), T);
  try {
    await exchange(portOf(t), headFor(t), 1 << 20);
    assert.equal(errors.length, 1);
    assert.match(errors[0], /^gateway: /);
    assert.ok(!errors[0].includes("secret-tok"));
  } finally {
    t.close();
  }
});

test("a failed mint closes that connection and reports, without the listener dying", async () => {
  const errors: string[] = [];
  const t = await openTunnel(async () => { throw Object.assign(new Error("your login has expired or was revoked"), { name: "Expired" }); }, (e) => errors.push(e.name));
  try {
    for (let i = 0; i < 2; i++) await exchange(portOf(t), headFor(t), 1 << 20);
    assert.deepEqual(errors, ["Expired", "Expired"]);
  } finally {
    t.close();
  }
});

test("the gateway closing first closes the local socket", async () => {
  const g = await gateway((ws) => ws.once("message", () => ws.close()));
  const t = await openTunnel(session(g.url), (e) => assert.fail(e), T);
  try {
    assert.equal((await exchange(portOf(t), headFor(t), 1 << 20)).length, 0); // resolved by close
  } finally {
    t.close();
    g.close();
  }
});

test("a stalled reader bounds what the gateway can push, with no listener pile-up", async () => {
  const warnings: string[] = [];
  const onWarn = (w: Error) => warnings.push(w.name);
  process.on("warning", onWarn);
  let gws: WS | undefined;
  const chunk = Buffer.alloc(64 * 1024, 1);
  const g = await gateway((ws) => {
    gws = ws;
    ws.once("message", () => { for (let i = 0; i < 512; i++) ws.send(chunk); }); // 32 MiB
  });
  const t = await openTunnel(session(g.url), (e) => assert.fail(e), T);
  const s = net.connect(portOf(t), "127.0.0.1", () => { s.write(headFor(t)); s.pause(); });
  s.on("error", () => undefined);
  try {
    await new Promise((r) => setTimeout(r, 1000));
    assert.ok(gws, "gateway was reached");
    assert.ok(gws!.bufferedAmount > 8 * 1024 * 1024, `gateway still holds most of it (buffered ${gws!.bufferedAmount})`);
    assert.deepEqual(warnings.filter((w) => w === "MaxListenersExceededWarning"), []);
  } finally {
    process.off("warning", onWarn);
    s.destroy();
    t.close();
    g.close();
  }
});

test("close() tears down open connections and the listener", async () => {
  const g = await gateway();
  const t = await openTunnel(session(g.url), (e) => assert.fail(e), T);
  const port = portOf(t);
  await new Promise<void>((resolve) => {
    const s = net.connect(port, "127.0.0.1", () => s.write(headFor(t)));
    s.on("data", () => t.close());
    s.on("close", () => resolve());
    s.on("error", () => undefined);
  });
  await assert.rejects(new Promise((res, rej) => net.connect(port, "127.0.0.1", res).on("error", rej)));
  g.close();
});
