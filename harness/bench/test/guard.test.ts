import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Writable } from "../src/guard.ts";

test("a failed write flips the guard and a good probe restores it", () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-guard-"));
  const changes: boolean[] = [];
  let probeShouldFail = false;
  const w = new Writable(d, (ok) => changes.push(ok), () => {
    if (probeShouldFail) throw Object.assign(new Error("EIO: i/o error"), { code: "EIO" });
  });
  assert.throws(() => w.run(() => { throw Object.assign(new Error("EIO: i/o error"), { code: "EIO" }); }), /EIO/);
  assert.equal(w.ok(), false);
  assert.match(w.reason()!, /EIO/);
  probeShouldFail = true;
  assert.equal(w.probe(), false);
  probeShouldFail = false;
  assert.equal(w.probe(), true);
  assert.deepEqual(changes, [false, true]);
});
