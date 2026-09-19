/**
 * O05 — restart recovery.
 *
 * `planRecovery` reads the durable logs and returns what the scheduler must do. It
 * never dispatches a handler and never writes: a step that was dispatched before the
 * crash is reported for reconciliation, never for a blind re-dispatch, and dependents
 * of an unresolved step are left out of the dispatch list. A retry candidate is only
 * discovery: the scheduler must call `OperationStore.retryStep` with the current
 * `TrustedActorContext` before dispatch. The caller must hold exclusive ownership.
 */
import {
  isTerminalOperationState,
  type CapabilityEffect,
  type OperationSnapshot,
  type StepRecord,
} from "./contracts.ts";
import { OperationStore, type RetryPolicy } from "./store.ts";

export type RecoveryAction =
  | {
      kind: "reconcile_step";
      operationId: string;
      stepId: string;
      capability: string;
      capabilityVersion: string;
      effect: CapabilityEffect;
      retry: RetryPolicy;
      /** True when the unknown outcome is already recorded on the step. */
      recorded: boolean;
      since: number;
      dispatchDigest?: string;
      idempotencyKey?: string;
      backendOperationId?: string;
    }
  | { kind: "dispatch_step"; operationId: string; stepId: string; capability: string; capabilityVersion: string }
  | {
      kind: "resume_abort";
      operationId: string;
      stepId: string;
      capability: string;
      capabilityVersion: string;
      dispatchDigest?: string;
      idempotencyKey?: string;
      backendOperationId?: string;
    }
  | {
      /** Non-executable discovery; `retryStep` is the authorization and policy gate. */
      kind: "retry_candidate";
      operationId: string;
      stepId: string;
      capability: string;
      capabilityVersion: string;
      retry: RetryPolicy;
      argDigest: string;
    }
  | { kind: "await_decision"; operationId: string; decisionId: string; stepId: string; expiresAt: number }
  | { kind: "expire_decision"; operationId: string; decisionId: string; stepId: string; expiresAt: number }
  | { kind: "expire_operation"; operationId: string; deadlineAt: number };

export type RecoveryPlan = { operationId: string; state: OperationSnapshot["state"]; actions: RecoveryAction[] };
export type RecoveryReport = {
  plans: RecoveryPlan[];
  /** Operations that already reached a terminal state; nothing is rescheduled for them. */
  terminal: string[];
  /** Files whose final record never finished writing when the store opened. */
  tornTails: string[];
};

export type RecoveryOptions = { now: number };

function dependenciesSatisfied(snapshot: OperationSnapshot, step: StepRecord): boolean {
  for (const dependency of step.dependencies ?? []) {
    const target = snapshot.steps.find((entry) => entry.key === dependency || entry.stepId === dependency);
    if (!target || target.state !== "succeeded") return false;
  }
  return true;
}

/**
 * Plans restart work for every non-terminal operation. Running steps are yields for
 * reconciliation; unknown outcomes stay visible; expired decisions are released; a
 * passed deadline is reported so the scheduler can expire the operation explicitly.
 */
export function planRecovery(store: OperationStore, options: RecoveryOptions): RecoveryReport {
  store.assertOwnership();
  const plans: RecoveryPlan[] = [];
  const terminal: string[] = [];
  for (const operationId of store.operationIds()) {
    const snapshot: OperationSnapshot = store.load(operationId);
    if (isTerminalOperationState(snapshot.state)) {
      terminal.push(operationId);
      continue;
    }
    const actions: RecoveryAction[] = [];
    const aborts = new Set(store.pendingAbortStepIds(operationId));
    // A passed deadline retires the operation; queued work must never be dispatched
    // again behind it, so `expire_operation` is the only action the deadline yields.
    const deadlinePassed = snapshot.deadlineAt !== undefined && options.now >= snapshot.deadlineAt;
    for (const step of snapshot.steps) {
      if (step.state === "running") {
        const unknown = snapshot.unknownOutcomes.find((outcome) => outcome.stepId === step.stepId);
        const dispatchDigest = unknown?.dispatchDigest ?? store.stepPayloadDigest(operationId, step.stepId);
        if (snapshot.state === "cancel_requested" && aborts.has(step.stepId)) {
          actions.push({
            kind: "resume_abort",
            operationId,
            stepId: step.stepId,
            capability: step.capability,
            capabilityVersion: step.capabilityVersion,
            ...(dispatchDigest !== undefined ? { dispatchDigest } : {}),
            ...(step.idempotencyKey !== undefined ? { idempotencyKey: step.idempotencyKey } : {}),
            ...(step.backendOperationId !== undefined ? { backendOperationId: step.backendOperationId } : {}),
          });
          continue;
        }
        actions.push({
          kind: "reconcile_step",
          operationId,
          stepId: step.stepId,
          capability: step.capability,
          capabilityVersion: step.capabilityVersion,
          effect: step.effect,
          retry: store.retryPolicyFor(step.capability, step.capabilityVersion),
          recorded: Boolean(unknown),
          since: step.startedAt ?? snapshot.updatedAt,
          ...(step.idempotencyKey !== undefined ? { idempotencyKey: step.idempotencyKey } : {}),
          ...(step.backendOperationId !== undefined ? { backendOperationId: step.backendOperationId } : {}),
          ...(dispatchDigest !== undefined ? { dispatchDigest } : {}),
        });
      } else if (
        step.state === "failed" &&
        !deadlinePassed &&
        step.error?.retryable === true
      ) {
        const retry = store.retryPolicyFor(step.capability, step.capabilityVersion);
        const argDigest = store.stepPayloadDigest(operationId, step.stepId);
        if (retry.class === "idempotent" && step.attempts < retry.maxAttempts && argDigest !== undefined) {
          actions.push({
            kind: "retry_candidate",
            operationId,
            stepId: step.stepId,
            capability: step.capability,
            capabilityVersion: step.capabilityVersion,
            retry,
            argDigest,
          });
        }
      } else if (step.state === "outcome_unknown") {
        const unknown = snapshot.unknownOutcomes.find((outcome) => outcome.stepId === step.stepId);
        actions.push({
          kind: "reconcile_step",
          operationId,
          stepId: step.stepId,
          capability: step.capability,
          capabilityVersion: step.capabilityVersion,
          effect: step.effect,
          retry: store.retryPolicyFor(step.capability, step.capabilityVersion),
          recorded: true,
          since: unknown?.since ?? snapshot.updatedAt,
          ...(step.idempotencyKey !== undefined ? { idempotencyKey: step.idempotencyKey } : {}),
          ...(step.backendOperationId !== undefined ? { backendOperationId: step.backendOperationId } : {}),
          ...(unknown?.dispatchDigest !== undefined ? { dispatchDigest: unknown.dispatchDigest } : {}),
        });
      } else if (step.state === "queued" && !deadlinePassed && dependenciesSatisfied(snapshot, step)) {
        actions.push({
          kind: "dispatch_step",
          operationId,
          stepId: step.stepId,
          capability: step.capability,
          capabilityVersion: step.capabilityVersion,
        });
      }
    }
    const recoveryBarrier = actions.some((action) => action.kind === "reconcile_step" || action.kind === "resume_abort");
    const stateBlocksDispatch = ["needs_input", "awaiting_approval", "reconciling", "cancel_requested", "expired"].includes(snapshot.state);
    if (recoveryBarrier || stateBlocksDispatch || deadlinePassed) {
      for (let index = actions.length - 1; index >= 0; index--) {
        if (actions[index].kind === "dispatch_step" || actions[index].kind === "retry_candidate") actions.splice(index, 1);
      }
    }
    for (const decision of snapshot.pendingDecisions) {
      const expired = decision.expiresAt <= options.now;
      actions.push({
        kind: expired ? "expire_decision" : "await_decision",
        operationId,
        decisionId: decision.decisionId,
        stepId: decision.stepId,
        expiresAt: decision.expiresAt,
      });
    }
    if (deadlinePassed && !snapshot.unknownOutcomes.length && snapshot.steps.every((step) => step.state !== "running")) {
      actions.push({ kind: "expire_operation", operationId, deadlineAt: snapshot.deadlineAt as number });
    }
    plans.push({ operationId, state: snapshot.state, actions });
  }
  return { plans, terminal, tornTails: [...store.tornTails] };
}

/** Convenience for callers that only need to know whether unknown effects remain. */
export function pendingReconciliation(store: OperationStore): Array<{ operationId: string; stepIds: string[] }> {
  const pending: Array<{ operationId: string; stepIds: string[] }> = [];
  for (const operationId of store.operationIds()) {
    const snapshot = store.load(operationId);
    if (!isTerminalOperationState(snapshot.state)) {
      const stepIds = snapshot.steps.filter((step) => step.state === "running" || step.state === "outcome_unknown").map((step) => step.stepId);
      if (stepIds.length) pending.push({ operationId, stepIds });
    }
  }
  return pending;
}
