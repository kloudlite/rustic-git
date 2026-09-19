/**
 * O09 — the operation view model.
 *
 * The renderer shows an operation as a projection of its durable log (design §10:
 * "UI state is a projection of the operation log"). This module freezes what a
 * projection may hold:
 *
 * - `OperationView` is what the renderer knows, as a JSON-safe record, so a view can
 *   be replayed, compared against the durable snapshot, or persisted by a test.
 * - `STEP_PHASE` / `OPERATION_PHASE_EDGES` state which durable phase means which
 *   transition. They are *semantics*, not a second transition table: O01's frozen
 *   `STEP_TRANSITIONS` / `OPERATION_TRANSITIONS` stay the authority, and
 *   `bench/test/operations-ui.test.ts` proves every edge declared here is legal there.
 * - Not in a view, on purpose: percentages, estimated durations, step states the log
 *   never reported, and any grant record. See `DecisionPresentation` and
 *   `../bridge.ts` — the renderer asks the trusted bridge to record a decision.
 *
 * Three things the frozen event envelope does not carry, so they come from the durable
 * snapshot (`OperationSnapshot`) or the trusted bridge, never from a guess:
 * workspace/tree scope and per-step targets, concrete approval content, and usage
 * counts. `eventDerivedProjection` names exactly what the log alone supports.
 */
import type {
  CapabilityEffect,
  CompactOperationResult,
  DecisionClass,
  OperationBudgets,
  OperationError,
  OperationEvent,
  OperationEventModel,
  OperationEventPhase,
  OperationScope,
  OperationSnapshot,
  OperationState,
  OperationTransitionTrigger,
  OperateRequest,
  PendingDecision,
  StepState,
  StepTransitionTrigger,
  UnknownOutcome,
} from "../../../bench/src/operations/contracts.ts";

/** Version of this projection. A view states which projection produced it. */
export const OPERATION_VIEW_VERSION = "v1";

/** Operation states with nothing further to happen. Pinned to O01's list by the test. */
export const TERMINAL_OPERATION_STATES = [
  "completed",
  "failed",
  "cancelled",
  "expired",
  "partial",
] as const satisfies readonly OperationState[];

/** Steps still owing work; no terminal operation may carry one. Pinned to O01's list. */
export const PENDING_STEP_STATES = [
  "queued",
  "awaiting_approval",
  "running",
  "outcome_unknown",
] as const satisfies readonly StepState[];

export function isTerminalOperation(state: OperationState | undefined): boolean {
  return state !== undefined && (TERMINAL_OPERATION_STATES as readonly string[]).includes(state);
}

export function isPendingStep(state: StepState): boolean {
  return (PENDING_STEP_STATES as readonly string[]).includes(state);
}

// ---------------------------------------------------------------------------
// What each durable phase means
// ---------------------------------------------------------------------------

export type StepPhaseEdge = { from: StepState; to: StepState; trigger: StepTransitionTrigger };
export type OperationPhaseEdge = { from: OperationState; to: OperationState; trigger: OperationTransitionTrigger };

export type StepPhaseEffect = {
  /** The state a step is created in when this phase is the first thing seen for it. */
  initial?: StepState;
  /** The transition this phase means, for each state it may be applied from. */
  edges?: readonly StepPhaseEdge[];
  /** The phase carries only redacted detail (model, timing, evidence) for this step. */
  observe?: true;
  /**
   * The log reports that an uncertain outcome was reconciled, but a phase cannot say to
   * what: `outcome_unknown` leaves through `reconcile_conclusive` to either `succeeded`
   * or `failed`. The view marks the observation and reloads the snapshot.
   */
  reconcile?: true;
};

/**
 * Phase → step meaning. `queued` is the only creation-from-nothing state O01 allows a
 * step to appear in without a transition, so it carries `initial` and no edges.
 */
export const STEP_PHASE: Record<OperationEventPhase, StepPhaseEffect> = {
  accepted: { observe: true },
  resolving: { observe: true },
  resolved: { observe: true },
  approval_required: {
    initial: "awaiting_approval",
    edges: [{ from: "queued", to: "awaiting_approval", trigger: "approval_required" }],
  },
  decision_recorded: {
    edges: [{ from: "awaiting_approval", to: "running", trigger: "approval_recorded" }],
  },
  queued: { initial: "queued" },
  dispatched: {
    edges: [
      { from: "queued", to: "running", trigger: "dispatch_started" },
      { from: "failed", to: "running", trigger: "retry_allowed" },
    ],
  },
  progress: { observe: true },
  succeeded: { edges: [{ from: "running", to: "succeeded", trigger: "result_observed" }] },
  failed: { edges: [{ from: "running", to: "failed", trigger: "result_failed" }] },
  unknown_outcome: {
    edges: [{ from: "running", to: "outcome_unknown", trigger: "outcome_unknown" }],
  },
  reconciled: { reconcile: true },
  needs_input: { observe: true },
  completed: { observe: true },
  partial: { observe: true },
  cancelled: {
    edges: [
      { from: "queued", to: "cancelled", trigger: "cancel_requested" },
      { from: "awaiting_approval", to: "cancelled", trigger: "cancel_requested" },
      { from: "running", to: "cancelled", trigger: "cancel_confirmed" },
    ],
  },
  expired: { observe: true },
};

export const STEP_REPAIR_ONLY_EDGES: readonly StepPhaseEdge[] = [
  { from: "awaiting_approval", to: "skipped", trigger: "approval_denied" },
  { from: "outcome_unknown", to: "succeeded", trigger: "reconcile_conclusive" },
  { from: "outcome_unknown", to: "failed", trigger: "reconcile_conclusive" },
];

/** The first phase of an operation, for a view that has not loaded a snapshot yet. */
export const OPERATION_PHASE_INITIAL: Partial<Record<OperationEventPhase, OperationState>> = {
  accepted: "accepted",
};

/**
 * Phase → operation edges. `resolved`, `progress`, `queued` and `succeeded` describe
 * steps and carry no operation state change; `reconciled` is ambiguous by construction
 * (see `STEP_PHASE.reconcile`) and is handled as an observation plus a snapshot reload.
 */
export const OPERATION_PHASE_EDGES: Partial<Record<OperationEventPhase, readonly OperationPhaseEdge[]>> = {
  resolving: [
    { from: "accepted", to: "resolving", trigger: "resolve_started" },
    { from: "awaiting_approval", to: "resolving", trigger: "resolve_started" },
    { from: "needs_input", to: "resolving", trigger: "resolve_started" },
  ],
  approval_required: [
    { from: "accepted", to: "awaiting_approval", trigger: "approval_required" },
    { from: "resolving", to: "awaiting_approval", trigger: "approval_required" },
    { from: "running", to: "awaiting_approval", trigger: "approval_required" },
    { from: "needs_input", to: "awaiting_approval", trigger: "approval_required" },
  ],
  decision_recorded: [
    { from: "awaiting_approval", to: "running", trigger: "approval_recorded" },
    { from: "needs_input", to: "running", trigger: "decision_supplied" },
  ],
  dispatched: [
    { from: "accepted", to: "running", trigger: "dispatch_started" },
    { from: "resolving", to: "running", trigger: "dispatch_started" },
    { from: "reconciling", to: "running", trigger: "dispatch_started" },
  ],
  failed: [
    { from: "accepted", to: "failed", trigger: "effects_failed" },
    { from: "resolving", to: "failed", trigger: "effects_failed" },
    { from: "awaiting_approval", to: "failed", trigger: "effects_failed" },
    { from: "running", to: "failed", trigger: "effects_failed" },
    { from: "needs_input", to: "failed", trigger: "effects_failed" },
    { from: "cancel_requested", to: "failed", trigger: "effects_failed" },
    { from: "reconciling", to: "failed", trigger: "reconcile_conclusive" },
  ],
  unknown_outcome: [
    { from: "resolving", to: "reconciling", trigger: "unknown_outcome" },
    { from: "running", to: "reconciling", trigger: "unknown_outcome" },
    { from: "cancel_requested", to: "reconciling", trigger: "unknown_outcome" },
  ],
  needs_input: [
    { from: "accepted", to: "needs_input", trigger: "decision_required" },
    { from: "resolving", to: "needs_input", trigger: "decision_required" },
    { from: "awaiting_approval", to: "needs_input", trigger: "decision_required" },
    { from: "running", to: "needs_input", trigger: "decision_required" },
    { from: "reconciling", to: "needs_input", trigger: "decision_required" },
  ],
  completed: [
    { from: "running", to: "completed", trigger: "effects_committed" },
    { from: "reconciling", to: "completed", trigger: "reconcile_conclusive" },
  ],
  partial: [
    { from: "running", to: "partial", trigger: "effects_partial" },
    { from: "needs_input", to: "partial", trigger: "effects_partial" },
    { from: "reconciling", to: "partial", trigger: "effects_partial" },
    { from: "cancel_requested", to: "partial", trigger: "effects_partial" },
  ],
  cancelled: [{ from: "cancel_requested", to: "cancelled", trigger: "cancel_settled" }],
  expired: [
    { from: "accepted", to: "expired", trigger: "deadline_reached" },
    { from: "resolving", to: "expired", trigger: "deadline_reached" },
    { from: "awaiting_approval", to: "expired", trigger: "deadline_reached" },
    { from: "running", to: "expired", trigger: "deadline_reached" },
    { from: "needs_input", to: "expired", trigger: "deadline_reached" },
  ],
};

/** Step-scoped phases that do not carry an operation-level conclusion. */
export const OPERATION_OBSERVATION_PHASES: readonly OperationEventPhase[] = [
  "resolved",
  "queued",
  "progress",
  "succeeded",
  "failed",
  "cancelled",
];

/**
 * The person's own cancel intent. O01 has no phase for a cancellation that is requested
 * but not settled, so the view takes the transition the frozen table defines for
 * `cancel_requested` and labels it "cancelling — not yet confirmed" until the durable
 * `cancelled` (or `partial`) phase arrives. A view never shows a cancellation it only
 * asked for as if it had happened.
 */
export const CANCEL_REQUEST_EDGES: readonly OperationPhaseEdge[] = [
  { from: "accepted", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "resolving", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "awaiting_approval", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "running", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "needs_input", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "reconciling", to: "cancel_requested", trigger: "cancel_requested" },
];

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

/**
 * One step, as the view knows it. Event-derived fields come from the durable log;
 * `targetRef`, `effect`, `capabilityVersion`, `resourceKeys` and `backendOperationId`
 * are snapshot-only, because the event envelope carries none of them.
 */
export type OperationStepView = {
  stepId: string;
  capability: string;
  state: StepState;
  targetRef?: string;
  capabilityVersion?: string;
  key?: string;
  effect?: CapabilityEffect;
  resourceKeys?: string[];
  dependencies: string[];
  /** Dispatch observations: a retry after a declared failure counts again. */
  attempts: number;
  retryCount?: number;
  queuedReason?: string;
  startedAt?: number;
  endedAt?: number;
  backendOperationId?: string;
  idempotencyKey?: string;
  evidenceRefs: string[];
  error?: OperationError;
  /** The latest event about this step, and what it carried. */
  lastSequence?: number;
  lastAt?: number;
  observedEvents: number;
  model?: OperationEventModel;
  summary?: string;
  decisionCode?: string;
  argPreview?: string;
  argDigest?: string;
  /** Elapsed milliseconds the durable log reported; never computed from a guess. */
  elapsedMs?: number;
  /** When the outcome became uncertain. */
  unknownSince?: number;
  /** A reconciliation was reported; only a snapshot says to what. */
  reconciledAt?: number;
};

export type OperationUsage = {
  steps: number;
  selectionRounds: number;
  generationCalls: number;
  attempts: number;
};

export type ConnectionStatus =
  /** No durable record loaded yet: the view knows nothing and shows nothing as fact. */
  | "waiting_snapshot"
  /** Catching up: a resync was requested and arriving events are held, not guessed at. */
  | "repairing"
  | "live"
  | "disconnected";

export type ResyncNeed = "snapshot" | "events_after";

export type ResyncReason =
  | "missing_snapshot"
  | "gap"
  | "unexpected_transition"
  | "missing_evidence"
  | "reconnect"
  | "reconciled_unknown"
  | "buffer_overflow"
  | "wrong_operation"
  | "invalid_snapshot"
  | "invalid_event"
  | "revision_regression"
  | "conflicting_duplicate"
  | "duplicate_identity_evicted"
  | "decision_outcome_unreported"
  | "illegal_retry";

/**
 * What the view asks the bridge for. `afterSequence` is the last sequence it applied, so
 * an `events_after` repair is a cursor fetch, never a fresh read of everything.
 */
export type ResyncRequest = {
  operationId: string;
  afterSequence: number;
  need: ResyncNeed;
  reason: ResyncReason;
  at: number;
};

export type OperationNoticeCode =
  | "cross_operation"
  | "stale_snapshot"
  | "unexpected_transition"
  | "evidence_missing"
  | "stream_gap"
  | "loading"
  | "reconcile_unreported"
  | "buffer_overflow"
  | "cancel_unavailable";

export type OperationNotice = {
  code: OperationNoticeCode;
  message: string;
  at: number;
  sequence?: number;
};

/**
 * Concrete decision content. Written by the trusted decision bridge (O08) or copied from
 * a durable snapshot's `PendingDecision`; the renderer never mints one, and nothing here
 * is a grant. `question` and `argPreview` are text a model may have influenced: they are
 * shown as content, and `stateLabel`/`decisionsOf` never read them as authorization.
 */
export type DecisionPresentation = {
  decisionId: string;
  operationId: string;
  stepId: string;
  decisionClass: DecisionClass;
  question: string;
  createdAt: number;
  expiresAt: number;
  /** The operation revision the decision was raised at; what an answer must cite. */
  revision: number;
  capability?: string;
  targetRef?: string;
  effect?: CapabilityEffect;
  argDigest?: string;
  argPreview?: string;
  dependencies?: string[];
  /** The bridge's own answer: false once the decision expired, was answered, or was superseded. */
  answerable: boolean;
  /** Why it cannot be answered now, in words a person reads. */
  unavailable?: string;
};

export type OperationView = {
  version: typeof OPERATION_VIEW_VERSION;
  operationId: string;
  status: ConnectionStatus;
  /** Absent until the durable record arrives: a view does not guess a state. */
  state?: OperationState;
  revision: number;
  lastSequence: number;
  createdAt?: number;
  updatedAt?: number;
  deadlineAt?: number;
  /** Authenticated owner identity from the durable record; owner-only controls use it. */
  actor?: OperationSnapshot["actor"];
  scope: OperationScope;
  request?: OperateRequest;
  budgets?: OperationBudgets;
  steps: OperationStepView[];
  /** From the durable snapshot only; the event envelope carries no decision content. */
  pendingDecisions: PendingDecision[];
  /** Snapshot-only counts. Never estimated, never counted twice. */
  usage?: OperationUsage;
  /** Snapshot-only: the compact result the main model also received. */
  result?: CompactOperationResult;
  unknownOutcomes: UnknownOutcome[];
  model?: OperationEventModel;
  /** Latest elapsed the durable log reported for the operation. */
  elapsedMs?: number;
  queueReason?: string;
  /** Applied events, newest last. A refused event is held, not applied. */
  events: OperationEvent[];
  /** Canonical identity for every applied sequence, independent of the display tail. */
  eventFingerprints: Record<number, string>;
  /** Held while a repair is pending, so a missed gap is never papered over. */
  held: OperationEvent[];
  /** Held events that had to be dropped: the buffer is bounded on purpose. */
  heldDropped: boolean;
  /** Concrete decisions the bridge presented. */
  decisions: DecisionPresentation[];
  resync?: ResyncRequest;
  notice?: OperationNotice;
  /** Set by the person's own cancel click; the durable state may still be `running`. */
  controlIntent?: { cancelRequestedAt?: number };
  /** A reconcile was reported and the conclusion is not in the log. */
  reconcileObservedAt?: number;
};

// ---------------------------------------------------------------------------
// Projections of the log alone
// ---------------------------------------------------------------------------

export type EventDerivedStep = {
  stepId: string;
  state: StepState;
  capability: string;
  dependencies: string[];
  attempts: number;
  queuedReason?: string;
  startedAt?: number;
  endedAt?: number;
  evidenceRefs: string[];
  error?: OperationError;
};

export type EventDerivedProjection = {
  state?: OperationState;
  revision: number;
  lastSequence: number;
  steps: EventDerivedStep[];
  unknownOutcomes: UnknownOutcome[];
};

/** Uncertain outcomes, read off the steps that are still in `outcome_unknown`. */
export function unknownOutcomesOf(view: OperationView): UnknownOutcome[] {
  if (view.unknownOutcomes.length) return view.unknownOutcomes.map((outcome) => ({ ...outcome }));
  return view.steps
    .filter((step) => step.state === "outcome_unknown")
    .map((step) => ({
      stepId: step.stepId,
      capability: step.capability,
      dispatchDigest: step.argDigest,
      since: step.unknownSince ?? step.lastAt ?? 0,
    }));
}

/**
 * Exactly what the durable log supports, so a fixture can compare a replayed view with
 * the snapshot the store wrote after the same events. Fields the envelope does not carry
 * (scope, target, effect, usage, result, decision content) are deliberately absent.
 */
export function eventDerivedProjection(view: OperationView): EventDerivedProjection {
  return {
    state: view.state,
    revision: view.revision,
    lastSequence: view.lastSequence,
    steps: view.steps.map((step) => ({
      stepId: step.stepId,
      state: step.state,
      capability: step.capability,
      dependencies: [...step.dependencies],
      attempts: step.attempts,
      queuedReason: step.queuedReason,
      startedAt: step.startedAt,
      endedAt: step.endedAt,
      evidenceRefs: [...step.evidenceRefs],
      error: step.error,
    })),
    unknownOutcomes: unknownOutcomesOf(view),
  };
}

export function stepById(view: OperationView, stepId: string): OperationStepView | undefined {
  return view.steps.find((step) => step.stepId === stepId);
}

export function stepsInState(view: OperationView, state: StepState): OperationStepView[] {
  return view.steps.filter((step) => step.state === state);
}

/** Running steps, in the order the log reported them. */
export function runningSteps(view: OperationView): OperationStepView[] {
  return stepsInState(view, "running");
}

/** How many internal calls are in flight, and what is waiting. Counts, never a percentage. */
export function runCounts(view: OperationView): {
  running: number;
  queued: number;
  awaitingApproval: number;
  unknown: number;
  settled: number;
  total: number;
} {
  const counts = { running: 0, queued: 0, awaitingApproval: 0, unknown: 0, settled: 0, total: view.steps.length };
  for (const step of view.steps) {
    if (step.state === "running") counts.running += 1;
    else if (step.state === "queued") counts.queued += 1;
    else if (step.state === "awaiting_approval") counts.awaitingApproval += 1;
    else if (step.state === "outcome_unknown") counts.unknown += 1;
    else counts.settled += 1;
  }
  return counts;
}

/** Steps whose observed run overlaps this one: the "concurrent calls" a person is asking about. */
export function concurrentSteps(view: OperationView, step: OperationStepView): OperationStepView[] {
  if (step.startedAt === undefined) return [];
  const start = step.startedAt;
  const end = step.endedAt ?? Number.MAX_SAFE_INTEGER;
  return view.steps.filter((other) => {
    if (other.stepId === step.stepId || other.startedAt === undefined) return false;
    const otherEnd = other.endedAt ?? Number.MAX_SAFE_INTEGER;
    return other.startedAt < end && start < otherEnd;
  });
}

/** The instruction (or exact objective) the operation was accepted for. */
export function requestLabel(view: OperationView): string | undefined {
  const request = view.request;
  if (!request) return undefined;
  if ("instruction" in request) return request.instruction;
  if ("action" in request && request.action === "exact") return request.request.objective;
  return undefined;
}
