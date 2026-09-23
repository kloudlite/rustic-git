import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Session, TIER_TOOLS, type Turn, type Hooks } from "../src/session.ts";
import { readRows } from "../src/rows.ts";
import type { SessionRow } from "../src/sessions.ts";

const row = (seq: number, tier: "top" | "main" | "sub"): SessionRow => ({ id: `s${seq}`, name: `s${seq}`, seq, created: 0, lastActive: 0, archived: false, tier, state: "open", workspace: tier === "top" ? undefined : `ws${seq}` });
const file = () => path.join(fs.mkdtempSync(path.join(os.tmpdir(), "sess-")), "1.jsonl");
const hooks = (over: Partial<Hooks> = {}): Hooks => ({ delegate: async () => "delegated", tell: () => {}, askPerson: async () => "yes", ...over });

test("a turn joins unread rows into one prompt and writes start then end", async () => {
  const seen: string[] = [];
  const turn: Turn = async (c) => { seen.push(c.prompt); return "answer"; };
  const s = new Session(row(1, "main"), file(), turn, hooks());
  s.receive("person", "one");
  s.receive(3, "two");
  await s.turn();
  assert.equal(seen.length, 1);
  assert.match(seen[0], /one[\s\S]*two/);
  assert.deepEqual(readRows(s.file).map((r) => r.kind), ["user", "user", "turn.start", "turn.end"]);
  assert.equal(s.hasUnread(), false);
  assert.equal(s.running, false);
});

test("an engine failure ends the turn with error and leaves the rows unread", async () => {
  const s = new Session(row(1, "main"), file(), async () => { throw new Error("llm down"); }, hooks());
  s.receive("person", "x");
  await s.turn();
  const end = readRows(s.file).at(-1) as { kind: string; error?: string };
  assert.equal(end.kind, "turn.end");
  assert.match(end.error!, /llm down/);
  assert.equal(s.hasUnread(), true);
});

test("tier allow-lists: top has no workspace tools, main is read-only, sub has all", () => {
  const names = (t: "top" | "main" | "sub") => new Session(row(1, t), file(), async () => "", hooks()).tools().map((x) => x.name);
  assert.ok(!names("top").includes("read"));
  assert.ok(names("top").includes("delegate"));
  assert.ok(names("main").includes("read") && !names("main").includes("write") && names("main").includes("push"));
  assert.ok(names("sub").includes("write") && !names("sub").includes("delegate"));
});

test("a read-only main refuses write even if asked by name", async () => {
  const turn: Turn = async (c) => { assert.equal(c.readOnly, true); assert.ok(!c.tools.some((t) => t.name === "write")); return "ok"; };
  const s = new Session(row(1, "main"), file(), turn, hooks());
  s.receive("person", "write a file");
  await s.turn();
});

test("delegate goes through the hook and the answer is the tool's text", async () => {
  const calls: string[] = [];
  const turn: Turn = async (c) => c.tools.find((t) => t.name === "delegate")!.run(c.cwd, { target: "", instruction: "do it" }, { user: c.user });
  const s = new Session(row(1, "main"), file(), turn, hooks({ delegate: async (_s, target, instruction) => { calls.push(`${target}|${instruction}`); return "delegated to 2"; } }));
  s.receive("person", "go");
  await s.turn();
  assert.deepEqual(calls, ["|do it"]);
  assert.equal((readRows(s.file).at(-1) as { answer: string }).answer, "delegated to 2");
});

test("abort marks the open turn interrupted and clears running", async () => {
  let release!: () => void;
  const s = new Session(row(1, "sub"), file(), (c) => new Promise((r) => { release = () => r("late"); c.signal.addEventListener("abort", () => r("aborted")); }), hooks());
  s.receive("person", "x");
  const p = s.turn();
  await new Promise((r) => setTimeout(r, 10));
  s.abort();
  await p;
  const kinds = readRows(s.file).map((r) => r.kind);
  assert.ok(kinds.includes("interrupted"));
  assert.ok(!kinds.includes("turn.end"));
  assert.equal(s.running, false);
  release();
});
