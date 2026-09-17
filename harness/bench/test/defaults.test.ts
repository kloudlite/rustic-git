import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Defaults } from "../src/defaults.ts";

test("defaults persist and merge", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "defaults-"));
  const d = new Defaults(dir);
  assert.deepEqual(d.get(), {});
  d.set({ model: "deepseek/deepseek-reasoner" });
  d.set({ thinking: "high" });
  assert.deepEqual(new Defaults(dir).get(), { model: "deepseek/deepseek-reasoner", thinking: "high" });
});

test("effort is dropped when set to undefined", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "defaults-"));
  const d = new Defaults(dir);
  d.set({ effort: "max" });
  d.set({ effort: undefined });
  assert.equal(d.get().effort, undefined);
});
