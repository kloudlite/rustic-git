import { expect, test } from "bun:test";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { host, listenTcp } from "./daemon.ts";
import { connect, RemoteBackend } from "./remote.ts";
import { connect as netConnect } from "node:net";
import { Peer } from "./wire.ts";
import type { Backend } from "./index.ts";

const relay = join(import.meta.dir, "relay.ts");
const sock = () => `/tmp/kl-test-${randomBytes(4).toString("hex")}.sock`;
const cmd = ["bun", "run", "--silent", relay];
const env = (s: string, wait = "5000") => ({ ...process.env, KL_HOST_SOCK: s, KL_HOST_WAIT_MS: wait });

function fakeBackend() {
  const disposed: string[] = [];
  const b: any = {
    sessions: {},
    fs: {},
    podfs: {},
    hello: async () => ({ protocol: 1, cwd: "/home/kl", tools: [] }),
    session: async (key: string) => ({
      messages: [],
      isClaude: false,
      busy: true,
      subscribe: () => () => {},
      dispose: async () => void disposed.push(key),
    }),
  };
  return { b: b as Backend, disposed };
}

/** One client through the real relay process. */
const client = (s: string) => connect(["env", `KL_HOST_SOCK=${s}`, "KL_HOST_WAIT_MS=5000", ...cmd]);

test("a relay carries hello to the daemon and exits 0 when stdin ends", async () => {
  const s = sock();
  const server = await host(fakeBackend().b, s);
  const child = Bun.spawn(cmd, { stdin: "pipe", stdout: "pipe", stderr: "pipe", env: env(s) });
  child.stdin.write('{"id":1,"op":"hello","args":null}\n');
  child.stdin.flush();
  const line = new TextDecoder().decode((await child.stdout[Symbol.asyncIterator]().next()).value);
  expect(JSON.parse(line).value.protocol).toBe(1);
  child.stdin.end();
  expect(await child.exited).toBe(0);
  server.close();
});

test("two relays at once both get hello", async () => {
  const s = sock();
  const server = await host(fakeBackend().b, s);
  const [a, b] = await Promise.all([client(s), client(s)]);
  expect(a.hello.protocol).toBe(1);
  expect(b.hello.protocol).toBe(1);
  server.close();
});

test("no daemon: the relay gives up with 'not running' and exit 1", async () => {
  const child = Bun.spawn(cmd, { stdin: "pipe", stdout: "pipe", stderr: "pipe", env: env(sock(), "300") });
  expect(await child.exited).toBe(1);
  expect(await new Response(child.stderr).text()).toContain("not running");
});

test("closing a connection disposes the views that connection opened, not others'", async () => {
  const s = sock();
  const { b, disposed } = fakeBackend();
  const server = await host(b, s);
  const open = async (key: string) => {
    const conn = netConnect(s);
    const peer = new Peer((l) => void conn.write(l));
    conn.on("data", (c: Buffer) => peer.feed(c));
    await peer.request("session.open", { key, model: { provider: "p", id: "m" }, tools: [] });
    return conn;
  };
  const a = await open("k1");
  const keep = await open("k2");
  a.destroy();
  await Bun.sleep(100);
  expect(disposed).toEqual(["k1"]);
  keep.destroy();
  server.close();
});

test("session.open refuses a tool without an object inputSchema", async () => {
  const s = sock();
  const { b } = fakeBackend();
  const server = await host(b, s);
  const conn = netConnect(s);
  const peer = new Peer((l) => void conn.write(l));
  conn.on("data", (c: Buffer) => peer.feed(c));
  const open = (tool: any) => peer.request("session.open", { key: "k", model: { provider: "p", id: "m" }, tools: [tool] });
  await expect(open({ name: "t", description: "", parameters: { type: "object" } })).rejects.toThrow(
    'tool t needs an inputSchema of type "object"',
  );
  await open({ name: "t", description: "", inputSchema: { type: "object", properties: {} } });
  conn.destroy();
  server.close();
});

/** A client on the daemon's TCP port, the way the gateway pump reaches it. */
function tcpClient(port: number) {
  const conn = netConnect({ host: "127.0.0.1", port });
  const peer = new Peer((line) => void conn.write(line));
  conn.on("data", (c: Buffer) => peer.feed(c));
  return { backend: new RemoteBackend(peer), close: () => conn.destroy() };
}

test("the TCP listener speaks the same wire as host.sock", async () => {
  const server = await listenTcp(fakeBackend().b, 0, "127.0.0.1");
  const port = (server.address() as any).port;
  const c = tcpClient(port);
  expect((await c.backend.hello()).protocol).toBe(1);
  c.close();
  server.close();
});

test("a TCP client leaving disposes its views", async () => {
  const { b, disposed } = fakeBackend();
  const server = await listenTcp(b, 0, "127.0.0.1");
  const c = tcpClient((server.address() as any).port);
  await c.backend.session("main", { model: { provider: "x", id: "y" }, tools: [] });
  c.close();
  await new Promise((r) => setTimeout(r, 50));
  expect(disposed).toEqual(["main"]);
  server.close();
});
