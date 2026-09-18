import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { ExchangeLog } from "../src/exchanges.ts";

const dir = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-ex-"));

test("views rebuilt from the log agree with the live ones", () => {
  const d = dir();
  const a = new ExchangeLog(d);
  a.record({ id: "e1", session: "s-1", workspace: "api", dir: "out", text: "run tests", state: "sent" });
  a.record({ id: "e2", session: "s-2", workspace: "api", dir: "out", text: "bump dep", state: "sent" });
  a.transition("e1", "done");
  const b = new ExchangeLog(d);
  assert.deepEqual(b.bySession("s-1"), a.bySession("s-1"));
  assert.deepEqual(b.byWorkspace("api"), a.byWorkspace("api"));
  assert.equal(b.bySession("s-1")[0].state, "done");
  // Both views are filters over one log, so every row is in both.
  const ws = b.byWorkspace("api").map((e) => e.id).sort();
  const ss = [...b.bySession("s-1"), ...b.bySession("s-2")].map((e) => e.id).sort();
  assert.deepEqual(ws, ss);
});

test("a discard line removes the session's rows from the workspace view, also after a restart", () => {
  const d = dir();
  const a = new ExchangeLog(d);
  a.record({ id: "e1", session: "s-1", workspace: "api", dir: "out", text: "x", state: "sent" });
  a.record({ id: "e2", session: "s-2", workspace: "api", dir: "out", text: "y", state: "sent" });
  a.discard("s-1");
  assert.deepEqual(a.byWorkspace("api").map((e) => e.id), ["e2"]);
  assert.deepEqual(new ExchangeLog(d).byWorkspace("api").map((e) => e.id), ["e2"]);
  assert.deepEqual(new ExchangeLog(d).bySession("s-1"), []);
});

test("after pages by timestamp", async () => {
  const a = new ExchangeLog(dir());
  const first = a.record({ id: "e1", session: "s-1", workspace: "api", dir: "out", text: "x", state: "sent" });
  await new Promise((r) => setTimeout(r, 2));
  a.record({ id: "e2", session: "s-1", workspace: "api", dir: "in", text: "ok", state: "done" });
  assert.deepEqual(a.bySession("s-1", first.ts).map((e) => e.id), ["e2"]);
});

test("active views include every open row after an unbounded log tail", () => {
  const a = new ExchangeLog(dir());
  a.record({ id: "open", session: "s-1", workspace: "api", dir: "out", text: "still open", state: "queued" });
  for (let i = 0; i < 501; i++) a.record({ id: `done-${i}`, session: "s-1", workspace: "api", dir: "out", text: "finished", state: "done" });
  assert.deepEqual(a.active().map((e) => e.id), ["open"]);
});
