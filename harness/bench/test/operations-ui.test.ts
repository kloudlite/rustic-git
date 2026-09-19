import { test } from "node:test";
import assert from "node:assert/strict";
import {
  OPERATION_EVENT_PHASES,
  OPERATION_TRANSITIONS,
  OPERATION_STATES,
  STEP_TRANSITIONS,
  STEP_STATES,
  validateEventSequence,
  validateOperationEvent,
  validateOperationSnapshot,
} from "../src/operations/contracts.ts";
import type { OperationEvent, OperationSnapshot } from "../src/operations/contracts.ts";
import {
  FIXTURE_CLOCK,
  PARALLEL_EVENTS,
  SCENARIOS,
  applyScenarioRepair,
  applyScriptEntry,
  openScenario,
  scenarioById,
  type OperationScenario,
} from "../../src/renderer/operations/fixtures/scenarios.ts";
import {
  applyOperationEvent,
  applyOperationEvents,
  applyOperationSnapshot,
  createOperationView,
  markCancelRequested,
  markDecisionPresented,
} from "../../src/renderer/operations/reduce.ts";
import {
  decisionRows,
  errorRows,
  progressLabel,
  costLabel,
  stalledSteps,
  stateLabel,
  stateTone,
  stepRows,
  stepStateLabel,
  stepStateTone,
  unknownRows,
  usageLabel,
} from "../../src/renderer/operations/present.ts";
import {
  additionalInputCallbackPayload,
  cancelCallbackPayload,
  decisionCallbackPayload,
} from "../../src/renderer/operations/bridge.ts";
import {
  OPERATION_PHASE_EDGES,
  OPERATION_OBSERVATION_PHASES,
  STEP_PHASE,
  STEP_REPAIR_ONLY_EDGES,
  isTerminalOperation,
  runCounts,
  type OperationView,
} from "../../src/renderer/operations/types.ts";

/**
 * O09's evidence. These are pure event/view-model tests on the fixtures: the durable
 * snapshots and logs each scenario declares are checked against the frozen O01 validators,
 * the projection is compared with the record the store kept, and the reading a person gets
 * is checked for the two things this slice exists to prevent — an invented success and a
 * decision the model could have granted.
 *
 * The components themselves are not loaded here: node cannot parse JSX, and the panel only
 * lays out what `present.ts` hands it. Everything the panel can say is therefore asserted
 * below, through the same selectors it renders.
 */

const scriptEvents = (scenario: OperationScenario): OperationEvent[] =>
  scenario.script.filter((entry): entry is { note: string; event: OperationEvent } => "event" in entry).map((entry) => entry.event);

/** The view the script leaves behind: the record loaded, then the log replayed. */
const scriptView = (scenario: OperationScenario): OperationView => openScenario(scenario);

/** The script, repaired the way the bridge would: cursor replay, or a fresh snapshot. */
const settledView = (scenario: OperationScenario): OperationView => {
  let view = openScenario(scenario);
  if (view.resync && scenario.repair) view = applyScenarioRepair(view, scenario);
  if (view.resync && scenario.reload) view = applyOperationSnapshot(view, scenario.reload);
  return view;
};

/** The view built from the durable record alone, which is what a reconnect lands on. */
const durableView = (scenario: OperationScenario): OperationView =>
  applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.expected);

/** Replay only the durable events: the person's own actions are not in the log. */
const replayEvents = (view: OperationView, scenario: OperationScenario): OperationView => {
  let next = view;
  for (const entry of scenario.script) {
    if ("event" in entry) next = applyOperationEvent(next, entry.event);
  }
  return next;
};

/**
 * What both the replay and the durable record must agree on. Anything the event envelope
 * does not carry (a typed error, scope, targets, usage, decision content) is deliberately
 * absent, because only the record can know it.
 */
const stepShape = (view: OperationView) =>
  view.steps.map((step) => ({
    stepId: step.stepId,
    state: step.state,
    capability: step.capability,
    dependencies: [...step.dependencies],
    attempts: step.attempts,
    queuedReason: step.queuedReason,
    evidenceRefs: [...step.evidenceRefs],
  }));

/** The most calls that were ever in flight, from the timings the record reports. */
function peakConcurrent(view: OperationView): number {
  const points: Array<[number, number]> = [];
  for (const step of view.steps) {
    if (step.startedAt === undefined) continue;
    points.push([step.startedAt, 1]);
    points.push([step.endedAt ?? Number.MAX_SAFE_INTEGER, -1]);
  }
  // An end sorts before a start at the same instant: back-to-back calls never overlap.
  points.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  let running = 0;
  let peak = 0;
  for (const [, delta] of points) {
    running += delta;
    peak = Math.max(peak, running);
  }
  return peak;
}

const json = (value: unknown): unknown => JSON.parse(JSON.stringify(value)) as unknown;

function collectKeys(value: unknown, into: string[] = []): string[] {
  if (Array.isArray(value)) {
    for (const item of value) collectKeys(item, into);
    return into;
  }
  if (value !== null && typeof value === "object") {
    for (const [key, child] of Object.entries(value)) {
      into.push(key);
      collectKeys(child, into);
    }
  }
  return into;
}

test("every fixture is a valid O01 record and a valid O01 log", () => {
  for (const scenario of SCENARIOS) {
    const records: Array<[string, OperationSnapshot | undefined]> = [
      ["seed", scenario.seed],
      ["expected", scenario.expected],
      ["reload", scenario.reload],
    ];
    for (const [name, snapshot] of records) {
      if (!snapshot) continue;
      const checked = validateOperationSnapshot(snapshot);
      assert.ok(checked.ok, `${scenario.id} ${name}: ${checked.ok ? "" : JSON.stringify(checked.issues)}`);
      assert.equal(snapshot.contractVersion, "v1");
      assert.equal(snapshot.operationId, scenario.expected.operationId);
      assert.ok((OPERATION_STATES as readonly string[]).includes(snapshot.state));
      for (const step of snapshot.steps) assert.ok((STEP_STATES as readonly string[]).includes(step.state));
    }
    assert.equal(scenario.seed.state, "accepted", `${scenario.id}: the seed is the accepted record`);
    assert.equal(scenario.seed.lastSequence, 0, `${scenario.id}: nothing has been written yet`);

    let previous: number | undefined;
    let at = -Infinity;
    for (const event of scriptEvents(scenario)) {
      const checked = validateOperationEvent(event);
      assert.ok(checked.ok, `${scenario.id} #${event.sequence}: ${checked.ok ? "" : JSON.stringify(checked.issues)}`);
      const sequenced = validateEventSequence(previous, event);
      assert.ok(sequenced.ok, `${scenario.id} #${event.sequence} is not monotonic`);
      assert.ok(event.at >= at, `${scenario.id} #${event.sequence}: the timestamp went backwards`);
      assert.ok((OPERATION_EVENT_PHASES as readonly string[]).includes(event.phase), `${scenario.id}: unknown phase ${event.phase}`);
      previous = event.sequence;
      at = event.at;
    }
    assert.equal(previous, scenario.expected.lastSequence, `${scenario.id}: the log and the record disagree about the last sequence`);
  }
});

test("replaying a scenario's log reaches the record the store kept", () => {
  for (const scenario of SCENARIOS) {
    const settled = settledView(scenario);
    assert.equal(settled.resync, undefined, `${scenario.id}: the repaired view is still asking for a repair`);
    assert.equal(settled.state, scenario.expected.state, `${scenario.id}: the projection disagrees with the durable state`);
    assert.equal(settled.lastSequence, scenario.expected.lastSequence, `${scenario.id}: wrong cursor`);
    assert.deepEqual(stepShape(settled), stepShape(durableView(scenario)), `${scenario.id}: replay disagrees with the record`);
  }
});

test("each scenario's declared expectation is what the view shows", () => {
  for (const scenario of SCENARIOS) {
    const scripted = scriptView(scenario);
    const expectation = scenario.expect;
    assert.equal(scripted.state, expectation.state, `${scenario.id}: state`);
    assert.equal(scripted.resync === undefined, expectation.settles, `${scenario.id}: repair state`);
    if (expectation.resync) {
      assert.ok(scripted.resync, `${scenario.id}: expected the view to ask for a repair`);
      assert.equal(scripted.resync.need, expectation.resync.need, `${scenario.id}: repair kind`);
      assert.equal(scripted.resync.reason, expectation.resync.reason, `${scenario.id}: repair reason`);
      assert.equal(scripted.resync.afterSequence, expectation.resync.afterSequence, `${scenario.id}: repair cursor`);
    }

    assert.deepEqual(
      scripted.steps.filter((step) => step.state === "outcome_unknown").map((step) => step.stepId),
      expectation.unknownSteps,
      `${scenario.id}: unknown outcomes`,
    );
    assert.equal(runCounts(scripted).unknown, expectation.unknownSteps.length);

    const answerable = decisionRows(scripted, FIXTURE_CLOCK + 1_000).filter((row) => row.answerable);
    assert.equal(answerable.length, expectation.openDecisions, `${scenario.id}: decisions that can be answered`);

    const reasons = scripted.steps.map((step) => step.queuedReason).filter((reason): reason is string => !!reason);
    for (const reason of expectation.queueReasons) {
      assert.ok(reasons.includes(reason), `${scenario.id}: the queue reason "${reason}" is not shown`);
    }

    assert.equal(peakConcurrent(settledView(scenario)), expectation.peakConcurrent, `${scenario.id}: peak concurrency`);

    if (expectation.errorCode) {
      const codes = errorRows(settledView(scenario)).map((row) => row.value.split(":")[0]);
      assert.ok(codes.includes(expectation.errorCode), `${scenario.id}: ${expectation.errorCode} is not shown`);
    }
    if (expectation.settles && isTerminalOperation(scenario.expected.state)) {
      assert.ok(!scripted.steps.some((step) => step.state === "outcome_unknown"));
    }
  }
});

test("the event reducer is idempotent by sequence", () => {
  const scenario = scenarioById("parallel-steps");
  const seed = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  const applied = applyOperationEvent(seed, PARALLEL_EVENTS[0]);
  assert.equal(applyOperationEvent(applied, PARALLEL_EVENTS[0]), applied, "a retained applied sequence returns the same view");
});

test("ingress rejects malformed payloads, revision regression, and conflicting duplicate sequences", () => {
  const scenario = scenarioById("parallel-steps");
  const seed = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  const accepted = applyOperationEvent(seed, PARALLEL_EVENTS[0]);

  const malformedSnapshot = applyOperationSnapshot(accepted, { ...scenario.expected, contractVersion: "v2" } as OperationSnapshot);
  assert.equal(malformedSnapshot.resync?.reason, "invalid_snapshot");
  assert.equal(malformedSnapshot.lastSequence, 1);

  const malformedEvent = applyOperationEvent(accepted, { ...PARALLEL_EVENTS[1], summary: "" });
  assert.equal(malformedEvent.resync?.reason, "invalid_event");
  assert.equal(malformedEvent.lastSequence, 1);

  const regressing = applyOperationEvent({ ...accepted, revision: 2 }, { ...PARALLEL_EVENTS[1], revision: 1 });
  assert.equal(regressing.resync?.reason, "revision_regression");
  assert.equal(regressing.lastSequence, 1);

  const conflicting = applyOperationEvent(accepted, { ...PARALLEL_EVENTS[0], summary: "different payload" });
  assert.equal(conflicting.resync?.reason, "conflicting_duplicate");
  assert.equal(conflicting.lastSequence, 1);
});

test("duplicate fingerprints include nested object values while preserving key-order equivalence", () => {
  const scenario = scenarioById("parallel-steps");
  const seed = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  const event: OperationEvent = {
    ...PARALLEL_EVENTS[0],
    model: { provider: "provider-a", model: "choice", version: "1" },
  };
  const applied = applyOperationEvent(seed, event);
  const reordered = applyOperationEvent(applied, {
    ...event,
    model: { version: "1", model: "choice", provider: "provider-a" },
  });
  assert.equal(reordered, applied, "nested key order does not change the fingerprint");

  const conflicting = applyOperationEvent(applied, {
    ...event,
    model: { provider: "provider-b", model: "choice", version: "1" },
  });
  assert.equal(conflicting.resync?.reason, "conflicting_duplicate");
});

test("duplicate conflicts are detected after the display tail has evicted the event", () => {
  const operationId = "op-long-log";
  let view = applyOperationSnapshot(createOperationView(operationId), {
    ...scenarioById("parallel-steps").seed,
    operationId,
  });
  for (let sequence = 1; sequence <= 205; sequence++) {
    view = applyOperationEvent(view, {
      operationId,
      sequence,
      revision: sequence,
      at: FIXTURE_CLOCK + sequence,
      phase: sequence === 1 ? "accepted" : "progress",
      summary: `event ${sequence}`,
    });
  }
  assert.equal(view.events[0].sequence, 6);
  const conflict = applyOperationEvent(view, {
    operationId,
    sequence: 1,
    revision: 1,
    at: FIXTURE_CLOCK + 1,
    phase: "accepted",
    summary: "conflicting accepted event",
  });
  assert.equal(conflict.resync?.reason, "conflicting_duplicate");
});

test("duplicate fingerprints stay bounded beyond the display tail", () => {
  const operationId = "op-bounded-fingerprints";
  let view = applyOperationSnapshot(createOperationView(operationId), {
    ...scenarioById("parallel-steps").seed,
    operationId,
  });
  for (let sequence = 1; sequence <= 1_000; sequence++) {
    view = applyOperationEvent(view, {
      operationId,
      sequence,
      revision: sequence,
      at: FIXTURE_CLOCK + sequence,
      phase: sequence === 1 ? "accepted" : "progress",
      summary: `event ${sequence}`,
    });
  }
  assert.ok(Object.keys(view.eventFingerprints).length <= 256);
  assert.equal(view.events.length, 200);
});

test("an identical recent duplicate is a no-op", () => {
  const scenario = scenarioById("parallel-steps");
  const seed = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  const applied = applyOperationEvent(seed, PARALLEL_EVENTS[0]);

  assert.equal(applyOperationEvent(applied, { ...PARALLEL_EVENTS[0] }), applied);
});

test("a conflicting recent duplicate requests authoritative repair", () => {
  const scenario = scenarioById("parallel-steps");
  const seed = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  const applied = applyOperationEvent(seed, PARALLEL_EVENTS[0]);
  const conflict = applyOperationEvent(applied, { ...PARALLEL_EVENTS[0], summary: "different payload" });

  assert.equal(conflict.resync?.reason, "conflicting_duplicate");
  assert.equal(conflict.resync?.need, "snapshot");
});

test("an evicted old duplicate requests authoritative repair instead of silently passing", () => {
  const operationId = "op-evicted-duplicate";
  let view = applyOperationSnapshot(createOperationView(operationId), {
    ...scenarioById("parallel-steps").seed,
    operationId,
  });
  for (let sequence = 1; sequence <= 1_000; sequence++) {
    view = applyOperationEvent(view, {
      operationId,
      sequence,
      revision: sequence,
      at: FIXTURE_CLOCK + sequence,
      phase: sequence === 1 ? "accepted" : "progress",
      summary: `event ${sequence}`,
    });
  }

  const oldDuplicate = applyOperationEvent(view, {
    operationId,
    sequence: 1,
    revision: 1,
    at: FIXTURE_CLOCK + 1,
    phase: "accepted",
    summary: "event 1",
  });
  assert.equal(oldDuplicate.resync?.reason, "duplicate_identity_evicted");
  assert.equal(oldDuplicate.resync?.need, "snapshot");
  assert.ok(Object.keys(view.eventFingerprints).length <= 256);
});

test("conflicting duplicates inside a repair batch request a snapshot", () => {
  const scenario = scenarioById("parallel-steps");
  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const event of PARALLEL_EVENTS.slice(0, 3)) view = applyOperationEvent(view, event);
  view = applyOperationEvent(view, PARALLEL_EVENTS[4]);
  const conflict = applyOperationEvents(view, [
    PARALLEL_EVENTS[3],
    { ...PARALLEL_EVENTS[3], summary: "conflicting repair payload" },
  ]);
  assert.equal(conflict.resync?.reason, "conflicting_duplicate");
  assert.equal(conflict.resync?.need, "snapshot");
});

test("an invalid event in a repair batch cannot advance the cursor or clear repair", () => {
  const scenario = scenarioById("parallel-steps");
  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  view = applyOperationEvent(view, PARALLEL_EVENTS[0]);
  view = applyOperationEvent(view, PARALLEL_EVENTS[2]);

  const repaired = applyOperationEvents(view, [{ ...PARALLEL_EVENTS[1], summary: "" }]);

  assert.equal(repaired.lastSequence, 1);
  assert.equal(repaired.resync?.reason, "invalid_event");
  assert.equal(repaired.resync?.need, "snapshot");
  assert.equal(repaired.notice?.message, "the operation event failed contract validation");
});

test("a repair snapshot rejects a conflicting held event covered by its cursor", () => {
  const scenario = scenarioById("parallel-steps");
  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const each of PARALLEL_EVENTS.slice(0, 5)) view = applyOperationEvent(view, each);
  view = applyOperationEvent(view, PARALLEL_EVENTS[6]);
  view = {
    ...view,
    held: [{ ...PARALLEL_EVENTS[4], summary: "conflicting held dispatch" }, ...view.held],
  };
  const repaired = applyOperationSnapshot(view, {
    ...scenario.seed,
    revision: 2,
    state: "running",
    lastSequence: 5,
    updatedAt: PARALLEL_EVENTS[4].at,
    steps: [{
      stepId: "step-read-a",
      key: "read_a",
      state: "running",
      capability: "file.read",
      capabilityVersion: "1.0.0",
      effect: "read",
      attempts: 1,
      startedAt: PARALLEL_EVENTS[4].at,
    }],
  });
  assert.equal(repaired.resync?.reason, "conflicting_duplicate");
  assert.equal(repaired.resync?.need, "snapshot");
});

test("a repair snapshot rejects a covered held event whose identity was evicted", () => {
  const operationId = "op-evicted-covered-held";
  let view = applyOperationSnapshot(createOperationView(operationId), {
    ...scenarioById("parallel-steps").seed,
    operationId,
  });
  const events: OperationEvent[] = [];
  for (let sequence = 1; sequence <= 301; sequence++) {
    events.push({
      operationId,
      sequence,
      revision: sequence,
      at: FIXTURE_CLOCK + sequence,
      phase: sequence === 1 ? "accepted" : "progress",
      summary: `event ${sequence}`,
    });
  }
  view = applyOperationEvents(view, events);
  view = {
    ...view,
    status: "repairing",
    held: [events[0]],
    resync: { operationId, afterSequence: 301, need: "snapshot", reason: "unexpected_transition", at: events[0].at },
  };

  const repaired = applyOperationSnapshot(view, {
    ...scenarioById("parallel-steps").seed,
    operationId,
    revision: 301,
    lastSequence: 301,
    updatedAt: FIXTURE_CLOCK + 301,
  });

  assert.equal(repaired.resync?.reason, "duplicate_identity_evicted");
  assert.equal(repaired.resync?.need, "snapshot");
  assert.deepEqual(repaired.held.map((event) => event.sequence), [1]);
});

test("a repair snapshot preserves held events newer than its cursor", () => {
  const scenario = scenarioById("parallel-steps");
  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const each of PARALLEL_EVENTS.slice(0, 3)) view = applyOperationEvent(view, each);
  view = applyOperationEvent(view, PARALLEL_EVENTS[4]);
  assert.equal(view.resync?.reason, "gap");

  const repaired = applyOperationSnapshot(view, {
    ...scenario.seed,
    revision: 2,
    state: "accepted",
    lastSequence: 4,
    updatedAt: PARALLEL_EVENTS[3].at,
    steps: [
      {
        stepId: "step-read-a",
        key: "read_a",
        state: "queued",
        capability: "file.read",
        capabilityVersion: "1.0.0",
        effect: "read",
        attempts: 0,
      },
      {
        stepId: "step-read-b",
        key: "read_b",
        state: "queued",
        capability: "file.read",
        capabilityVersion: "1.0.0",
        effect: "read",
        attempts: 0,
      },
    ],
  });
  assert.equal(repaired.lastSequence, 5, "the held dispatch newer than the snapshot is replayed");
  assert.equal(repaired.steps.find((step) => step.stepId === "step-read-a")?.state, "running");
});

test("renderer phase edges are exact projections of the frozen O01 transition tables", () => {
  const operationEdges = Object.values(OPERATION_PHASE_EDGES).flat().map((edge) => json(edge));
  for (const edge of operationEdges) {
    assert.ok(OPERATION_TRANSITIONS.some((candidate) => JSON.stringify(candidate) === JSON.stringify(edge)), JSON.stringify(edge));
  }

  const stepEdges = [...Object.values(STEP_PHASE).flatMap((effect) => effect.edges ?? []), ...STEP_REPAIR_ONLY_EDGES].map((edge) => json(edge));
  assert.deepEqual(
    new Set(stepEdges.map((edge) => JSON.stringify(edge))),
    new Set(STEP_TRANSITIONS.filter((edge) => edge.trigger !== "dependency_failed").map((edge) => JSON.stringify(edge))),
  );
});

test("every O01 transition is explicitly event-mapped or repair-only", () => {
  const operationMapped = Object.values(OPERATION_PHASE_EDGES).flat().map((edge) => JSON.stringify(edge));
  const operationRepairOnly = OPERATION_TRANSITIONS
    .filter((edge) => edge.trigger === "cancel_requested")
    .map((edge) => JSON.stringify(edge));
  assert.deepEqual(new Set([...operationMapped, ...operationRepairOnly]), new Set(OPERATION_TRANSITIONS.map((edge) => JSON.stringify(edge))));

  const stepMapped = Object.values(STEP_PHASE).flatMap((effect) => effect.edges ?? []).map((edge) => JSON.stringify(edge));
  const stepRepairOnly = STEP_REPAIR_ONLY_EDGES.map((edge) => JSON.stringify(edge));
  const dependencyRepairOnly = STEP_TRANSITIONS.filter((edge) => edge.trigger === "dependency_failed").map((edge) => JSON.stringify(edge));
  assert.deepEqual(new Set([...stepMapped, ...stepRepairOnly, ...dependencyRepairOnly]), new Set(STEP_TRANSITIONS.map((edge) => JSON.stringify(edge))));
  assert.deepEqual(
    new Set(OPERATION_OBSERVATION_PHASES),
    new Set(["resolved", "queued", "progress", "succeeded", "failed", "cancelled"]),
    "step-only operation observations are an explicit repair-only category",
  );
});

test("first-seen dispatch and repeated transition phases are refused", () => {
  const scenario = scenarioById("parallel-steps");
  const operationId = scenario.expected.operationId;
  const seed = applyOperationSnapshot(createOperationView(operationId), scenario.seed);
  const accepted = applyOperationEvent(seed, {
    operationId,
    sequence: 1,
    revision: 1,
    at: FIXTURE_CLOCK,
    phase: "accepted",
    summary: "accepted",
  });
  const firstSeenDispatch = applyOperationEvent(accepted, {
    operationId,
    sequence: 2,
    revision: 2,
    at: FIXTURE_CLOCK + 50,
    phase: "dispatched",
    summary: "started without queueing",
    stepId: "step-first-seen",
    capability: "file.edit",
  });
  assert.equal(firstSeenDispatch.resync?.reason, "unexpected_transition");
  assert.equal(firstSeenDispatch.steps.length, 0);
  const queued = applyOperationEvent(accepted, {
    operationId,
    sequence: 2,
    revision: 2,
    at: FIXTURE_CLOCK + 100,
    phase: "queued",
    summary: "queued",
    stepId: "step-a",
    capability: "file.edit",
  });
  const running = applyOperationEvent(queued, {
    operationId,
    sequence: 3,
    revision: 3,
    at: FIXTURE_CLOCK + 200,
    phase: "dispatched",
    summary: "started",
    stepId: "step-a",
    capability: "file.edit",
  });
  assert.equal(running.steps[0].attempts, 1);

  const repeatedDispatch = applyOperationEvent(running, {
    operationId,
    sequence: 4,
    revision: 4,
    at: FIXTURE_CLOCK + 300,
    phase: "dispatched",
    summary: "started again without a failed retry",
    stepId: "step-a",
    capability: "file.edit",
  });
  assert.equal(repeatedDispatch.resync?.reason, "illegal_retry");
  assert.equal(repeatedDispatch.lastSequence, 3);
  assert.equal(repeatedDispatch.steps[0].attempts, 1, "an illegal repeat is not an attempt");

  const repeatedAccepted = applyOperationEvent(running, {
    operationId,
    sequence: 4,
    revision: 4,
    at: FIXTURE_CLOCK + 300,
    phase: "accepted",
    summary: "accepted again",
  });
  assert.equal(repeatedAccepted.resync?.reason, "unexpected_transition");
  assert.equal(repeatedAccepted.lastSequence, 3);
});

test("same-state step observations and same-destination operation phases request snapshot authority", () => {
  const scenario = scenarioById("parallel-steps");
  const operationId = scenario.expected.operationId;
  const seeded = applyOperationSnapshot(createOperationView(operationId), scenario.seed);
  const accepted = applyOperationEvent(seeded, PARALLEL_EVENTS[0]);
  const observed = applyOperationEvent(accepted, { ...PARALLEL_EVENTS[1], sequence: 2 });
  const queued = applyOperationEvent(observed, PARALLEL_EVENTS[2]);
  const repeatedQueue = applyOperationEvent(queued, { ...PARALLEL_EVENTS[2], sequence: 4, revision: 3, at: FIXTURE_CLOCK + 350 });
  assert.equal(repeatedQueue.resync?.reason, "unexpected_transition");
  assert.equal(repeatedQueue.lastSequence, 3);

  const running = applyOperationEvent(queued, { ...PARALLEL_EVENTS[4], sequence: 4 });
  const repeatedDispatch = applyOperationEvent(running, { ...PARALLEL_EVENTS[4], sequence: 5, revision: 3, at: FIXTURE_CLOCK + 450 });
  assert.equal(repeatedDispatch.resync?.reason, "illegal_retry");
  assert.equal(repeatedDispatch.state, "running");
});

test("a missing sequence asks for the cursor and is never applied out of order", () => {
  const scenario = scenarioById("parallel-steps");
  const events = scriptEvents(scenario);
  const [first, second, third, fourth, fifth] = events;

  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const event of [first, second, third]) view = applyOperationEvent(view, event);
  assert.equal(view.lastSequence, 3, JSON.stringify({ lastSequence: view.lastSequence, held: view.held.map((event) => event.sequence), resync: view.resync }));

  view = applyOperationEvent(view, fifth);
  assert.equal(view.lastSequence, 3, "the gap is not applied out of order");
  assert.equal(view.status, "repairing");
  assert.deepEqual(view.resync, {
    operationId: scenario.expected.operationId,
    afterSequence: 3,
    need: "events_after",
    reason: "gap",
    at: fifth.at,
  });

  view = applyOperationEvent(view, fifth);
  assert.equal(view.held.length, 2, "a repeat during a repair is held, not applied");
  assert.equal(view.lastSequence, 3);

  view = applyOperationEvents(view, [fourth]);
  assert.equal(view.resync, undefined, "the cursor replay cleared the repair");
  assert.equal(view.lastSequence, 5);
  view = applyOperationEvents(view, events.slice(5));
  assert.equal(view.resync?.need, "snapshot");
  view = applyOperationSnapshot(view, scenario.expected);
  assert.equal(view.state, "completed");
  assert.deepEqual(stepShape(view), stepShape(durableView(scenario)), "the repaired replay matches the record");

  const cold = applyOperationEvent(createOperationView(scenario.expected.operationId), first);
  assert.equal(cold.status, "repairing", "a view with no record refuses to stream events into a state it does not know");
  assert.equal(cold.resync?.need, "snapshot");
  assert.equal(cold.state, undefined, "no state is inferred before the record loads");
});

test("a repair batch retains every event beyond an unresolved gap", () => {
  const scenario = scenarioById("parallel-steps");
  const events = scriptEvents(scenario);
  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const event of events.slice(0, 3)) view = applyOperationEvent(view, event);
  view = { ...view, status: "repairing", resync: { operationId: scenario.expected.operationId, afterSequence: 3, need: "events_after", reason: "gap", at: FIXTURE_CLOCK } };

  const later = [5, 6, 7].map((sequence): OperationEvent => ({
    operationId: scenario.expected.operationId,
    sequence,
    revision: 3,
    at: FIXTURE_CLOCK + sequence * 100,
    phase: "progress",
    summary: `event ${sequence} beyond the gap`,
  }));
  view = applyOperationEvents(view, later);

  assert.equal(view.lastSequence, 3);
  assert.equal(view.resync?.reason, "gap");
  assert.deepEqual(view.held.map((event) => event.sequence), [5, 6, 7]);

  let repaired = { ...view, held: [], heldDropped: false, lastSequence: 2, resync: { ...view.resync!, afterSequence: 2 } };
  repaired = applyOperationEvents(repaired, [{
    operationId: scenario.expected.operationId,
    sequence: 3,
    revision: 3,
    at: FIXTURE_CLOCK + 400,
    phase: "progress",
    summary: "fourth event requires snapshot repair",
    stepId: "missing-step",
  }, ...later]);
  assert.equal(repaired.lastSequence, 2);
  assert.equal(repaired.resync?.need, "snapshot");
  assert.deepEqual(repaired.held.map((event) => event.sequence), [3, 5, 6, 7]);
});

test("terminal cancellation requests snapshot authority even with arbitrary evidence", () => {
  const scenario = scenarioById("parallel-steps");
  const seed = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  const operationId = scenario.expected.operationId;
  const at = FIXTURE_CLOCK;
  const accepted: OperationEvent = { operationId, sequence: 1, revision: 1, at, phase: "accepted", summary: "accepted" };
  const queued: OperationEvent = {
    operationId,
    sequence: 2,
    revision: 2,
    at: at + 100,
    phase: "queued",
    summary: "step-a is queued",
    stepId: "step-a",
    capability: "file.edit",
    queueReason: "mutation lane busy",
  };
  const dispatched: OperationEvent = {
    operationId,
    sequence: 3,
    revision: 3,
    at: at + 200,
    phase: "dispatched",
    summary: "step-a started",
    stepId: "step-a",
    capability: "file.edit",
  };
  let running = applyOperationEvent(seed, accepted);
  running = applyOperationEvent(running, queued);
  running = applyOperationEvent(running, dispatched);
  assert.equal(running.state, "running");
  assert.equal(running.steps[0].state, "running");

  const claimedSuccess: OperationEvent = {
    operationId,
    sequence: 4,
    revision: 4,
    at: at + 400,
    phase: "succeeded",
    summary: "step-a reported finished",
    stepId: "step-a",
    capability: "file.edit",
  };
  const refused = applyOperationEvent(running, claimedSuccess);
  assert.equal(refused.resync?.reason, "missing_evidence", "success with no evidence is a claim, not a fact");
  assert.equal(refused.lastSequence, 3, "the claim is not applied");
  assert.equal(refused.steps[0].state, "running", "the step is not marked succeeded");

  const evidenced: OperationEvent = { ...claimedSuccess, evidenceRefs: ["ev-check"] };
  const succeeded = applyOperationEvent(running, evidenced);
  assert.equal(succeeded.steps[0].state, "succeeded");

  const cancelEvent: OperationEvent = { ...claimedSuccess, phase: "cancelled", summary: "step-a stopped" };
  const cancelRefused = applyOperationEvent(running, cancelEvent);
  assert.equal(cancelRefused.resync?.reason, "missing_evidence", "cancel_confirmed means evidence, not a request");
  assert.equal(cancelRefused.steps[0].state, "running");

  // Even with evidence, a cancellation nobody asked for is not applied: the log has no phase
  // for the person's request, so the view reloads the record instead of guessing.
  const confirmedCancel: OperationEvent = { ...cancelEvent, evidenceRefs: ["ev-cancel-check"] };
  const unrequested = applyOperationEvent(running, confirmedCancel);
  assert.equal(unrequested.resync, undefined);
  assert.equal(unrequested.steps[0].state, "cancelled");
  assert.equal(unrequested.state, "running");

  const asked = markCancelRequested(running, FIXTURE_CLOCK + 350);
  assert.equal(asked.state, "running");
  assert.equal(asked.controlIntent?.cancelRequestedAt, FIXTURE_CLOCK + 350);
  const confirmed = applyOperationEvent(asked, confirmedCancel);
  assert.equal(confirmed.resync, undefined);
  assert.equal(confirmed.steps[0].state, "cancelled");
  assert.equal(confirmed.state, "running", "a settled step is not a settled operation");
});

test("local cancellation intent cannot authorize durable operation cancellation", () => {
  const scenario = scenarioById("cancellation");
  const running = openScenario(scenario, 3);
  const asked = markCancelRequested(running, FIXTURE_CLOCK + 200);
  const terminalEvent = scenario.script[7].event;
  const terminal = applyOperationEvent(asked, { ...terminalEvent, sequence: running.lastSequence + 1 });
  assert.equal(terminal.state, "running");
  assert.equal(terminal.resync?.reason, "unexpected_transition");
});

test("terminal operation events always request authoritative snapshot settlement", () => {
  const scenario = scenarioById("parallel-steps");
  const beforeTerminal = openScenario(scenario, scenario.script.length - 1);
  const terminal = scriptEvents(scenario).at(-1)!;
  const noEvidence = applyOperationEvent(beforeTerminal, { ...terminal, evidenceRefs: undefined });
  assert.equal(noEvidence.resync?.need, "snapshot");

  const arbitraryEvidence = applyOperationEvent(beforeTerminal, terminal);
  assert.equal(arbitraryEvidence.resync?.need, "snapshot");
  assert.equal(arbitraryEvidence.state, "running");

  const pendingView = openScenario(scenario, 10);
  const stillPending = applyOperationEvent(pendingView, { ...terminal, sequence: pendingView.lastSequence + 1 });
  assert.equal(stillPending.resync?.reason, "unexpected_transition");
});

test("a request to cancel is not a cancellation, and cancelling twice is the same view", () => {
  const scenario = scenarioById("cancellation");
  const view = openScenario(scenario, 3);
  assert.equal(view.state, "running", "the script has dispatched one call and queued another");
  const asked = applyScriptEntry(view, scenario.script[3]);
  assert.equal(asked.state, "running");
  assert.equal(asked.controlIntent?.cancelRequestedAt, FIXTURE_CLOCK + 200);
  assert.ok(cancelCallbackPayload(asked) === undefined, "a second cancel request has nowhere to go");
  assert.equal(markCancelRequested(asked, FIXTURE_CLOCK + 250), asked, "asking twice changes nothing");
  assert.equal(settledView(scenario).state, "cancelled", "only the durable settlement ends it");
});

test("a worker going quiet is not a completion", () => {
  const scenario = scenarioById("worker-silence");
  const view = settledView(scenario);
  assert.equal(view.state, "running");
  assert.equal(isTerminalOperation(view.state), false);
  assert.deepEqual(
    view.steps.filter((step) => step.state === "running").map((step) => step.stepId),
    ["step-read-a", "step-read-b"],
  );
  assert.equal(runCounts(view).running, 2);
  assert.equal(stalledSteps(view, FIXTURE_CLOCK + 300_000).length, 2, "both calls are reported as quiet");
  assert.match(progressLabel(view), /2 running/);
  assert.ok(!progressLabel(view).includes("%"));
  assert.ok(!stateLabel(view.state).toLowerCase().includes("complete"));
  assert.deepEqual(stepShape(view), stepShape(durableView(scenario)), "the record still says running");
  assert.equal(applyOperationSnapshot(view, scenario.expected).state, "running");
});

test("a decision is concrete, expiring, and cannot be turned into a grant", () => {
  const view = settledView(scenarioById("needs-input"));
  const rows = decisionRows(view, FIXTURE_CLOCK + 1_000);
  assert.equal(rows.length, 2);
  const approval = rows.find((row) => row.decisionClass === "user_authorization")!;
  const question = rows.find((row) => row.decisionClass === "additional_input")!;

  assert.ok(approval.answerable, "the approval can be answered once the bridge presents it");
  assert.ok(approval.subject.includes("file.edit") && approval.subject.includes("ws-api"), `subject: ${approval.subject}`);
  assert.equal(approval.preview, "src/config.ts: 3000 → 30000", "the content being approved is shown");
  assert.ok(approval.digest?.startsWith("sha256:"));
  assert.equal(approval.revision, 4);
  assert.deepEqual(approval.dependencies, ["read_a"]);
  assert.match(approval.expiryLabel, /^expires in /);

  const grant = decisionCallbackPayload(approval, "granted")!;
  assert.deepEqual(grant, {
    operationId: approval.operationId,
    stepId: approval.stepId,
    decisionId: approval.decisionId,
    expectedRevision: 4,
    outcome: "granted",
  });
  assert.deepEqual(
    Object.keys(grant).sort(),
    ["decisionId", "expectedRevision", "operationId", "outcome", "stepId"],
    "the renderer sends an intent: no record id, no payload digest, no policy source, no actor",
  );
  assert.equal(decisionCallbackPayload(question, "granted"), undefined, "facts never approve anything");
  assert.equal(additionalInputCallbackPayload(approval, "yes"), undefined, "an approval is not a text box");
  assert.equal(additionalInputCallbackPayload(question, "   "), undefined, "an empty answer sends nothing");
  const answer = additionalInputCallbackPayload(question, "the api repo")!;
  assert.deepEqual(answer.inputs, { answer: "the api repo" });
  assert.deepEqual(Object.keys(answer).sort(), ["decisionId", "expectedRevision", "inputs", "operationId", "stepId"]);

  const lapsed = decisionRows(view, FIXTURE_CLOCK + 400 + 600_000 + 1).find((row) => row.decisionId === approval.decisionId)!;
  assert.equal(lapsed.expired, true);
  assert.equal(lapsed.answerable, false);
  assert.equal(decisionCallbackPayload(lapsed, "granted"), undefined, "an expired window answers nothing");

  const superseded = decisionRows({ ...view, revision: 9, state: "running" }, FIXTURE_CLOCK + 1_000).find(
    (row) => row.decisionId === approval.decisionId,
  )!;
  assert.equal(superseded.answerable, false);
  assert.match(superseded.unavailable ?? "", /raised at revision 4/);

  const blind = decisionRows({ ...view, decisions: [] }, FIXTURE_CLOCK + 1_000).find(
    (row) => row.decisionClass === "user_authorization",
  )!;
  assert.equal(blind.answerable, false, "an approval with no preview is never offered");
  assert.match(blind.unavailable ?? "", /preview has not arrived/);
  assert.equal(decisionCallbackPayload(blind, "granted"), undefined);
});

test("a denied approval requires and applies an authoritative repair snapshot", () => {
  const scenario = scenarioById("needs-input");
  const awaiting = {
    ...scenario.expected,
    state: "awaiting_approval" as const,
    lastSequence: scenario.expected.lastSequence,
    steps: scenario.expected.steps.map((step) => ({ ...step, state: "awaiting_approval" as const })),
  };
  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), awaiting);
  view = applyOperationEvent(view, {
    operationId: scenario.expected.operationId,
    sequence: scenario.expected.lastSequence + 1,
    revision: 5,
    at: FIXTURE_CLOCK + 500,
    phase: "decision_recorded",
    summary: "the user denied the write",
    stepId: scenario.expected.steps[1].stepId,
    capability: scenario.expected.steps[1].capability,
    decisionCode: "denied",
  });

  assert.equal(view.resync?.reason, "decision_outcome_unreported");
  assert.equal(view.state, "awaiting_approval");
  assert.equal(view.steps[1].state, "awaiting_approval");
  const deniedSnapshot: OperationSnapshot = {
    ...awaiting,
    revision: 5,
    lastSequence: scenario.expected.lastSequence + 1,
    updatedAt: FIXTURE_CLOCK + 500,
    pendingDecisions: [],
    steps: awaiting.steps.map((step, index) => index === 1 ? { ...step, state: "skipped" as const } : step),
  };
  view = applyOperationSnapshot(view, deniedSnapshot);
  assert.equal(view.resync, undefined);
  assert.equal(view.steps[1].state, "skipped");
});

test("decision supplied takes the exact needs_input to running edge", () => {
  const scenario = scenarioById("needs-input");
  const waiting = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.expected);
  const supplied = applyOperationEvent(waiting, {
    operationId: scenario.expected.operationId,
    sequence: scenario.expected.lastSequence + 1,
    revision: 5,
    at: FIXTURE_CLOCK + 500,
    phase: "decision_recorded",
    summary: "the requested target was supplied",
    stepId: scenario.expected.steps[1].stepId,
    capability: scenario.expected.steps[1].capability,
    decisionCode: "supplied",
  });
  assert.equal(supplied.state, "running");
  assert.equal(supplied.resync, undefined);
});

test("a retry requires matching event count and retryable frozen failure metadata", () => {
  const scenario = scenarioById("provider-outage");
  const failed = {
    ...durableView(scenario),
    state: "running" as const,
  };
  const event: OperationEvent = {
    operationId: scenario.expected.operationId,
    sequence: 7,
    revision: 5,
    at: FIXTURE_CLOCK + 31_000,
    phase: "dispatched",
    summary: "retry started",
    stepId: scenario.expected.steps[0].stepId,
    capability: scenario.expected.steps[0].capability,
  };
  const missing = applyOperationEvent(failed, event);
  assert.equal(missing.resync?.reason, "illegal_retry");
  assert.equal(missing.steps[0].state, "failed");

  const claimed = applyOperationEvent(failed, { ...event, retryCount: 1 });
  assert.equal(claimed.resync, undefined);
  assert.equal(claimed.state, "running");
  assert.equal(claimed.steps[0].state, "running");
  assert.equal(claimed.steps[0].attempts, 2);
  assert.equal(claimed.steps[0].retryCount, 1);

  const wrongCount = applyOperationEvent(failed, { ...event, retryCount: 2 });
  assert.equal(wrongCount.resync?.reason, "illegal_retry");

  const notRetryable = applyOperationEvent({
    ...failed,
    steps: failed.steps.map((step) => ({ ...step, error: step.error ? { ...step.error, retryable: false } : undefined })),
  }, { ...event, retryCount: 1 });
  assert.equal(notRetryable.resync?.reason, "illegal_retry");

  const terminal = applyOperationEvent(durableView(scenario), { ...event, retryCount: 1 });
  assert.equal(terminal.resync?.reason, "illegal_retry");
  assert.equal(terminal.state, "failed");
});

test("snapshot unknown outcomes are authoritative even when event detail is absent", () => {
  const scenario = scenarioById("unknown-outcome");
  const view = durableView(scenario);
  assert.deepEqual(view.unknownOutcomes, scenario.expected.unknownOutcomes);
  assert.deepEqual(unknownRows(view).map((row) => row.key), ["unknown:step-write-b"]);
});

test("repair snapshots replace authoritative unknown outcomes before replaying held newer events", () => {
  const scenario = scenarioById("unknown-outcome");
  let view = durableView(scenario);
  const held = {
    operationId: scenario.expected.operationId,
    sequence: scenario.expected.lastSequence + 2,
    revision: scenario.expected.revision + 2,
    at: FIXTURE_CLOCK + 2_000,
    phase: "progress" as const,
    summary: "newer observation",
  };
  view = applyOperationEvent(view, held);
  const repaired = applyOperationSnapshot(view, {
    ...scenario.expected,
    revision: scenario.expected.revision + 1,
    lastSequence: scenario.expected.lastSequence + 1,
    updatedAt: FIXTURE_CLOCK + 1_500,
    unknownOutcomes: [],
    steps: scenario.expected.steps.map((step) =>
      step.stepId === "step-write-b" ? { ...step, state: "failed" as const, endedAt: FIXTURE_CLOCK + 1_500 } : step,
    ),
  });
  assert.deepEqual(repaired.unknownOutcomes, []);
  assert.equal(repaired.lastSequence, held.sequence);
});

test("a reconciliation with one committed write reports changed effects", () => {
  const scenario = scenarioById("unknown-reconciled");
  assert.equal(scenario.expected.steps.filter((step) => step.state === "succeeded").length, 1);
  assert.equal(scenario.expected.steps.filter((step) => step.state === "failed").length, 1);
  assert.equal(scenario.expected.state, "partial");
  assert.equal(scenario.expected.result?.state, "partial");
  assert.equal(scenario.expected.result?.changed, true);
});

test("a completed edit reports that it changed the target", () => {
  assert.equal(scenarioById("parallel-steps").expected.result?.changed, true);
});

test("cancellation remains available while reconciliation is in progress", () => {
  const scenario = scenarioById("unknown-outcome");
  const view = durableView(scenario);
  assert.equal(view.state, "reconciling");
  const requested = markCancelRequested(view, FIXTURE_CLOCK + 1_000);
  assert.equal(requested.state, "reconciling");
  assert.equal(requested.controlIntent?.cancelRequestedAt, FIXTURE_CLOCK + 1_000);
});

test("decision presentations must match the exact pending record and lifecycle", () => {
  const base = settledView(scenarioById("needs-input"));
  const approval = base.decisions.find((decision) => decision.decisionClass === "user_authorization")!;
  const cases = [
    { ...approval, operationId: "op-other" },
    { ...approval, stepId: "step-other" },
    { ...approval, decisionClass: "user_preference" as const },
    { ...approval, revision: approval.revision - 1 },
    { ...approval, expiresAt: FIXTURE_CLOCK },
  ];
  for (const presentation of cases) {
    const rows = decisionRows(markDecisionPresented(base, presentation, FIXTURE_CLOCK + 1_000), FIXTURE_CLOCK + 1_000);
    const row = rows.find((candidate) => candidate.decisionId === approval.decisionId)!;
    assert.equal(row.answerable, false, JSON.stringify(presentation));
    assert.equal(decisionCallbackPayload(row, "granted"), undefined);
  }

  const nonpending = markDecisionPresented({ ...base, pendingDecisions: [] }, approval, FIXTURE_CLOCK + 1_000);
  const row = decisionRows(nonpending, FIXTURE_CLOCK + 1_000).find((candidate) => candidate.decisionId === approval.decisionId)!;
  assert.equal(row.answerable, false);

  const wrongLifecycle = decisionRows({ ...base, state: "running" }, FIXTURE_CLOCK + 1_000)
    .find((candidate) => candidate.decisionId === approval.decisionId)!;
  assert.equal(wrongLifecycle.answerable, false);

  const wrongStepState = decisionRows({
    ...base,
    steps: base.steps.map((step) => step.stepId === approval.stepId ? { ...step, state: "succeeded" as const } : step),
  }, FIXTURE_CLOCK + 1_000).find((candidate) => candidate.decisionId === approval.decisionId)!;
  assert.equal(wrongStepState.answerable, false);
});

test("an already-expired decision presentation is unavailable at ingress", () => {
  const base = settledView(scenarioById("needs-input"));
  const approval = base.decisions.find((decision) => decision.decisionClass === "user_authorization")!;
  const presented = markDecisionPresented({ ...base, decisions: [] }, approval, approval.expiresAt + 1);
  const stored = presented.decisions.find((decision) => decision.decisionId === approval.decisionId)!;
  assert.equal(stored.answerable, false);
  assert.match(stored.unavailable ?? "", /expired/);
});

test("fixtures carry independently specified request and result semantics", () => {
  const expected = {
    "parallel-steps": ["In src/config.ts, change the timeout to 30000.", "completed", true],
    "queued-dependencies": ["Update the config and repin the package.", undefined, undefined],
    "needs-input": ["Point the api workspace at the new timeout.", undefined, undefined],
    "provider-outage": ["In src/config.ts, change the timeout to 30000.", "failed", false],
    cancellation: ["In src/config.ts, change the timeout to 30000.", "cancelled", false],
    "partial-effect": ["Apply the timeout change to both config files.", "partial", true],
    "unknown-outcome": ["In src/config.ts, change the timeout to 30000.", undefined, undefined],
    "unknown-reconciled": ["In src/config.ts, change the timeout to 30000.", "partial", true],
    "worker-silence": ["Read both config files and report the timeout.", undefined, undefined],
    reconnect: ["In src/config.ts, change the timeout to 30000.", "completed", true],
  } as const;
  for (const scenario of SCENARIOS) {
    const [instruction, resultState, changed] = expected[scenario.id as keyof typeof expected];
    assert.deepEqual(scenario.seed.request, { instruction }, `${scenario.id}: seed request`);
    assert.deepEqual(scenario.expected.request, { instruction }, `${scenario.id}: durable request`);
    assert.equal(scenario.expected.result?.state, resultState, `${scenario.id}: result state`);
    assert.equal(scenario.expected.result?.changed, changed, `${scenario.id}: changed semantics`);
  }
});

test("controls and cost are explicitly unavailable while state is contradictory", () => {
  const scenario = scenarioById("parallel-steps");
  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const each of PARALLEL_EVENTS.slice(0, 3)) view = applyOperationEvent(view, each);
  view = applyOperationEvent(view, PARALLEL_EVENTS[4]);
  assert.ok(view.resync);
  assert.equal(cancelCallbackPayload(view), undefined);
  assert.equal(costLabel(view), "cost not reported");
});

test("usage, elapsed and progress only repeat durable numbers", () => {
  const scenario = scenarioById("parallel-steps");
  const events = scriptEvents(scenario);
  let eventOnly = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const event of events.slice(0, 6)) eventOnly = applyOperationEvent(eventOnly, event);
  assert.deepEqual(eventOnly.usage, scenario.seed.usage, "usage is the record's number, never counted from events");
  assert.match(usageLabel(eventOnly.usage), /^0 steps/);

  const cold = createOperationView(scenario.expected.operationId);
  assert.equal(cold.usage, undefined, "a view with no record knows no usage");
  assert.match(usageLabel(cold.usage), /not reported/);

  const loaded = applyOperationSnapshot(eventOnly, scenario.expected);
  assert.deepEqual(loaded.usage, scenario.expected.usage);
  assert.match(usageLabel(loaded.usage), /3 steps/);
  assert.match(usageLabel(loaded.usage), /1 generation call/);

  for (const each of SCENARIOS) {
    const view = settledView(each);
    assert.ok(!progressLabel(view).includes("%"), `${each.id}: a count was turned into a percentage`);
    const keys = collectKeys(json(view));
    assert.deepEqual(
      // Whole key names only: `generationCalls` contains "ratio" without being one.
      keys.filter((key) => /^(percent|percentage|progress|progressPercent|ratio|fraction|completion|complete)$/i.test(key)),
      [],
      `${each.id}: the view carries a synthetic progress field`,
    );
  }
});

test("every state reads differently, and success, partial, failure and uncertainty differ", () => {
  for (const state of OPERATION_STATES) {
    assert.notEqual(stateLabel(state), stateLabel(undefined), `${state} has no label`);
    assert.ok(stateLabel(state).length > 0);
  }
  for (const state of STEP_STATES) assert.ok(stepStateLabel(state).length > 0, `${state} has no label`);
  assert.equal(stateTone("completed"), "success");
  assert.equal(stateTone("partial"), "warning");
  assert.equal(stateTone("failed"), "danger");
  assert.notEqual(stateLabel("partial"), stateLabel("completed"));
  assert.notEqual(stateLabel("cancelled"), stateLabel("failed"));
  assert.equal(stepStateTone("succeeded"), "success");
  assert.equal(stepStateTone("failed"), "danger");
  assert.equal(stepStateTone("outcome_unknown"), "warning");

  const unknown = settledView(scenarioById("unknown-outcome"));
  assert.equal(unknown.state, "reconciling");
  assert.notEqual(stateTone(unknown.state), "success");
  assert.equal(stepRows(unknown, FIXTURE_CLOCK + 2_000).find((row) => row.stepId === "step-write-b")!.tone, "warning");
  assert.equal(runCounts(unknown).unknown, 1);
  assert.equal(runCounts(unknown).settled, 1, "the applied write stays counted as settled");
  assert.ok(!isTerminalOperation(unknown.state));
});

test("the scenario set covers the acceptance list", () => {
  const ids = SCENARIOS.map((scenario) => scenario.id);
  assert.equal(new Set(ids).size, ids.length, "scenario ids are unique");
  for (const required of [
    "parallel-steps",
    "queued-dependencies",
    "needs-input",
    "provider-outage",
    "cancellation",
    "partial-effect",
    "unknown-outcome",
    "unknown-reconciled",
    "worker-silence",
    "reconnect",
  ]) {
    assert.ok(ids.includes(required), `missing the ${required} fixture`);
  }
  for (const scenario of SCENARIOS) {
    assert.ok(scenario.title.length > 0 && scenario.summary.length > 0, `${scenario.id}: needs a description`);
    assert.ok(scenario.script.length > 0, `${scenario.id}: needs a script`);
    assert.ok((OPERATION_STATES as readonly string[]).includes(scenario.expect.state));
    assert.equal(scenario.expect.settles, scenario.expect.resync === undefined, `${scenario.id}: settles and resync disagree`);
  }
  assert.equal(scenarioById("unknown-outcome").expect.unknownSteps.length, 1);
  assert.equal(scenarioById("provider-outage").expect.errorCode, "provider_failure");
  assert.equal(scenarioById("provider-outage").expected.state, "failed");
  assert.equal(scenarioById("partial-effect").expected.state, "partial");
  assert.equal(scenarioById("cancellation").expected.state, "cancelled");
  assert.equal(scenarioById("parallel-steps").expected.state, "completed");
  assert.ok(scenarioById("needs-input").expect.openDecisions > 1);
  assert.equal(scenarioById("reconnect").expect.resync?.reason, "gap");
  assert.equal(scenarioById("unknown-reconciled").expect.resync?.reason, "reconciled_unknown");
});

test("the envelope's own limits are reported, not hidden", () => {
  // A skipped step has a state in O01 but no phase that reports it, so only the durable
  // record can show one. The enum check below is what keeps that gap explicit.
  assert.ok((STEP_STATES as readonly string[]).includes("skipped"));
  assert.ok(!(OPERATION_EVENT_PHASES as readonly string[]).includes("skipped"));

  // The same for a failed step's typed error: the log carries the code in `decisionCode`
  // and the words in `summary`, and only the record says whether the failure is retryable.
  const scenario = scenarioById("provider-outage");
  let eventOnly = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const loggedEvent of scriptEvents(scenario)) eventOnly = applyOperationEvent(eventOnly, loggedEvent);
  const logged = eventOnly.steps[0];
  assert.equal(logged.state, "failed");
  assert.equal(logged.decisionCode, "provider_failure");
  assert.equal(logged.error, undefined, "the event envelope has no typed error");
  assert.match(errorRows(eventOnly)[0].value, /provider_failure/);
  assert.match(errorRows(eventOnly)[0].value, /retryability not reported/);

  const durable = durableView(scenario);
  assert.equal(durable.steps[0].error?.code, "provider_failure");
  assert.equal(durable.steps[0].error?.retryable, true, "only the record says it is retryable");
  assert.match(errorRows(durable)[0].value, /retryable/);
  assert.ok((durable.steps[0].error?.message.length ?? 0) > 0, "the record carries the failure's words");
});
