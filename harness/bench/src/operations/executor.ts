import { canonicalDigest, type ExactCall, type JsonValue, type OperationError, type OperationSnapshot, type TrustedActorContext } from "./contracts.ts";
import type { CapabilityDispatchDeps, CapabilityDispatchResult, CapabilityRegistry } from "./capabilities.ts";
import { OperationScheduler, type ScheduledOperationResult, type ScheduledStepResult } from "./scheduler.ts";

export interface ExecutorStore {
  assertOwnership(): void;
  queueStep(operationId: string, input: { key?: string; capability: string; targetRef?: string; dependencies?: string[] }): OperationSnapshot;
  startStep(operationId: string, stepId: string, input: { argDigest: string; idempotencyKey?: string }): OperationSnapshot;
  recordStepOutcome(operationId: string, stepId: string, input: { outcome: "succeeded" | "failed"; evidenceRefs?: string[]; error?: OperationError }): OperationSnapshot;
  markOutcomeUnknown(operationId: string, stepId: string, input?: { summary?: string }): OperationSnapshot;
  reconcileStep(operationId: string, stepId: string, input: { conclusion: "succeeded" | "failed"; evidenceRefs?: string[]; error?: OperationError }): OperationSnapshot;
  cancelStep(operationId: string, stepId: string, input: { evidenceRefs: string[] }): OperationSnapshot;
  requestCancel(operationId: string, input?: { reason?: string }): OperationSnapshot;
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
  deadlineAt?: number;
  dispatch?: CapabilityDispatchDeps;
};

export class OperationExecutor {
  #store: ExecutorStore;
  #registry: Pick<CapabilityRegistry, "get" | "dispatch">;
  #scheduler: OperationScheduler;
  #reconcile?: (input: ReconciliationInput) => Promise<ReconciliationResult>;

  constructor(options: {
    store: ExecutorStore;
    registry: Pick<CapabilityRegistry, "get" | "dispatch">;
    scheduler: OperationScheduler;
    reconcile?: (input: ReconciliationInput) => Promise<ReconciliationResult>;
  }) {
    this.#store = options.store;
    this.#registry = options.registry;
    this.#scheduler = options.scheduler;
    this.#reconcile = options.reconcile;
  }

  async execute(input: ExecuteInput): Promise<ExecutorResult> {
    this.#store.assertOwnership();
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
      const scheduled = await this.#scheduler.submit({
        operationId: input.operationId,
        calls: input.calls,
        descriptor: (name) => this.#registry.get(name),
        maxConcurrentReads: this.#store.load(input.operationId).budgets.maxConcurrentReads,
        maxConcurrentMutations: this.#store.load(input.operationId).budgets.maxConcurrentMutations,
        ...(input.signal ? { signal: input.signal } : {}),
        ...(input.deadlineAt ? { deadlineAt: input.deadlineAt } : {}),
        run: async ({ call, descriptor, args, signal }) => {
          this.#store.assertOwnership();
          const stepId = stepIds.get(call.key)!;
          const argDigest = canonicalDigest(args);
          this.#store.startStep(input.operationId, stepId, { argDigest, idempotencyKey: `${input.operationId}/${stepId}` });
          const outcome = await this.#registry.dispatch(call.capability, args, { ...(input.dispatch ?? {}), signal }, call.capabilityVersion);
          return this.#record(input, call, stepId, descriptor.effect, args, signal, outcome);
        },
      });
      this.#store.settle(input.operationId);
      return this.#aggregate(input.operationId, scheduled);
    } finally {
      input.signal?.removeEventListener("abort", cancellation);
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
      const evidenceRefs = [`${input.operationId}/${stepId}/abort-confirmed`];
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

  #aggregate(operationId: string, scheduled: ScheduledOperationResult): ExecutorResult {
    const snapshot = this.#store.load(operationId);
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
    const state = unknownOutcomes.length ? "reconciling" : outcomes.every((outcome) => outcome === "succeeded") ? "completed" : outcomes.some((outcome) => outcome === "succeeded") ? "partial" : outcomes.every((outcome) => outcome === "cancelled") ? "cancelled" : "failed";
    return { operationId, state, steps, completedEvidence, failures, unknownOutcomes };
  }
}
