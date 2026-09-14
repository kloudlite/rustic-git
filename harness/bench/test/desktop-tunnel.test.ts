import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import net from "node:net";
import type { AddressInfo } from "node:net";
import { WebSocketServer } from "ws";
import { openTunnel } from "../../src/connect/tunnel.ts";

// The stub gateway is plain ws on loopback; production refuses anything but wss.
const T = { insecureLoopback: true };

/** A stub gateway: /tunnel/bench-k echoes binary frames and records each upgrade's bearer. */
async function gateway() {
  const bearers: string[] = [];
  const urls: string[] = [];
  const srv = http.createServer((_q, r) => r.writeHead(404).end());
  const wss = new WebSocketServer({ noServer: true });
  srv.on("upgrade", (req, sock, head) => {
    urls.push(req.url ?? "");
    if (req.url !== "/tunnel/bench-k") return sock.destroy();
    bearers.push(req.headers.authorization ?? "");
    wss.handleUpgrade(req, sock, head, (ws) => ws.on("message", (d, bin) => ws.send(d, { binary: bin })));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const url = `ws://127.0.0.1:${(srv.address() as AddressInfo).port}/tunnel/bench-k`;
  return { url, bearers, urls, close: () => (wss.close(), srv.close()) };
}

const roundTrip = (port: number, payload: Buffer) =>
  new Promise<Buffer>((resolve, reject) => {
    const s = net.connect(port, "127.0.0.1", () => void s.write(payload));
    let got = Buffer.alloc(0);
    s.on("data", (d) => {
      got = Buffer.concat([got, d]);
      if (got.length >= payload.length) (s.end(), resolve(got));
    });
    s.on("error", reject);
  });

test("each TCP connection mints its own token and reaches the gateway with it", async () => {
  const g = await gateway();
  let n = 0;
  const t = await openTunnel(async () => ({ id: "bench-k", token: `s${++n}`, gateway: g.url, expires_at: "2030" }), (e) => assert.fail(e), T);
  try {
    const port = Number(new URL(t.base).port);
    assert.equal((await roundTrip(port, Buffer.from("a"))).toString(), "a");
    assert.equal((await roundTrip(port, Buffer.from("b"))).toString(), "b");
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
    const echoed = await roundTrip(Number(new URL(t.base).port), Buffer.from("early bytes"));
    assert.equal(echoed.toString(), "early bytes");
  } finally {
    t.close();
    g.close();
  }
});

test("a large payload survives the splice in both directions", async () => {
  const g = await gateway();
  const t = await openTunnel(async () => ({ id: "bench-k", token: "s", gateway: g.url, expires_at: "2030" }), (e) => assert.fail(e), T);
  try {
    const big = Buffer.alloc(8 * 1024 * 1024, 7);
    const echoed = await roundTrip(Number(new URL(t.base).port), big);
    assert.ok(echoed.equals(big));
  } finally {
    t.close();
    g.close();
  }
});

test("it listens on 127.0.0.1 only", async () => {
  const t = await openTunnel(async () => { throw new Error("unused"); }, () => undefined);
  try {
    assert.match(t.base, /^http:\/\/127\.0\.0\.1:\d+$/);
  } finally {
    t.close();
  }
});

const oneConn = (port: number) =>
  new Promise<void>((resolve) => {
    const s = net.connect(port, "127.0.0.1");
    s.on("close", () => resolve());
    s.on("error", () => undefined);
  });

test("a plain ws gateway is refused without the test-only option", async () => {
  const errors: string[] = [];
  const t = await openTunnel(async () => ({ id: "bench-k", token: "secret-tok", gateway: "ws://127.0.0.1:1/tunnel/bench-k", expires_at: "2030" }), (e) => errors.push(e.message));
  try {
    await oneConn(Number(new URL(t.base).port));
    assert.equal(errors.length, 1);
    assert.match(errors[0], /wss/);
    assert.ok(!errors[0].includes("secret-tok"));
  } finally {
    t.close();
  }
});

test("a failed mint closes that connection and reports, without the listener dying", async () => {
  const errors: string[] = [];
  const t = await openTunnel(async () => { throw Object.assign(new Error("your login has expired or was revoked"), { name: "Expired" }); }, (e) => errors.push(e.name));
  try {
    const port = Number(new URL(t.base).port);
    for (let i = 0; i < 2; i++) await oneConn(port);
    assert.deepEqual(errors, ["Expired", "Expired"]);
  } finally {
    t.close();
  }
});

test("close() tears down open connections", async () => {
  const g = await gateway();
  const t = await openTunnel(async () => ({ id: "bench-k", token: "s", gateway: g.url, expires_at: "2030" }), (e) => assert.fail(e), T);
  const port = Number(new URL(t.base).port);
  await roundTrip(port, Buffer.from("x")).catch(() => undefined);
  const closed = new Promise<void>((resolve) => {
    const s = net.connect(port, "127.0.0.1", () => s.write("y"));
    s.on("data", () => t.close());
    s.on("close", () => resolve());
    s.on("error", () => undefined);
  });
  await closed;
  await assert.rejects(new Promise((res, rej) => net.connect(port, "127.0.0.1", res).on("error", rej)));
  g.close();
});
