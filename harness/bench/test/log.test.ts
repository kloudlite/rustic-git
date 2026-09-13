import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { appendLine, readLines, replaceJson, readJson } from "../src/log.ts";

const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-log-"));

test("appended lines read back in order", () => {
  const f = path.join(tmp(), "a.jsonl");
  appendLine(f, { n: 1 });
  appendLine(f, { n: 2 });
  assert.deepEqual(readLines(f), [{ n: 1 }, { n: 2 }]);
});

test("a torn last line is skipped, earlier lines survive", () => {
  const f = path.join(tmp(), "a.jsonl");
  appendLine(f, { n: 1 });
  fs.appendFileSync(f, '{"n":2,"tex');
  assert.deepEqual(readLines(f), [{ n: 1 }]);
  appendLine(f, { n: 3 });
  // The torn fragment is now a middle line: still unreadable, still skipped,
  // and the line after it is intact because appendLine starts on a fresh line.
  assert.deepEqual(readLines(f), [{ n: 1 }, { n: 3 }]);
});

test("a missing file is empty, not an error", () => {
  assert.deepEqual(readLines(path.join(tmp(), "none.jsonl")), []);
});

test("replaceJson is atomic: no temp file left, old content until rename", () => {
  const d = tmp();
  const f = path.join(d, "sessions.json");
  replaceJson(f, [{ id: "bench" }]);
  replaceJson(f, [{ id: "bench" }, { id: "s-2" }]);
  assert.deepEqual(readJson(f, []), [{ id: "bench" }, { id: "s-2" }]);
  assert.deepEqual(fs.readdirSync(d), ["sessions.json"]);
});
