import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import type { OperationEvent, OperationSnapshot, OperationState } from "../src/operations/contracts.ts";
import { openScenario, scenarioById } from "../../src/renderer/operations/fixtures/scenarios.ts";
import {
  collectOperationEventPages,
  createOperationStore,
  operationIdFromAction,
  operationTaskRow,
} from "../../src/renderer/operations/store.ts";
import type { CompactOperationResult } from "../src/operations/contracts.ts";
import { BenchResponseError } from "../../src/bench-client.ts";

const flush = async () => {
  await new Promise((resolve) => setTimeout(resolve, 0));
};

function source(snapshot: OperationSnapshot, batches: OperationEvent[][] = []) {
  const calls: string[] = [];
  let changed: ((lastSequence: number) => void) | undefined;
  let disconnected: ((connected: boolean) => void) | undefined;
  let disposed = 0;
  return {
    calls,
    bridge: {
      async loadSnapshot(operationId: string) {
        calls.push(`snapshot:${operationId}`);
        return snapshot;
      },
      async loadEvents(operationId: string, after: number) {
        calls.push(`events:${operationId}:${after}`);
        return batches.shift() ?? [];
      },
      watch(operationId: string, onChanged: (lastSequence: number) => void) {
        calls.push(`watch:${operationId}`);
        changed = onChanged;
        return () => { disposed += 1; };
      },
      onConnection(onConnected: (connected: boolean) => void) {
        disconnected = onConnected;
        return () => { disposed += 1; };
      },
    },
    changed(lastSequence: number) { changed?.(lastSequence); },
    connected(value: boolean) { disconnected?.(value); },
    disposed: () => disposed,
  };
}

test("operation store loads one snapshot-first projection per operation and replays cursor events", async () => {
  const scenario = scenarioById("parallel-steps");
  const events = scenario.script.flatMap((entry) => "event" in entry ? [entry.event] : []);
  const expected = openScenario(scenario);
  const seeded = { ...scenario.seed, lastSequence: 0, revision: 1 };
  const fixture = source(seeded, [events]);
  const store = createOperationStore(fixture.bridge);

  const first = store.open(expected.operationId);
  const second = store.open(expected.operationId);
  assert.strictEqual(first, second);
  await flush();

  assert.deepEqual(fixture.calls.slice(0, 3), [
    `snapshot:${expected.operationId}`,
    `events:${expected.operationId}:0`,
    `watch:${expected.operationId}`,
  ]);
  assert.equal(first.view().lastSequence, events.at(-2)!.sequence);
  assert.equal(first.view().state, "running");
  assert.equal(store.entries().length, 1);
});

test("operation store catches up notifications, reconnects from its cursor, resyncs, and disposes", async () => {
  const scenario = scenarioById("parallel-steps");
  const events = scenario.script.flatMap((entry) => "event" in entry ? [entry.event] : []);
  const [firstEvent, ...rest] = events;
  const fixture = source({ ...scenario.seed, lastSequence: 0, revision: 1 }, [[firstEvent], rest, []]);
  const store = createOperationStore(fixture.bridge);
  const entry = store.open(scenario.seed.operationId);
  await flush();

  fixture.changed(rest.at(-2)!.sequence);
  await flush();
  assert.equal(entry.view().lastSequence, rest.at(-2)!.sequence);

  fixture.connected(false);
  assert.equal(entry.view().status, "disconnected");
  const cursor = entry.view().lastSequence;
  fixture.connected(true);
  await flush();
  assert.ok(fixture.calls.includes(`events:${scenario.seed.operationId}:${cursor}`));

  await entry.resync({ operationId: scenario.seed.operationId, afterSequence: entry.view().lastSequence, need: "snapshot", reason: "invalid_snapshot", at: Date.now() });
  assert.equal(fixture.calls.filter((call) => call.startsWith("snapshot:")).length, 2);

  store.dispose();
  assert.equal(fixture.disposed(), 2);
  assert.equal(store.entries().length, 0);
});

test("bench lifecycle events immediately disconnect and refresh operation projections", async () => {
  const scenario = scenarioById("parallel-steps");
  const fixture = source({ ...scenario.seed, lastSequence: 0, revision: 1 }, [[], []]);
  const store = createOperationStore(fixture.bridge);
  const entry = store.open(scenario.seed.operationId);
  await flush();
  store.connection(false);
  assert.equal(entry.view().status, "disconnected");
  store.connection(true);
  await flush();
  assert.ok(fixture.calls.filter((call) => call.startsWith("events:")).length >= 2);
});

test("operate actions expose an operation ID only from a validated compact result", () => {
  const result: CompactOperationResult = { operationId: "op-result", revision: 1, state: "running", summary: "Started" };
  assert.equal(operationIdFromAction({ tool: "operate", output: JSON.stringify(result), args: { operationId: "op-arg" } }), "op-result");
  assert.equal(operationIdFromAction({ tool: "operate", output: JSON.stringify({ operationId: "op-invalid" }), args: { operationId: "op-arg" } }), undefined);
  assert.equal(operationIdFromAction({ tool: "operate", output: "accepted op-args", args: { operation_id: "op-args" } }), undefined);
  assert.equal(operationIdFromAction({ tool: "read", output: JSON.stringify({ operationId: "op-no" }), args: { operationId: "op-no" } }), undefined);
});

test("operation store serializes refreshes and ignores responses after disposal", async () => {
  const scenario = scenarioById("parallel-steps");
  let resolveEvents!: (events: OperationEvent[]) => void;
  let eventCalls = 0;
  const store = createOperationStore({
    loadSnapshot: async () => ({ ...scenario.seed, lastSequence: 0, revision: 1 }),
    loadEvents: async () => {
      eventCalls += 1;
      return new Promise<OperationEvent[]>((resolve) => { resolveEvents = resolve; });
    },
  });
  const entry = store.open(scenario.seed.operationId, { sessionId: "s-1", workspaceId: "ws-1" });
  await flush();
  const first = entry.reload();
  const second = entry.reload();
  assert.equal(eventCalls, 1);
  store.disposeSession("s-1");
  resolveEvents([]);
  await Promise.all([first, second]);
  assert.equal(store.entries().length, 0);
});

test("operation store queues the highest notification received while loading", async () => {
  const scenario = scenarioById("parallel-steps");
  let resolveFirst!: (events: OperationEvent[]) => void;
  const calls: number[] = [];
  let changed: ((sequence: number) => void) | undefined;
  const store = createOperationStore({
    loadSnapshot: async () => ({ ...scenario.seed, lastSequence: 0, revision: 1 }),
    loadEvents: async (_id, after) => {
      calls.push(after);
      if (calls.length === 1) return new Promise<OperationEvent[]>((resolve) => { resolveFirst = resolve; });
      return [];
    },
    watch: (_id, notify) => (changed = notify, () => undefined),
  });
  store.open(scenario.seed.operationId);
  await flush();
  const reload = store.entries()[0]!.reload();
  changed?.(4);
  changed?.(9);
  resolveFirst([]);
  await reload;
  await flush();
  await flush();
  assert.deepEqual(calls, [0]);
  store.dispose();
});

test("archiving retains operation projections while transcript deletion disposes them", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.seed, loadEvents: async () => [] });
  store.open(scenario.seed.operationId, { sessionId: "s-1" });
  await flush();
  store.archiveSession("s-1");
  assert.deepEqual(store.entries().map((entry) => entry.operationId), [scenario.seed.operationId]);
  store.disposeSession("s-1");
  assert.deepEqual(store.entries(), []);
});

test("operation store preserves typed bench error details", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({
    loadSnapshot: async () => { throw new BenchResponseError(409, { error: { code: "stale_revision", message: "revision changed", expectedRevision: 3, actualRevision: 4 } }); },
    loadEvents: async () => [],
  });
  const entry = store.open(scenario.seed.operationId);
  await flush();
  assert.deepEqual(entry.error(), { kind: "snapshot", code: "stale_revision", message: "revision changed", retryable: true, status: 409, details: { expectedRevision: 3, actualRevision: 4 } });
});

test("operation IDs cannot be reopened under another session or workspace", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  store.open("op-one", { sessionId: "s-1", workspaceId: "ws-1" });
  assert.equal(store.open("op-one", { sessionId: "s-2", workspaceId: "ws-1" }).error()?.code, "operation_owner_mismatch");
  assert.equal(store.open("op-one", { sessionId: "s-1", workspaceId: "ws-2" }).error()?.code, "operation_owner_mismatch");
  store.open("op-two", { sessionId: "s-2", workspaceId: "ws-2" });
  await flush();
  assert.deepEqual(store.taskRows("s-1", "ws-1").map((row) => row.id), ["op-one"]);
  assert.deepEqual(store.taskRows("s-1", "ws-other"), []);
  store.disposeSession("s-1");
  assert.deepEqual(store.entries().map((entry) => entry.operationId), ["op-two"]);
  store.dispose();
});

test("operation owner mismatch is contained as a typed projection error", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  store.open("op-one", { sessionId: "s-1", workspaceId: "ws-1" });
  const mismatched = store.open("op-one", { sessionId: "s-2", workspaceId: "ws-1" });
  assert.deepEqual(mismatched.error(), { kind: "snapshot", code: "operation_owner_mismatch", message: "operation owner mismatch", retryable: true });
  store.dispose();
});

test("projection released during its initial load never installs a watcher", async () => {
  const scenario = scenarioById("parallel-steps");
  let resolveSnapshot!: (snapshot: OperationSnapshot) => void;
  let watchCalls = 0;
  const store = createOperationStore({
    loadSnapshot: () => new Promise<OperationSnapshot>((resolve) => { resolveSnapshot = resolve; }),
    loadEvents: async () => [],
    watch: () => (watchCalls += 1, () => undefined),
  });
  const projection = store.open(scenario.expected.operationId);
  projection.dispose();
  resolveSnapshot(scenario.expected);
  await flush();
  assert.equal(watchCalls, 0);
  store.dispose();
});

test("reconnect uses the serialized catch-up path and preserves typed failures", async () => {
  const scenario = scenarioById("parallel-steps");
  let resolveFirst!: (events: OperationEvent[]) => void;
  let calls = 0;
  const typed = new BenchResponseError(503, { error: { code: "operation_source_unavailable", message: "operation source unavailable" } });
  const fixture = source({ ...scenario.seed, lastSequence: 0, revision: 1 });
  const store = createOperationStore({
    ...fixture.bridge,
    loadSnapshot: async () => ({ ...scenario.seed, lastSequence: 0, revision: 1 }),
    loadEvents: async () => {
      calls += 1;
      if (calls === 1) return new Promise<OperationEvent[]>((resolve) => { resolveFirst = resolve; });
      throw typed;
    },
  });
  const projection = store.open(scenario.seed.operationId);
  await flush();
  store.connection(false);
  store.connection(true);
  assert.equal(calls, 1);
  resolveFirst([]);
  await flush();
  await flush();
  assert.equal(calls, 2);
  assert.deepEqual(projection.error(), { kind: "events", code: "operation_source_unavailable", message: "operation source unavailable", retryable: true, status: 503 });
  store.dispose();
});

test("the renderer store has no dependency on the Node bench client", () => {
  const source = fs.readFileSync(new URL("../../src/renderer/operations/store.ts", import.meta.url), "utf8");
  assert.doesNotMatch(source, /bench-client\.ts/);
});

test("event page replay fails typed when a page makes no progress", async () => {
  const scenario = scenarioById("parallel-steps");
  const events = scenario.script.flatMap((entry) => "event" in entry ? [entry.event] : []);
  const cursors: (string | undefined)[] = [];
  await assert.rejects(() => collectOperationEventPages(async (after) => {
    cursors.push(after);
    if (after === "0") return { events: events.slice(0, 1), nextCursor: "1", hasMore: true };
    return { events: events.slice(1, 2), nextCursor: "1", hasMore: true };
  }, 0), (error: Error & { code?: string }) => error.code === "operation_cursor_stalled");
  assert.deepEqual(cursors, ["0", "1"]);
});

test("store rejects a foreign snapshot before using its cursor", async () => {
  const scenario = scenarioById("parallel-steps");
  let eventCalls = 0;
  const store = createOperationStore({
    loadSnapshot: async () => ({ ...scenario.seed, operationId: "other-operation", actor: { ...scenario.seed.actor, sessionId: "other-session" } }),
    loadEvents: async () => (eventCalls += 1, []),
  });
  const entry = store.open(scenario.seed.operationId, { sessionId: scenario.seed.actor.sessionId });
  await flush();
  assert.equal(entry.error()?.code, "snapshot_owner_mismatch");
  assert.equal(eventCalls, 0);
});

test("operation task rows map lifecycle states and retain the same projection for detail", () => {
  const scenario = scenarioById("parallel-steps");
  const base = openScenario(scenario);
  const cases: [OperationState, string][] = [
    ["running", "running"],
    ["awaiting_approval", "waiting"],
    ["needs_input", "waiting"],
    ["reconciling", "reconciling"],
    ["partial", "partial"],
    ["failed", "failed"],
    ["cancelled", "cancelled"],
    ["completed", "completed"],
  ];
  for (const [state, expected] of cases) {
    const view = { ...base, state };
    const row = operationTaskRow(view);
    assert.equal(row.state, expected);
    assert.strictEqual(row.projection.view(), view);
    assert.equal(row.id, view.operationId);
  }
});
