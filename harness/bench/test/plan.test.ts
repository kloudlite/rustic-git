import { test } from "node:test";
import assert from "node:assert/strict";
import { itemText, nudge, reduce } from "../src/plan.ts";
import type { PlanItem } from "../src/ledger.ts";

/** The harness keeps the plan true: every rule is (plan, event) → plan, and nothing else. */
const plan = (...xs: [string, PlanItem["state"]][]): PlanItem[] => xs.map(([text, state]) => ({ text, state }));

test("an ask becomes a plan item, and its answer ticks it", () => {
  const started = reduce([], { type: "asked", exchange: "x1", to: "svelte-frontend", task: "run the tests\nand report" })!;
  assert.deepEqual(started.map((i) => [itemText(i), i.state]), [["svelte-frontend: run the tests", "doing"]]);
  // The same ask twice is one item: the bench folds its own events more than once.
  assert.equal(reduce(started, { type: "asked", exchange: "x1", to: "svelte-frontend", task: "run the tests" }), undefined);

  const answered = reduce(started, { type: "answered", exchange: "x1" })!;
  assert.deepEqual(answered.map((i) => i.state), ["done"]);
  assert.equal(reduce(answered, { type: "answered", exchange: "x1" }), undefined, "nothing moved");
  // An answer to an ask this session never made changes nothing.
  assert.equal(reduce(started, { type: "answered", exchange: "nope" }), undefined);

  const failed = reduce(started, { type: "ask_failed", exchange: "x1", task: "svelte-frontend" })!;
  assert.deepEqual(failed.map((i) => [i.state, i.why]), [["later", "failed: svelte-frontend"]]);
});

test("a decline moves what the turn was doing to later, with the reason", () => {
  const p = plan(["clone the repo", "done"], ["delete the workspace", "doing"], ["write the tests", "todo"]);
  const after = reduce(p, { type: "declined" })!;
  assert.deepEqual(after.map((i) => [i.state, i.why]), [["done", undefined], ["later", "declined by the person"], ["todo", undefined]]);
  // Nothing in hand: a decline with no doing item is not somebody else's item moved.
  assert.equal(reduce(plan(["a", "todo"]), { type: "declined" }), undefined);
});

test("work starting moves the first waiting item into doing, and only then", () => {
  assert.deepEqual(reduce(plan(["a", "todo"], ["b", "todo"]), { type: "working" })!.map((i) => i.state), ["doing", "todo"]);
  // Something already in hand is not replaced by the next thing that runs a tool.
  assert.equal(reduce(plan(["a", "doing"], ["b", "todo"]), { type: "working" }), undefined);
  assert.equal(reduce(plan(["a", "done"]), { type: "working" }), undefined);
  assert.equal(reduce([], { type: "working" }), undefined);
});

test("what the harness says when a turn ends with the plan out of date", () => {
  // Real work and no plan at all.
  assert.equal(nudge([], 3), "[harness] write the plan for this work with the plan tool, then continue");
  assert.equal(nudge([], 1), undefined, "one call is not work that needed a plan");
  assert.equal(nudge([], 0), undefined);
  // A plan left mid-flight.
  assert.equal(nudge(plan(["a", "doing"], ["b", "doing"], ["c", "done"]), 5), "[harness] the plan still shows 2 item(s) doing — mark each done or later (with why) before you stop");
  // A plan that is current says nothing at all.
  assert.equal(nudge(plan(["a", "done"], ["b", "later"]), 5), undefined);
});
