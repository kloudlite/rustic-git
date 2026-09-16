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
import { checkPty, checkSession } from "../../src/pty-ipc.ts";
import { makeTab, nextIndex, scopeOfTab, sessionIndex, sessionName, sessionsOfTab, slug } from "../../src/renderer/components/terminal/tabs.ts";
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
  assert.match(b.banner, /workspaces resolve by their tool servers/);
  assert.equal(b.label, "bench");
  assert.equal(b.owner, "m1");
  assert.equal(b.session, "kl-m1-1");

  const w = makeTab(machine, "Kloudlite", "ws-51480ba5", "ws-51480ba5", 2);
  assert.match(w.banner, /^kloudlite shell · rustic-git · Kloudlite\r\n/);
  assert.match(w.banner, /the workspace is the working directory/);
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

test("pty ipc: a session name is the tool server's rule, refused before tmux sees it", () => {
  assert.equal(checkSession(undefined), undefined);
  assert.equal(checkSession("kl-m1-1"), "kl-m1-1");
  for (const bad of ["-lead", "Upper", "has space", "a".repeat(49), "semi;colon", "", 7])
    assert.throws(() => checkSession(bad), /not a session name/, String(bad));
});

test("pty ipc: only a tab id and a real scope are accepted", () => {
  assert.deepEqual(checkPty("t3", "bench"), { id: "t3", scope: "bench" });
  assert.deepEqual(checkPty("t1", "ws-0123456789abcdef"), { id: "t1", scope: "ws-0123456789abcdef" });
  for (const bad of [["x1", "bench"], ["t1", "machine"], ["t1", "ws-51480ba5"], ["t1", "../events"], [1, "bench"], ["t1", 2]] as [unknown, unknown][]) {
    assert.throws(() => checkPty(bad[0], bad[1]), /not a (terminal id|shell scope)/);
  }
});

test("BenchClient.pty: refused while offline, a shell from the pod's tool server once connected", async (t) => {
  // The bench scope splices to the workspace container beside it; stand that container in here.
  const tools = new WebSocketServer({ host: "127.0.0.1", port: 7788 });
  const listening = await new Promise<boolean>((r) => (tools.once("listening", () => r(true)), tools.once("error", () => r(false))));
  if (!listening) return void t.skip("127.0.0.1:7788 is busy on this machine");
  tools.on("connection", (up) => {
    up.on("message", (d: Buffer, binary: boolean) => {
      if (!binary) return;
      const line = d.toString("utf8");
      if (line.startsWith("exit ")) return void (up.send(JSON.stringify({ exit: Number(line.slice(5)) })), up.close());
      up.send(Buffer.from("kl-ok"), { binary: true });
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
    w.on("message", (d: Buffer, binary: boolean) => (binary ? (out += d.toString()) : json.push(JSON.parse(d.toString()))));
    await new Promise((r, j) => (w.once("open", r), w.once("error", j)));
    w.send(JSON.stringify({ resize: { cols: 100, rows: 30 } }));
    w.send(Buffer.from("printf kl-%s ok\n"), { binary: true });
    await until(() => out.includes("kl-ok"), 10_000, "the shell's output");
    const closed = new Promise((r) => w.once("close", r));
    w.send(Buffer.from("exit 3\n"), { binary: true });
    await closed;
    assert.deepEqual(json, [{ exit: 3 }]);
  } finally {
    c.close();
    await bench.stop();
    await srv.close();
    tools.close();
  }
});
