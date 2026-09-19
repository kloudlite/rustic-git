/**
 * O09 — fixture-driven operation scenarios.
 *
 * Every scenario is a *durable script*: the snapshot the store had when the operation was
 * accepted, the events the log then wrote (plus the person's own actions, which no phase
 * carries), and the snapshot the store held afterwards. The UI test replays the script into
 * a view and compares it with that final snapshot, so the fixtures check the projection
 * against the store rather than against a second copy of the expectations.
 *
 * The scenarios cover the slice's acceptance list: parallel steps, queued dependencies,
 * a pending decision, a provider outage, cancellation, a partial effect, an unknown
 * outcome, a reconciliation whose conclusion the log does not name, and a reconnected
 * stream with a gap.
 */
import type {
  OperationError,
  OperationErrorCode,
  OperationEvent,
  OperationEventPhase,
  OperationScope,
  OperationSnapshot,
  OperationState,
  OperateRequest,
  PendingDecision,
  StepRecord,
  UnknownOutcome,
} from "../../../../bench/src/operations/contracts.ts";
import { applyOperationEvent, applyOperationEvents, applyOperationSnapshot, createOperationView, markCancelRequested, markDecisionPresented } from "../reduce.ts";
import type { OperationView } from "../types.ts";
import type { DecisionPresentation, OperationUsage, ResyncNeed, ResyncReason } from "../types.ts";

/** One clock for every fixture, so two runs of a test read the same numbers. */
const T = 1_760_000_000_000;

/** The fixtures' own clock, exported so a test can render labels at a known instant. */
export const FIXTURE_CLOCK = T;

const hex = (value: number): string => value.toString(16).padStart(64, "0");
const digest = (value: number): string => `sha256:${hex(value)}`;

const ACTOR = { actorId: "user-1", tenantId: "tenant-1", sessionId: "s-1", turnId: "turn-4" };
const WORKSPACE: OperationScope = { workspaceId: "ws-api", treeId: "tree-7" };

/** The policy defaults; a fixture narrows nothing, so `validateBudgets` accepts them. */
const BUDGETS = {
  maxSteps: 12,
  maxSelectionRounds: 3,
  maxGenerationCalls: 2,
  maxConcurrentReads: 4,
  maxConcurrentMutations: 2,
  operationDeadlineMs: 600_000,
  handleWithinMs: 2_000,
  providerTimeoutMs: 30_000,
  maxGeneratedPayloadBytes: 65_536,
  maxTextFileBytes: 1_048_576,
  maxReadSnapshotBytes: 4_194_304,
};

const ZERO_USAGE: OperationUsage = { steps: 0, selectionRounds: 0, generationCalls: 0, attempts: 0 };

/** The accepted operation, before its first event: O01's `accepted` state, nothing done. */
function seed(operationId: string, instruction: string, at = T, scope: OperationScope = WORKSPACE): OperationSnapshot {
  return {
    contractVersion: "v1",
    operationId,
    revision: 1,
    state: "accepted",
    createdAt: at,
    updatedAt: at,
    deadlineAt: at + BUDGETS.operationDeadlineMs,
    actor: ACTOR,
    scope,
    request: { instruction } satisfies OperateRequest,
    requestDigest: digest(11),
    dedupeKey: "s-1:turn-4:call-9",
    budgets: { ...BUDGETS },
    steps: [],
    pendingDecisions: [],
    unknownOutcomes: [],
    usage: { ...ZERO_USAGE },
    lastSequence: 0,
  };
}

type SnapshotInput = {
  operationId: string;
  state: OperationState;
  revision: number;
  lastSequence: number;
  createdAt: number;
  updatedAt: number;
  steps?: StepRecord[];
  pendingDecisions?: PendingDecision[];
  unknownOutcomes?: UnknownOutcome[];
  usage?: OperationUsage;
  result?: OperationSnapshot["result"];
  scope?: OperationScope;
  request?: OperateRequest;
};

function snapshot(input: SnapshotInput): OperationSnapshot {
  return {
    contractVersion: "v1",
    operationId: input.operationId,
    revision: input.revision,
    state: input.state,
    createdAt: input.createdAt,
    updatedAt: input.updatedAt,
    deadlineAt: input.createdAt + BUDGETS.operationDeadlineMs,
    actor: ACTOR,
    scope: input.scope ?? WORKSPACE,
    request: input.request ?? ({ instruction: "In src/config.ts, change the timeout to 30000." } satisfies OperateRequest),
    requestDigest: digest(11),
    dedupeKey: "s-1:turn-4:call-9",
    budgets: { ...BUDGETS },
    steps: input.steps ?? [],
    pendingDecisions: input.pendingDecisions ?? [],
    unknownOutcomes: input.unknownOutcomes ?? [],
    usage: input.usage ?? { ...ZERO_USAGE },
    lastSequence: input.lastSequence,
    result: input.result,
  };
}

/** Fields an event carries from the step table; `key` is O01's `StepRecord` name, not an event field. */
type EventFields = Partial<OperationEvent> & { key?: string };

function event(
  operationId: string,
  sequence: number,
  revision: number,
  at: number,
  phase: OperationEventPhase,
  summary: string,
  fields: EventFields = {},
): OperationEvent {
  // A step's `key` names it in the durable record, never in the event envelope — the O01
  // validator refuses an unknown field, and the `STEP` table carries `key` so snapshots can
  // name their steps. Spreading one into `fields` would otherwise leak it into every event.
  const { key: _key, ...rest } = fields;
  return { operationId, sequence, revision, at, phase, summary, ...rest };
}

const STEP = {
  readA: { stepId: "step-read-a", key: "read_a", capability: "file.read" },
  readB: { stepId: "step-read-b", key: "read_b", capability: "file.read" },
  editC: { stepId: "step-edit-c", key: "edit_c", capability: "file.edit" },
  writeA: { stepId: "step-write-a", key: "write_a", capability: "file.edit" },
  writeB: { stepId: "step-write-b", key: "write_b", capability: "file.edit" },
  writeC: { stepId: "step-write-c", key: "write_c", capability: "file.edit" },
} as const;

// ---------------------------------------------------------------------------
// 1. Parallel reads, then a dependent edit
// ---------------------------------------------------------------------------

const PARALLEL_OP = "op-parallel-reads-1";

export const PARALLEL_EVENTS: readonly OperationEvent[] = [
  event(PARALLEL_OP, 1, 1, T + 0, "accepted", "operate accepted for turn-4"),
  event(PARALLEL_OP, 2, 1, T + 200, "resolved", "two reads, then one edit to src/config.ts", {
    model: { provider: "typesafe", model: "choice", version: "3" },
    decisionCode: "resolved_calls",
  }),
  event(PARALLEL_OP, 3, 2, T + 300, "queued", "read_a queued on the read lane", {
    ...STEP.readA,
    queueReason: "read lane: 2 of 4 slots free",
  }),
  event(PARALLEL_OP, 4, 2, T + 320, "queued", "read_b queued on the read lane", {
    ...STEP.readB,
    queueReason: "read lane: 2 of 4 slots free",
  }),
  event(PARALLEL_OP, 5, 2, T + 400, "dispatched", "read_a started", { ...STEP.readA }),
  event(PARALLEL_OP, 6, 2, T + 420, "dispatched", "read_b started", { ...STEP.readB }),
  event(PARALLEL_OP, 7, 2, T + 700, "progress", "read_a read 12 KiB", {
    ...STEP.readA,
    elapsedMs: 300,
    evidenceRefs: ["ev-read-a-partial"],
  }),
  event(PARALLEL_OP, 8, 3, T + 800, "succeeded", "read_a returned the timeout line", {
    ...STEP.readA,
    evidenceRefs: ["ev-read-a"],
  }),
  event(PARALLEL_OP, 9, 4, T + 820, "queued", "edit_c queued after read_a", {
    ...STEP.editC,
    dependencies: [STEP.readA.key],
    argDigest: digest(3),
    argPreview: "src/config.ts: 3000 → 30000",
  }),
  event(PARALLEL_OP, 10, 4, T + 840, "dispatched", "edit_c started after read_a", {
    ...STEP.editC,
    dependencies: [STEP.readA.key],
    argDigest: digest(3),
    argPreview: "src/config.ts: 3000 → 30000",
  }),
  event(PARALLEL_OP, 11, 4, T + 1000, "succeeded", "read_b returned the server timeout line", {
    ...STEP.readB,
    evidenceRefs: ["ev-read-b"],
  }),
  event(PARALLEL_OP, 12, 4, T + 1200, "progress", "edit_c wrote the new content", {
    ...STEP.editC,
    elapsedMs: 200,
  }),
  event(PARALLEL_OP, 13, 5, T + 1400, "succeeded", "edit_c applied to src/config.ts", {
    ...STEP.editC,
    elapsedMs: 580,
    evidenceRefs: ["ev-edit-c"],
  }),
  event(PARALLEL_OP, 14, 6, T + 1500, "completed", "one file changed; two reads succeeded", {
    evidenceRefs: ["ev-edit-c"],
  }),
];

const PARALLEL_EXPECTED = snapshot({
  operationId: PARALLEL_OP,
  state: "completed",
  revision: 6,
  lastSequence: 14,
  createdAt: T,
  updatedAt: T + 1500,
  steps: [
    {
      stepId: STEP.readA.stepId,
      key: STEP.readA.key,
      state: "succeeded",
      capability: STEP.readA.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "read",
      resourceKeys: ["workspace.file:src/config.ts"],
      attempts: 1,
      queuedReason: "read lane: 2 of 4 slots free",
      startedAt: T + 400,
      endedAt: T + 800,
      evidenceRefs: ["ev-read-a-partial", "ev-read-a"],
    },
    {
      stepId: STEP.readB.stepId,
      key: STEP.readB.key,
      state: "succeeded",
      capability: STEP.readB.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "read",
      resourceKeys: ["workspace.file:src/server.ts"],
      attempts: 1,
      queuedReason: "read lane: 2 of 4 slots free",
      startedAt: T + 420,
      endedAt: T + 1000,
      evidenceRefs: ["ev-read-b"],
    },
    {
      stepId: STEP.editC.stepId,
      key: STEP.editC.key,
      state: "succeeded",
      capability: STEP.editC.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      resourceKeys: ["workspace.file:src/config.ts"],
      dependencies: [STEP.readA.key],
      attempts: 1,
      startedAt: T + 840,
      endedAt: T + 1400,
      evidenceRefs: ["ev-edit-c"],
    },
  ],
  usage: { steps: 3, selectionRounds: 1, generationCalls: 1, attempts: 3 },
  result: {
    operationId: PARALLEL_OP,
    revision: 6,
    state: "completed",
    summary: "one file changed; two reads succeeded",
    changed: true,
    evidenceRefs: ["ev-edit-c"],
  },
});

// ---------------------------------------------------------------------------
// 2. Queued dependencies: one call running, two waiting for reasons
// ---------------------------------------------------------------------------

const QUEUED_OP = "op-queued-dependencies-2";
const MUTATION_LANE = "mutation lane: workspace.packages is being replaced";
const DEPENDENCY_LANE = "waiting for write_b";

const QUEUED_EVENTS: readonly OperationEvent[] = [
  event(QUEUED_OP, 1, 1, T + 0, "accepted", "operate accepted for turn-4"),
  event(QUEUED_OP, 2, 2, T + 90, "queued", "read_a queued", { ...STEP.readA }),
  event(QUEUED_OP, 3, 2, T + 100, "dispatched", "read_a started", { ...STEP.readA }),
  event(QUEUED_OP, 4, 3, T + 120, "queued", "write_b waits for the mutation lane", {
    ...STEP.writeB,
    dependencies: [STEP.readA.key],
    queueReason: MUTATION_LANE,
  }),
  event(QUEUED_OP, 5, 3, T + 130, "queued", "write_c waits for write_b", {
    ...STEP.writeC,
    dependencies: [STEP.writeB.key],
    queueReason: DEPENDENCY_LANE,
  }),
  event(QUEUED_OP, 6, 3, T + 900, "progress", "read_a is still reading", {
    ...STEP.readA,
    elapsedMs: 800,
    evidenceRefs: ["ev-read-a-partial"],
  }),
];

const QUEUED_EXPECTED = snapshot({
  operationId: QUEUED_OP,
  state: "running",
  revision: 3,
  lastSequence: 6,
  createdAt: T,
  updatedAt: T + 900,
  request: { instruction: "Update the config and repin the package." },
  steps: [
    {
      stepId: STEP.readA.stepId,
      key: STEP.readA.key,
      state: "running",
      capability: STEP.readA.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "read",
      attempts: 1,
      startedAt: T + 100,
      evidenceRefs: ["ev-read-a-partial"],
    },
    {
      stepId: STEP.writeB.stepId,
      key: STEP.writeB.key,
      state: "queued",
      capability: STEP.writeB.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      dependencies: [STEP.readA.key],
      attempts: 0,
      queuedReason: MUTATION_LANE,
    },
    {
      stepId: STEP.writeC.stepId,
      key: STEP.writeC.key,
      state: "queued",
      capability: STEP.writeC.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      dependencies: [STEP.writeB.key],
      attempts: 0,
      queuedReason: DEPENDENCY_LANE,
    },
  ],
  usage: { steps: 3, selectionRounds: 1, generationCalls: 0, attempts: 1 },
});

// ---------------------------------------------------------------------------
// 3. A concrete approval and a question that needs facts
// ---------------------------------------------------------------------------

const INPUT_OP = "op-needs-input-3";
const DECISION_APPROVAL = "dec-approve-edit";
const DECISION_QUESTION = "dec-need-target";
const DECISION_WINDOW_MS = 600_000;

const INPUT_EVENTS: readonly OperationEvent[] = [
  event(INPUT_OP, 1, 1, T + 0, "accepted", "operate accepted for turn-4"),
  event(INPUT_OP, 2, 2, T + 90, "queued", "read_a queued", { ...STEP.readA }),
  event(INPUT_OP, 3, 2, T + 100, "dispatched", "read_a started", { ...STEP.readA }),
  event(INPUT_OP, 4, 3, T + 300, "succeeded", "read_a returned the timeout line", {
    ...STEP.readA,
    evidenceRefs: ["ev-read-a"],
  }),
  event(INPUT_OP, 5, 4, T + 350, "queued", "write_b waits for approval", {
    ...STEP.writeB,
    dependencies: [STEP.readA.key],
    queueReason: "needs approval before a write",
  }),
  event(INPUT_OP, 6, 4, T + 400, "needs_input", "the request names a repository that exists twice", {
    ...STEP.writeB,
    decisionCode: "need_input",
  }),
];

const INPUT_PENDING: PendingDecision[] = [
  {
    decisionId: DECISION_QUESTION,
    operationId: INPUT_OP,
    stepId: STEP.writeB.stepId,
    decisionClass: "additional_input",
    question: "Which workspace should “the api repo” mean?",
    createdAt: T + 400,
    expiresAt: T + 400 + DECISION_WINDOW_MS,
    revision: 4,
  },
  {
    decisionId: DECISION_APPROVAL,
    operationId: INPUT_OP,
    stepId: STEP.writeB.stepId,
    decisionClass: "user_authorization",
    question: "Apply the timeout change to src/config.ts?",
    createdAt: T + 400,
    expiresAt: T + 400 + DECISION_WINDOW_MS,
    revision: 4,
  },
];

const INPUT_EXPECTED = snapshot({
  operationId: INPUT_OP,
  state: "needs_input",
  revision: 4,
  lastSequence: 6,
  createdAt: T,
  updatedAt: T + 400,
  request: { instruction: "Point the api workspace at the new timeout." },
  steps: [
    {
      stepId: STEP.readA.stepId,
      key: STEP.readA.key,
      state: "succeeded",
      capability: STEP.readA.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "read",
      attempts: 1,
      startedAt: T + 100,
      endedAt: T + 300,
      evidenceRefs: ["ev-read-a"],
    },
    {
      stepId: STEP.writeB.stepId,
      key: STEP.writeB.key,
      state: "queued",
      capability: STEP.writeB.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      dependencies: [STEP.readA.key],
      attempts: 0,
      queuedReason: "needs approval before a write",
    },
  ],
  pendingDecisions: INPUT_PENDING,
  usage: { steps: 2, selectionRounds: 1, generationCalls: 1, attempts: 1 },
});

/** What the trusted bridge hands the renderer for `dec-approve-edit`. */
export const INPUT_APPROVAL_PRESENTATION: DecisionPresentation = {
  decisionId: DECISION_APPROVAL,
  operationId: INPUT_OP,
  stepId: STEP.writeB.stepId,
  decisionClass: "user_authorization",
  question: "Apply the timeout change to src/config.ts?",
  createdAt: T + 400,
  expiresAt: T + 400 + DECISION_WINDOW_MS,
  revision: 4,
  capability: STEP.writeB.capability,
  targetRef: "ws-api",
  effect: "write",
  argDigest: digest(5),
  argPreview: "src/config.ts: 3000 → 30000",
  dependencies: [STEP.readA.key],
  answerable: true,
};

// ---------------------------------------------------------------------------
// 4. Provider outage: generation never answered, so nothing was written
// ---------------------------------------------------------------------------

const OUTAGE_OP = "op-provider-outage-4";
const OUTAGE_MESSAGE = "the generation provider did not answer within 30 s";

const OUTAGE_EVENTS: readonly OperationEvent[] = [
  event(OUTAGE_OP, 1, 1, T + 0, "accepted", "operate accepted for turn-4"),
  event(OUTAGE_OP, 2, 2, T + 100, "resolving", "resolving the edit target", {
    model: { provider: "typesafe", model: "choice", version: "3" },
  }),
  event(OUTAGE_OP, 3, 3, T + 150, "queued", "edit_c queued behind the generation call", {
    ...STEP.editC,
  }),
  event(OUTAGE_OP, 4, 3, T + 200, "dispatched", "edit_c started", { ...STEP.editC }),
  event(OUTAGE_OP, 5, 4, T + 30_200, "failed", "the generation provider never answered", {
    ...STEP.editC,
    decisionCode: "provider_failure",
    evidenceRefs: ["ev-provider-timeout"],
  }),
  event(OUTAGE_OP, 6, 4, T + 30_300, "failed", "no content was generated; nothing was written", {
    decisionCode: "provider_failure",
    evidenceRefs: ["ev-provider-timeout"],
  }),
];

const OUTAGE_ERROR: OperationError = {
  code: "provider_failure",
  message: OUTAGE_MESSAGE,
  retryable: true,
  refs: ["ev-provider-timeout"],
};

const OUTAGE_EXPECTED = snapshot({
  operationId: OUTAGE_OP,
  state: "failed",
  revision: 4,
  lastSequence: 6,
  createdAt: T,
  updatedAt: T + 30_300,
  steps: [
    {
      stepId: STEP.editC.stepId,
      key: STEP.editC.key,
      state: "failed",
      capability: STEP.editC.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      attempts: 1,
      startedAt: T + 200,
      endedAt: T + 30_200,
      evidenceRefs: ["ev-provider-timeout"],
      error: OUTAGE_ERROR,
    },
  ],
  usage: { steps: 1, selectionRounds: 1, generationCalls: 1, attempts: 1 },
  result: {
    operationId: OUTAGE_OP,
    revision: 4,
    state: "failed",
    summary: "no content was generated; nothing was written",
    changed: false,
    error: OUTAGE_ERROR,
  },
});

// ---------------------------------------------------------------------------
// 5. Cancellation: asked for, then settled with evidence
// ---------------------------------------------------------------------------

const CANCEL_OP = "op-cancel-5";
const CANCEL_EVIDENCE = "ev-cancel-check";

const CANCEL_EVENTS: readonly OperationEvent[] = [
  event(CANCEL_OP, 1, 1, T + 0, "accepted", "operate accepted for turn-4"),
  event(CANCEL_OP, 2, 2, T + 90, "queued", "read_a queued", { ...STEP.readA }),
  event(CANCEL_OP, 3, 2, T + 100, "dispatched", "read_a started", { ...STEP.readA }),
  event(CANCEL_OP, 4, 3, T + 120, "queued", "write_b queued behind read_a", {
    ...STEP.writeB,
    dependencies: [STEP.readA.key],
  }),
  event(CANCEL_OP, 5, 4, T + 300, "cancelled", "read_a stopped before any effect", {
    ...STEP.readA,
    decisionCode: "cancel_confirmed",
    evidenceRefs: [CANCEL_EVIDENCE],
  }),
  event(CANCEL_OP, 6, 4, T + 310, "cancelled", "write_b never ran", { ...STEP.writeB }),
  event(CANCEL_OP, 7, 4, T + 320, "cancelled", "cancelled: no effect was applied", {
    evidenceRefs: [CANCEL_EVIDENCE],
  }),
];

const CANCEL_EXPECTED = snapshot({
  operationId: CANCEL_OP,
  state: "cancelled",
  revision: 4,
  lastSequence: 7,
  createdAt: T,
  updatedAt: T + 320,
  steps: [
    {
      stepId: STEP.readA.stepId,
      key: STEP.readA.key,
      state: "cancelled",
      capability: STEP.readA.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "read",
      attempts: 1,
      startedAt: T + 100,
      endedAt: T + 300,
      evidenceRefs: [CANCEL_EVIDENCE],
    },
    {
      stepId: STEP.writeB.stepId,
      key: STEP.writeB.key,
      state: "cancelled",
      capability: STEP.writeB.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      dependencies: [STEP.readA.key],
      attempts: 0,
      endedAt: T + 310,
    },
  ],
  usage: { steps: 2, selectionRounds: 1, generationCalls: 0, attempts: 1 },
  result: {
    operationId: CANCEL_OP,
    revision: 4,
    state: "cancelled",
    summary: "cancelled: no effect was applied",
    changed: false,
    evidenceRefs: [CANCEL_EVIDENCE],
  },
});

// ---------------------------------------------------------------------------
// 6. Partial effect: one edit landed, one failed
// ---------------------------------------------------------------------------

const PARTIAL_OP = "op-partial-effect-6";
const PARTIAL_MESSAGE = "the second edit was rejected: its target had changed";

const PARTIAL_EVENTS: readonly OperationEvent[] = [
  event(PARTIAL_OP, 1, 1, T + 0, "accepted", "operate accepted for turn-4"),
  event(PARTIAL_OP, 2, 2, T + 80, "queued", "write_a queued", { ...STEP.writeA }),
  event(PARTIAL_OP, 3, 2, T + 90, "queued", "write_b queued", { ...STEP.writeB }),
  event(PARTIAL_OP, 4, 2, T + 100, "dispatched", "write_a started", { ...STEP.writeA }),
  event(PARTIAL_OP, 5, 2, T + 110, "dispatched", "write_b started", { ...STEP.writeB }),
  event(PARTIAL_OP, 6, 3, T + 400, "succeeded", "write_a applied to src/config.ts", {
    ...STEP.writeA,
    evidenceRefs: ["ev-write-a"],
  }),
  event(PARTIAL_OP, 7, 4, T + 600, "failed", "write_b was rejected", {
    ...STEP.writeB,
    decisionCode: "execution_failure",
    evidenceRefs: ["ev-write-b-rejected"],
  }),
  event(PARTIAL_OP, 8, 5, T + 650, "partial", "1 of 2 edits applied; the second failed", {
    evidenceRefs: ["ev-write-a", "ev-write-b-rejected"],
  }),
];

const PARTIAL_ERROR: OperationError = {
  code: "execution_failure",
  message: PARTIAL_MESSAGE,
  retryable: false,
  refs: ["ev-write-b-rejected"],
};

const PARTIAL_EXPECTED = snapshot({
  operationId: PARTIAL_OP,
  state: "partial",
  revision: 5,
  lastSequence: 8,
  createdAt: T,
  updatedAt: T + 650,
  request: { instruction: "Apply the timeout change to both config files." },
  steps: [
    {
      stepId: STEP.writeA.stepId,
      key: STEP.writeA.key,
      state: "succeeded",
      capability: STEP.writeA.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      attempts: 1,
      startedAt: T + 100,
      endedAt: T + 400,
      evidenceRefs: ["ev-write-a"],
    },
    {
      stepId: STEP.writeB.stepId,
      key: STEP.writeB.key,
      state: "failed",
      capability: STEP.writeB.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      attempts: 1,
      startedAt: T + 110,
      endedAt: T + 600,
      evidenceRefs: ["ev-write-b-rejected"],
      error: PARTIAL_ERROR,
    },
  ],
  usage: { steps: 2, selectionRounds: 1, generationCalls: 1, attempts: 2 },
  result: {
    operationId: PARTIAL_OP,
    revision: 5,
    state: "partial",
    summary: "1 of 2 edits applied; the second failed",
    changed: true,
    evidenceRefs: ["ev-write-a"],
    error: PARTIAL_ERROR,
  },
});

// ---------------------------------------------------------------------------
// 7. Unknown outcome: dispatched, never answered
// ---------------------------------------------------------------------------

const UNKNOWN_OP = "op-unknown-outcome-7";
const UNKNOWN_DISPATCH_DIGEST = digest(7);

const UNKNOWN_EVENTS: readonly OperationEvent[] = [
  event(UNKNOWN_OP, 1, 1, T + 0, "accepted", "operate accepted for turn-4"),
  event(UNKNOWN_OP, 2, 2, T + 80, "queued", "write_a queued", { ...STEP.writeA }),
  event(UNKNOWN_OP, 3, 2, T + 90, "queued", "write_b queued", { ...STEP.writeB }),
  event(UNKNOWN_OP, 4, 2, T + 100, "dispatched", "write_a started", { ...STEP.writeA }),
  event(UNKNOWN_OP, 5, 2, T + 110, "dispatched", "write_b started", { ...STEP.writeB }),
  event(UNKNOWN_OP, 6, 3, T + 300, "succeeded", "write_a applied", {
    ...STEP.writeA,
    evidenceRefs: ["ev-write-a"],
  }),
  event(UNKNOWN_OP, 7, 4, T + 900, "unknown_outcome", "write_b was dispatched; the answer never came", {
    ...STEP.writeB,
    decisionCode: "timeout_after_dispatch",
    argDigest: UNKNOWN_DISPATCH_DIGEST,
    evidenceRefs: ["ev-write-b-dispatched"],
  }),
  event(UNKNOWN_OP, 8, 4, T + 950, "progress", "an effect may have been applied: reconciling", {
    evidenceRefs: ["ev-write-b-dispatched"],
  }),
];

const UNKNOWN_EXPECTED = snapshot({
  operationId: UNKNOWN_OP,
  state: "reconciling",
  revision: 4,
  lastSequence: 8,
  createdAt: T,
  updatedAt: T + 950,
  steps: [
    {
      stepId: STEP.writeA.stepId,
      key: STEP.writeA.key,
      state: "succeeded",
      capability: STEP.writeA.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      attempts: 1,
      startedAt: T + 100,
      endedAt: T + 300,
      evidenceRefs: ["ev-write-a"],
    },
    {
      stepId: STEP.writeB.stepId,
      key: STEP.writeB.key,
      state: "outcome_unknown",
      capability: STEP.writeB.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      attempts: 1,
      startedAt: T + 110,
      evidenceRefs: ["ev-write-b-dispatched"],
    },
  ],
  unknownOutcomes: [
    {
      stepId: STEP.writeB.stepId,
      capability: STEP.writeB.capability,
      dispatchDigest: UNKNOWN_DISPATCH_DIGEST,
      since: T + 900,
    },
  ],
  usage: { steps: 2, selectionRounds: 1, generationCalls: 1, attempts: 2 },
});

// ---------------------------------------------------------------------------
// 8. Reconciliation: the log says it happened, not how it ended
// ---------------------------------------------------------------------------

const RECONCILE_OP = "op-reconciled-8";

const RECONCILE_EVENTS: readonly OperationEvent[] = [
  ...UNKNOWN_EVENTS.map((entry) => ({ ...entry, operationId: RECONCILE_OP })),
  event(RECONCILE_OP, 9, 5, T + 1200, "reconciled", "backend records show the write was not applied", {
    ...STEP.writeB,
    evidenceRefs: ["ev-reconcile"],
  }),
  event(RECONCILE_OP, 10, 5, T + 1250, "reconciled", "reconciliation concluded", {
    evidenceRefs: ["ev-reconcile"],
  }),
];

const RECONCILE_EXPECTED = snapshot({
  operationId: RECONCILE_OP,
  state: "partial",
  revision: 5,
  lastSequence: 10,
  createdAt: T,
  updatedAt: T + 1250,
  steps: [
    {
      stepId: STEP.writeA.stepId,
      key: STEP.writeA.key,
      state: "succeeded",
      capability: STEP.writeA.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      attempts: 1,
      startedAt: T + 100,
      endedAt: T + 300,
      evidenceRefs: ["ev-write-a"],
    },
    {
      stepId: STEP.writeB.stepId,
      key: STEP.writeB.key,
      state: "failed",
      capability: STEP.writeB.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "write",
      attempts: 1,
      startedAt: T + 110,
      endedAt: T + 1200,
      evidenceRefs: ["ev-write-b-dispatched", "ev-reconcile"],
      error: {
        code: "unknown_outcome",
        message: "backend records show the dispatch did not apply",
        retryable: false,
        refs: ["ev-reconcile"],
      },
    },
  ],
  usage: { steps: 2, selectionRounds: 1, generationCalls: 1, attempts: 2 },
  result: {
    operationId: RECONCILE_OP,
    revision: 5,
    state: "partial",
    summary: "reconciliation concluded one write committed and another failed",
    changed: true,
    evidenceRefs: ["ev-reconcile"],
  },
});

// ---------------------------------------------------------------------------
// 9. The worker stopped reporting: silence is not completion
// ---------------------------------------------------------------------------

const SILENT_OP = "op-worker-silence-9";

const SILENT_EVENTS: readonly OperationEvent[] = [
  event(SILENT_OP, 1, 1, T + 0, "accepted", "operate accepted for turn-4"),
  event(SILENT_OP, 2, 2, T + 80, "queued", "read_a queued", { ...STEP.readA }),
  event(SILENT_OP, 3, 2, T + 90, "queued", "read_b queued", { ...STEP.readB }),
  event(SILENT_OP, 4, 2, T + 100, "dispatched", "read_a started", { ...STEP.readA }),
  event(SILENT_OP, 5, 2, T + 120, "dispatched", "read_b started", { ...STEP.readB }),
  event(SILENT_OP, 6, 2, T + 900, "progress", "read_a is still reading", {
    ...STEP.readA,
    elapsedMs: 800,
    evidenceRefs: ["ev-read-a-partial"],
  }),
  // The session that started this turn went away. That says nothing about either call.
  event(SILENT_OP, 7, 3, T + 1_200, "progress", "the agent session for this turn ended; the operation is still running"),
];

const SILENT_EXPECTED = snapshot({
  operationId: SILENT_OP,
  state: "running",
  revision: 3,
  lastSequence: 7,
  createdAt: T,
  updatedAt: T + 1_200,
  request: { instruction: "Read both config files and report the timeout." },
  steps: [
    {
      stepId: STEP.readA.stepId,
      key: STEP.readA.key,
      state: "running",
      capability: STEP.readA.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "read",
      attempts: 1,
      startedAt: T + 100,
      evidenceRefs: ["ev-read-a-partial"],
    },
    {
      stepId: STEP.readB.stepId,
      key: STEP.readB.key,
      state: "running",
      capability: STEP.readB.capability,
      capabilityVersion: "1.0.0",
      targetRef: "ws-api",
      effect: "read",
      attempts: 1,
      startedAt: T + 120,
    },
  ],
  usage: { steps: 2, selectionRounds: 1, generationCalls: 0, attempts: 2 },
});

// ---------------------------------------------------------------------------
// The scripts
// ---------------------------------------------------------------------------

export type FixtureAction =
  | { kind: "cancel_requested"; at: number }
  | { kind: "snapshot_loaded"; snapshot: OperationSnapshot }
  | { kind: "decision_presented"; decision: DecisionPresentation };

export type FixtureScriptEntry = { note: string } & ({ event: OperationEvent } | { action: FixtureAction });

export type ScenarioExpectation = {
  state: OperationState;
  /** True when the script leaves nothing to repair. */
  settles: boolean;
  /** Set when the script deliberately leaves the view asking for a repair. */
  resync?: { need: ResyncNeed; reason: ResyncReason; afterSequence: number };
  /** The most steps the log shows running at the same time. */
  peakConcurrent: number;
  /** Queue reasons a person must be able to read. */
  queueReasons: string[];
  /** Steps whose outcome is still unknown at the end of the script. */
  unknownSteps: string[];
  /** Decisions that can be answered right now. */
  openDecisions: number;
  /** An error a person must be able to read, once the snapshot is loaded. */
  errorCode?: OperationErrorCode;
};

export type OperationScenario = {
  id: string;
  title: string;
  summary: string;
  seed: OperationSnapshot;
  script: readonly FixtureScriptEntry[];
  /** Events the bridge returns for an `events_after` repair, in order. */
  repair?: readonly OperationEvent[];
  /** The durable record the store held once the script had run. */
  expected: OperationSnapshot;
  expect: ScenarioExpectation;
  /** The snapshot to load after a deliberate `need: "snapshot"` resync. */
  reload?: OperationSnapshot;
};

const entry = (note: string, event: OperationEvent): FixtureScriptEntry => ({ note, event });
const act = (note: string, action: FixtureAction): FixtureScriptEntry => ({ note, action });

export const SCENARIOS: readonly OperationScenario[] = [
  {
    id: "parallel-steps",
    title: "Two reads run together, then the edit they feed",
    summary: "Concurrent running calls, overlapping intervals, a dependent third step, and a truthful completion.",
    seed: seed(PARALLEL_OP, "In src/config.ts, change the timeout to 30000."),
    script: [
      ...PARALLEL_EVENTS.map((event) => entry(`${event.phase} #${event.sequence}`, event)),
      act("the bridge loads the authoritative completion", { kind: "snapshot_loaded", snapshot: PARALLEL_EXPECTED }),
    ],
    expected: PARALLEL_EXPECTED,
    expect: {
      state: "completed",
      settles: true,
      peakConcurrent: 2,
      queueReasons: ["read lane: 2 of 4 slots free"],
      unknownSteps: [],
      openDecisions: 0,
    },
  },
  {
    id: "queued-dependencies",
    title: "One call running, two queued with reasons",
    summary: "Queue reasons and dependencies are readable, and a running step is never reported finished while it is silent.",
    seed: seed(QUEUED_OP, "Update the config and repin the package."),
    script: QUEUED_EVENTS.map((event) => entry(`${event.phase} #${event.sequence}`, event)),
    expected: QUEUED_EXPECTED,
    expect: {
      state: "running",
      settles: true,
      peakConcurrent: 1,
      queueReasons: [MUTATION_LANE, DEPENDENCY_LANE],
      unknownSteps: [],
      openDecisions: 0,
    },
  },
  {
    id: "needs-input",
    title: "An operation waiting for a person",
    summary: "A concrete approval with its content and window, beside a question for facts; both come from the trusted bridge.",
    seed: seed(INPUT_OP, "Point the api workspace at the new timeout."),
    script: [
      ...INPUT_EVENTS.map((event) => entry(`${event.phase} #${event.sequence}`, event)),
      act("the bridge loads the final operation snapshot", { kind: "snapshot_loaded", snapshot: INPUT_EXPECTED }),
      act("the bridge presents the approval preview", { kind: "decision_presented", decision: INPUT_APPROVAL_PRESENTATION }),
    ],
    expected: INPUT_EXPECTED,
    expect: {
      state: "needs_input",
      settles: true,
      peakConcurrent: 1,
      queueReasons: ["needs approval before a write"],
      unknownSteps: [],
      openDecisions: 2,
    },
  },
  {
    id: "provider-outage",
    title: "The generation provider never answered",
    summary: "A failed step with the provider error class, and no claim that anything was written.",
    seed: seed(OUTAGE_OP, "In src/config.ts, change the timeout to 30000."),
    script: [
      ...OUTAGE_EVENTS.map((event) => entry(`${event.phase} #${event.sequence}`, event)),
      act("the bridge loads the authoritative failure", { kind: "snapshot_loaded", snapshot: OUTAGE_EXPECTED }),
    ],
    expected: OUTAGE_EXPECTED,
    expect: {
      state: "failed",
      settles: true,
      peakConcurrent: 1,
      queueReasons: [],
      unknownSteps: [],
      openDecisions: 0,
      errorCode: "provider_failure",
    },
  },
  {
    id: "cancellation",
    title: "Cancelled after one call started",
    summary: "The person's own cancel request takes the table's cancel_requested edge, and only the durable settlement ends the operation.",
    seed: seed(CANCEL_OP, "In src/config.ts, change the timeout to 30000."),
    script: [
      entry("accepted #1", CANCEL_EVENTS[0]),
      entry("queued #2", CANCEL_EVENTS[1]),
      entry("dispatched #3", CANCEL_EVENTS[2]),
      act("the person asks to cancel", { kind: "cancel_requested", at: T + 200 }),
      entry("queued #4", CANCEL_EVENTS[3]),
      entry("cancelled #5", CANCEL_EVENTS[4]),
      entry("cancelled #6", CANCEL_EVENTS[5]),
      entry("cancelled #7", CANCEL_EVENTS[6]),
      act("the bridge loads the authoritative cancellation settlement", { kind: "snapshot_loaded", snapshot: CANCEL_EXPECTED }),
    ],
    expected: CANCEL_EXPECTED,
    expect: {
      state: "cancelled",
      settles: true,
      peakConcurrent: 1,
      queueReasons: [],
      unknownSteps: [],
      openDecisions: 0,
    },
  },
  {
    id: "partial-effect",
    title: "One edit landed, one failed",
    summary: "A partial outcome that keeps the completed effect visible and never reads as success.",
    seed: seed(PARTIAL_OP, "Apply the timeout change to both config files."),
    script: [
      ...PARTIAL_EVENTS.map((event) => entry(`${event.phase} #${event.sequence}`, event)),
      act("the bridge loads the authoritative partial result", { kind: "snapshot_loaded", snapshot: PARTIAL_EXPECTED }),
    ],
    expected: PARTIAL_EXPECTED,
    expect: {
      state: "partial",
      settles: true,
      peakConcurrent: 2,
      queueReasons: [],
      unknownSteps: [],
      openDecisions: 0,
      errorCode: "execution_failure",
    },
  },
  {
    id: "unknown-outcome",
    title: "A write that may not have landed",
    summary: "An uncertain mutation stays visible and is never retried or called failed by the view.",
    seed: seed(UNKNOWN_OP, "In src/config.ts, change the timeout to 30000."),
    script: [
      ...UNKNOWN_EVENTS.map((event) => entry(`${event.phase} #${event.sequence}`, event)),
      act("the bridge loads the authoritative unknown outcome", { kind: "snapshot_loaded", snapshot: UNKNOWN_EXPECTED }),
    ],
    expected: UNKNOWN_EXPECTED,
    expect: {
      state: "reconciling",
      settles: true,
      peakConcurrent: 2,
      queueReasons: [],
      unknownSteps: [STEP.writeB.stepId],
      openDecisions: 0,
    },
  },
  {
    id: "unknown-reconciled",
    title: "Reconciliation happened, the log does not say how it ended",
    summary: "A reconciled phase is applied as an observation, and the view asks for the snapshot instead of guessing success or failure.",
    seed: seed(RECONCILE_OP, "In src/config.ts, change the timeout to 30000."),
    script: [
      ...UNKNOWN_EVENTS.map((event) => entry(`${event.phase} #${event.sequence}`, { ...event, operationId: RECONCILE_OP })),
      act("the bridge loads the authoritative unknown outcome before reconciliation", {
        kind: "snapshot_loaded",
        snapshot: { ...UNKNOWN_EXPECTED, operationId: RECONCILE_OP },
      }),
      ...RECONCILE_EVENTS.slice(UNKNOWN_EVENTS.length).map((event) => entry(`${event.phase} #${event.sequence}`, event)),
    ],
    expected: RECONCILE_EXPECTED,
    reload: RECONCILE_EXPECTED,
    expect: {
      state: "reconciling",
      settles: false,
      resync: { need: "snapshot", reason: "reconciled_unknown", afterSequence: 9 },
      peakConcurrent: 2,
      queueReasons: [],
      unknownSteps: [STEP.writeB.stepId],
      openDecisions: 0,
      errorCode: "unknown_outcome",
    },
  },
  {
    id: "worker-silence",
    title: "The worker stopped reporting",
    summary: "A worker that goes quiet is not a finished operation: both calls stay running and the record is unchanged.",
    seed: seed(SILENT_OP, "Read both config files and report the timeout."),
    script: [
      ...SILENT_EVENTS.map((event) => entry(`${event.phase} #${event.sequence}`, event)),
      act("the bridge reloads the still-running record", { kind: "snapshot_loaded", snapshot: SILENT_EXPECTED }),
    ],
    expected: SILENT_EXPECTED,
    expect: {
      state: "running",
      settles: true,
      peakConcurrent: 2,
      queueReasons: [],
      unknownSteps: [],
      openDecisions: 0,
    },
  },
  {
    id: "reconnect",
    title: "A reconnected stream with one event missing",
    summary: "The gap is held, the cursor is requested, and the repaired log lands on the same record as an uninterrupted replay.",
    seed: seed(PARALLEL_OP, "In src/config.ts, change the timeout to 30000."),
    script: [
      ...PARALLEL_EVENTS.slice(0, 3).map((event) => entry(`${event.phase} #${event.sequence}`, event)),
      ...PARALLEL_EVENTS.slice(4).map((event) => entry(`${event.phase} #${event.sequence} (after the gap)`, event)),
    ],
    repair: [PARALLEL_EVENTS[3]],
    expected: PARALLEL_EXPECTED,
    reload: PARALLEL_EXPECTED,
    expect: {
      state: "accepted",
      settles: false,
      resync: { need: "events_after", reason: "gap", afterSequence: 3 },
      peakConcurrent: 2,
      queueReasons: ["read lane: 2 of 4 slots free"],
      unknownSteps: [],
      openDecisions: 0,
    },
  },
];

export function scenarioById(id: string): OperationScenario {
  const found = SCENARIOS.find((scenario) => scenario.id === id);
  if (!found) throw new Error(`no operation scenario named ${id}`);
  return found;
}

/** Apply one script entry: a durable event, or an action only a person can take. */
export function applyScriptEntry(view: OperationView, scripted: FixtureScriptEntry): OperationView {
  if ("event" in scripted) return applyOperationEvent(view, scripted.event);
  if (scripted.action.kind === "cancel_requested") return markCancelRequested(view, scripted.action.at);
  if (scripted.action.kind === "snapshot_loaded") return applyOperationSnapshot(view, scripted.action.snapshot);
  return markDecisionPresented(view, scripted.action.decision, scripted.action.decision.createdAt + 1);
}

/** Open a scenario: load its seed record, then replay the first `upto` script entries. */
export function openScenario(scenario: OperationScenario, upto = scenario.script.length): OperationView {
  let view = applyOperationSnapshot(createOperationView(scenario.expected.operationId), scenario.seed);
  for (const scripted of scenario.script.slice(0, upto)) view = applyScriptEntry(view, scripted);
  return view;
}

/** Answer a pending `events_after` request with the scenario's repair batch. */
export function applyScenarioRepair(view: OperationView, scenario: OperationScenario): OperationView {
  return applyOperationEvents(view, scenario.repair ?? []);
}
