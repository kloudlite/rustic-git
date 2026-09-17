import { test } from "node:test";
import assert from "node:assert/strict";
import { brief, itemText, nudge, reduce } from "../src/plan.ts";
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

test("what crosses to the asking session is a standup answer, not a transcript", () => {
  // A bench handed 300 lines of diff is a bench whose context is gone by the third ask (§18).
  const withCode = ["done: added /healthz", "```ts", "export function healthz() {", "  return 200;", "}", "```", "tests pass"].join("\n");
  assert.equal(brief(withCode, "api"), "done: added /healthz\n\ntests pass");

  // Long answers are cut, and say where the rest is.
  const long = Array.from({ length: 30 }, (_, i) => `line ${i}`).join("\n");
  const cut = brief(long, "svelte-frontend");
  assert.equal(cut.split("\n").length, 13, cut);
  assert.match(cut, /… \(full reply in the svelte-frontend tab\)$/);
  assert.match(cut, /^line 0\nline 1/);

  // A short answer is left exactly as it was.
  assert.equal(brief("done: nothing to change", "api"), "done: nothing to change");
  // An unterminated fence still goes: it is the thing this exists to stop.
  assert.equal(brief("partial\n```\nhalf a file", "api"), "partial");
  // 1,200 characters is the other bound.
  assert.ok(brief("x".repeat(3000), "api").length < 1260);
});

test("a conversation that fills its window is summarised, keeping the plan and what is outstanding", async () => {
  const { Bench } = await import("../src/bench.ts");
  const { FAKE } = await import("./fake-pi.ts");
  const fs = await import("node:fs");
  const os = await import("node:os");
  const path = await import("node:path");
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-compact-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const id = bench.sessions.all().find((s) => !s.archived)!.id;
    bench.plans.set(id, [{ text: "clone the repo", state: "done" }, { text: "add the endpoint", state: "doing" }]);
    const rows: unknown[] = [];
    bench.onEvent((ev) => ev.type === "compacted" && rows.push(ev));

    // Under the mark: nothing happens, because compaction is a cost and a risk.
    await (bench as any).compactIfFull(id, 100, 1000);
    assert.deepEqual(rows, []);
    // No window reported: a fixed token count is the fallback, not a guess at how big it is.
    await (bench as any).compactIfFull(id, 100_000, undefined);
    assert.deepEqual(rows, [], "100k is under the fallback");
    await (bench as any).compactIfFull(id, 170_000, undefined);
    assert.equal(rows.length, 1, "past it, the same summary happens");
    rows.length = 0;

    // Past it: pi is asked to summarise, and TOLD what must survive.
    await (bench as any).compactIfFull(id, 850, 1000);
    const said = ((await bench.rpc(id, { type: "get_state" })).data as { compacted: string[] }).compacted;
    assert.equal(said.length, 2, "the fallback above and this one");
    assert.match(said[0], /the plan, with state: add the endpoint \(doing\)/);
    assert.ok(!said[0].includes("clone the repo"), "what is done is not what must survive");
    assert.match(said[0], /nothing is outstanding/);
    assert.match(said[0], /Drop tool output, file contents/);
    assert.equal(rows.length, 1, "and the transcript says it happened");
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * A turn that only asked something is not a turn that forgot its plan. The owner watched the
 * harness nudge a one-line clarifying question, and then nudge again because the single item was
 * "doing" while it waited on HIM (2026-09-17).
 */
test("no nudge for a turn that is asking, or an item that is waiting on a person", () => {
  // Two changes is an errand; three is work.
  assert.equal(nudge([], 2), undefined);
  assert.equal(nudge([], 3), "[harness] write the plan for this work with the plan tool, then continue");
  // The turn ended by asking the person: nothing to plan, nothing to tick.
  assert.equal(nudge([], 5, "Which of the two sandboxes should I clear?"), undefined);
  assert.equal(nudge(plan(["a", "doing"]), 5, "Do you want me to delete it?"), undefined);
  // An item that IS the question is waiting, not forgotten.
  assert.equal(nudge(plan(["ask karthik which region?", "doing"]), 5), undefined);
  assert.equal(nudge(plan(["waiting on you to choose a region", "doing"]), 5), undefined);
  // A real one still gets its line.
  assert.equal(
    nudge(plan(["migrate the schema", "doing"]), 5, "I have started the migration."),
    "[harness] the plan still shows 1 item(s) doing — mark each done or later (with why) before you stop",
  );
});
