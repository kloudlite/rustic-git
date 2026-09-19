/**
 * O05 — operation and step state transitions over the frozen O01 tables.
 *
 * `applyTransition` is the only way a durable snapshot changes. It checks the
 * permitted `(from, to, trigger)` edge, checks the fact behind that trigger, validates
 * the result with `validateOperationSnapshot`, and returns the events that belong to
 * the same durable commit. It never touches a file: `store.ts` owns fsync and naming.
 *
 * Guards exist so a caller cannot claim a state the evidence does not support: a
 * succeeded step needs success evidence, a cancelled running step needs proof that no
 * effect was applied, `completed` needs every step settled with no failure, and an
 * operation holding unknown effects never reaches a terminal state.
 */
import {
  canTransitionOperation,
  canTransitionStep,
  isTerminalOperationState,
  isTerminalStepState,
  validateOperationSnapshot,
  type CompactOperationResult,
  type OperationError,
  type OperationErrorCode,
  type OperationEvent,
  type OperationEventPhase,
  type OperationSnapshot,
  type OperationState,
  type OperationTransitionTrigger,
  type PendingDecision,
  type StepRecord,
  type StepState,
  type StepTransitionTrigger,
  type UnknownOutcome,
} from "./contracts.ts";

export type NewEvent = {
  phase: OperationEventPhase;
  summary: string;
  at?: number;
  stepId?: string;
  capability?: string;
  decisionCode?: string;
  queueReason?: string;
  dependencies?: string[];
  model?: { provider: string; model: string; version: string };
  retryCount?: number;
  elapsedMs?: number;
  evidenceRefs?: string[];
  argDigest?: string;
  argPreview?: string;
};

/** The recorded decision a transition is acting on; the trigger must match its outcome. */
export type DecisionUse = { recordId: string; outcome: "granted" | "denied" };

export type StepChange = {
  stepId: string;
  to: StepState;
  trigger: StepTransitionTrigger;
  /** Fields recorded with the change; `attempts`, `state`, and `stepId` are store-owned. */
  patch?: Partial<Omit<StepRecord, "stepId" | "state" | "attempts">>;
  /** Evidence that no effect was applied; `cancel_confirmed` requires it. */
  cancelEvidence?: string[];
  /** Declared retry class; only `idempotent` may restart a failed step. */
  retry?: { class: "none" | "idempotent" | "reconcile_required"; maxAttempts: number };
  /** Digest of the args being dispatched; an intent digest is mandatory before dispatch. */
  dispatchDigest?: string;
};

export type OperationChange = { to: OperationState; trigger: OperationTransitionTrigger };

export type TransitionInput = {
  now: number;
  operation?: OperationChange;
  steps?: StepChange[];
  addSteps?: StepRecord[];
  addDecisions?: Array<Omit<PendingDecision, "revision">>;
  removeDecisions?: string[];
  addUnknownOutcomes?: UnknownOutcome[];
  removeUnknownOutcomes?: string[];
  usage?: OperationSnapshot["usage"];
  deadlineAt?: number;
  decisionUse?: DecisionUse;
  /** `false` keeps the revision for event/decision-only commits. */
  bumpRevision?: boolean;
  events?: NewEvent[];
  /** Record the truthful terminal state after the other changes, when one is due. */
  settle?: boolean;
  /** Treat failed steps as settled when trusted policy has no retry left. */
  settleFailed?: boolean;
};

export type TransitionSuccess = { ok: true; changed: true; snapshot: OperationSnapshot; events: OperationEvent[] };
export type TransitionNoop = { ok: true; changed: false; snapshot: OperationSnapshot; events: [] };
export type TransitionFailure = { ok: false; error: OperationError };
export type TransitionResult = TransitionSuccess | TransitionNoop | TransitionFailure;

export type SettlePlan = { to: TerminalState; trigger: OperationTransitionTrigger };
export type TerminalState = "completed" | "partial" | "failed" | "cancelled" | "expired";

export function storeError(
  code: OperationErrorCode,
  message: string,
  extra: { missing?: string[]; refs?: string[]; retryable?: boolean } = {},
): OperationError {
  const error: OperationError = { code, message, retryable: extra.retryable ?? false };
  if (extra.missing?.length) error.missing = extra.missing;
  if (extra.refs?.length) error.refs = extra.refs;
  return error;
}

const failure = (error: OperationError): TransitionFailure => ({ ok: false, error });

/**
 * The outcome the settled work supports: `completed` keeps only successes and skips,
 * `partial` keeps committed effects beside work that failed, was skipped, or was
 * cancelled, `cancelled` needs no committed effect, and `failed` is everything else.
 */
export function settledState(snapshot: OperationSnapshot, settleFailed = false): TerminalState | undefined {
  if (snapshot.unknownOutcomes.length || snapshot.pendingDecisions.length) return undefined;
  // An operation with no steps has settled nothing: it is not a failure to be measured,
  // though an explicit cancellation with nothing left to do is settled. Both states are
  // accepted here because a guard sees the state the transition is moving to.
  if (!snapshot.steps.length) {
    return snapshot.state === "cancel_requested" || snapshot.state === "cancelled" ? "cancelled" : undefined;
  }
  if (!snapshot.steps.every((step) => isTerminalStepState(step.state) || (settleFailed && step.state === "failed"))) return undefined;
  let succeeded = 0;
  let cancelled = 0;
  let interrupted = 0;
  for (const step of snapshot.steps) {
    if (step.state === "succeeded") succeeded += 1;
    else if (step.state === "cancelled") cancelled += 1;
    else interrupted += 1;
  }
  const blocked = interrupted + cancelled;
  let state: TerminalState;
  if (succeeded === 0 && cancelled > 0 && interrupted === 0) state = "cancelled";
  else if (succeeded > 0 && blocked > 0) state = "partial";
  else if (succeeded > 0) state = "completed";
  else state = "failed";
  return state;
}

export function settledOutcome(snapshot: OperationSnapshot, settleFailed = false): SettlePlan | undefined {
  const state = settledState(snapshot, settleFailed);
  if (!state) return undefined;
  if (snapshot.state === state) return undefined;
  const trigger: OperationTransitionTrigger =
    snapshot.state === "reconciling"
      ? state === "partial"
        ? "effects_partial"
        : "reconcile_conclusive"
      : state === "completed"
        ? "effects_committed"
        : state === "partial"
          ? "effects_partial"
          : state === "cancelled"
            ? "cancel_settled"
            : "effects_failed";
  if (!canTransitionOperation(snapshot.state, state, trigger)) return undefined;
  return { to: state, trigger };
}

/**
 * The next terminal state when the work is settled. A committed effect is never hidden
 * behind `expired`: `completed`/`partial` win, and a passed deadline only ends an
 * operation that never committed anything.
 */
export function settlePlan(snapshot: OperationSnapshot, now: number, settleFailed = false): SettlePlan | undefined {
  if (isTerminalOperationState(snapshot.state)) return undefined;
  const outcome = settledOutcome(snapshot, settleFailed);
  if (!outcome) return undefined;
  const deadlinePassed = snapshot.deadlineAt !== undefined && now >= snapshot.deadlineAt;
  if (deadlinePassed && (outcome.to === "cancelled" || outcome.to === "failed") && canTransitionOperation(snapshot.state, "expired", "deadline_reached")) {
    return { to: "expired", trigger: "deadline_reached" };
  }
  return outcome;
}

function advanceStep(step: StepRecord, change: StepChange, now: number): StepRecord {
  const next: StepRecord = { ...step, ...(change.patch ?? {}), state: change.to };
  if (change.to === "running" && step.state !== "running") {
    delete next.endedAt;
    delete next.error;
    if (change.patch?.backendOperationId === undefined) delete next.backendOperationId;
    next.attempts = step.attempts + 1;
    next.startedAt = now;
  }
  if (isTerminalStepState(change.to) && next.endedAt === undefined) next.endedAt = now;
  return next;
}

function stepGuard(previous: OperationSnapshot, candidate: OperationSnapshot, input: TransitionInput): OperationError | undefined {
  for (const change of input.steps ?? []) {
    const error = singleStepGuard(previous, candidate, input, change);
    if (error) return error;
  }
  return undefined;
}

function singleStepGuard(
  previous: OperationSnapshot,
  candidate: OperationSnapshot,
  input: TransitionInput,
  change: StepChange,
): OperationError | undefined {
  const invalid = (message: string) => storeError("invalid_transition", message);
  const before = previous.steps.find((step) => step.stepId === change.stepId);
  const after = candidate.steps.find((step) => step.stepId === change.stepId);
  if (!before || !after) return storeError("validation_failure", `unknown step ${change.stepId}`);
  switch (change.trigger) {
    case "dispatch_started":
      if (after.effect !== "read") {
        if (!after.idempotencyKey) return invalid(`a ${after.effect} step records its idempotency key before dispatch`);
        if (!change.dispatchDigest) return invalid(`a ${after.effect} step records the digest of its approved args before dispatch`);
      }
      return undefined;
    case "approval_recorded":
      return input.decisionUse?.outcome === "granted" ? undefined : invalid("approval_recorded needs a recorded grant");
    case "approval_denied":
      if (input.decisionUse?.outcome !== "denied") return invalid("approval_denied needs a recorded denial");
      if (after.state !== "skipped") return invalid("a denial skips the step, it never dispatches");
      return undefined;
    case "cancel_confirmed":
      return change.cancelEvidence?.length ? undefined : invalid("cancel_confirmed needs evidence that no effect was applied");
    case "result_observed":
      return after.evidenceRefs?.length ? undefined : invalid("a succeeded step records success evidence");
    case "result_failed":
      return after.error ? undefined : invalid("a failed step records an error");
    case "outcome_unknown":
      return candidate.unknownOutcomes.some((outcome) => outcome.stepId === change.stepId)
        ? undefined
        : invalid("an unknown outcome is recorded with its dispatch digest");
    case "reconcile_conclusive":
      if (after.state === "succeeded") return after.evidenceRefs?.length ? undefined : invalid("a reconciled success records evidence");
      if (after.state === "failed") return after.error ? undefined : invalid("a reconciled failure records an error");
      return undefined;
    case "retry_allowed":
      if (change.retry?.class !== "idempotent") return invalid("only a declared idempotent retry class may restart a failed step");
      if (change.retry && before.attempts >= change.retry.maxAttempts) {
        return invalid(`step ${after.stepId} exhausted its ${change.retry.maxAttempts} declared attempts`);
      }
      return undefined;
    default:
      return undefined;
  }
}

function operationGuard(previous: OperationSnapshot, candidate: OperationSnapshot, input: TransitionInput): OperationError | undefined {
  const change = input.operation;
  if (!change) return undefined;
  const invalid = (message: string) => storeError("invalid_transition", message);
  switch (change.trigger) {
    case "dispatch_started":
      return candidate.steps.some((step) => step.state === "running") ? undefined : invalid("running needs a dispatched step");
    case "approval_required":
      return candidate.pendingDecisions.some((decision) => decision.decisionClass !== "additional_input")
        ? undefined
        : invalid("awaiting_approval needs a pending user decision");
    case "decision_required":
      return candidate.pendingDecisions.some((decision) => decision.decisionClass === "additional_input")
        ? undefined
        : invalid("needs_input needs a pending additional-input decision");
    case "approval_recorded":
      return input.decisionUse?.outcome === "granted" ? undefined : invalid("approval_recorded needs a recorded grant");
    case "effects_committed":
      return settledState(candidate, input.settleFailed) === "completed"
        ? undefined
        : invalid("completed needs every step settled with a successful effect and no failure");
    case "effects_partial":
      return settledState(candidate, input.settleFailed) === "partial"
        ? undefined
        : invalid("partial needs committed effects beside failed, skipped, or cancelled work");
    case "effects_failed":
      return settledState(candidate, input.settleFailed) === "failed" ? undefined : invalid("failed needs settled work with no committed effect");
    case "unknown_outcome":
      return candidate.unknownOutcomes.length ? undefined : invalid("reconciling needs at least one unresolved outcome");
    case "reconcile_conclusive":
      if (candidate.unknownOutcomes.length) return invalid("reconciliation must resolve every unknown outcome first");
      if (candidate.state === "completed") {
        return settledState(candidate, input.settleFailed) === "completed" ? undefined : invalid("completed needs settled successful effects");
      }
      if (candidate.state === "failed") {
        return settledState(candidate, input.settleFailed) === "failed" ? undefined : invalid("failed needs settled work with no committed effect");
      }
      return undefined;
    case "cancel_settled":
      return settledState(candidate) === "cancelled"
        ? undefined
        : invalid("cancellation settles only when nothing was committed and no work remains");
    case "deadline_reached":
      if (candidate.deadlineAt === undefined || input.now < candidate.deadlineAt) return invalid("the operation deadline has not passed");
      if (candidate.pendingDecisions.length || candidate.unknownOutcomes.length) {
        return invalid("expiry cannot drop a pending decision or an unresolved outcome");
      }
      return candidate.steps.every((step) => isTerminalStepState(step.state)) ? undefined : invalid("expiry waits for settled steps");
    default:
      return undefined;
  }
}

function describeSettlement(snapshot: OperationSnapshot, state: TerminalState): string {
  let succeeded = 0;
  let interrupted = 0;
  for (const step of snapshot.steps) {
    if (step.state === "succeeded") succeeded += 1;
    else if (step.state !== "skipped") interrupted += 1;
  }
  const total = snapshot.steps.length;
  switch (state) {
    case "completed":
      return `${succeeded} of ${total} steps completed; no failure was recorded.`;
    case "partial":
      return `${succeeded} of ${total} steps completed; ${interrupted} did not apply and no rollback was attempted.`;
    case "cancelled":
      return "Cancelled before any step committed a change.";
    case "expired":
      return "The operation deadline passed before its work settled.";
    default:
      return total === 0 ? "The operation ended before any step ran." : `No step completed; ${total} could not proceed.`;
  }
}

/**
 * The compact projection of an operation: the only shape returned to the main model.
 * A terminal operation returns its stored result; anything else is projected live.
 */
export function compactView(snapshot: OperationSnapshot): CompactOperationResult {
  if (snapshot.result) return snapshot.result;
  const result: CompactOperationResult = {
    operationId: snapshot.operationId,
    revision: snapshot.revision,
    state: snapshot.state,
    summary: `Operation is ${snapshot.state}.`,
  };
  const evidence = [...new Set(snapshot.steps.flatMap((step) => step.evidenceRefs ?? []))].slice(0, 16);
  if (evidence.length) result.evidenceRefs = evidence;
  if (snapshot.steps.some((step) => step.state === "succeeded" && step.effect !== "read")) result.changed = true;
  const pending = snapshot.pendingDecisions[0];
  if (pending) {
    result.decision = {
      decisionId: pending.decisionId,
      decisionClass: pending.decisionClass,
      question: pending.question,
      expiresAt: pending.expiresAt,
    };
  }
  const failure = snapshot.steps.find((step) => step.state === "failed" && step.error)?.error;
  if (failure) result.error = failure;
  if (snapshot.unknownOutcomes.length) result.unknownOutcomes = snapshot.unknownOutcomes.map((outcome) => outcome.stepId);
  return result;
}

/**
 * Applies one durable change. The returned snapshot is already validated against the
 * frozen contract, so a caller can persist it without re-checking the tables.
 */
export function applyTransition(previous: OperationSnapshot, input: TransitionInput): TransitionResult {
  const now = input.now;
  const stepChanges = input.steps ?? [];
  const stepIndexes = new Map<string, number>();
  for (const change of stepChanges) {
    const index = previous.steps.findIndex((step) => step.stepId === change.stepId);
    if (index < 0) return failure(storeError("validation_failure", `unknown step ${change.stepId}`));
    if (stepIndexes.has(change.stepId)) return failure(storeError("invalid_transition", `step ${change.stepId} changes twice in one commit`));
    const before = previous.steps[index];
    const metadataOnly = before.state === "running" && change.to === "running" && change.trigger === "dispatch_started";
    if (!metadataOnly && !canTransitionStep(before.state, change.to, change.trigger)) {
      return failure(
        storeError("invalid_transition", `step ${change.stepId}: "${before.state}" -> "${change.to}" is not permitted by "${change.trigger}"`),
      );
    }
    stepIndexes.set(change.stepId, index);
  }
  const requestedOperation = input.operation;
  if (requestedOperation && requestedOperation.to !== previous.state && !canTransitionOperation(previous.state, requestedOperation.to, requestedOperation.trigger)) {
    return failure(
      storeError("invalid_transition", `operation: "${previous.state}" -> "${requestedOperation.to}" is not permitted by "${requestedOperation.trigger}"`),
    );
  }

  const stepsChanged = stepChanges.length > 0 || Boolean(input.addSteps?.length);
  const steps: StepRecord[] = stepsChanged ? [...previous.steps] : previous.steps;
  for (const change of stepChanges) {
    const index = stepIndexes.get(change.stepId);
    if (index === undefined) continue;
    steps[index] = advanceStep(previous.steps[index], change, now);
  }
  if (input.addSteps?.length) steps.push(...input.addSteps);

  const removedDecisions = input.removeDecisions ?? [];
  const decisionsChanged = removedDecisions.length > 0 || (input.addDecisions?.length ?? 0) > 0;
  const keptDecisions = removedDecisions.length
    ? previous.pendingDecisions.filter((decision) => !removedDecisions.includes(decision.decisionId))
    : previous.pendingDecisions;

  const removedOutcomes = input.removeUnknownOutcomes ?? [];
  const addedOutcomes = input.addUnknownOutcomes ?? [];
  const unknownChanged = removedOutcomes.length > 0 || addedOutcomes.length > 0;
  const keptOutcomes = removedOutcomes.length
    ? previous.unknownOutcomes.filter((outcome) => !removedOutcomes.includes(outcome.stepId))
    : previous.unknownOutcomes;
  const unknownOutcomes = addedOutcomes.length ? [...keptOutcomes, ...addedOutcomes] : keptOutcomes;

  const usageChanged = input.usage !== undefined;
  const deadlineChanged = input.deadlineAt !== undefined && input.deadlineAt !== previous.deadlineAt;
  const requestedState = requestedOperation && requestedOperation.to !== previous.state ? requestedOperation.to : previous.state;
  const newEvents = input.events ?? [];

  const draft: OperationSnapshot = {
    ...previous,
    state: requestedState,
    steps,
    // Added decisions keep a placeholder revision until the commit revision is known;
    // the draft only has to be truthful about which decisions are still pending.
    pendingDecisions: [...keptDecisions, ...(input.addDecisions ?? []).map((decision) => ({ ...decision, revision: previous.revision }))],
    unknownOutcomes: [...unknownOutcomes],
    usage: input.usage ?? previous.usage,
    lastSequence: previous.lastSequence + newEvents.length,
    ...(input.deadlineAt !== undefined ? { deadlineAt: input.deadlineAt } : {}),
  };
  const plan = input.settle ? settlePlan(draft, now, input.settleFailed) : undefined;
  const finalState = plan ? plan.to : requestedState;
  const bodyChanged =
    finalState !== previous.state || stepsChanged || decisionsChanged || unknownChanged || usageChanged || deadlineChanged;
  if (!bodyChanged && newEvents.length === 0) return { ok: true, changed: false, snapshot: previous, events: [] };

  const revision = input.bumpRevision === false ? previous.revision : previous.revision + 1;
  const effectiveOperation: OperationChange | undefined = plan ? { to: plan.to, trigger: plan.trigger } : requestedOperation;

  const events: OperationEvent[] = [];
  let sequence = previous.lastSequence;
  for (const event of newEvents) {
    sequence += 1;
    events.push({ ...event, operationId: previous.operationId, sequence, revision, at: event.at ?? now });
  }
  const pendingDecisions = input.addDecisions?.length
    ? [...keptDecisions, ...input.addDecisions.map((decision) => ({ ...decision, revision }))]
    : keptDecisions;

  const resolved: OperationSnapshot = {
    ...previous,
    state: finalState,
    revision,
    updatedAt: input.bumpRevision === false ? previous.updatedAt : now,
    steps,
    pendingDecisions: [...pendingDecisions],
    unknownOutcomes: [...unknownOutcomes],
    usage: input.usage ?? previous.usage,
    lastSequence: sequence,
    ...(input.deadlineAt !== undefined ? { deadlineAt: input.deadlineAt } : {}),
  };
  if (plan) {
    const summary = describeSettlement(resolved, plan.to);
    sequence += 1;
    events.push({
      operationId: previous.operationId,
      sequence,
      at: now,
      phase: plan.to,
      revision,
      summary,
    });
    resolved.lastSequence = sequence;
    resolved.result = { ...compactView(resolved), state: plan.to, revision, summary };
  }

  const guarded = { ...input, operation: effectiveOperation };
  const guardError = stepGuard(previous, resolved, guarded) ?? operationGuard(previous, resolved, guarded);
  if (guardError) return failure(guardError);

  const validated = validateOperationSnapshot(resolved);
  if (!validated.ok) {
    const first = validated.issues[0];
    return failure(storeError("invalid_transition", `${first.path}: ${first.message}`));
  }
  return { ok: true, changed: true, snapshot: validated.value, events };
}
