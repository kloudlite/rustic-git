import { canonicalDigest, type ExactCall, type JsonValue, type OperationError, type OperationSnapshot, type RecordedDecision, type TrustedActorContext } from "./contracts.ts";
import type { CapabilityApprovalRequest, CapabilityDispatchDeps, CapabilityDispatchResult, CapabilityRegistry } from "./capabilities.ts";
import { OperationScheduler, SchedulerValidationError, validateSchedulePlan, type ScheduledOperationResult, type ScheduledStepResult } from "./scheduler.ts";
import { DEFAULT_DECISION_TTL_MS } from "./store.ts";
import type { RecoveryAction } from "./recovery.ts";

export interface ExecutorStore {
  assertOwnership(): void;
  queueStep(operationId: string, input: { key?: string; capability: string; targetRef?: string; dependencies?: string[] }): OperationSnapshot;
  startStep(operationId: string, stepId: string, input: { argDigest: string; idempotencyKey?: string }): OperationSnapshot;
  recordStepOutcome(operationId: string, stepId: string, input: { outcome: "succeeded" | "failed"; evidenceRefs?: string[]; error?: OperationError }): OperationSnapshot;
  retryStep(operationId: string, stepId: string, input: { retry: { class: "none" | "idempotent" | "reconcile_required"; maxAttempts: number }; argDigest: string; idempotencyKey?: string }, currentContext: TrustedActorContext): OperationSnapshot;
  markOutcomeUnknown(operationId: string, stepId: string, input?: { summary?: string }): OperationSnapshot;
  reconcileStep(operationId: string, stepId: string, input: { conclusion: "succeeded" | "failed"; evidenceRefs?: string[]; error?: OperationError }): OperationSnapshot;
  cancelStep(operationId: string, stepId: string, input: { evidenceRefs: string[] }): OperationSnapshot;
  requireDecision(operationId: string, stepId: string, input: { decisionId: string; decisionClass: "user_authorization" | "user_preference"; question: string; payloadDigest: string }): OperationSnapshot;
  recordDecision(record: RecordedDecision, currentContext: TrustedActorContext): OperationSnapshot;
  resume(input: { request: { action: "resume"; operationId: string; decisionId: string; expectedRevision: number; resolution: { kind: "recorded_user_decision"; recordId: string } }; context: TrustedActorContext }): { outcome: "dispatch" | "refuse_step" | "supply_input"; snapshot: OperationSnapshot };
  requestCancel(operationId: string, input?: { reason?: string }): OperationSnapshot;
  expire(operationId: string): OperationSnapshot;
  settle(operationId: string): { snapshot: OperationSnapshot };
  load(operationId: string): OperationSnapshot;
}

export type ReconciliationResult =
  | { conclusion: "succeeded"; evidenceRefs: string[] }
  | { conclusion: "failed"; error: OperationError }
  | { conclusion: "unknown" };

export type ReconciliationInput = {
  operationId: string;
  stepId: string;
  call: ExactCall;
  args: Record<string, JsonValue>;
  context: TrustedActorContext;
  signal: AbortSignal;
};

export type ExecutorStepResult = { key: string; outcome: "succeeded" | "failed" | "skipped" | "cancelled" | "unknown" };
export type ExecutorResult = {
  operationId: string;
  state: "completed" | "partial" | "failed" | "cancelled" | "reconciling";
  steps: ExecutorStepResult[];
  completedEvidence: string[];
  failures: OperationError[];
  unknownOutcomes: string[];
};

export type ExecuteInput = {
  operationId: string;
  context: TrustedActorContext;
  calls: readonly ExactCall[];
  signal?: AbortSignal;
};

export type RecoverInput = {
  operationId: string;
  context: TrustedActorContext;
  actions: readonly RecoveryAction[];
  calls: Readonly<Record<string, ExactCall>>;
  signal?: AbortSignal;
};

export class OperationExecutor {
  #store: ExecutorStore;
  #registry: Pick<CapabilityRegistry, "get" | "dispatch">;
  #scheduler: OperationScheduler;
  #dispatchFor: (context: TrustedActorContext) => CapabilityDispatchDeps;
  #reconcile?: (input: ReconciliationInput) => Promise<ReconciliationResult>;

  constructor(options: {
    store: ExecutorStore;
    registry: Pick<CapabilityRegistry, "get" | "dispatch">;
    scheduler: OperationScheduler;
    dispatchFor: (context: TrustedActorContext) => CapabilityDispatchDeps;
    reconcile?: (input: ReconciliationInput) => Promise<ReconciliationResult>;
  }) {
    this.#store = options.store;
    this.#registry = options.registry;
    this.#scheduler = options.scheduler;
    this.#dispatchFor = options.dispatchFor;
    this.#reconcile = options.reconcile;
  }

  async execute(input: ExecuteInput): Promise<ExecutorResult> {
    this.#store.assertOwnership();
    const plan = validateSchedulePlan(input.calls, (name) => this.#registry.get(name));
    if (!plan.ok) throw new SchedulerValidationError("validation_failure", plan.issues.map((entry) => entry.message).join("; "), plan.issues);
    const stepIds = new Map<string, string>();
    for (const call of input.calls) {
      const snapshot = this.#store.queueStep(input.operationId, { key: call.key, capability: call.capability, ...(call.targetRef ? { targetRef: call.targetRef } : {}), ...(call.dependsOn?.length ? { dependencies: call.dependsOn } : {}) });
      const queued = snapshot.steps.find((step) => step.key === call.key);
      if (!queued) throw new Error(`the durable store did not record step ${call.key}`);
      stepIds.set(call.key, queued.stepId);
    }
    const cancellation = () => this.#store.requestCancel(input.operationId, { reason: "executor abort" });
    input.signal?.addEventListener("abort", cancellation, { once: true });
    try {
      const deadlineAt = this.#store.load(input.operationId).deadlineAt;
      let scheduled: ScheduledOperationResult;
      try {
        scheduled = await this.#scheduler.submit({
        operationId: input.operationId,
        calls: input.calls,
        descriptor: (name) => this.#registry.get(name),
        maxConcurrentReads: this.#store.load(input.operationId).budgets.maxConcurrentReads,
        maxConcurrentMutations: this.#store.load(input.operationId).budgets.maxConcurrentMutations,
        ...(input.signal ? { signal: input.signal } : {}),
        ...(deadlineAt !== undefined ? { deadlineAt } : {}),
        run: async ({ call, descriptor, args, signal }) => {
          this.#store.assertOwnership();
          const stepId = stepIds.get(call.key)!;
          const argDigest = canonicalDigest(args);
          const deps = this.#dispatchFor(input.context);
          let dispatchDeps: CapabilityDispatchDeps = { ...deps, signal };
          let approvalOutcome: "dispatch" | "refuse_step" | undefined;
          if (descriptor.effect === "read" || descriptor.approval.required === "none") {
            this.#store.startStep(input.operationId, stepId, { argDigest, idempotencyKey: `${input.operationId}/${stepId}` });
          } else {
            const decisionId = `approval-${stepId}`;
            const expectedRevision = this.#store.load(input.operationId).revision + 1;
            const approve = deps.approve;
            const approveWithDecision = async (request: CapabilityApprovalRequest) => {
              const waiting = this.#store.requireDecision(input.operationId, stepId, {
                decisionId,
                decisionClass: descriptor.approval.required === "user" ? "user_authorization" : "user_preference",
                question: request.prompt,
                payloadDigest: request.payloadDigest,
              });
              if (!approve) throw new Error("approval bridge unavailable");
              const record = await approve(request);
              this.#store.recordDecision(record, input.context);
              const resumed = this.#store.resume({
                request: { action: "resume", operationId: input.operationId, decisionId, expectedRevision: waiting.revision, resolution: { kind: "recorded_user_decision", recordId: record.recordId } },
                context: input.context,
              });
              approvalOutcome = resumed.outcome === "dispatch" ? "dispatch" : "refuse_step";
              return record;
            };
            const snapshot = this.#store.load(input.operationId);
            dispatchDeps = { ...deps, signal, approve: approveWithDecision, decision: {
              actorId: input.context.actorId,
              tenantId: input.context.tenantId,
              sessionId: input.context.sessionId,
              operationId: input.operationId,
              stepId,
              decisionId,
              decisionClass: descriptor.approval.required === "user" ? "user_authorization" : "user_preference",
              revision: expectedRevision,
              expiryBound: Math.min(Date.now() + DEFAULT_DECISION_TTL_MS, snapshot.deadlineAt ?? Number.MAX_SAFE_INTEGER),
            } };
          }
          let outcome = await this.#registry.dispatch(call.capability, args, dispatchDeps, call.capabilityVersion);
          if (approvalOutcome === "refuse_step") return { outcome: "skipped" };
          if (descriptor.effect !== "read" && descriptor.approval.required !== "none" && approvalOutcome !== "dispatch") {
            this.#store.startStep(input.operationId, stepId, { argDigest, idempotencyKey: `${input.operationId}/${stepId}` });
          }
          while (outcome.outcome === "failed" && outcome.error.retryable && descriptor.retry.class === "idempotent") {
            this.#store.recordStepOutcome(input.operationId, stepId, { outcome: "failed", error: outcome.error });
            const step = this.#store.load(input.operationId).steps.find((entry) => entry.stepId === stepId);
            if (!step || step.attempts >= descriptor.retry.maxAttempts) break;
            this.#store.retryStep(input.operationId, stepId, { retry: descriptor.retry, argDigest, idempotencyKey: `${input.operationId}/${stepId}` }, input.context);
            outcome = await this.#registry.dispatch(call.capability, args, dispatchDeps, call.capabilityVersion);
          }
          return this.#record(input, call, stepId, descriptor.effect, args, signal, outcome);
        },
        });
      } catch (error) {
        if (error instanceof SchedulerValidationError && error.code === "deadline_exceeded") {
          this.#store.expire(input.operationId);
        }
        throw error;
      }
      const settled = deadlineAt !== undefined && Date.now() >= deadlineAt ? this.#store.expire(input.operationId) : this.#store.settle(input.operationId).snapshot;
      return this.#aggregate(settled, scheduled);
    } finally {
      input.signal?.removeEventListener("abort", cancellation);
    }
  }

  async recover(input: RecoverInput): Promise<void> {
    const signal = input.signal ?? new AbortController().signal;
    for (const action of input.actions) {
      this.#store.assertOwnership();
      if (action.operationId !== input.operationId) throw new Error(`recovery action belongs to ${action.operationId}`);
      if (action.kind === "await_decision" || action.kind === "dispatch_step" || action.kind === "retry_candidate" || action.kind === "resume_abort") continue;
      if (action.kind === "expire_decision" || action.kind === "expire_operation") {
        this.#store.expire(input.operationId);
        continue;
      }
      const call = input.calls[action.stepId];
      if (!call) throw new Error(`missing trusted recovery call for ${action.stepId}`);
      const descriptor = this.#registry.get(action.capability);
      if (!descriptor || descriptor.version !== action.capabilityVersion) throw new Error(`stale recovery capability ${action.capability}@${action.capabilityVersion}`);
      const args = call.args ?? {};
      if (action.kind === "reconcile_step") {
        if (!this.#reconcile) continue;
        const conclusion = await this.#reconcile({ operationId: input.operationId, stepId: action.stepId, call, args, context: input.context, signal });
        if (conclusion.conclusion !== "unknown") this.#store.reconcileStep(input.operationId, action.stepId, conclusion);
        continue;
      }
      const digest = action.kind === "retry_candidate" ? action.argDigest : canonicalDigest(args);
      if (action.kind === "retry_candidate") {
        this.#store.retryStep(input.operationId, action.stepId, { retry: action.retry, argDigest: digest, idempotencyKey: `${input.operationId}/${action.stepId}` }, input.context);
      } else {
        this.#store.startStep(input.operationId, action.stepId, { argDigest: digest, idempotencyKey: `${input.operationId}/${action.stepId}` });
      }
      const outcome = await this.#registry.dispatch(action.capability, args, { ...this.#dispatchFor(input.context), signal }, action.capabilityVersion);
      await this.#record({ operationId: input.operationId, context: input.context, calls: [call], ...(input.signal ? { signal: input.signal } : {}) }, call, action.stepId, descriptor.effect, args, signal, outcome);
    }
  }

  async #record(input: ExecuteInput, call: ExactCall, stepId: string, effect: string, args: Record<string, JsonValue>, signal: AbortSignal, outcome: CapabilityDispatchResult): Promise<ScheduledStepResult> {
    if (outcome.outcome === "completed") {
      const evidenceRefs = [`${input.operationId}/${stepId}/result`];
      this.#store.recordStepOutcome(input.operationId, stepId, { outcome: "succeeded", evidenceRefs });
      return { outcome: "succeeded", value: outcome.result, evidenceRefs };
    }
    const error: OperationError = outcome.outcome === "failed"
      ? outcome.error
      : { code: outcome.code, message: outcome.reason, retryable: false };
    if (error.code === "cancelled" || signal.aborted) {
      if (effect !== "read" && !(error.code === "cancelled" && error.refs?.length)) {
        this.#store.markOutcomeUnknown(input.operationId, stepId);
        if (!this.#reconcile) return { outcome: "unknown" };
        const conclusion = await this.#reconcile({ operationId: input.operationId, stepId, call, args, context: input.context, signal });
        if (conclusion.conclusion === "unknown") return { outcome: "unknown" };
        this.#store.reconcileStep(input.operationId, stepId, conclusion);
        return conclusion.conclusion === "succeeded"
          ? { outcome: "succeeded", value: {}, evidenceRefs: conclusion.evidenceRefs }
          : { outcome: "failed", error: new Error(conclusion.error.message) };
      }
      const evidenceRefs = error.refs ?? [`${input.operationId}/${stepId}/abort-confirmed`];
      this.#store.cancelStep(input.operationId, stepId, { evidenceRefs });
      return { outcome: "cancelled", evidenceRefs };
    }
    if (error.code === "unknown_outcome" && effect !== "read") {
      this.#store.markOutcomeUnknown(input.operationId, stepId);
      if (!this.#reconcile) return { outcome: "unknown" };
      const conclusion = await this.#reconcile({ operationId: input.operationId, stepId, call, args, context: input.context, signal });
      if (conclusion.conclusion === "unknown") return { outcome: "unknown" };
      if (conclusion.conclusion === "succeeded") {
        this.#store.reconcileStep(input.operationId, stepId, conclusion);
        return { outcome: "succeeded", value: {}, evidenceRefs: conclusion.evidenceRefs };
      }
      this.#store.reconcileStep(input.operationId, stepId, conclusion);
      return { outcome: "failed", error: new Error(conclusion.error.message) };
    }
    this.#store.recordStepOutcome(input.operationId, stepId, { outcome: "failed", error });
    return { outcome: "failed", error: new Error(error.message) };
  }

  #aggregate(snapshot: OperationSnapshot, scheduled: ScheduledOperationResult): ExecutorResult {
    const byKey = new Map(snapshot.steps.map((step) => [step.key ?? step.stepId, step]));
    const steps: ExecutorStepResult[] = scheduled.steps.map((step) => {
      const stored = byKey.get(step.key);
      const outcome = step.outcome === "skipped" || (step.outcome === "cancelled" && stored?.state === "queued") ? "skipped" : step.outcome;
      return { key: step.key, outcome };
    });
    const completedEvidence = snapshot.steps.flatMap((step) => step.state === "succeeded" ? step.evidenceRefs ?? [] : []);
    const failures = snapshot.steps.flatMap((step) => step.error ? [step.error] : []);
    const unknownOutcomes = steps.filter((step) => step.outcome === "unknown").map((step) => step.key);
    const outcomes = steps.map((step) => step.outcome);
    const state = snapshot.state === "expired" ? "cancelled" : snapshot.state === "completed" || snapshot.state === "partial" || snapshot.state === "failed" || snapshot.state === "cancelled" || snapshot.state === "reconciling" ? snapshot.state : unknownOutcomes.length ? "reconciling" : outcomes.every((outcome) => outcome === "succeeded") ? "completed" : outcomes.some((outcome) => outcome === "succeeded") ? "partial" : outcomes.every((outcome) => outcome === "cancelled") ? "cancelled" : "failed";
    return { operationId: snapshot.operationId, state, steps, completedEvidence, failures, unknownOutcomes };
  }
}
