import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { append, readRows, unread, openTurn, nextTurn, lastEnd, pending, type Row } from "../src/rows.ts";

const tmp = () => path.join(fs.mkdtempSync(path.join(os.tmpdir(), "rows-")), "1.jsonl");
const u = (text: string, ts = 1): Row => ({ kind: "user", ts, from: "person", text });

test("unread is every user row after the last turn.end", () => {
  const f = tmp();
  append(f, u("a"));
  append(f, { kind: "turn.start", ts: 2, turn: 1 });
  append(f, { kind: "turn.end", ts: 3, turn: 1, answer: "ok" });
  append(f, u("b", 4));
  append(f, u("c", 5));
  const rows = readRows(f);
  assert.deepEqual(unread(rows).map((r) => r.text), ["b", "c"]);
  assert.equal(openTurn(rows), undefined);
  assert.equal(nextTurn(rows), 2);
  assert.equal(lastEnd(rows)?.answer, "ok");
});

test("a turn.start without an end is open; an interrupted one is not", () => {
  const f = tmp();
  append(f, u("a"));
  append(f, { kind: "turn.start", ts: 2, turn: 1 });
  assert.equal(openTurn(readRows(f)), 1);
  append(f, { kind: "interrupted", ts: 3, turn: 1 });
  assert.equal(openTurn(readRows(f)), undefined);
  // the user row stays unread: an interrupted turn answers nothing
  assert.equal(unread(readRows(f)).length, 1);
});

test("an error end leaves the rows unread", () => {
  const f = tmp();
  append(f, u("a"));
  append(f, { kind: "turn.start", ts: 2, turn: 1 });
  append(f, { kind: "turn.end", ts: 3, turn: 1, error: "llm down" });
  assert.deepEqual(unread(readRows(f)).map((r) => r.text), ["a"]);
});

test("pending is false after an error end until a new user row", () => {
  const f = tmp();
  append(f, u("a"));
  append(f, { kind: "turn.start", ts: 2, turn: 1 });
  append(f, { kind: "turn.end", ts: 3, turn: 1, error: "llm down" });
  assert.equal(pending(readRows(f)), false);
  assert.equal(unread(readRows(f)).length, 1);
  append(f, u("b", 4));
  assert.equal(pending(readRows(f)), true);
});

test("a missing file reads as no rows", () => {
  assert.deepEqual(readRows(tmp()), []);
  assert.equal(nextTurn([]), 1);
});
