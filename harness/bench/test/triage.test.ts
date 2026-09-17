import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";
import { order, question } from "../src/triage.ts";

/** A model's answer is untrusted input: whatever it is, no message is ever lost. */
test("the fork's order is taken when it is an order, and ignored when it is not", () => {
  assert.deepEqual(order('[{"index": 2, "reason": "unblocks the build"}, {"index": 0}, {"index": 1, "reason": "can wait"}]', 3), [
    { index: 2, reason: "unblocks the build" },
    { index: 0, reason: undefined },
    { index: 1, reason: "can wait" },
  ]);
  // Prose around the JSON is fine; models add it.
  assert.deepEqual(order('Here you go:\n[{"index":1},{"index":0}]\nHope that helps', 2)?.map((r) => r.index), [1, 0]);
  // Bare numbers are an order too.
  assert.deepEqual(order("[1, 0]", 2)?.map((r) => r.index), [1, 0]);

  // Anything it forgot keeps its place, at the back: a forgotten prompt must not be a dropped one.
  assert.deepEqual(order('[{"index":2}]', 4)?.map((r) => r.index), [2, 0, 1, 3]);
  // Out of range, repeated, or not a number: skipped, never trusted into an index.
  assert.deepEqual(order('[{"index":9},{"index":1},{"index":1},{"index":-1},{"index":"x"}]', 2)?.map((r) => r.index), [1, 0]);

  // No order at all: keep the one that was there.
  for (const bad of ["", "no idea", "{}", "[]", "[{}]", '["a"]', "[oops"]) assert.equal(order(bad, 3), undefined, JSON.stringify(bad));
});

test("the question names every item by its index and asks for one shape of answer", () => {
  const q = question(["[from workspace api] the tests pass", "also please rename the button"]);
  assert.match(q, /Answer ONLY with JSON: \[\{index, reason\}\]/);
  assert.match(q, /^0: \[from workspace api\] the tests pass$/m);
  assert.match(q, /^1: also please rename the button$/m);
  // Long ones are cut: the fork is ordering them, not reading them.
  assert.ok(question(["x".repeat(500)]).split("\n").pop()!.length < 210);
});

test("a queue that builds up mid-turn is ordered by a fork of the session, and nothing is lost", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-triage-"));
  // The fork answers with an order; the fake echoes what it was asked, so the answer is planted here.
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const id = bench.sessions.all().find((s) => !s.archived)!.id;
    // A turn that never ends, so everything after it queues.
    await bench.rpc(id, { type: "prompt", message: "hang" });
    await until(() => bench.busy(), 2_000, "mid-turn");
    for (const m of ["a question that can wait", "[from workspace api] the tests pass", "and one more thing"]) await bench.rpc(id, { type: "follow_up", message: m });

    // The fork's answer is whatever it says; a fake that echoes gives no JSON, which is exactly the
    // fallback case: the queue goes back in the order it was in, and nothing is dropped.
    const r = await bench.triageNow(id);
    assert.deepEqual(r?.order, [0, 1, 2], "no order from the fork keeps the order that was there");
    const state = (await bench.rpc(id, { type: "clear_queue" })).data as { followUp: string[] };
    assert.deepEqual(state.followUp, ["a question that can wait", "[from workspace api] the tests pass", "and one more thing"]);
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
