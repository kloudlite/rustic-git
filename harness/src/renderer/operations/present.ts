/**
 * O09 — turning an `OperationView` into what a person reads.
 *
 * Every string here is derived from a durable field or a stated observation. There is no
 * percentage, no "almost done", and no duration measured from something that never
 * started: a step that has not been dispatched shows its queue reason instead of a clock,
 * and an operation with no reported usage says so rather than showing zero.
 */
import type {
  CapabilityEffect,
  OperationError,
  OperationEventModel,
  OperationScope,
  OperationState,
  StepState,
} from "../../../bench/src/operations/contracts.ts";
import {
  concurrentSteps,
  isPendingStep,
  isTerminalOperation,
  requestLabel,
  runCounts,
  unknownOutcomesOf,
  type DecisionPresentation,
  type OperationStepView,
  type OperationUsage,
  type OperationView,
} from "./types.ts";

export type Tone = "neutral" | "accent" | "success" | "warning" | "danger";

export type RowKind =
  | "state"
  | "scope"
  | "model"
  | "time"
  | "usage"
  | "queue"
  | "dependency"
  | "evidence"
  | "error"
  | "unknown"
  | "decision"
  | "text"
  /** Model-facing text: shown as content, never read as authorization. */
  | "untrusted_text";

export type OperationRow = { key: string; label: string; value: string; kind: RowKind; tone?: Tone };

const OPERATION_STATE_LABEL: Record<OperationState, string> = {
  accepted: "accepted",
  resolving: "resolving what to call",
  awaiting_approval: "waiting for your approval",
  running: "running",
  needs_input: "waiting for input",
  reconciling: "outcome unknown — reconciling",
  cancel_requested: "cancelling — not yet confirmed",
  completed: "completed",
  partial: "partly completed — effects remain",
  failed: "failed",
  cancelled: "cancelled",
  expired: "expired",
};

const OPERATION_STATE_TONE: Record<OperationState, Tone> = {
  accepted: "neutral",
  resolving: "neutral",
  awaiting_approval: "warning",
  running: "accent",
  needs_input: "warning",
  reconciling: "warning",
  cancel_requested: "warning",
  completed: "success",
  partial: "warning",
  failed: "danger",
  cancelled: "neutral",
  expired: "neutral",
};

const STEP_STATE_LABEL: Record<StepState, string> = {
  queued: "queued",
  awaiting_approval: "waiting for approval",
  running: "running",
  succeeded: "succeeded",
  failed: "failed",
  skipped: "skipped",
  cancelled: "cancelled",
  outcome_unknown: "outcome unknown — not retried",
};

const STEP_STATE_TONE: Record<StepState, Tone> = {
  queued: "neutral",
  awaiting_approval: "warning",
  running: "accent",
  succeeded: "success",
  failed: "danger",
  skipped: "neutral",
  cancelled: "neutral",
  outcome_unknown: "warning",
};

const DECISION_CLASS_LABEL: Record<DecisionPresentation["decisionClass"], string> = {
  user_authorization: "your approval",
  user_preference: "your choice",
  additional_input: "needed information",
};

/** A state never says more than the durable record: an unloaded view says so. */
export function stateLabel(state: OperationState | undefined): string {
  return state === undefined ? "loading the operation record…" : OPERATION_STATE_LABEL[state];
}

export function stateTone(state: OperationState | undefined): Tone {
  return state === undefined ? "neutral" : OPERATION_STATE_TONE[state];
}

export function stepStateLabel(state: StepState): string {
  return STEP_STATE_LABEL[state];
}

export function stepStateTone(state: StepState): Tone {
  return STEP_STATE_TONE[state];
}

export function decisionClassLabel(decisionClass: DecisionPresentation["decisionClass"]): string {
  return DECISION_CLASS_LABEL[decisionClass];
}

export function effectLabel(effect: CapabilityEffect | undefined): string {
  return effect ?? "effect not reported";
}

/** `320 ms`, `4.2 s`, `3 m 20 s`, `1 h 4 m` — a duration someone can compare. */
export function durationLabel(ms: number | undefined): string | undefined {
  if (ms === undefined) return undefined;
  const value = Math.max(0, Math.round(ms));
  if (value < 1_000) return `${value} ms`;
  if (value < 60_000) return `${(value / 1_000).toFixed(1)} s`;
  const minutes = Math.floor(value / 60_000);
  const seconds = Math.floor((value % 60_000) / 1_000);
  if (minutes < 60) return `${minutes} m ${seconds} s`;
  const hours = Math.floor(minutes / 60);
  return `${hours} h ${minutes % 60} m`;
}

export function relativeLabel(at: number | undefined, now: number): string | undefined {
  if (at === undefined) return undefined;
  const ago = now - at;
  if (ago < 1_000) return "just now";
  if (ago < 60_000) return `${Math.round(ago / 1_000)}s ago`;
  if (ago < 3_600_000) return `${Math.floor(ago / 60_000)}m ago`;
  return `${Math.floor(ago / 3_600_000)}h ago`;
}

export function modelLabel(model: OperationEventModel | undefined): string | undefined {
  return model ? `${model.provider}/${model.model} ${model.version}` : undefined;
}

export function scopeLabel(scope: OperationScope): string {
  const parts = [
    scope.workspaceId ? `workspace ${scope.workspaceId}` : undefined,
    scope.treeId ? `tree ${scope.treeId}` : undefined,
    scope.repositoryId ? `repo ${scope.repositoryId}` : undefined,
  ].filter((part): part is string => !!part);
  return parts.length ? parts.join(" · ") : "scope not reported";
}

export function usageLabel(usage: OperationUsage | undefined): string {
  if (!usage) return "usage not reported yet";
  const steps = `${usage.steps} ${usage.steps === 1 ? "step" : "steps"}`;
  const rounds = `${usage.selectionRounds} selection ${usage.selectionRounds === 1 ? "round" : "rounds"}`;
  const calls = `${usage.generationCalls} generation ${usage.generationCalls === 1 ? "call" : "calls"}`;
  return `${steps} · ${rounds} · ${calls} · ${usage.attempts} attempts`;
}

export function costLabel(_view: OperationView): string {
  return "cost not reported";
}

/**
 * Wall-clock elapsed. A finished operation is measured to its last durable update; a live
 * one is measured to `now`. A reported `elapsedMs` wins, because it is the log's own number.
 */
export function operationElapsed(view: OperationView, now: number): number | undefined {
  if (view.elapsedMs !== undefined) return view.elapsedMs;
  if (view.createdAt === undefined) return undefined;
  const end = isTerminalOperation(view.state) ? view.updatedAt ?? view.createdAt : now;
  return Math.max(0, end - view.createdAt);
}

export function operationElapsedLabel(view: OperationView, now: number): string | undefined {
  const elapsed = operationElapsed(view, now);
  if (elapsed === undefined) return undefined;
  return isTerminalOperation(view.state) ? `${durationLabel(elapsed)} total` : `${durationLabel(elapsed)} so far`;
}

/** How long the latest attempt has run, or how long the whole step took once it settled. */
export function stepElapsedLabel(step: OperationStepView, now: number): string | undefined {
  if (step.endedAt !== undefined && step.startedAt !== undefined) {
    return durationLabel(step.endedAt - step.startedAt);
  }
  if (step.elapsedMs !== undefined) return `${durationLabel(step.elapsedMs)} reported`;
  if (step.startedAt !== undefined) return `${durationLabel(now - step.startedAt)} so far`;
  return undefined;
}

export type StepRow = {
  key: string;
  stepId: string;
  title: string;
  state: StepState;
  stateLabel: string;
  tone: Tone;
  effect: string;
  target?: string;
  capabilityVersion?: string;
  dependencyLabel?: string;
  queueLabel?: string;
  concurrencyLabel?: string;
  timeLabel?: string;
  attemptsLabel: string;
  evidence: string[];
  model?: string;
  errorLabel?: string;
  errorTone?: Tone;
  observedLine?: string;
  reconcileLabel?: string;
};

export function stepRows(view: OperationView, now: number): StepRow[] {
  return view.steps.map((step) => {
    const overlapping = concurrentSteps(view, step);
    // A failure the durable record has not been loaded for still has the log's own words:
    // the phase carried the error code and the message, just no typed error object.
    const reportedFailure = !step.error && step.state === "failed" && step.decisionCode !== undefined;
    // A skipped step never ran and never had an outcome of its own — `errorTone` stays neutral
    // rather than the danger a failure gets — but the person still needs to see WHY it will
    // never run, in the same collapsed-row slot a failure's summary appears in.
    const reportedSkip = step.state === "skipped" && step.decisionCode !== undefined;
    return {
      key: step.stepId,
      stepId: step.stepId,
      title: step.capability || "capability not reported",
      state: step.state,
      stateLabel: stepStateLabel(step.state),
      tone: stepStateTone(step.state),
      effect: effectLabel(step.effect),
      target: step.targetRef,
      capabilityVersion: step.capabilityVersion,
      dependencyLabel: step.dependencies.length ? `after ${step.dependencies.join(", ")}` : undefined,
      queueLabel: step.state === "queued" || step.state === "awaiting_approval" ? step.queuedReason : undefined,
      concurrencyLabel:
        overlapping.length && (step.state === "running" || step.state === "outcome_unknown")
          ? `${overlapping.length + 1} calls overlapped`
          : undefined,
      timeLabel: stepElapsedLabel(step, now),
      attemptsLabel: step.attempts === 1 ? "1 attempt" : `${step.attempts} attempts`,
      evidence: step.evidenceRefs,
      model: modelLabel(step.model),
      errorLabel: step.error
        ? `${step.error.code}: ${step.error.message}`
        : reportedFailure
          ? `${step.decisionCode}: ${step.summary ?? "reported failed"}`
          : reportedSkip
            ? (step.summary ?? `${step.decisionCode}`)
            : undefined,
      errorTone: step.error ? (step.error.retryable ? "warning" : "danger") : reportedFailure ? "danger" : reportedSkip ? "neutral" : undefined,
      observedLine: step.summary,
      reconcileLabel:
        step.state === "outcome_unknown" && step.reconciledAt !== undefined
          ? `reconciled ${relativeLabel(step.reconciledAt, now) ?? ""}`.trim()
          : undefined,
    };
  });
}

/** One step's full detail, as label/value rows the panel expands to. */
export function stepDetailRows(view: OperationView, step: OperationStepView, now: number): OperationRow[] {
  const rows: OperationRow[] = [];
  const push = (label: string, value: string | undefined, kind: RowKind, tone?: Tone) => {
    if (value !== undefined && value !== "") rows.push({ key: `${step.stepId}:${label}`, label, value, kind, tone });
  };
  push("target", step.targetRef, "scope");
  push("effect", step.effect ? effectLabel(step.effect) : undefined, "scope");
  push("capability", step.capabilityVersion ? `${step.capability} ${step.capabilityVersion}` : step.capability || undefined, "text");
  push("key", step.key, "text");
  push("depends on", step.dependencies.join(", "), "dependency");
  push("queue reason", step.queuedReason, "queue");
  push("attempts", step.attempts ? String(step.attempts) : undefined, "usage");
  push("started", relativeLabel(step.startedAt, now), "time");
  push("ended", relativeLabel(step.endedAt, now), "time");
  push("model", modelLabel(step.model), "model");
  push("decision", step.decisionCode, "decision");
  push("arg digest", step.argDigest, "text");
  push("backend operation", step.backendOperationId, "text");
  push("idempotency key", step.idempotencyKey, "text");
  push("resources", step.resourceKeys?.join(", "), "scope");
  if (step.error) {
    rows.push({
      key: `${step.stepId}:error`,
      label: "error",
      value: `${step.error.code} — ${step.error.message}${step.error.retryable ? " (retryable)" : ""}`,
      kind: "error",
      tone: step.error.retryable ? "warning" : "danger",
    });
    if (step.error.missing?.length) {
      rows.push({ key: `${step.stepId}:missing`, label: "missing", value: step.error.missing.join(", "), kind: "error" });
    }
  }
  if (step.argPreview) {
    rows.push({ key: `${step.stepId}:preview`, label: "approved content", value: step.argPreview, kind: "untrusted_text" });
  }
  if (step.summary) {
    rows.push({ key: `${step.stepId}:summary`, label: "reported", value: step.summary, kind: "untrusted_text" });
  }
  if (step.observedEvents === 0) {
    rows.push({
      key: `${step.stepId}:events`,
      label: "events",
      value: `no event in this view yet (${view.events.length} applied for the operation)`,
      kind: "text",
    });
  }
  return rows;
}

/** The facts about the operation itself: what was asked, where it runs, and how it is going. */
export function operationRows(view: OperationView, now: number): OperationRow[] {
  const rows: OperationRow[] = [];
  const asked = requestLabel(view);
  if (asked) rows.push({ key: "asked", label: "asked", value: asked, kind: "untrusted_text" });
  rows.push({ key: "state", label: "state", value: stateLabel(view.state), kind: "state", tone: stateTone(view.state) });
  rows.push({ key: "scope", label: "scope", value: scopeLabel(view.scope), kind: "scope" });
  rows.push({
    key: "model",
    label: "model",
    value: modelLabel(view.model) ?? "model not reported",
    kind: "model",
  });
  rows.push({
    key: "elapsed",
    label: "elapsed",
    value: operationElapsedLabel(view, now) ?? "not started yet",
    kind: "time",
  });
  rows.push({ key: "usage", label: "usage", value: usageLabel(view.usage), kind: "usage" });
  if (view.queueReason) rows.push({ key: "queue", label: "queue reason", value: view.queueReason, kind: "queue" });
  if (view.result?.summary) {
    rows.push({ key: "result", label: "result", value: view.result.summary, kind: "untrusted_text" });
  }
  return rows;
}

export type DecisionRow = {
  key: string;
  decisionId: string;
  stepId: string;
  operationId: string;
  decisionClass: DecisionPresentation["decisionClass"];
  decisionLabel: string;
  effect?: CapabilityEffect;
  question: string;
  subject: string;
  expiresAt: number;
  expiryLabel: string;
  expired: boolean;
  answerable: boolean;
  unavailable?: string;
  digest?: string;
  preview?: string;
  dependencies: string[];
  revision: number;
};

function expiryLabel(expiresAt: number, now: number): { label: string; expired: boolean } {
  if (expiresAt <= now) return { label: `expired ${relativeLabel(expiresAt, now)}`, expired: true };
  const left = durationLabel(expiresAt - now) ?? "soon";
  return { label: `expires in ${left}`, expired: false };
}

function subjectOf(decision: DecisionPresentation, step: OperationStepView | undefined): string {
  const capability = decision.capability ?? step?.capability ?? "capability not reported";
  const target = decision.targetRef ?? step?.targetRef;
  const effect = decision.effect ?? step?.effect;
  return [capability, target, effect ? effectLabel(effect) : undefined].filter(Boolean).join(" · ");
}

/**
 * The open decisions, and any that closed while the panel was open. Concrete content comes
 * from the bridge (`view.decisions`); without it, an approval is shown but stays
 * unanswerable — nobody approves a mutation they cannot see — while a question for facts
 * can be answered from the snapshot's own `question`.
 */
export function decisionRows(view: OperationView, now: number): DecisionRow[] {
  const presented = new Map(view.decisions.map((decision) => [decision.decisionId, decision]));
  const rows: DecisionRow[] = [];
  for (const pending of view.pendingDecisions) {
    const shown = presented.get(pending.decisionId);
    presented.delete(pending.decisionId);
    const decision: DecisionPresentation =
      shown ??
      {
        decisionId: pending.decisionId,
        operationId: pending.operationId,
        stepId: pending.stepId,
        decisionClass: pending.decisionClass,
        question: pending.question,
        createdAt: pending.createdAt,
        expiresAt: pending.expiresAt,
        revision: pending.revision,
        answerable: pending.decisionClass === "additional_input",
        unavailable:
          pending.decisionClass === "additional_input"
            ? undefined
            : "the approval preview has not arrived: approving blind is not offered",
      };
    rows.push(decisionRow(decision, view, now));
  }
  for (const decision of presented.values()) rows.push(decisionRow(decision, view, now));
  return rows;
}

function decisionRow(decision: DecisionPresentation, view: OperationView, now: number): DecisionRow {
  const step = view.steps.find((candidate) => candidate.stepId === decision.stepId);
  const pending = view.pendingDecisions.find((candidate) => candidate.decisionId === decision.decisionId);
  const expiry = expiryLabel(decision.expiresAt, now);
  const superseded =
    view.revision > decision.revision && !isTerminalOperation(view.state)
      ? `raised at revision ${decision.revision}; the operation is at ${view.revision}`
      : undefined;
  const unavailable = [
    decision.unavailable,
    !pending ? "this decision is no longer open" : undefined,
    pending && (
      pending.operationId !== decision.operationId ||
      pending.stepId !== decision.stepId ||
      pending.decisionClass !== decision.decisionClass ||
      pending.revision !== decision.revision ||
      pending.expiresAt !== decision.expiresAt
    ) ? "the presentation does not match the pending decision" : undefined,
    view.resync ? "the operation state is being repaired" : undefined,
    view.state !== "awaiting_approval" && view.state !== "needs_input" ? `the operation is ${stateLabel(view.state)}` : undefined,
    step && step.state !== "awaiting_approval" && step.state !== "queued" ? `the step is ${stepStateLabel(step.state)}` : undefined,
    expiry.expired ? "the decision window has passed" : undefined,
    superseded,
  ]
    .filter((part): part is string => !!part)
    .join(" · ");
  return {
    key: decision.decisionId,
    decisionId: decision.decisionId,
    stepId: decision.stepId,
    operationId: decision.operationId,
    decisionClass: decision.decisionClass,
    decisionLabel: decisionClassLabel(decision.decisionClass),
    effect: decision.effect ?? step?.effect,
    question: decision.question,
    subject: subjectOf(decision, step),
    expiresAt: decision.expiresAt,
    expiryLabel: expiry.label,
    expired: expiry.expired,
    // A decision raised at an older revision cannot be answered: the bridge would refuse the
    // stale revision, so offering the button would be a lie about what a click can do.
    answerable: decision.answerable && pending !== undefined && !expiry.expired && superseded === undefined && !view.resync && !unavailable,
    unavailable: unavailable || undefined,
    digest: decision.argDigest ?? step?.argDigest,
    preview: decision.argPreview ?? step?.argPreview,
    dependencies: decision.dependencies ?? step?.dependencies ?? [],
    revision: decision.revision,
  };
}

export function decisionDetailRows(decision: DecisionRow): OperationRow[] {
  const rows: OperationRow[] = [
    { key: "decision:class", label: "asks for", value: decision.decisionLabel, kind: "decision" },
    { key: "decision:subject", label: "about", value: decision.subject, kind: "scope" },
    { key: "decision:step", label: "step", value: decision.stepId, kind: "text" },
    { key: "decision:expiry", label: "window", value: decision.expiryLabel, kind: "time", tone: decision.expired ? "warning" : undefined },
    { key: "decision:revision", label: "revision", value: String(decision.revision), kind: "text" },
  ];
  if (decision.dependencies.length) {
    rows.push({ key: "decision:deps", label: "depends on", value: decision.dependencies.join(", "), kind: "dependency" });
  }
  if (decision.digest) rows.push({ key: "decision:digest", label: "content digest", value: decision.digest, kind: "text" });
  if (decision.preview) {
    rows.push({ key: "decision:preview", label: "content", value: decision.preview, kind: "untrusted_text" });
  }
  return rows;
}

export function unknownRows(view: OperationView): OperationRow[] {
  return unknownOutcomesOf(view).map((outcome) => ({
    key: `unknown:${outcome.stepId}`,
    label: outcome.capability || "capability not reported",
    value: `not retried, not called failed: ${outcome.dispatchDigest ? `dispatched as ${outcome.dispatchDigest}` : "dispatch digest not reported"}`,
    kind: "unknown",
    tone: "warning",
  }));
}

export function errorRows(view: OperationView): OperationRow[] {
  const rows: OperationRow[] = view.steps
    .filter((step) => step.error || (step.state === "failed" && step.decisionCode !== undefined))
    .map((step) => ({
      key: `error:${step.stepId}`,
      label: step.capability || step.stepId,
      value: step.error
        ? `${step.error.code}: ${step.error.message}${step.error.retryable ? " (retryable)" : ""}`
        : `${step.decisionCode}: ${step.summary ?? "reported failed"} (retryability not reported)`,
      kind: "error" as RowKind,
      tone: (step.error?.retryable ? "warning" : "danger") as Tone,
    }));
  const resultError: OperationError | undefined = view.result?.error;
  if (resultError && !rows.some((row) => row.value.startsWith(resultError.code))) {
    rows.push({
      key: "error:operation",
      label: "operation",
      value: `${resultError.code}: ${resultError.message}${resultError.retryable ? " (retryable)" : ""}`,
      kind: "error",
      tone: resultError.retryable ? "warning" : "danger",
    });
  }
  return rows;
}

export function evidenceRows(view: OperationView): OperationRow[] {
  const refs = new Set<string>();
  for (const step of view.steps) for (const ref of step.evidenceRefs) refs.add(ref);
  for (const ref of view.result?.evidenceRefs ?? []) refs.add(ref);
  return [...refs].map((ref) => ({ key: `evidence:${ref}`, label: ref, value: "evidence", kind: "evidence" as RowKind }));
}

/** Counts, said in words. Deliberately never a percentage: the total is not known early. */
export function progressLabel(view: OperationView): string {
  const counts = runCounts(view);
  if (!counts.total) return "no steps reported yet";
  const parts = [`${counts.settled} of ${counts.total} calls settled`];
  if (counts.running) parts.push(`${counts.running} running`);
  if (counts.queued) parts.push(`${counts.queued} queued`);
  if (counts.awaitingApproval) parts.push(`${counts.awaitingApproval} waiting for approval`);
  if (counts.unknown) parts.push(`${counts.unknown} unknown`);
  return parts.join(" · ");
}

export function concurrencyLabel(view: OperationView): string {
  const counts = runCounts(view);
  if (counts.running > 1) return `${counts.running} calls running at once`;
  if (counts.running === 1) return "1 call running";
  return "nothing running";
}

/**
 * Steps that have reported nothing for a while. This is a hint, not a state change: a
 * worker that stopped talking is not a finished step, and the view keeps saying `running`
 * until a durable phase says otherwise.
 */
export function stalledSteps(view: OperationView, now: number, thresholdMs = 60_000): OperationStepView[] {
  return view.steps.filter(
    (step) =>
      (isPendingStep(step.state)) &&
      step.lastAt !== undefined &&
      now - step.lastAt >= thresholdMs,
  );
}

/** A headline for the collapsed row: what was asked, in one line. */
export function panelTitle(view: OperationView): string {
  return requestLabel(view) ?? view.operationId;
}

export function panelMeta(view: OperationView, now: number): string[] {
  const parts = [
    stateLabel(view.state),
    modelLabel(view.model),
    operationElapsedLabel(view, now),
    view.steps.length ? progressLabel(view) : undefined,
  ];
  return parts.filter((part): part is string => !!part);
}
