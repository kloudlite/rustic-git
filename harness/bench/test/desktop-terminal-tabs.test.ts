import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";
import { BenchClient } from "../../src/bench-client.ts";
import { checkPty, checkWatch, closeSocket, readTtydFrame } from "../../src/pty-ipc.ts";
import { makeTab, nextIndex, scopeOfTab, sessionIndex, sessionName, sessionsOfTab, slug, type TermTab } from "../../src/renderer/components/terminal/tabs.ts";
import { WebSocketServer } from "ws";
import type { Machine, Workspace } from "../../src/renderer/model.ts";

const ws = (id: string, name: string, branch: string, state: Workspace["state"]) => ({ id, name, branch, state }) as Workspace;
const machine = {
  owner: "karthik@kloudlite.io",
  workspaces: [ws("ws-51480ba5", "rustic-git", "master", "running"), ws("ws-0000000000000001", "docs", "main", "stopped")],
} as Machine;

test("scopeOfTab: the tab is the scope; a stopped workspace has nothing to attach to", () => {
  assert.equal(scopeOfTab(machine, "ws-51480ba5"), "ws-51480ba5");
  assert.equal(scopeOfTab(machine, "ws-0000000000000001"), "bench"); // stopped
  assert.equal(scopeOfTab(machine, "m1"), "bench");
});

test("session names: kl-<slug>-<n>, always a legal tmux name", () => {
  assert.equal(sessionName("ws-51480ba5", 1), "kl-ws-51480ba5-1");
  assert.equal(slug("Karthik@Kloudlite.io"), "karthik-kloudlite-io");
  assert.equal(slug("---"), "tab");
  const long = sessionName("x".repeat(80), 12);
  assert.ok(long.length <= 48, long);
  for (const n of [sessionName("ws-51480ba5", 1), long, sessionName("--weird--name--", 3)])
    assert.match(n, /^[a-z0-9][a-z0-9-]{0,47}$/, n);
});

test("a listing materialises this tab's terminals and nobody else's", () => {
  const names = ["kl-ws-51480ba5-1", "kl-ws-51480ba5-3", "kl-other-1", "kl-ws-51480ba5-x", "scratch"];
  assert.deepEqual(sessionsOfTab(names, "ws-51480ba5"), ["kl-ws-51480ba5-1", "kl-ws-51480ba5-3"]);
  assert.equal(sessionIndex("kl-ws-51480ba5-3", "ws-51480ba5"), 3);
  assert.equal(sessionIndex("kl-other-1", "ws-51480ba5"), 0);
  // The next "+" fills the gap rather than colliding with 1 or 3.
  assert.equal(nextIndex(sessionsOfTab(names, "ws-51480ba5"), "ws-51480ba5"), 2);
  assert.equal(nextIndex([], "ws-51480ba5"), 1);
});

test("makeTab: the banner names the scope, the tab owns it, the session is the tab's", () => {
  const b = makeTab(machine, "Kloudlite", "m1", "bench", 1);
  assert.match(b.banner, /^kloudlite shell · bench · Kloudlite\r\n/);
  // The banner says what the shell IS: the person's home in that pod, with no code in it (§2.1).
  assert.match(b.banner, /the person's home in the bench pod/);
  assert.equal(b.label, "bench");
  assert.equal(b.owner, "m1");
  assert.equal(b.session, "kl-m1-1");

  const w = makeTab(machine, "Kloudlite", "ws-51480ba5", "ws-51480ba5", 2);
  assert.match(w.banner, /^kloudlite shell · rustic-git · Kloudlite\r\n/);
  assert.match(w.banner, /the person's home in this workspace's pod — the code is not mounted here/);
  assert.equal(w.session, "kl-ws-51480ba5-2");
  assert.notEqual(w.id, b.id); // one id per tab, never reused
});

// The drawer shows the active session tab's terminals only; the rest stay
// mounted and keep their sockets.
test("tabs belong to the tab they were opened from", () => {
  const all = [makeTab(machine, "K", "m1", "bench", 1), makeTab(machine, "K", "ws-51480ba5", "ws-51480ba5", 1), makeTab(machine, "K", "m1", "bench", 2)];
  assert.deepEqual(all.filter((t) => t.owner === "m1").map((t) => t.session), ["kl-m1-1", "kl-m1-2"]);
  assert.deepEqual(all.filter((t) => t.owner === "ws-51480ba5").map((t) => t.session), ["kl-ws-51480ba5-1"]);
});

// A tab names a shell; the socket IS that shell (spec §2.3).
const tab = (id: string, session: string, at: number, owner = "m1") => ({ id, session, owner, label: "bench", scope: "bench", banner: "", at }) as TermTab;
const NOW = 1_000_000;

test("pty ipc: only a tab id and a real scope are accepted", () => {
  assert.deepEqual(checkPty("t3", "bench"), { id: "t3", scope: "bench" });
  assert.deepEqual(checkPty("t1", "ws-0123456789abcdef"), { id: "t1", scope: "ws-0123456789abcdef" });
  for (const bad of [["x1", "bench"], ["t1", "machine"], ["t1", "ws-51480ba5"], ["t1", "../events"], [1, "bench"], ["t1", 2]] as [unknown, unknown][]) {
    assert.throws(() => checkPty(bad[0], bad[1]), /not a (terminal id|shell scope)/);
  }
});

/**
 * The file-system watch is named by the WORKSPACE it follows — one per workspace, no tab and no id.
 * It was opened through the shell's own check, which wants a `t3`-shaped id, so every open threw
 * "not a terminal id" on the owner's desktop and the Files tab heard nothing (2026-09-18).
 */
test("watch ipc: a workspace scope opens one; a terminal id is not a scope", () => {
  assert.equal(checkWatch("ws-0123456789abcdef"), "ws-0123456789abcdef");
  assert.equal(checkWatch("bench"), "bench");
  for (const bad of ["t3", "machine", "ws-51480ba5", "../events", 1, undefined]) assert.throws(() => checkWatch(bad), /not a shell scope/, String(bad));
});

test("BenchClient.pty: refused while offline, a shell from the pod's shell sidecar once connected", async (t) => {
  // The bench scope splices to the `shell` sidecar in its own pod; stand that container in here,
  // speaking ttyd: `0` input, `0` output (spec §2.3).
  const tools = new WebSocketServer({ host: "127.0.0.1", port: 7790 });
  const listening = await new Promise<boolean>((r) => (tools.once("listening", () => r(true)), tools.once("error", () => r(false))));
  if (!listening) return void t.skip("127.0.0.1:7790 is busy on this machine");
  tools.on("connection", (up) => {
    up.on("message", (d: Buffer) => {
      const frame = d.toString("utf8");
      // The first frame is ttyd's auth JSON; after that `0` is what was typed.
      if (!frame.startsWith("0")) return;
      const line = frame.slice(1);
      if (line.startsWith("exit")) return void up.close();
      up.send(`0kl-ok`);
    });
  });
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "desk-pty-"));
  const bench = new Bench({ dir: path.join(dir, "bench"), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0, "127.0.0.1");
  const c = new BenchClient(`http://127.0.0.1:${srv.port}`, () => undefined, path.join(dir, "cache.json"));
  try {
    assert.throws(() => c.pty("bench"), /not connected/);
    c.start();
    await until(() => c.connected(), 5_000, "the client to connect");

    const w = c.pty("bench");
    let out = "";
    const json: Record<string, unknown>[] = [];
    w.on("message", (d: Buffer, binary: boolean) => {
      const text = d.toString();
      if (!binary && text.startsWith("0")) return void (out += text.slice(1));
      if (!binary && /^[12]/.test(text)) return; // ttyd's title and preferences
      if (binary) return void (out += text);
      json.push(JSON.parse(text) as Record<string, unknown>);
    });
    await new Promise((r, j) => (w.once("open", r), w.once("error", j)));
    w.send(JSON.stringify({ resize: { cols: 100, rows: 30 } }));
    w.send("0printf kl-%s ok\n");
    await until(() => out.includes("kl-ok"), 10_000, "the shell's output");
    // A shell that ends just closes: ttyd has no exit frame, and nothing reattaches (spec §2.3).
    const closed = new Promise((r) => w.once("close", r));
    w.send("0exit\n");
    await closed;
    assert.deepEqual(json, [], "no control frames of our own once the shell is attached");
  } finally {
    c.close();
    await bench.stop();
    await srv.close();
    tools.close();
  }
});

/**
 * What the desktop does with each ttyd frame. The owner's terminal printed
 * `1/nix/profile/current/bin/zsh -l (ws)2{ "disableLeaveAlert": true, … }` before the shell ended
 * (2026-09-18): ttyd 1.7 sends the title and the preferences as TEXT frames, and they were passed
 * through whole because only binary frames were being stripped.
 */
test("ttyd frames: the opcode is the first byte, text or binary, and an unknown one is dropped", () => {
  const text = (s: string) => readTtydFrame(Buffer.from(s, "utf8"), false);
  const binary = (s: string) => readTtydFrame(Buffer.from(s, "utf8"), true);

  // Output, either way it arrives.
  const asBinary = binary("0hello");
  assert.equal(asBinary.kind, "data");
  assert.equal(Buffer.from((asBinary as { data: Uint8Array }).data).toString(), "hello");
  const asText = text("0hello");
  assert.equal(asText.kind, "data");
  assert.equal(Buffer.from((asText as { data: Uint8Array }).data).toString(), "hello");

  // A TEXT title is a title, not scrollback.
  assert.deepEqual(text("1/nix/profile/current/bin/zsh -l (ws)"), { kind: "title", title: "/nix/profile/current/bin/zsh -l (ws)" });
  // ttyd's preferences are this app's business, not the terminal's.
  assert.deepEqual(text(`2${JSON.stringify({ disableLeaveAlert: true, fontFamily: "IBM Plex Mono", fontSize: 13 })}`), { kind: "ignore" });
  // The bench's own control frames still reach the exit path.
  assert.deepEqual(text(JSON.stringify({ exit: 3 })), { kind: "exit", code: 3 });
  assert.deepEqual(text(JSON.stringify({ error: "shell 10.42.3.190:7790 did not answer" })), { kind: "exit", error: "shell 10.42.3.190:7790 did not answer" });
  // Anything else is dropped rather than printed: an opcode ttyd adds later must not land in a
  // person's scrollback.
  for (const odd of ["9whatever", "", "not json at all"]) assert.deepEqual(text(odd), { kind: "ignore" }, odd);
  assert.deepEqual(binary(JSON.stringify({ exit: 0 })), { kind: "ignore" }, "a binary frame is never control JSON");
});

/**
 * `ws` THROWS from `close()` while a socket is still CONNECTING, and an uncaught throw in the main
 * process is Electron's crash dialog — which is what a Files tab left before its watch opened did
 * to the owner's desktop (2026-09-18).
 */
test("closing a socket that never connected does not throw", () => {
  const calls: string[] = [];
  const connecting = {
    readyState: 0,
    close() {
      calls.push("close");
      throw new Error("WebSocket was closed before the connection was established");
    },
    terminate() {
      calls.push("terminate");
    },
  };
  closeSocket(connecting);
  assert.deepEqual(calls, ["terminate"], "a connecting socket is terminated, never closed");

  // An open one is closed the ordinary way, and a close that throws anyway is swallowed.
  const open = { readyState: 1, close: () => calls.push("close-open"), terminate: () => calls.push("nope") };
  closeSocket(open);
  assert.deepEqual(calls, ["terminate", "close-open"]);
  assert.doesNotThrow(() => closeSocket({ readyState: 1, close: () => { throw new Error("already gone"); } }));
  assert.doesNotThrow(() => closeSocket(undefined));
});
