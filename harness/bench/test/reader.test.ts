import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { transcript, page } from "../src/reader.ts";

// A pi session file written by hand in pi's own format (SessionHeader +
// SessionMessageEntry, session-manager.d.ts), so the test needs no model.
function sessionFile(dir: string, cwd: string) {
  const f = path.join(dir, "2026-09-13T00-00-00_abc.jsonl");
  const t = new Date().toISOString();
  const lines = [
    { type: "session", version: 3, id: "abc", timestamp: t, cwd },
    { type: "message", id: "m1", parentId: null, timestamp: t, message: { role: "user", content: "hello", timestamp: 1 } },
    { type: "message", id: "m2", parentId: "m1", timestamp: t, message: { role: "assistant", content: [{ type: "text", text: "hi" }], api: "x", provider: "x", model: "x", usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } }, stopReason: "stop", timestamp: 2 } },
  ];
  fs.writeFileSync(f, lines.map((l) => JSON.stringify(l)).join("\n") + "\n");
  return f;
}

test("a stopped session's transcript is read without starting an agent", () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-read-"));
  const ms = transcript(sessionFile(d, "/home/kl")) as { role: string }[];
  assert.deepEqual(ms.map((m) => m.role), ["user", "assistant"]);
});

test("page takes after as an index and limit as a count, and tail takes the newest", () => {
  assert.deepEqual(page([1, 2, 3, 4], 1, 2), { messages: [2, 3], total: 4, from: 1 });
  assert.deepEqual(page([1, 2], 5), { messages: [], total: 2, from: 5 });
  // What a window opening a long thread asks for: the newest N, and where they begin.
  assert.deepEqual(page([1, 2, 3, 4, 5], 0, undefined, 2), { messages: [4, 5], total: 5, from: 3 });
  assert.deepEqual(page([1, 2], 0, undefined, 60), { messages: [1, 2], total: 2, from: 0 }, "a short thread is all of it");
});
