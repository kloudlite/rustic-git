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
import { checkPty } from "../../src/pty-ipc.ts";
import { makeTab, scopesOf } from "../../src/renderer/components/terminal/tabs.ts";
import type { Machine, Workspace } from "../../src/renderer/model.ts";

const ws = (id: string, name: string, branch: string, state: Workspace["state"]) => ({ id, name, branch, state }) as Workspace;
const machine = {
  owner: "karthik@kloudlite.io",
  workspaces: [ws("ws-51480ba5", "rustic-git", "master", "running"), ws("ws-0000000000000001", "docs", "main", "stopped")],
} as Machine;

test("scopesOf: the bench first, a stopped workspace listed but disabled", () => {
  const s = scopesOf(machine);
  assert.deepEqual(s[0], { id: "bench", label: "bench", sub: "your machine in the team", kind: "bench" });
  assert.deepEqual(s[1], { id: "ws-51480ba5", label: "rustic-git", sub: "master", kind: "workspace", disabled: false });
  assert.deepEqual(s[2], { id: "ws-0000000000000001", label: "docs", sub: "stopped", kind: "workspace", disabled: true });
});

test("makeTab: the banner names the scope and the team", () => {
  const b = makeTab(machine, "Kloudlite", "bench");
  assert.match(b.banner, /^kloudlite shell · bench · Kloudlite\r\n/);
  assert.match(b.banner, /workspaces resolve by their tool servers/);
  assert.equal(b.label, "bench");

  const w = makeTab(machine, "Kloudlite", "ws-51480ba5");
  assert.match(w.banner, /^kloudlite shell · rustic-git · Kloudlite\r\n/);
  assert.match(w.banner, /the workspace is the working directory/);
  assert.notEqual(w.id, b.id); // one id per tab, never reused
});

test("pty ipc: only a tab id and a real scope are accepted", () => {
  assert.deepEqual(checkPty("t3", "bench"), { id: "t3", scope: "bench" });
  assert.deepEqual(checkPty("t1", "ws-0123456789abcdef"), { id: "t1", scope: "ws-0123456789abcdef" });
  for (const bad of [["x1", "bench"], ["t1", "machine"], ["t1", "ws-51480ba5"], ["t1", "../events"], [1, "bench"], ["t1", 2]] as [unknown, unknown][]) {
    assert.throws(() => checkPty(bad[0], bad[1]), /not a (terminal id|shell scope)/);
  }
});

test("BenchClient.pty: refused while offline, a real shell once connected", async () => {
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
  }
});
