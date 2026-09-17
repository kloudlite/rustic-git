import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import WebSocket, { WebSocketServer } from "ws";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { authFrame, shellAddress, SHELL_PORT, TTYD_SUBPROTOCOL } from "../src/pty.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

/**
 * A terminal is a socket to the pod's `shell` sidecar, which runs ttyd (spec §2.3). The bench
 * splices the two sockets and adds ttyd's opening frame; everything else crosses unchanged, and
 * there is no session, no reattach and no tmux behind it.
 */
async function up(resolveTools?: (ws: string) => Promise<string>) {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-pty-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0, "127.0.0.1", undefined, undefined, { resolveTools });
  return { srv, port: srv.port, down: async () => (await bench.stop(), await srv.close()) };
}

const opened = (w: WebSocket) => new Promise((r, j) => (w.once("open", r), w.once("error", j)));

/** Everything the client was sent, split the way ttyd frames it. */
function collect(w: WebSocket) {
  const out = { text: "", title: "", json: [] as Record<string, unknown>[] };
  w.on("message", (d: Buffer, binary: boolean) => {
    const frame = d.toString();
    if (binary) return void (out.text += frame);
    if (frame.startsWith("0")) return void (out.text += frame.slice(1));
    if (frame.startsWith("1")) return void (out.title = frame.slice(1));
    if (frame.startsWith("2")) return; // ttyd's preferences
    out.json.push(JSON.parse(frame) as Record<string, unknown>);
  });
  return out;
}
const resize = (w: WebSocket, cols: number, rows: number) => w.send(JSON.stringify({ resize: { cols, rows } }));

/**
 * A stand-in ttyd: echoes what was typed, names itself, and closes on `exit`. It must listen on
 * the SHELL PORT, because the splice takes the tool server's address and swaps the port — same
 * pod, the sidecar beside it.
 */
function fakeTtyd() {
  const wss = new WebSocketServer({ host: "127.0.0.1", port: SHELL_PORT, handleProtocols: (s) => (s.has(TTYD_SUBPROTOCOL) ? TTYD_SUBPROTOCOL : false) });
  const seen: string[] = [];
  wss.on("connection", (up, req) => {
    seen.push(req.url ?? "");
    up.send(`2${JSON.stringify({ fontSize: 13 })}`);
    up.send("1shell");
    up.on("message", (d: Buffer) => {
      const frame = d.toString();
      seen.push(frame);
      if (frame.startsWith("0") && frame.slice(1).startsWith("exit")) return void up.close();
      if (frame.startsWith("0")) up.send(`0got:${frame.slice(1)}`);
    });
  });
  // A machine with 7790 busy skips rather than lies.
  return { wss, seen, ready: new Promise<boolean>((r) => (wss.once("listening", () => r(true)), wss.once("error", () => r(false)))) };
}

test("the splice speaks ttyd: an auth frame, then input and output unchanged", async (t0) => {
  const shell = fakeTtyd();
  if (!(await shell.ready)) return void t0.skip(`127.0.0.1:${SHELL_PORT} is busy on this machine`);
  // The workspace's tool-server address; the splice swaps the port to the sidecar's.
  const t = await up(async () => "127.0.0.1:7788");
  const w = new WebSocket(`ws://127.0.0.1:${t.port}/pty?scope=ws-0123456789abcdef`);
  try {
    await opened(w);
    const got = collect(w);
    resize(w, 100, 30);
    w.send("0printf hi\n");
    await until(() => got.text.includes("got:printf hi"), 5_000, "the echo through the splice");
    // ttyd's own subprotocol and route, its opening frame built from the size the client asked for.
    assert.equal(shell.seen[0], "/ws");
    assert.deepEqual(JSON.parse(shell.seen[1]), { AuthToken: "", columns: 100, rows: 30 });
    assert.equal(shell.seen[2], "0printf hi\n");
    // Title and preferences cross too; this test reads them where a terminal would.
    assert.equal(got.title, "shell");
  } finally {
    w.close();
    await t.down();
    shell.wss.close();
  }
});

test("a shell that ends closes the socket, and nothing retries", async (t0) => {
  const shell = fakeTtyd();
  if (!(await shell.ready)) return void t0.skip(`127.0.0.1:${SHELL_PORT} is busy on this machine`);
  const t = await up(async () => "127.0.0.1:7788");
  const w = new WebSocket(`ws://127.0.0.1:${t.port}/pty?scope=ws-0123456789abcdef`);
  try {
    await opened(w);
    collect(w);
    const closed = new Promise<number>((r) => w.once("close", (code: number) => r(code)));
    resize(w, 80, 24);
    w.send("0exit\n");
    await closed;
    assert.equal(w.readyState, WebSocket.CLOSED);
    // One connection, not a ladder of them: the socket WAS the shell.
    assert.equal(shell.seen.filter((x) => x === "/ws").length, 1);
  } finally {
    await t.down();
    shell.wss.close();
  }
});

test("nothing listening in the pod is one error frame and a close", async () => {
  const t = await up(async () => "127.0.0.1:7788");
  const w = new WebSocket(`ws://127.0.0.1:${t.port}/pty?scope=ws-0123456789abcdef`);
  try {
    await opened(w);
    const got = collect(w);
    resize(w, 80, 24);
    await new Promise((r) => w.once("close", r));
    assert.match(String(got.json[0]?.error ?? ""), /shell 127\.0\.0\.1:7790 did not answer/);
  } finally {
    await t.down();
  }
});

test("the shell is the sidecar's port, in the same pod as the tool server", () => {
  assert.equal(SHELL_PORT, 7790);
  assert.equal(shellAddress("10.42.3.190:7788"), "10.42.3.190:7790");
  assert.equal(shellAddress("10.42.3.190"), "10.42.3.190:7790");
  assert.equal(authFrame({ cols: 120, rows: 40 }), '{"AuthToken":"","columns":120,"rows":40}');
});

test("a bad scope is refused at the handshake, and there is no session to name", async () => {
  const t = await up();
  try {
    const bad = new WebSocket(`ws://127.0.0.1:${t.port}/pty?scope=not-a-workspace`);
    await assert.rejects(() => opened(bad), /Unexpected server response: 400/);
  } finally {
    await t.down();
  }
});
