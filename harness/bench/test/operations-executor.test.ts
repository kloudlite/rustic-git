import assert from "node:assert/strict";
import { test } from "node:test";
import { OperationExecutor, type ExecutorStore, type ReconciliationResult } from "../src/operations/executor.ts";
import { OperationScheduler } from "../src/operations/scheduler.ts";
import type { CapabilityDescriptor, ExactCall, JsonValue, OperationError, OperationSnapshot, TrustedActorContext } from "../src/operations/contracts.ts";
import type { CapabilityDispatchResult, CapabilityRegistry } from "../src/operations/capabilities.ts";

const context: TrustedActorContext = {
  actorId: "actor-1", tenantId: "tenant-1", sessionId: "session-1", turnId: "turn-1", toolCallId: "call-1", turnRevision: 1, scope: { workspaceId: "ws-1" },
};

const descriptor = (effect: CapabilityDescriptor["effect"] = "read", retry: CapabilityDescriptor["retry"]["class"] = "idempotent"): CapabilityDescriptor => ({
  capability: effect === "read" ? "read.item" : "write.item",
  version: "1.0.0",
  title: "item", summary: "item", guide: "item", effect, scope: "workspace",
  inputSchema: { type: "object", additionalProperties: true },
  outputSchema: { type: "object", additionalProperties: true },
  arguments: [], rules: [], limits: {}, examples: [], errors: [],
  retry: { class: retry, maxAttempts: retry === "idempotent" ? 2 : 1, reconciliation: retry === "reconcile_required" ? "preconditions" : "none" },
  approval: { required: "none", payloadDigestRequired: false, binds: [] },
  evidence: { success: ["result"], unknownOutcome: retry === "reconcile_required" ? ["state"] : [] },
  resourceAccess: { reads: [], writes: effect === "read" ? [] : ["workspace.item"], conflictKeys: effect === "read" ? [] : ["workspace.item"], exclusive: false },
  contractVersion: "v1",
});

const READ = descriptor();
const WRITE = descriptor("write", "reconcile_required");
const calls = (...items: Array<Partial<ExactCall> & Pick<ExactCall, "key">>): ExactCall[] => items.map((item) => ({ capability: "read.item", capabilityVersion: "1.0.0", ...item }));

class MemoryStore implements ExecutorStore {
  log: string[] = [];
  owned = true;
  steps = new Map<string, { state: string; effect: string; evidenceRefs?: string[]; error?: OperationError }>();
  state: OperationSnapshot["state"] = "accepted";
  assertOwnership(): void { this.log.push("ownership"); if (!this.owned) throw new Error("not owned"); }
  queueStep(_operationId: string, input: { key?: string; capability: string }): OperationSnapshot { this.log.push(`queue:${input.key}`); this.steps.set(input.key!, { state: "queued", effect: input.capability.startsWith("write") ? "write" : "read" }); return this.snapshot(); }
  startStep(_operationId: string, stepId: string): OperationSnapshot { this.log.push(`intent:${stepId}`); this.steps.get(stepId)!.state = "running"; return this.snapshot(); }
  recordStepOutcome(_operationId: string, stepId: string, input: { outcome: "succeeded" | "failed"; evidenceRefs?: string[]; error?: OperationError }): OperationSnapshot { this.log.push(`outcome:${stepId}:${input.outcome}`); Object.assign(this.steps.get(stepId)!, { state: input.outcome, evidenceRefs: input.evidenceRefs, error: input.error }); return this.snapshot(); }
  markOutcomeUnknown(_operationId: string, stepId: string): OperationSnapshot { this.log.push(`unknown:${stepId}`); this.steps.get(stepId)!.state = "outcome_unknown"; this.state = "reconciling"; return this.snapshot(); }
  reconcileStep(_operationId: string, stepId: string, input: { conclusion: "succeeded" | "failed"; evidenceRefs?: string[]; error?: OperationError }): OperationSnapshot { this.log.push(`reconcile:${stepId}:${input.conclusion}`); Object.assign(this.steps.get(stepId)!, { state: input.conclusion, evidenceRefs: input.evidenceRefs, error: input.error }); return this.snapshot(); }
  cancelStep(_operationId: string, stepId: string, input: { evidenceRefs: string[] }): OperationSnapshot { this.log.push(`cancel:${stepId}`); Object.assign(this.steps.get(stepId)!, { state: "cancelled", evidenceRefs: input.evidenceRefs }); return this.snapshot(); }
  requestCancel(_operationId: string): OperationSnapshot { this.log.push("cancel-requested"); for (const step of this.steps.values()) if (step.state === "queued") step.state = "cancelled"; return this.snapshot(); }
  settle(): { snapshot: OperationSnapshot } { return { snapshot: this.snapshot() }; }
  load(): OperationSnapshot { return this.snapshot(); }
  snapshot(): OperationSnapshot {
    return {
      contractVersion: "v1", operationId: "op-1", revision: 1, state: this.state, createdAt: 1, updatedAt: 1,
      actor: { actorId: context.actorId, tenantId: context.tenantId, sessionId: context.sessionId, turnId: context.turnId }, scope: context.scope,
      request: { instruction: "test" }, requestDigest: "sha256:" + "0".repeat(64), dedupeKey: "key",
      budgets: { maxSteps: 12, maxSelectionRounds: 3, maxGenerationCalls: 2, maxConcurrentReads: 4, maxConcurrentMutations: 2, operationDeadlineMs: 60_000, handleWithinMs: 2_000, providerTimeoutMs: 1_000, maxGeneratedPayloadBytes: 1, maxTextFileBytes: 1, maxReadSnapshotBytes: 1 },
      steps: [...this.steps.entries()].map(([key, step]) => ({ stepId: key, key, state: step.state as any, capability: step.effect === "write" ? "write.item" : "read.item", capabilityVersion: "1.0.0", effect: step.effect as any, attempts: step.state === "queued" ? 0 : 1, ...(step.evidenceRefs ? { evidenceRefs: step.evidenceRefs } : {}), ...(step.error ? { error: step.error } : {}) })),
      pendingDecisions: [], unknownOutcomes: [], usage: { steps: this.steps.size, selectionRounds: 0, generationCalls: 0, attempts: 0 }, lastSequence: this.log.length,
    };
  }
}

function registry(store: MemoryStore, outcomes: CapabilityDispatchResult[], descriptors: Record<string, CapabilityDescriptor> = { "read.item": READ, "write.item": WRITE }): Pick<CapabilityRegistry, "get" | "dispatch"> {
  return {
    get: (name) => descriptors[name],
    dispatch: async (name, args, deps, version) => {
      store.log.push(`dispatch:${name}:${(args as any).value ?? ""}`);
      assert.equal(version, "1.0.0");
      assert.equal(deps.signal instanceof AbortSignal, true);
      return outcomes.shift()!;
    },
  };
}

test("persists intent before trusted O02 dispatch and keeps actor context out of arguments", async () => {
  const store = new MemoryStore();
  const executor = new OperationExecutor({ store, registry: registry(store, [{ outcome: "completed", capability: "write.item", version: "1.0.0", result: { id: "x" } }]), scheduler: new OperationScheduler({ maxConcurrent: 2 }) });
  await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item", args: { value: "x" } }) });
  assert.deepEqual(store.log.slice(0, 4), ["ownership", "queue:write", "ownership", "intent:write"]);
  assert.ok(store.log.indexOf("intent:write") < store.log.indexOf("dispatch:write.item:x"));
});

test("reconciles unknown mutation outcomes before retrying or dispatching dependents", async () => {
  const store = new MemoryStore();
  const reconciled: string[] = [];
  const executor = new OperationExecutor({
    store,
    registry: registry(store, [
      { outcome: "failed", capability: "write.item", version: "1.0.0", code: "unknown_outcome", error: { code: "unknown_outcome", message: "lost response", retryable: false } },
      { outcome: "completed", capability: "read.item", version: "1.0.0", result: {} },
    ]),
    scheduler: new OperationScheduler({ maxConcurrent: 2 }),
    reconcile: async (input): Promise<ReconciliationResult> => (reconciled.push(input.stepId), { conclusion: "succeeded", evidenceRefs: ["backend-version:2"] }),
  });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item" }, { key: "after", dependsOn: ["write"] }) });
  assert.deepEqual(reconciled, ["write"]);
  assert.equal(store.log.filter((entry) => entry.startsWith("dispatch:write.item")).length, 1);
  assert.ok(store.log.indexOf("reconcile:write:succeeded") < store.log.indexOf("dispatch:read.item:"));
  assert.equal(result.state, "completed");
});

test("leaves inconclusive mutation reconciliation observable", async () => {
  const store = new MemoryStore();
  const executor = new OperationExecutor({ store, registry: registry(store, [{ outcome: "failed", capability: "write.item", version: "1.0.0", code: "unknown_outcome", error: { code: "unknown_outcome", message: "lost", retryable: false } }]), scheduler: new OperationScheduler({ maxConcurrent: 1 }), reconcile: async () => ({ conclusion: "unknown" }) });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item" }) });
  assert.equal(result.state, "reconciling");
  assert.deepEqual(result.unknownOutcomes, ["write"]);
});

test("preserves external version conflicts and exact partial aggregation without rollback", async () => {
  const store = new MemoryStore();
  const executor = new OperationExecutor({ store, registry: registry(store, [
    { outcome: "completed", capability: "write.item", version: "1.0.0", result: { id: "x" } },
    { outcome: "failed", capability: "write.item", version: "1.0.0", code: "revision_conflict", error: { code: "revision_conflict", message: "version changed", retryable: false } },
  ]), scheduler: new OperationScheduler({ maxConcurrent: 1 }) });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "first", capability: "write.item" }, { key: "conflict", capability: "write.item" }, { key: "blocked", dependsOn: ["conflict"] }) });
  assert.equal(result.state, "partial");
  assert.deepEqual(result.steps.map((step) => [step.key, step.outcome]), [["first", "succeeded"], ["conflict", "failed"], ["blocked", "skipped"]]);
  assert.equal(result.failures[0]?.code, "revision_conflict");
  assert.equal(store.log.some((entry) => entry.includes("rollback")), false);
});

test("abort propagation preserves committed evidence and reports cancelled remainder", async () => {
  const store = new MemoryStore();
  const controller = new AbortController();
  let dispatches = 0;
  const reg = registry(store, []);
  reg.dispatch = async (name, _args, deps) => {
    dispatches += 1;
    if (dispatches === 1) return { outcome: "completed", capability: name, version: "1.0.0", result: { done: true } };
    await new Promise<void>((resolve) => deps.signal?.addEventListener("abort", () => resolve(), { once: true }));
    return { outcome: "failed", capability: name, version: "1.0.0", code: "cancelled", error: { code: "cancelled", message: "aborted", retryable: false } };
  };
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }) });
  const running = executor.execute({ operationId: "op-1", context, signal: controller.signal, calls: calls({ key: "done" }, { key: "running" }, { key: "queued" }) });
  while (dispatches < 2) await new Promise((resolve) => setTimeout(resolve, 0));
  controller.abort();
  const result = await running;
  assert.equal(result.state, "partial");
  assert.deepEqual(result.completedEvidence, ["op-1/done/result"]);
  assert.deepEqual(result.steps.map((step) => step.outcome), ["succeeded", "cancelled", "cancelled"]);
});

test("exclusive bench ownership is checked before queueing and every dispatch", async () => {
  const store = new MemoryStore();
  store.owned = false;
  const executor = new OperationExecutor({ store, registry: registry(store, []), scheduler: new OperationScheduler({ maxConcurrent: 1 }) });
  await assert.rejects(executor.execute({ operationId: "op-1", context, calls: calls({ key: "read" }) }), /not owned/);
  assert.deepEqual(store.log, ["ownership"]);
});
