/**
 * O09 — the operation event reducer.
 *
 * One pure fold: durable snapshot + durable events in, `OperationView` out. The rules it
 * exists to keep:
 *
 * - **Idempotent by sequence.** An event whose sequence the view already applied changes
 *   nothing and returns the *same object*, so replaying a log twice is invisible.
 * - **A gap is repaired, never papered over.** A missing sequence puts the view in
 *   `repairing` and it asks the bridge for events after the cursor it actually applied.
 *   Events arriving meanwhile are held; the bridge answers by replaying, or by handing
 *   over a snapshot when history was compacted.
 * - **No invented success.** A step only reaches a state along an edge O01's frozen table
 *   allows. A phase that contradicts what the view knows (or that would settle an
 *   operation while work is still owed) is refused: the view keeps its state, holds the
 *   event, and reloads the snapshot. A worker that stops reporting is not a completion:
 *   every step state comes from a durable phase, and silence leaves it `running`.
 * - **Outcomes need their evidence.** O01's `cancel_confirmed` means evidence that no
 *   effect was applied, and a capability's declared success evidence is what makes a
 *   `succeeded` phase a fact rather than a claim. Both are refused without evidence, and
 *   the durable record is reloaded instead — the same way a percentage would have been a
 *   guess.
 * - **Uncertainty stays visible.** An `outcome_unknown` step is never retried, settled or
 *   hidden; a reconciliation marks the observation and reloads the snapshot, because no
 *   phase says whether the concluded outcome was success or failure.
 * - **The renderer never mints authorization** (`../bridge.ts`), and text from the log is
 *   content, not a control.
 */
import type {
  OperationEvent,
  OperationSnapshot,
  OperationState,
  PendingDecision,
  StepRecord,
  StepState,
} from "../../../bench/src/operations/contracts.ts";
import { validateOperationEvent, validateOperationSnapshot } from "../../../bench/src/operations/contracts.ts";
import {
  CANCEL_REQUEST_EDGES,
  OPERATION_PHASE_EDGES,
  OPERATION_PHASE_INITIAL,
  OPERATION_OBSERVATION_PHASES,
  OPERATION_VIEW_VERSION,
  PENDING_STEP_STATES,
  STEP_PHASE,
  isTerminalOperation,
  type DecisionPresentation,
  type OperationNotice,
  type OperationNoticeCode,
  type OperationStepView,
  type OperationView,
  type ResyncNeed,
  type ResyncReason,
  type ResyncRequest,
} from "./types.ts";

/** How many applied events a view keeps for display. */
export const EVENT_TAIL = 200;

/** Applied identities retained exactly beyond the display tail without unbounded copies. */
export const EVENT_FINGERPRINT_TAIL = 256;

/** How many events may be held while a repair is pending before the oldest are dropped. */
export const HELD_EVENT_LIMIT = 256;

/**
 * Phases that only the operation's own event may settle it with. A step-scoped `failed`
 * means that call failed — which is exactly how an operation becomes `partial` — and a
 * step-scoped `cancelled` is a step outcome, not the operation's. O05 emits the
 * operation's terminal phase without a `stepId`; the view refuses to read one from a step.
 */
const OPERATION_TERMINAL_PHASES: readonly OperationEvent["phase"][] = ["completed", "partial", "failed", "cancelled", "expired"];

function request(view: OperationView, need: ResyncNeed, reason: ResyncReason, at: number): ResyncRequest {
  return { operationId: view.operationId, afterSequence: view.lastSequence, need, reason, at };
}

/**
 * A fresh view for one operation. It knows nothing yet, and says so: `status` is
 * `waiting_snapshot` and it asks for the durable record before it will show a state.
 */
export function createOperationView(operationId: string, at = 0): OperationView {
  return {
    version: OPERATION_VIEW_VERSION,
    operationId,
    status: "waiting_snapshot",
    revision: 0,
    lastSequence: 0,
    scope: {},
    steps: [],
    pendingDecisions: [],
    unknownOutcomes: [],
    events: [],
    eventFingerprints: {},
    held: [],
    heldDropped: false,
    decisions: [],
    resync: { operationId, afterSequence: 0, need: "snapshot", reason: "missing_snapshot", at },
  };
}

/**
 * Replace the projection with the durable record. The snapshot is the authority for
 * everything the log cannot say (scope, targets, effects, usage, decisions, result), so
 * it also clears a pending repair and then drains whatever was held.
 *
 * A snapshot for another operation, or one older than what this view already applied, is
 * refused: a late reply must never rewind a view that got further.
 */
export function applyOperationSnapshot(view: OperationView, snapshot: OperationSnapshot): OperationView {
  const checked = validateOperationSnapshot(snapshot);
  if (!checked.ok) return snapshotRepair(view, "invalid_snapshot", snapshot.updatedAt ?? 0);
  if (snapshot.operationId !== view.operationId) {
    return notice(view, {
      code: "cross_operation",
      message: `snapshot for ${snapshot.operationId} ignored: this view shows ${view.operationId}`,
      at: snapshot.updatedAt,
    });
  }
  if (snapshot.lastSequence < view.lastSequence) {
    return notice(view, {
      code: "stale_snapshot",
      message: `snapshot at sequence ${snapshot.lastSequence} is behind the applied sequence ${view.lastSequence}`,
      at: snapshot.updatedAt,
    });
  }
  if (snapshot.revision < view.revision) return snapshotRepair(view, "revision_regression", snapshot.updatedAt);
  const steps = snapshot.steps.map((record) => mergeStep(view, record));
  const decisions = withAnswerability(view.decisions, snapshot.pendingDecisions, snapshot.state);
  const loaded: OperationView = {
    ...view,
    status: view.status === "disconnected" ? "disconnected" : "live",
    state: snapshot.state,
    revision: snapshot.revision,
    lastSequence: snapshot.lastSequence,
    createdAt: snapshot.createdAt,
    updatedAt: snapshot.updatedAt,
    deadlineAt: snapshot.deadlineAt,
    actor: snapshot.actor,
    scope: snapshot.scope,
    request: snapshot.request,
    budgets: snapshot.budgets,
    steps,
    pendingDecisions: snapshot.pendingDecisions,
    usage: snapshot.usage,
    result: snapshot.result,
    unknownOutcomes: snapshot.unknownOutcomes.map((outcome) => ({ ...outcome })),
    decisions,
    notice: undefined,
  };
  for (const held of view.held) {
    if (held.sequence <= view.lastSequence) {
      const fingerprint = view.eventFingerprints[held.sequence];
      if (fingerprint === undefined) {
        return snapshotRepair({ ...loaded, held: view.held }, "duplicate_identity_evicted", held.at);
      }
      if (fingerprint !== canonical(held)) {
        return snapshotRepair({ ...loaded, held: view.held }, "conflicting_duplicate", held.at);
      }
    }
  }
  return drainHeld({ ...loaded, resync: undefined });
}

/**
 * Fold one event. Returns the same view object when the event is one it already applied:
 * idempotency is by sequence, not by trusting the caller to deduplicate.
 */
export function applyOperationEvent(view: OperationView, event: OperationEvent): OperationView {
  const checked = validateOperationEvent(event);
  if (!checked.ok) return snapshotRepair(view, "invalid_event", typeof event.at === "number" ? event.at : 0);
  if (event.operationId !== view.operationId) {
    return notice(view, {
      code: "cross_operation",
      message: `event for ${event.operationId} ignored: this view shows ${view.operationId}`,
      at: event.at,
      sequence: event.sequence,
    });
  }
  if (event.sequence <= view.lastSequence) {
    const applied = view.eventFingerprints[event.sequence];
    if (applied === undefined) return snapshotRepair(view, "duplicate_identity_evicted", event.at);
    return applied !== canonical(event) ? snapshotRepair(view, "conflicting_duplicate", event.at) : view;
  }
  if (event.revision < view.revision) return snapshotRepair(view, "revision_regression", event.at);
  if (view.resync) return holdEvent(view, event, repairOf(view.resync));
  if (view.status !== "live") {
    return view.status === "disconnected"
      ? holdEvent(view, event, { need: "events_after", reason: "reconnect", message: repairMessage("reconnect") })
      : holdEvent(view, event, { need: "snapshot", reason: "missing_snapshot", message: repairMessage("missing_snapshot") });
  }
  if (event.sequence > view.lastSequence + 1) {
    return holdEvent(view, event, { need: "events_after", reason: "gap", message: repairMessage("gap") });
  }
  return applyContiguous(view, event);
}

/**
 * Fold a batch. When a cursor repair is pending, the batch *is* the repair: it is applied
 * together with whatever was held, in sequence order. Otherwise events apply one by one.
 */
export function applyOperationEvents(view: OperationView, events: readonly OperationEvent[]): OperationView {
  if (view.resync?.need === "events_after" && events.length) {
    return drainHeld(view, events);
  }
  let next = view;
  for (const event of events) next = applyOperationEvent(next, event);
  return next;
}

/** The stream dropped. Nothing is assumed about the gap until the bridge answers. */
export function markDisconnected(view: OperationView, at: number): OperationView {
  return {
    ...view,
    status: view.status === "waiting_snapshot" ? "waiting_snapshot" : "disconnected",
    updatedAt: Math.max(view.updatedAt ?? 0, at),
    notice: {
      code: "stream_gap",
      message: "the event stream dropped: events are held and the cursor is repaired on reconnect",
      at,
    },
  };
}

/**
 * Reconnect asks for the cursor, never for a fresh operation: the durable log is the same
 * one, and `afterSequence` is the last sequence this view actually applied.
 */
export function markReconnected(view: OperationView, at: number): OperationView {
  const repair = view.resync ?? request(view, "events_after", "reconnect", at);
  return {
    ...view,
    status: "repairing",
    updatedAt: Math.max(view.updatedAt ?? 0, at),
    resync: repair,
  };
}

/**
 * The person asked to cancel. O01 has no phase for a request that has not settled, so the
 * view takes the table's `cancel_requested` transition and keeps showing that it is only a
 * request; the durable `cancelled`/`partial` phase is what ends the operation. No revision
 * is invented: this is not a durable fact.
 */
export function markCancelRequested(view: OperationView, at: number): OperationView {
  const from = view.state;
  if (from === undefined) {
    return notice(view, { code: "cancel_unavailable", message: "the operation record has not loaded yet", at });
  }
  if (view.controlIntent?.cancelRequestedAt !== undefined) return view;
  const edge = CANCEL_REQUEST_EDGES.find((candidate) => candidate.from === from);
  if (!edge) {
    return notice(view, {
      code: "cancel_unavailable",
      message: `a ${from} operation cannot be asked to cancel`,
      at,
    });
  }
  return {
    ...view,
    controlIntent: { ...view.controlIntent, cancelRequestedAt: at },
    notice: undefined,
  };
}

/** Present concrete decision content the trusted bridge handed over. */
export function markDecisionPresented(view: OperationView, decision: DecisionPresentation, now: number): OperationView {
  const pending = view.pendingDecisions.find((candidate) => candidate.decisionId === decision.decisionId);
  const mismatch =
    !pending ||
    pending.operationId !== decision.operationId ||
    pending.stepId !== decision.stepId ||
    pending.decisionClass !== decision.decisionClass ||
    pending.revision !== decision.revision ||
    pending.expiresAt !== decision.expiresAt;
  const presented = mismatch
    ? { ...decision, answerable: false, unavailable: "the presentation does not match the pending decision" }
    : decision.expiresAt <= now
      ? { ...decision, answerable: false, unavailable: "this decision has expired" }
    : decision;
  const others = view.decisions.filter((existing) => existing.decisionId !== decision.decisionId);
  return { ...view, decisions: [...others, presented] };
}

/** Forget a decision the bridge says is gone (answered elsewhere, or withdrawn). */
export function markDecisionWithdrawn(view: OperationView, decisionId: string, unavailable: string): OperationView {
  return {
    ...view,
    decisions: view.decisions.map((decision) =>
      decision.decisionId === decisionId ? { ...decision, answerable: false, unavailable } : decision,
    ),
  };
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

type Repair = { need: ResyncNeed; reason: ResyncReason; message: string };
type StepPlan = { ok: true; steps: OperationStepView[]; repair?: Repair } | { ok: false; repair: Repair };
type OperationPlan = { ok: true; state: OperationState; reconcileObservedAt?: number; repair?: Repair } | { ok: false; repair: Repair };

function notice(view: OperationView, issue: OperationNotice): OperationView {
  return { ...view, notice: issue };
}

function noticeCode(reason: ResyncReason): OperationNoticeCode {
  if (reason === "buffer_overflow") return "buffer_overflow";
  if (reason === "gap" || reason === "reconnect") return "stream_gap";
  if (reason === "missing_snapshot") return "loading";
  if (reason === "reconciled_unknown") return "reconcile_unreported";
  if (reason === "missing_evidence") return "evidence_missing";
  if (reason === "wrong_operation") return "cross_operation";
  return "unexpected_transition";
}

function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value !== null && typeof value === "object") {
    return `{${Object.keys(value as Record<string, unknown>)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonical((value as Record<string, unknown>)[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

function fingerprint(event: OperationEvent): string {
  return canonical(event);
}

function rememberFingerprint(view: OperationView, event: OperationEvent): Record<number, string> {
  const fingerprints = { ...view.eventFingerprints, [event.sequence]: fingerprint(event) };
  const evicted = event.sequence - EVENT_FINGERPRINT_TAIL;
  if (evicted > 0) delete fingerprints[evicted];
  return fingerprints;
}

function snapshotRepair(view: OperationView, reason: ResyncReason, at: number): OperationView {
  return {
    ...view,
    status: "repairing",
    resync: request(view, "snapshot", reason, at),
    notice: { code: "unexpected_transition", message: repairMessage(reason), at },
  };
}

function repairMessage(reason: ResyncReason): string {
  switch (reason) {
    case "gap":
      return "the log skipped events: the cursor was requested";
    case "reconnect":
      return "the stream reconnected: the cursor was requested";
    case "missing_snapshot":
      return "the durable operation record has not loaded yet";
    case "reconciled_unknown":
      return "a reconciliation was reported without saying how it ended";
    case "missing_evidence":
      return "the log reported an outcome without the evidence that makes it a fact";
    case "buffer_overflow":
      return "held events were dropped: the cursor was requested again";
    case "wrong_operation":
      return "the event belongs to another operation";
    case "unexpected_transition":
      return "the log disagrees with the state this view holds";
    case "invalid_snapshot":
      return "the operation snapshot failed contract validation";
    case "invalid_event":
      return "the operation event failed contract validation";
    case "revision_regression":
      return "the operation revision moved backwards";
    case "conflicting_duplicate":
      return "the same event sequence carried a different payload";
    case "duplicate_identity_evicted":
      return "the event sequence is older than the retained identity ledger";
    case "decision_outcome_unreported":
      return "the decision event does not carry enough outcome detail";
    case "illegal_retry":
      return "the retry lacks the metadata or durable failure needed to authorize it";
  }
}

function repairOf(pending: ResyncRequest): Repair {
  return { need: pending.need, reason: pending.reason, message: repairMessage(pending.reason) };
}

function holdEvent(view: OperationView, event: OperationEvent, repair: Repair): OperationView {
  const held = [...view.held, event];
  const dropped = held.length > HELD_EVENT_LIMIT;
  return {
    ...view,
    status: "repairing",
    held: dropped ? held.slice(held.length - HELD_EVENT_LIMIT) : held,
    heldDropped: view.heldDropped || dropped,
    resync:
      view.resync ??
      ({ operationId: view.operationId, afterSequence: view.lastSequence, need: repair.need, reason: repair.reason, at: event.at } as ResyncRequest),
    notice: { code: noticeCode(repair.reason), message: repair.message, at: event.at, sequence: event.sequence },
  };
}

function refused(view: OperationView, event: OperationEvent, repair: Repair): OperationView {
  return holdEvent(view, event, repair);
}

/**
 * Apply a contiguous event: plan the step effect and the operation effect, refuse the whole
 * event if either contradicts what the view knows, then commit both together.
 */
function applyContiguous(view: OperationView, event: OperationEvent): OperationView {
  const stepPlan: StepPlan = event.stepId ? planStep(view, event) : { ok: true, steps: view.steps };
  if (!stepPlan.ok) return refused(view, event, stepPlan.repair);
  if (event.phase === "dispatched" && isTerminalOperation(view.state)) {
    return refused(view, event, {
      need: "snapshot",
      reason: "illegal_retry",
      message: repairMessage("illegal_retry"),
    });
  }
  const operationPlan: OperationPlan = planOperation(view, stepPlan.steps, event);
  if (!operationPlan.ok) return refused(view, event, operationPlan.repair);

  let next: OperationView = {
    ...view,
    status: "live",
    state: operationPlan.state,
    revision: Math.max(view.revision, event.revision),
    lastSequence: event.sequence,
    updatedAt: Math.max(view.updatedAt ?? 0, event.at),
    steps: stepPlan.steps,
    model: event.model ?? view.model,
    elapsedMs: event.elapsedMs ?? view.elapsedMs,
    queueReason: event.queueReason ?? view.queueReason,
    events: [...view.events, event].slice(-EVENT_TAIL),
    eventFingerprints: rememberFingerprint(view, event),
    reconcileObservedAt: operationPlan.reconcileObservedAt ?? view.reconcileObservedAt,
  };
  const repair = stepPlan.repair ?? operationPlan.repair;
  if (isTerminalOperation(next.state)) {
    // A terminal operation carries no open work: O01 refuses a snapshot that does.
    next = {
      ...next,
      pendingDecisions: [],
      decisions: withAnswerability(next.decisions, [], next.state),
    };
  }
  if (repair) {
    next = {
      ...next,
      status: "repairing",
      resync: request(next, repair.need, repair.reason, event.at),
      notice: { code: noticeCode(repair.reason), message: repair.message, at: event.at, sequence: event.sequence },
    };
  }
  return next;
}

function planStep(view: OperationView, event: OperationEvent): StepPlan {
  const stepId = event.stepId!;
  if (event.phase === "decision_recorded" && event.decisionCode === "denied") {
    return {
      ok: false,
      repair: { need: "snapshot", reason: "decision_outcome_unreported", message: repairMessage("decision_outcome_unreported") },
    };
  }
  const effect = STEP_PHASE[event.phase];
  const existing = view.steps.find((step) => step.stepId === stepId);
  // O01 has no "skipped" event phase: a step that depended on one that failed is recorded as
  // an ordinary `progress` event carrying `decisionCode: "dependency_failed"`. Without this
  // branch a `progress` event is a pure observation (`STEP_PHASE.progress.observe`) and the step
  // would sit on "queued" — a step that will never run — until the next snapshot reload, which
  // is exactly the silent state the owner rule refuses.
  if (event.phase === "progress" && event.decisionCode === "dependency_failed") {
    if (!existing || existing.state !== "queued") {
      return {
        ok: false,
        repair: { need: "snapshot", reason: "unexpected_transition", message: `step ${stepId} was skipped from a state the view did not expect` },
      };
    }
    return { ok: true, steps: replace(view.steps, applyStepEdge(observeStep(existing, event), "skipped", event)) };
  }
  if (event.phase === "decision_recorded" && event.decisionCode !== "denied" && existing?.state === "queued") {
    return { ok: true, steps: replace(view.steps, observeStep(existing, event)) };
  }
  if (!existing) {
    const initial = effect.initial;
    if (!initial && event.phase === "dispatched") {
      return {
        ok: false,
        repair: { need: "snapshot", reason: "unexpected_transition", message: `step ${stepId} was dispatched before it was queued` },
      };
    }
    if (!initial) {
      return {
        ok: false,
        repair: {
          need: "snapshot",
          reason: "unexpected_transition",
          message: `a "${event.phase}" phase names step ${stepId} before the view saw it start`,
        },
      };
    }
    return { ok: true, steps: [...view.steps, newStep(event, initial)] };
  }
  const observed = observeStep(existing, event);
  if (event.phase === "dispatched" && existing.state === "running") {
    return { ok: false, repair: { need: "snapshot", reason: "illegal_retry", message: repairMessage("illegal_retry") } };
  }
  if (effect.reconcile) {
    return {
      ok: true,
      steps: replace(view.steps, { ...observed, reconciledAt: event.at }),
      repair: {
        need: "snapshot",
        reason: "reconciled_unknown",
        message: `step ${stepId} was reconciled: only the durable record says to what`,
      },
    };
  }
  if (!effect.edges) {
    if (effect.initial === existing.state) {
      return {
        ok: false,
        repair: { need: "snapshot", reason: "unexpected_transition", message: `a repeated "${event.phase}" phase is not a transition` },
      };
    }
    return { ok: true, steps: replace(view.steps, observed) };
  }
  const edge = effect.edges.find((candidate) => candidate.from === existing.state);
  if (!edge) {
    return {
      ok: false,
      repair: {
        need: "snapshot",
        reason: "unexpected_transition",
        message: `a "${event.phase}" phase cannot follow ${existing.state} for step ${stepId}`,
      },
    };
  }
  if (edge.trigger === "retry_allowed") {
    if (isTerminalOperation(view.state) || existing.error?.retryable !== true || event.retryCount !== existing.attempts) {
      return { ok: false, repair: { need: "snapshot", reason: "illegal_retry", message: repairMessage("illegal_retry") } };
    }
  }
  // O01 defines `running -> cancelled` as `cancel_confirmed`: evidence that no effect was
  // applied. Without it the phase is a claim, and the durable record is asked for instead.
  // The same holds for a success with none of the evidence its capability declares.
  if (edge.trigger === "cancel_confirmed" && !event.evidenceRefs?.length) {
    return {
      ok: false,
      repair: {
        need: "snapshot",
        reason: "missing_evidence",
        message: `step ${stepId} was reported cancelled without evidence that no effect applied`,
      },
    };
  }
  if (edge.to === "succeeded" && !event.evidenceRefs?.length) {
    return {
      ok: false,
      repair: {
        need: "snapshot",
        reason: "missing_evidence",
        message: `step ${stepId} was reported succeeded with no evidence`,
      },
    };
  }
  return { ok: true, steps: replace(view.steps, applyStepEdge(observed, edge.to, event)) };
}

function planOperation(view: OperationView, steps: readonly OperationStepView[], event: OperationEvent): OperationPlan {
  if (event.phase === "decision_recorded" && event.decisionCode !== "denied" && view.state === "needs_input") {
    return { ok: true, state: "running" };
  }
  const initial = OPERATION_PHASE_INITIAL[event.phase];
  if (initial !== undefined) {
    return view.state === undefined || (view.state === initial && view.lastSequence === 0)
      ? { ok: true, state: initial }
      : { ok: false, repair: { need: "snapshot", reason: "unexpected_transition", message: `a repeated "${event.phase}" phase is not a transition` } };
  }
  if (event.stepId !== undefined && OPERATION_OBSERVATION_PHASES.includes(event.phase)) {
    return { ok: true, state: view.state ?? "accepted" };
  }
  if (event.stepId !== undefined && event.phase === "dispatched" && view.state === "running") {
    return { ok: true, state: view.state };
  }
  if (event.phase === "reconciled") {
    return {
      ok: true,
      state: view.state ?? "reconciling",
      reconcileObservedAt: event.at,
      repair: {
        need: "snapshot",
        reason: "reconciled_unknown",
        message: "the log reports a reconciliation but not its conclusion",
      },
    };
  }
  const edges = OPERATION_PHASE_EDGES[event.phase];
  const current = view.state;
  if (!edges) return { ok: true, state: current ?? "accepted" };
  if (current === undefined) return { ok: true, state: edges[0].to };
  const edge = edges.find((candidate) => candidate.from === current);
  if (!edge) {
    return {
      ok: false,
      repair: {
        need: "snapshot",
        reason: "unexpected_transition",
        message: `a "${event.phase}" phase cannot follow ${current}`,
      },
    };
  }
  const issue = terminalIssue(edge.to, steps);
  if (issue) {
    return { ok: false, repair: { need: "snapshot", reason: "unexpected_transition", message: issue } };
  }
  if (isTerminalOperation(edge.to) && !event.evidenceRefs?.length) {
    return { ok: false, repair: { need: "snapshot", reason: "missing_evidence", message: `${edge.to} was reported without terminal evidence` } };
  }
  if (isTerminalOperation(edge.to)) {
    return { ok: false, repair: { need: "snapshot", reason: "unexpected_transition", message: "the event cannot prove the durable operation result" } };
  }
  return { ok: true, state: edge.to };
}

/**
 * The same truthfulness rule O01 enforces on a terminal snapshot, applied to a log that
 * claims one: no work still owed, and `completed` / `partial` must match their steps.
 * Handing over a snapshot instead of showing a half-truth is the point.
 */
function terminalIssue(state: OperationState, steps: readonly OperationStepView[]): string | undefined {
  if (!isTerminalOperation(state)) return undefined;
  const pending = steps.filter((step) => (PENDING_STEP_STATES as readonly StepState[]).includes(step.state));
  if (pending.length) {
    return `${state} reported while ${pending.map((step) => `${step.stepId} is ${step.state}`).join(", ")}`;
  }
  const states = steps.map((step) => step.state);
  if (state === "completed") {
    if (!states.includes("succeeded")) return "completed reported with no succeeded step";
    if (states.some((step) => step === "failed" || step === "cancelled")) {
      return "completed reported alongside a failed or cancelled step";
    }
  }
  if (state === "partial") {
    if (!states.includes("succeeded")) return "partial reported with no completed effect";
    if (!states.some((step) => step === "failed" || step === "skipped" || step === "cancelled")) {
      return "partial reported with nothing settled short of success";
    }
  }
  return undefined;
}

function newStep(event: OperationEvent, state: StepState): OperationStepView {
  return {
    stepId: event.stepId!,
    capability: event.capability ?? "",
    state,
    dependencies: event.dependencies ? [...event.dependencies] : [],
    attempts: event.phase === "dispatched" ? 1 : 0,
    retryCount: event.retryCount,
    queuedReason: event.queueReason,
    startedAt: event.phase === "dispatched" ? event.at : undefined,
    evidenceRefs: event.evidenceRefs ? [...event.evidenceRefs] : [],
    lastSequence: event.sequence,
    lastAt: event.at,
    observedEvents: 1,
    model: event.model,
    summary: event.summary,
    decisionCode: event.decisionCode,
    argPreview: event.argPreview,
    argDigest: event.argDigest,
    elapsedMs: event.elapsedMs,
    unknownSince: state === "outcome_unknown" ? event.at : undefined,
  };
}

/** Everything a step event adds: redacted detail, timing, evidence, and attempts. */
function observeStep(step: OperationStepView, event: OperationEvent): OperationStepView {
  return {
    ...step,
    capability: event.capability ?? step.capability,
    dependencies: event.dependencies ? [...event.dependencies] : step.dependencies,
    attempts: event.phase === "dispatched" ? step.attempts + 1 : step.attempts,
    retryCount: event.retryCount ?? step.retryCount,
    queuedReason: event.queueReason ?? step.queuedReason,
    evidenceRefs: mergeEvidence(step.evidenceRefs, event.evidenceRefs),
    lastSequence: event.sequence,
    lastAt: event.at,
    observedEvents: step.observedEvents + 1,
    model: event.model ?? step.model,
    summary: event.summary ?? step.summary,
    decisionCode: event.decisionCode ?? step.decisionCode,
    argPreview: event.argPreview ?? step.argPreview,
    argDigest: event.argDigest ?? step.argDigest,
    elapsedMs: event.elapsedMs ?? step.elapsedMs,
  };
}

function applyStepEdge(step: OperationStepView, to: StepState, event: OperationEvent): OperationStepView {
  const next: OperationStepView = { ...step, state: to };
  if (to === "running") {
    next.startedAt = event.at;
    next.endedAt = undefined;
    next.unknownSince = undefined;
    next.reconciledAt = undefined;
  }
  if (to === "succeeded" || to === "failed" || to === "skipped" || to === "cancelled") {
    next.endedAt = event.at;
  }
  if (to === "outcome_unknown") {
    next.unknownSince = event.at;
  }
  return next;
}

function mergeEvidence(previous: readonly string[], added: readonly string[] | undefined): string[] {
  if (!added?.length) return [...previous];
  const merged = [...previous];
  for (const ref of added) if (!merged.includes(ref)) merged.push(ref);
  return merged;
}

function replace(steps: readonly OperationStepView[], step: OperationStepView): OperationStepView[] {
  return steps.map((candidate) => (candidate.stepId === step.stepId ? step : candidate));
}

/** Snapshot step record + whatever the log already told this view about the step. */
function mergeStep(view: OperationView, record: StepRecord): OperationStepView {
  const known = view.steps.find((step) => step.stepId === record.stepId);
  return {
    stepId: record.stepId,
    capability: record.capability,
    state: record.state,
    targetRef: record.targetRef,
    capabilityVersion: record.capabilityVersion,
    key: record.key,
    effect: record.effect,
    resourceKeys: record.resourceKeys ? [...record.resourceKeys] : undefined,
    dependencies: record.dependencies ? [...record.dependencies] : [],
    attempts: record.attempts,
    retryCount: known?.retryCount,
    queuedReason: record.queuedReason,
    startedAt: record.startedAt,
    endedAt: record.endedAt,
    backendOperationId: record.backendOperationId,
    idempotencyKey: record.idempotencyKey,
    evidenceRefs: record.evidenceRefs ? [...record.evidenceRefs] : [],
    error: record.error,
    lastSequence: known?.lastSequence,
    lastAt: known?.lastAt,
    observedEvents: known?.observedEvents ?? 0,
    model: known?.model,
    summary: known?.summary,
    decisionCode: known?.decisionCode,
    argPreview: known?.argPreview,
    argDigest: known?.argDigest,
    elapsedMs: known?.elapsedMs,
    unknownSince: record.state === "outcome_unknown" ? known?.unknownSince : undefined,
    reconciledAt: record.state === "outcome_unknown" ? known?.reconciledAt : undefined,
  };
}

/** A decision stops being answerable when its operation settles or it leaves the pending set. */
function withAnswerability(
  decisions: readonly DecisionPresentation[],
  pending: readonly PendingDecision[],
  state: OperationState | undefined,
): DecisionPresentation[] {
  const open = new Set(pending.map((decision) => decision.decisionId));
  return decisions.map((decision) => {
    if (isTerminalOperation(state)) {
      return { ...decision, answerable: false, unavailable: "the operation has finished" };
    }
    if (!open.has(decision.decisionId)) {
      return { ...decision, answerable: false, unavailable: "this decision is no longer open" };
    }
    return decision;
  });
}

function sortUniqueBySequence(events: readonly OperationEvent[]): { events: OperationEvent[]; conflict?: OperationEvent } {
  const seen = new Map<number, string>();
  const unique: OperationEvent[] = [];
  for (const event of events) {
    const fingerprint = canonical(event);
    const previous = seen.get(event.sequence);
    if (previous !== undefined) {
      if (previous !== fingerprint) return { events: unique, conflict: event };
      continue;
    }
    seen.set(event.sequence, fingerprint);
    unique.push(event);
  }
  return { events: unique.sort((a, b) => a.sequence - b.sequence) };
}

/**
 * Apply everything held (plus an optional repair batch) in sequence order. It stops holding
 * only when nothing is missing: a hole anywhere re-requests the cursor from there.
 */
function drainHeld(view: OperationView, extras: readonly OperationEvent[] = []): OperationView {
  const candidates = [...view.held, ...extras];
  for (const event of candidates) {
    const checked = validateOperationEvent(event);
    if (!checked.ok) return snapshotRepair(view, "invalid_event", typeof event.at === "number" ? event.at : 0);
  }
  const matching = candidates.filter((event) => event.operationId === view.operationId);
  for (const event of matching) {
    const applied = view.eventFingerprints[event.sequence];
    if (event.sequence <= view.lastSequence) {
      if (applied === undefined) {
        if (event.sequence <= view.lastSequence - EVENT_FINGERPRINT_TAIL) {
          return snapshotRepair(view, "duplicate_identity_evicted", event.at);
        }
      } else if (applied !== canonical(event)) return snapshotRepair(view, "conflicting_duplicate", event.at);
    }
  }
  const sorted = sortUniqueBySequence(matching.filter((event) => event.sequence > view.lastSequence));
  if (sorted.conflict) return snapshotRepair(view, "conflicting_duplicate", sorted.conflict.at);
  const pending = sorted.events;
  const dropped = view.heldDropped;
  let next: OperationView = { ...view, held: [], heldDropped: false, resync: undefined, notice: undefined };
  for (let index = 0; index < pending.length; index++) {
    const event = pending[index];
    if (event.sequence <= next.lastSequence) continue;
    if (event.sequence > next.lastSequence + 1) {
      const remaining = pending.slice(index);
      for (const held of remaining) next = holdEvent(next, held, {
        need: "events_after",
        reason: "gap",
        message: `events after ${next.lastSequence} are still missing`,
      });
      break;
    }
    next = applyContiguous(next, event);
    const resync = next.resync;
    if (resync) {
      const remaining = pending.slice(index + 1);
      for (const held of remaining) next = holdEvent(next, held, repairOf(resync));
      break;
    }
  }
  if (dropped && !next.resync) {
    next = {
      ...next,
      status: "repairing",
      resync: request(next, "events_after", "buffer_overflow", next.updatedAt ?? 0),
      notice: {
        code: "buffer_overflow",
        message: "older held events were dropped while repairing: the cursor was requested again",
        at: next.updatedAt ?? 0,
      },
    };
  }
  return {
    ...next,
    status: next.resync ? "repairing" : view.status === "disconnected" ? "disconnected" : "live",
  };
}
