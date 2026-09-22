import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Scheduler } from "../src/scheduler.ts";
import { Session, type Hooks, type Turn } from "../src/session.ts";
import { SessionList } from "../src/sessions.ts";
import { append, readRows } from "../src/rows.ts";

const hooks: Hooks = { delegate: async () => "d", tell: () => {}, askPerson: async () => "" };
const setup = (turn: Turn, max?: number) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sched-"));
  const list = new SessionList(dir);
  const sched = new Scheduler(list, (row) => new Session(row, list.logFile(row), turn, hooks), max);
  return { dir, list, sched };
};
const tick = () => new Promise((r) => setTimeout(r, 20));

test("boot marks an open turn interrupted and does not resume it", async () => {
  const ran: string[] = [];
  const { list, sched } = setup(async (c) => { ran.push(c.prompt); return "ok"; });
  const s = list.create(undefined, { tier: "main", state: "open", workspace: "w" });
  append(list.logFile(s), { kind: "user", ts: 1, from: "person", text: "old" });
  append(list.logFile(s), { kind: "turn.start", ts: 2, turn: 1 });
  sched.boot();
  await tick();
  const kinds = readRows(list.logFile(s)).map((r) => r.kind);
  assert.deepEqual(kinds.slice(0, 3), ["user", "turn.start", "interrupted"]);
  // the user row is still unread, so the scheduler runs a NEW turn 2 for it — but never re-runs turn 1
  assert.deepEqual(ran, ["old"]);
  assert.ok(kinds.includes("turn.end"));
});

test("boot re-delivers a child answer whose parent user row is missing", async () => {
  const { list, sched } = setup(async () => "ok");
  const main = list.create(undefined, { tier: "main", state: "open", workspace: "m" });
  const sub = list.create(undefined, { tier: "sub", state: "open", parent: main.seq, workspace: "c" });
  append(list.logFile(sub), { kind: "user", ts: 1, from: main.seq, text: "do" });
  append(list.logFile(sub), { kind: "turn.start", ts: 2, turn: 1 });
  append(list.logFile(sub), { kind: "turn.end", ts: 3, turn: 1, answer: "did it" });
  sched.boot();
  await tick();
  const parent = readRows(list.logFile(main));
  const got = parent.find((r) => r.kind === "user" && r.from === sub.seq && r.childTurn === 1);
  assert.ok(got, "parent got the child's answer");
  sched.boot(); // idempotent: matched by child seq and turn index
  assert.equal(readRows(list.logFile(main)).filter((r) => r.kind === "user").length, 1);
});

test("the cap holds and waiting sessions run in seq order", async () => {
  let live = 0, peak = 0;
  const order: number[] = [];
  const gates: (() => void)[] = [];
  const { list, sched } = setup((c) => new Promise((r) => { live++; peak = Math.max(peak, live); order.push(Number(c.prompt)); gates.push(() => { live--; r("ok"); }); }), 2);
  for (let i = 1; i <= 4; i++) { const s = list.create(undefined, { tier: "main", state: "open", workspace: `w${i}` }); append(list.logFile(s), { kind: "user", ts: i, from: "person", text: String(s.seq) }); }
  sched.boot();
  await tick();
  assert.equal(peak, 2);
  gates.shift()!(); await tick();
  gates.shift()!(); await tick();
  gates.shift()!(); gates.shift()!(); await tick();
  assert.deepEqual(order, [1, 2, 3, 4]);
});

test("a closed session never runs", async () => {
  const ran: string[] = [];
  const { list, sched } = setup(async (c) => { ran.push(c.prompt); return "ok"; });
  const s = list.create(undefined, { tier: "sub", state: "closed", workspace: "w" });
  append(list.logFile(s), { kind: "user", ts: 1, from: "person", text: "x" });
  sched.boot();
  await tick();
  assert.deepEqual(ran, []);
});
