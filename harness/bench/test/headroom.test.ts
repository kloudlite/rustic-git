import { test } from "node:test";
import assert from "node:assert/strict";
import { compress, retrieve } from "../src/engine/headroom.ts";

test("json-array: smart_sample keeps error, outlier, head, tail", () => {
  // Not every item has the same key set (the error/outlier rows carry an extra field), which is what
  // routes this through smart_sample instead of the lossless table path.
  const items = Array.from({ length: 40 }, (_, i) => {
    const base = { id: i, value: i === 20 ? 9999 : i };
    if (i === 5) return { ...base, note: "raised an exception here" };
    if (i === 20) return { ...base, flag: "outlier" };
    return base;
  });
  const original = JSON.stringify(items);
  const c = compress(original, "");
  assert.equal(c.strategy.startsWith("smart_sample"), true);
  assert.ok(c.after < c.before * 0.8);
  assert.match(c.text, /Retrieve original: hash=/);
  assert.equal(retrieve(c.hash!), original);
});

test("json-array: uniform key set becomes a lossless table with no hash", () => {
  const items = Array.from({ length: 60 }, (_, i) => ({ a: i, b: `row-with-some-padding-${i}` }));
  const original = JSON.stringify(items);
  const c = compress(original, "");
  assert.equal(c.strategy, "table");
  assert.equal(c.hash, undefined);
  assert.match(c.text, /^keys: a, b/);
});

test("search: grep -n shaped lines grouped and truncated per file", () => {
  const files = ["a.ts", "b.ts", "c.ts", "d.ts"];
  const lines: string[] = [];
  for (const f of files) for (let i = 0; i < 8; i++) lines.push(`${f}:${i}: match number ${i} in ${f}`);
  const original = lines.join("\n");
  const c = compress(original, "");
  assert.equal(c.strategy, "search");
  assert.ok(c.after < c.before * 0.8);
  assert.match(c.text, /Retrieve original: hash=/);
  assert.equal(retrieve(c.hash!), original);
  assert.match(c.text, /… \+\d+ more in/);
});

test("diff: keeps headers and +/- lines, drops context", () => {
  const ctx = Array.from({ length: 200 }, (_, i) => ` unchanged context line ${i}`).join("\n");
  const original = [
    "diff --git a/file.ts b/file.ts",
    "--- a/file.ts",
    "+++ b/file.ts",
    "@@ -1,3 +1,3 @@",
    ctx,
    "-const x = 1;",
    "+const x = 2;",
  ].join("\n");
  const c = compress(original, "");
  assert.equal(c.strategy, "diff");
  assert.ok(c.after < c.before * 0.8);
  assert.match(c.text, /Retrieve original: hash=/);
  assert.equal(retrieve(c.hash!), original);
  assert.ok(!c.text.includes("unchanged context line"));
});

test("log: collapses repeats, keeps head/tail and error lines", () => {
  const lines: string[] = [];
  for (let i = 0; i < 100; i++) {
    if (i === 50) lines.push("2026-09-22 12:00:00 ERROR something failed badly");
    else lines.push(`2026-09-22 12:00:${String(i % 60).padStart(2, "0")} INFO heartbeat ${i % 3}`);
  }
  const original = lines.join("\n");
  const c = compress(original, "");
  assert.equal(c.strategy, "log");
  assert.ok(c.after < c.before * 0.8);
  assert.match(c.text, /Retrieve original: hash=/);
  assert.equal(retrieve(c.hash!), original);
  assert.match(c.text, /ERROR something failed badly/);
});

test("text: dedupes and head/tails plain lines over 80", () => {
  const lines = Array.from({ length: 200 }, (_, i) => `plain output line number ${i} with enough padding to matter`);
  const original = lines.join("\n");
  const c = compress(original, "");
  assert.equal(c.strategy, "text");
  assert.ok(c.after < c.before * 0.8);
  assert.match(c.text, /Retrieve original: hash=/);
  assert.equal(retrieve(c.hash!), original);
  assert.match(c.text, /lines omitted/);
});

test("short text passes through unchanged", () => {
  const c = compress("too short to bother with");
  assert.equal(c.strategy, "pass");
  assert.equal(c.text, "too short to bother with");
  assert.equal(c.hash, undefined);
});

test("error-prefixed text passes even when long", () => {
  const text = "error: " + "x".repeat(2000);
  const c = compress(text);
  assert.equal(c.strategy, "pass");
  assert.equal(c.text, text);
});

test("garbage that throws inside a compressor passes", () => {
  // A string that starts like a diff (so tryDiff's regex work runs) but is otherwise adversarial;
  // compress() must fail open regardless of what any detector throws internally.
  const text = "diff --git " + "\u0000".repeat(2000);
  const c = compress(text);
  assert.equal(c.strategy, "pass");
  assert.equal(c.text, text);
});

test("a result under the 20% savings threshold passes instead", () => {
  // 1000 unique short lines: dedupe removes nothing and head/tail trimming alone won't clear 20% on this shape,
  // so the size gate should fall back to pass. Guard the assumption directly rather than asserting an exact strategy.
  const lines = Array.from({ length: 40 }, (_, i) => `line ${i}`);
  const text = lines.join("\n"); // under MIN_CHARS entirely -> exercises the earliest gate, still "passes"
  const c = compress(text.repeat(1)); // keep short; real 25%-shorter-than-threshold case below
  assert.equal(c.strategy, "pass");

  // Construct text just over MIN_CHARS whose best compression saves less than 20%.
  const nearThreshold = "abcdefghij".repeat(85); // 850 chars, no newlines, no structure -> text fallback, no dedupe/trim triggers (<=80 lines)
  const c2 = compress(nearThreshold);
  assert.equal(c2.strategy, "pass");
  assert.equal(c2.text, nearThreshold);
});
