import assert from "node:assert/strict";
import test from "node:test";
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
