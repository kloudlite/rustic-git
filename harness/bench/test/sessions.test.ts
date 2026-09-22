import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { SessionList } from "../src/sessions.ts";

const dir = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-sessions-"));

test("create numbers after the highest seq and persists", () => {
  const d = dir();
  const a = new SessionList(d);
  const s1 = a.create();
  const s2 = a.create();
  assert.equal(s1.id, "s-1");
  assert.equal(s2.name, "session 2");
  a.update("s-1", { archived: true, file: "/bench/sessions/x.jsonl" });
  const b = new SessionList(d);
  assert.deepEqual(b.all().map((s) => [s.id, s.archived]), [["s-1", true], ["s-2", false]]);
});

test("merge adds only unknown ids, so a re-run is a no-op", () => {
  const d = dir();
  const a = new SessionList(d);
  const row = { id: "bench", name: "session 1", seq: 1, created: 1, lastActive: 1, archived: false };
  assert.deepEqual(a.merge([row]), ["bench"]);
  assert.deepEqual(a.merge([{ ...row, name: "changed" }]), []);
  assert.equal(new SessionList(d).get("bench")!.name, "session 1");
  assert.equal(a.create().id, "s-2");
});

test("an id is never reused, even after the row is removed and the list reopened", () => {
  const d = dir();
  const a = new SessionList(d);
  a.create();
  a.create();
  a.remove("s-2");
  const b = new SessionList(d);
  assert.equal(b.create().id, "s-3");
});

test("merge advances nextSeq past every merged row's seq, so a deleted import is never reused", () => {
  const d = dir();
  const a = new SessionList(d);
  a.merge([{ id: "s-5", name: "session 5", seq: 5, created: 1, lastActive: 1, archived: false }]);
  a.remove("s-5");
  assert.equal(a.create().id, "s-6");
});

test("remove drops the row", () => {
  const a = new SessionList(dir());
  a.create();
  a.remove("s-1");
  assert.equal(a.all().length, 0);
  assert.throws(() => a.update("s-1", {}), /no session s-1/);
});

test("tree fields persist and children are found by parent seq", () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "tree-"));
  const l = new SessionList(d);
  const top = l.create(undefined, { tier: "top", state: "open" });
  const main = l.create(undefined, { tier: "main", state: "open", workspace: "ws1" });
  const sub = l.create(undefined, { tier: "sub", state: "open", parent: main.seq, workspace: "ws1-c1" });
  assert.deepEqual(l.children(main.seq).map((s) => s.seq), [sub.seq]);
  assert.equal(l.bySeq(top.seq)?.tier, "top");
  assert.equal(l.logFile(sub), path.join(d, "sessions", `${sub.seq}.jsonl`));
  const again = new SessionList(d);
  assert.equal(again.bySeq(sub.seq)?.parent, main.seq);
});
