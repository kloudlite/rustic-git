import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import WebSocket, { WebSocketServer } from "ws";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";

async function up(resolveTools?: (ws: string) => Promise<string>) {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-pty-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0, "127.0.0.1", undefined, undefined, { resolveTools });
  return { srv, port: srv.port, down: async () => (await bench.stop(), await srv.close()) };
}

const opened = (w: WebSocket) => new Promise((r, j) => (w.once("open", r), w.once("error", j)));

/** Collects both halves of the protocol: raw PTY bytes and the control JSON. */
function collect(w: WebSocket) {
  const out = { text: "", json: [] as Record<string, unknown>[] };
  w.on("message", (d: Buffer, binary: boolean) => {
    if (binary) out.text += d.toString("utf8");
    else out.json.push(JSON.parse(d.toString()) as Record<string, unknown>);
  });
  return out;
}
const bin = (w: WebSocket, s: string) => w.send(Buffer.from(s, "utf8"), { binary: true });
const resize = (w: WebSocket, cols: number, rows: number) => w.send(JSON.stringify({ resize: { cols, rows } }));

/** The workspace container's tool server lives on a fixed port; bind it only if this machine has it free. */
async function fakeLocalTools(): Promise<WebSocketServer | undefined> {
  const wss = new WebSocketServer({ host: "127.0.0.1", port: 7788 });
  return new Promise((r) => {
    wss.once("listening", () => r(wss));
    wss.once("error", () => r(undefined));
  });
}

test("bench scope: the socket splices to the workspace container's own tool server", async (t) => {
  const tools = await fakeLocalTools();
  if (!tools) return void t.skip("127.0.0.1:7788 is busy on this machine");
  const seen: string[] = [];
  tools.on("connection", (up, req) => {
    seen.push(req.url ?? "");
    up.on("message", (d: Buffer, binary: boolean) => {
      if (!binary) return void seen.push(d.toString());
      up.send(Buffer.from("got:" + d.toString("utf8")), { binary: true });
      up.send(JSON.stringify({ exit: 3 }));
      up.close();
    });
  });
  const b = await up();
  const w = new WebSocket(`ws://127.0.0.1:${b.port}/pty?scope=bench`);
  try {
    await opened(w);
    const got = collect(w);
    const closed = new Promise((r) => w.once("close", r));
    resize(w, 100, 30);
    bin(w, "printf hi\n");
    await closed;
    assert.equal(got.text, "got:printf hi\n");
    assert.deepEqual(got.json, [{ exit: 3 }]);
    assert.equal(seen[0], "/stream/pty");
    assert.deepEqual(JSON.parse(seen[1]), { resize: { cols: 100, rows: 30 } });
  } finally {
    w.close();
    await b.down();
    tools.close();
  }
});

test("bench scope: nothing listening in the pod is one error frame and a close", async (t) => {
  const tools = await fakeLocalTools();
  if (!tools) return void t.skip("127.0.0.1:7788 is busy on this machine");
  tools.close();
  await new Promise((r) => setTimeout(r, 50));
  const b = await up();
  const w = new WebSocket(`ws://127.0.0.1:${b.port}/pty?scope=bench`);
  try {
    await opened(w);
    const got = collect(w);
    resize(w, 80, 24);
    await new Promise((r) => w.once("close", r));
    assert.match(String(got.json[0]?.error ?? ""), /127\.0\.0\.1:7788 did not answer/);
  } finally {
    await b.down();
  }
});

test("workspace scope: the resize and the bytes cross to the tool server and the exit comes back", async () => {
  // A stand-in tool server: upper-cases binary frames, exits on {"bye":1}.
  const seen: string[] = [];
  const wss = new WebSocketServer({ port: 0, host: "127.0.0.1" });
  wss.on("connection", (up, req) => {
    seen.push(req.url ?? "");
    up.on("message", (d: Buffer, binary: boolean) => {
      if (binary) return void up.send(Buffer.from(d.toString("utf8").toUpperCase(), "utf8"), { binary: true });
      seen.push(d.toString());
      if (d.toString().includes("bye")) (up.send(JSON.stringify({ exit: 0 })), up.close());
    });
  });
  await new Promise((r) => wss.once("listening", r));
  const addr = `127.0.0.1:${(wss.address() as { port: number }).port}`;
  const t = await up(async () => addr);
  const w = new WebSocket(`ws://127.0.0.1:${t.port}/pty?scope=ws-0123456789abcdef`);
  try {
    await opened(w);
    const got = collect(w);
    resize(w, 120, 40);
    bin(w, "hello");
    await until(() => got.text.includes("HELLO"), 5_000, "the echo through the splice");
    const closed = new Promise((r) => w.once("close", r));
    w.send(JSON.stringify({ bye: 1 }));
    await closed;
    assert.deepEqual(got.json, [{ exit: 0 }]);
    assert.equal(seen[0], "/stream/pty");
    assert.deepEqual(JSON.parse(seen[1]), { resize: { cols: 120, rows: 40 } });
  } finally {
    w.close();
    await new Promise((r) => wss.close(r));
    await t.down();
  }
});

test("workspace scope: a resolve failure is one error frame and a close", async () => {
  const t = await up(async () => {
    throw new Error("workspace ws-0123456789abcdef is stopped");
  });
  const w = new WebSocket(`ws://127.0.0.1:${t.port}/pty?scope=ws-0123456789abcdef`);
  try {
    await opened(w);
    const got = collect(w);
    resize(w, 80, 24);
    await new Promise((r) => w.once("close", r));
    assert.deepEqual(got.json, [{ error: "workspace ws-0123456789abcdef is stopped" }]);
  } finally {
    await t.down();
  }
});

test("bytes typed right after the resize survive a slow workspace resolve", async () => {
  // A fake tool server that echoes what it receives; the resolve takes 300 ms, longer than any
  // tick — exactly the window that lost the probe's command on 2026-09-16.
  const tool = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  tool.on("connection", (up) => {
    up.on("message", (d: Buffer, binary: boolean) => {
      if (!binary) return;
      up.send(Buffer.from("got:" + d.toString("utf8")), { binary: true });
      up.send(JSON.stringify({ exit: 0 }));
      up.close();
    });
  });
  await new Promise<void>((r) => tool.once("listening", () => r()));
  const addr = `127.0.0.1:${(tool.address() as { port: number }).port}`;
  const t = await up(() => new Promise((r) => setTimeout(() => r(addr), 300)));
  try {
    const w = new WebSocket(`ws://127.0.0.1:${t.port}/pty?scope=ws-0123456789abcdef`);
    const out = collect(w);
    const closed = new Promise<void>((r) => w.once("close", () => r()));
    await opened(w);
    resize(w, 100, 30);
    bin(w, "pwd\n");
    await closed;
    assert.equal(out.text, "got:pwd\n");
    assert.deepEqual(out.json, [{ exit: 0 }]);
  } finally {
    await t.down();
    tool.close();
  }
});
