import { test } from "node:test";
import assert from "node:assert/strict";
import { batchImport, isLaptopRow, safeJsonlName, toItem, type ImportItem, type LooseFile } from "../../src/import-payload.ts";

test("safeJsonlName keeps a plain *.jsonl basename and refuses anything path-shaped", () => {
  assert.equal(safeJsonlName("s-1.jsonl"), "s-1.jsonl");
  assert.equal(safeJsonlName("../../etc/passwd.jsonl"), undefined);
  assert.equal(safeJsonlName("dir/s-1.jsonl"), undefined);
  assert.equal(safeJsonlName("s-1.json"), undefined);
  assert.equal(safeJsonlName("s-1.jsonl.bak"), undefined);
});

test("isLaptopRow admits only the laptop's own bench sessions", () => {
  assert.equal(isLaptopRow("bench"), true);
  assert.equal(isLaptopRow("s-12"), true);
  assert.equal(isLaptopRow("w-api"), false);
  assert.equal(isLaptopRow("e-x"), false);
});

test("toItem carries the row through and defaults created/lastActive/archived", () => {
  const item = toItem({ id: "s-1", name: "a", seq: 1 }, "s-1.jsonl", "content");
  assert.equal(item.name, "s-1.jsonl");
  assert.equal(item.content, "content");
  assert.equal(item.row.archived, false);
  assert.equal(typeof item.row.created, "number");
});

test("batchImport ships everything in one batch when it fits", () => {
  const items: ImportItem[] = [toItem({ id: "s-1", name: "a", seq: 1 }, "s-1.jsonl", "x")];
  const loose: LooseFile[] = [{ name: "s-2.jsonl", content: "y" }];
  const batches = batchImport(items, loose);
  assert.equal(batches.length, 1);
  assert.deepEqual(batches[0].items, items);
  assert.deepEqual(batches[0].loose, loose);
});

test("batchImport splits once the running batch would cross maxBytes, never leaving an empty batch", () => {
  const items: ImportItem[] = [
    toItem({ id: "s-1", name: "a", seq: 1 }, "s-1.jsonl", "x".repeat(50)),
    toItem({ id: "s-2", name: "b", seq: 2 }, "s-2.jsonl", "y".repeat(50)),
    toItem({ id: "s-3", name: "c", seq: 3 }, "s-3.jsonl", "z".repeat(50)),
  ];
  const batches = batchImport(items, [], 150);
  assert.ok(batches.length > 1, "the three items do not fit one 150-byte batch");
  for (const b of batches) assert.ok(b.items.length + b.loose.length > 0);
  assert.deepEqual(batches.flatMap((b) => b.items), items);
});

test("batchImport still ships a single oversized item alone rather than dropping it", () => {
  const huge: ImportItem = toItem({ id: "s-1", name: "a", seq: 1 }, "s-1.jsonl", "x".repeat(1000));
  const batches = batchImport([huge], [], 10);
  assert.equal(batches.length, 1);
  assert.deepEqual(batches[0].items, [huge]);
});

test("batchImport returns nothing for nothing", () => {
  assert.deepEqual(batchImport([], []), []);
});
