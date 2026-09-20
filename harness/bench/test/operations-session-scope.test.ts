import assert from "node:assert/strict";
import test from "node:test";
import { createRoot } from "solid-js";
import { scenarioById } from "../../src/renderer/operations/fixtures/scenarios.ts";
import { createOperationStore } from "../../src/renderer/operations/store.ts";

const flush = async () => {
  await new Promise((resolve) => setTimeout(resolve, 0));
};

test("a mismatched owner view carries no live controls: Approve/Cancel cannot be reached from it", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  store.open("op-one", { sessionId: "s-1", workspaceId: "ws-1" });
  const mismatched = store.open("op-one", { sessionId: "s-2", workspaceId: "ws-1" });
  assert.deepEqual(mismatched.controls, {});
  assert.equal(await mismatched.resync(), undefined);
  assert.equal(await mismatched.reload(), undefined);
  assert.doesNotThrow(() => mismatched.dispose());
  store.dispose();
});

test("two callers sharing one operation: the first to dispose does not pull it out from under the second", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  const first = store.open(scenario.expected.operationId, { sessionId: "s-1" });
  const second = store.open(scenario.expected.operationId, { sessionId: "s-1" });
  await flush();
  assert.equal(store.entries().length, 1, "one projection is shared, not duplicated");
  first.dispose();
  assert.equal(store.entries().length, 1, "still held: the second caller's view is live");
  // The still-open second view keeps updating: its own accessor still resolves the projection.
  assert.equal(second.view().operationId, scenario.expected.operationId);
  second.dispose();
  assert.equal(store.entries().length, 0, "released once nothing holds it");
  store.dispose();
});

test("a projection opened before its session resolved (sessionId '') learns the real session id and becomes disposable by it", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  const early = store.open(scenario.expected.operationId); // no owner yet: sessionId defaults to ""
  assert.equal(early.sessionId, "");
  const resolved = store.open(scenario.expected.operationId, { sessionId: "s-9" });
  assert.equal(resolved.sessionId, "s-9");
  store.disposeSession("s-9");
  assert.equal(store.entries().length, 0, "disposeSession found it under the session it was filled in with");
});

test("filling in a placeholder sessionId notifies entries(), so a reactive taskRows() memo picks the projection up without an unrelated store change", () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  // `taskRows` filters by `projection.sessionId`, a plain field, not a signal, so a Solid memo
  // built from its return value has nothing reactive to track unless the caller also reads
  // `entries()` — exactly what `Inspector.tsx`'s `Tasks` listing does. Node's `solid-js` module
  // resolves to the non-reactive server build outside a browser/vite condition, so this test
  // exercises the same contract at the signal level rather than through `createMemo`'s actual
  // recomputation: `entries()` must be called with a NEW array (a distinct reference — that is
  // what `setEntries` does, and what a subscribed memo would re-run on) at the moment ownership
  // resolves, not only on some later, unrelated store write.
  createRoot(() => {
    store.open(scenario.expected.operationId, { sessionId: "" });
    assert.equal(store.taskRows("s-1", undefined).length, 0, "not yet owned by s-1");
    // Captured AFTER the unowned open (which already calls `setEntries` once, for the initial
    // creation) so this isolates the fill-in path specifically, not the creation path.
    const beforeFillIn = store.entries();
    store.open(scenario.expected.operationId, { sessionId: "s-1" });
    assert.notStrictEqual(store.entries(), beforeFillIn, "entries() must have been notified for the memo to see the fill-in");
    assert.equal(store.taskRows("s-1", undefined).length, 1);
  });
  store.dispose();
});
