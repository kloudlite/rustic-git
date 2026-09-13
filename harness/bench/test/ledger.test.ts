import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Tasks, Procs } from "../src/ledger.ts";

const dir = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-ledger-"));

test("tasks fold from the log and in-flight ones are lost after a restart", () => {
  const d = dir();
  const a = new Tasks(d);
  a.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "running", started: 1 });
  a.transition({ id: "t2", session: "s-1", tool: "Bash", arg: "ls", state: "running", started: 2 });
  a.transition({ id: "t1", n: 1, state: "background" });
  a.transition({ id: "t2", state: "done", ended: 3 });
  const b = new Tasks(d);
  assert.deepEqual(b.markLost().map((t) => t.id), ["t1"]);
  assert.equal(new Tasks(d).all().find((t) => t.id === "t1")!.state, "lost");
  assert.equal(new Tasks(d).markLost().length, 0);
});

test("procs keep other sessions' rows; a restart loses every open row, even one whose old pid is alive now", () => {
  const d = dir();
  const p = new Procs(d);
  // process.pid is alive: a pid reused in a restarted container must not keep the row open.
  p.snapshot("s-1", [{ id: "p1", name: "vite", command: "npm run dev", pid: process.pid, started: 1 }]);
  p.snapshot("s-2", [{ id: "p1", name: "tunnel", command: "kl tunnel", pid: 200, started: 1 }]);
  p.snapshot("s-1", [{ id: "p1", name: "vite", command: "npm run dev", pid: process.pid, started: 1 }, { id: "p2", name: "w", command: "w", pid: 300, started: 2, ended: 5, code: 0 }]);
  assert.equal(p.all().length, 3);
  const lost = new Procs(d).markLost();
  assert.deepEqual(lost.map((r) => `${r.session}/${r.id}`).sort(), ["s-1/p1", "s-2/p1"]);
  const row = new Procs(d).all().find((r) => r.session === "s-2")!;
  assert.equal(row.lost, true);
  assert.equal(typeof row.ended, "number");
  assert.equal(new Procs(d).all().find((r) => r.id === "p2")!.lost, undefined, "an ended row stays as it was");
});

test("procs markLost for one session leaves another's open row open", () => {
  const p = new Procs(dir());
  p.snapshot("s-1", [{ id: "p1", name: "a", command: "a", pid: 1, started: 1 }]);
  p.snapshot("s-2", [{ id: "p1", name: "b", command: "b", pid: 2, started: 1 }]);
  assert.deepEqual(p.markLost("s-1").map((r) => r.session), ["s-1"]);
  assert.equal(p.all().find((r) => r.session === "s-2")!.ended, undefined);
});

test("markLost for one session leaves another session's running task running", () => {
  const t = new Tasks(dir());
  t.transition({ id: "a", session: "s-1", tool: "Bash", arg: "x", state: "running", started: 1 });
  t.transition({ id: "b", session: "s-2", tool: "Bash", arg: "y", state: "background", started: 1 });
  assert.deepEqual(t.markLost("s-1").map((r) => r.id), ["a"]);
  assert.equal(t.all().find((r) => r.id === "b")!.state, "background");
});
