import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import { OperationExecutor, type ExecutorStore, type ReconciliationResult } from "../src/operations/executor.ts";
import { OperationScheduler } from "../src/operations/scheduler.ts";
import type { CapabilityDescriptor, ExactCall, JsonValue, OperationError, OperationSnapshot, RecordedDecision, TrustedActorContext } from "../src/operations/contracts.ts";
import { CAPABILITY_CONTRACTS, CapabilityRegistry } from "../src/operations/capabilities.ts";
import type { CapabilityDispatchResult } from "../src/operations/capabilities.ts";
import { OperationStore, type CapabilityMetadata } from "../src/operations/store.ts";
import type { RecoveryAction } from "../src/operations/recovery.ts";
import { capabilityRuntime } from "./operations-runtime-fixture.ts";
import { DispatchAuthority, type DispatchToken } from "../src/operations/dispatch-authority.ts";

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
const APPROVED_WRITE: CapabilityDescriptor = { ...WRITE, approval: { required: "user", payloadDigestRequired: true, binds: ["actor", "tenant", "session", "operation", "step", "payloadDigest", "revision", "policySource", "expiry"] } };
const calls = (...items: Array<Partial<ExactCall> & Pick<ExactCall, "key">>): ExactCall[] => items.map((item) => ({ capability: "read.item", capabilityVersion: "1.0.0", ...item }));
const callForRecovery = (key: string, capability = "read.item"): ExactCall => ({ key, capability, capabilityVersion: "1.0.0" });
const dispatchFor = () => ({});

class MemoryStore implements ExecutorStore {
  authority = new DispatchAuthority();
  log: string[] = [];
  owned = true;
  steps = new Map<string, { state: string; effect: string; evidenceRefs?: string[]; error?: OperationError; attempts?: number }>();
  state: OperationSnapshot["state"] = "accepted";
  revision = 1;
  pendingDecisions: OperationSnapshot["pendingDecisions"] = [];
  deadlineAt?: number;
  settledState?: OperationSnapshot["state"];
  assertOwnership(): void { this.log.push("ownership"); if (!this.owned) throw new Error("not owned"); }
  queueStep(_operationId: string, input: { key?: string; capability: string }): OperationSnapshot { this.log.push(`queue:${input.key}`); this.steps.set(input.key!, { state: "queued", effect: input.capability.startsWith("write") ? "write" : "read" }); return this.snapshot(); }
  startStep(_operationId: string, stepId: string): OperationSnapshot { this.log.push("intent:" + stepId); const step = this.steps.get(stepId)!; step.state = "running"; step.attempts = (step.attempts ?? 0) + 1; return this.snapshot(); }
  retryStep(operationId: string, stepId: string): { snapshot: OperationSnapshot; dispatchToken: DispatchToken } { this.log.push("retry:" + stepId); const step = this.steps.get(stepId)!; step.state = "running"; step.attempts = (step.attempts ?? 0) + 1; return { snapshot: this.snapshot(), dispatchToken: this.authority.issue({ operationId, stepId, capability: step.effect === "write" ? "write.item" : "read.item", version: "1.0.0", payloadDigest: "sha256:" + "0".repeat(64), attempt: step.attempts }) }; }
  recordStepOutcome(_operationId: string, stepId: string, input: { outcome: "succeeded" | "failed"; evidenceRefs?: string[]; error?: OperationError }): OperationSnapshot { this.log.push(`outcome:${stepId}:${input.outcome}`); Object.assign(this.steps.get(stepId)!, { state: input.outcome, evidenceRefs: input.evidenceRefs, error: input.error }); return this.snapshot(); }
  markOutcomeUnknown(_operationId: string, stepId: string): OperationSnapshot { this.log.push(`unknown:${stepId}`); this.steps.get(stepId)!.state = "outcome_unknown"; this.state = "reconciling"; return this.snapshot(); }
  reconcileStep(_operationId: string, stepId: string, input: { conclusion: "succeeded" | "failed"; evidenceRefs?: string[]; error?: OperationError }): OperationSnapshot { this.log.push(`reconcile:${stepId}:${input.conclusion}`); Object.assign(this.steps.get(stepId)!, { state: input.conclusion, evidenceRefs: input.evidenceRefs, error: input.error }); this.state = "running"; return this.snapshot(); }
  cancelStep(_operationId: string, stepId: string, input: { evidenceRefs: string[] }): OperationSnapshot { this.log.push(`cancel:${stepId}`); Object.assign(this.steps.get(stepId)!, { state: "cancelled", evidenceRefs: input.evidenceRefs }); return this.snapshot(); }
  requireDecision(operationId: string, stepId: string, input: { decisionId: string; decisionClass: "user_authorization" | "user_preference"; question: string }): OperationSnapshot { this.log.push(`decision:${stepId}`); this.revision += 1; this.steps.get(stepId)!.state = "awaiting_approval"; this.pendingDecisions = [{ decisionId: input.decisionId, operationId, stepId, decisionClass: input.decisionClass, question: input.question, createdAt: 1, expiresAt: 100_000, revision: this.revision }]; return this.snapshot(); }
  recordDecision(_record: RecordedDecision): OperationSnapshot { this.log.push("decision-recorded"); return this.snapshot(); }
  resume(input: { request: { operationId: string; decisionId: string; resolution: { recordId: string } } }): { outcome: "dispatch"; snapshot: OperationSnapshot; dispatchToken: DispatchToken } { const pending = this.pendingDecisions.find((entry) => entry.decisionId === input.request.decisionId)!; this.log.push(`resume:${pending.stepId}`); this.pendingDecisions = []; const step = this.steps.get(pending.stepId)!; step.state = "running"; step.attempts = (step.attempts ?? 0) + 1; return { outcome: "dispatch", snapshot: this.snapshot(), dispatchToken: this.authority.issue({ operationId: input.request.operationId, stepId: pending.stepId, capability: "write.item", version: "1.0.0", payloadDigest: "sha256:" + "1".repeat(64), attempt: step.attempts }) }; }
  requestCancel(_operationId: string): OperationSnapshot { this.log.push("cancel-requested"); for (const step of this.steps.values()) if (step.state === "queued") step.state = "cancelled"; return this.snapshot(); }
  expire(): OperationSnapshot { this.log.push("expire"); this.state = "expired"; return this.snapshot(); }
  settle(): { snapshot: OperationSnapshot } { if (this.settledState) this.state = this.settledState; return { snapshot: this.snapshot() }; }
  load(): OperationSnapshot { return this.snapshot(); }
  snapshot(): OperationSnapshot {
    return {
      contractVersion: "v1", operationId: "op-1", revision: this.revision, state: this.state, createdAt: 1, updatedAt: 1,
      ...(this.deadlineAt !== undefined ? { deadlineAt: this.deadlineAt } : {}),
      actor: { actorId: context.actorId, tenantId: context.tenantId, sessionId: context.sessionId, turnId: context.turnId }, scope: context.scope,
      request: { instruction: "test" }, requestDigest: "sha256:" + "0".repeat(64), dedupeKey: "key",
      budgets: { maxSteps: 12, maxSelectionRounds: 3, maxGenerationCalls: 2, maxConcurrentReads: 4, maxConcurrentMutations: 2, operationDeadlineMs: 60_000, handleWithinMs: 2_000, providerTimeoutMs: 1_000, maxGeneratedPayloadBytes: 1, maxTextFileBytes: 1, maxReadSnapshotBytes: 1 },
      steps: [...this.steps.entries()].map(([key, step]) => ({ stepId: key, key, state: step.state as any, capability: step.effect === "write" ? "write.item" : "read.item", capabilityVersion: "1.0.0", effect: step.effect as any, attempts: step.attempts ?? (step.state === "queued" ? 0 : 1), ...(step.evidenceRefs ? { evidenceRefs: step.evidenceRefs } : {}), ...(step.error ? { error: step.error } : {}) })),
      pendingDecisions: this.pendingDecisions, unknownOutcomes: [], usage: { steps: this.steps.size, selectionRounds: 0, generationCalls: 0, attempts: 0 }, lastSequence: this.log.length,
    };
  }
}

function registry(store: MemoryStore, outcomes: CapabilityDispatchResult[], descriptors: Record<string, CapabilityDescriptor> = { "read.item": READ, "write.item": WRITE }): Pick<CapabilityRegistry, "get" | "prepare" | "dispatch"> {
  return {
    get: (name) => descriptors[name],
    prepare: () => ({ ok: true, args: {}, states: {}, descriptor: APPROVED_WRITE }),
    dispatch: async (name, args, deps, version) => {
      store.log.push(`dispatch:${name}:${(args as any).value ?? ""}`);
      assert.equal(version, "1.0.0");
      assert.equal(deps.signal instanceof AbortSignal, true);
      return outcomes.shift()!;
    },
  };
}

function approvedRegistry(store: MemoryStore): Pick<CapabilityRegistry, "get" | "prepare" | "dispatch"> {
  const runtime = capabilityRuntime({ "environment.restore": async () => ({ ok: true, value: {} }) });
  const real = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"], store.authority);
  return {
    get: () => APPROVED_WRITE,
    prepare: (_name, _args, deps) => ({ ok: true, approval: { capability: "write.item", version: "1.0.0", effect: "write", prompt: "Approve the exact write", args: {}, payloadDigest: "sha256:" + "1".repeat(64), expectation: { ...deps.decision!, payloadDigest: "sha256:" + "1".repeat(64), now: 10 } } }),
    dispatch: async (name, _args, deps) => {
      const token = deps.dispatchToken;
      const allowed = token !== undefined && store.authority.consume(token, { operationId: deps.dispatchOperationId!, stepId: deps.dispatchStepId!, capability: name, version: "1.0.0", payloadDigest: "sha256:" + "1".repeat(64), attempt: deps.dispatchAttempt! });
      if (!allowed) return { outcome: "refused", capability: name, version: "1.0.0", code: "permission_denied", reason: "unauthorized" };
      store.log.push(`dispatch:${name}`);
      void real;
      void runtime;
      return { outcome: "completed", capability: name, version: "1.0.0", result: {} };
    },
  };
}

test("persists intent before trusted O02 dispatch and keeps actor context out of arguments", async () => {
  const store = new MemoryStore();
  let trustedContext: TrustedActorContext | undefined;
  const executor = new OperationExecutor({ store, registry: registry(store, [{ outcome: "completed", capability: "write.item", version: "1.0.0", result: { id: "x" } }]), scheduler: new OperationScheduler({ maxConcurrent: 2 }), dispatchFor: (actor) => (trustedContext = actor, {}) });
  await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item", args: { value: "x" } }) });
  assert.equal(trustedContext, context);
  assert.deepEqual(store.log.slice(0, 4), ["ownership", "queue:write", "ownership", "intent:write"]);
  assert.ok(store.log.indexOf("intent:write") < store.log.indexOf("dispatch:write.item:x"));
});

test("approval-required mutations enter running only through the recorded O05 resume", async () => {
  const store = new MemoryStore();
  let approvals = 0;
  const reg = approvedRegistry(store);
  const executor = new OperationExecutor({
    store,
    registry: reg,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: () => ({ approve: async ({ expectation }) => (approvals += 1, {
      recordId: "record-1", operationId: expectation.operationId, stepId: expectation.stepId,
      decisionId: expectation.decisionId, decisionClass: expectation.decisionClass as "user_authorization",
      actorId: expectation.actorId, tenantId: expectation.tenantId, sessionId: expectation.sessionId,
      payloadDigest: expectation.payloadDigest, revision: expectation.revision, policySource: "user_ui",
      outcome: "granted", recordedAt: expectation.now, expiresAt: expectation.now + 1,
    }) }),
  });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item" }) });
  assert.equal(result.state, "completed", JSON.stringify(result));
  assert.equal(approvals, 1);
  assert.equal(store.log.includes("intent:write"), false);
  assert.ok(store.log.indexOf("decision:write") < store.log.indexOf("resume:write"));
  assert.ok(store.log.indexOf("resume:write") < store.log.indexOf("dispatch:write.item"));
});

test("real O02 dispatch and O05 store enforce actor-bound approval before mutation", async () => {
  const authority = new DispatchAuthority();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"], authority);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "operation-executor-"));
  const ownership = { ownerId: "executor-test", assertHeld: () => {} };
  const metadata = (name: string, version: string): CapabilityMetadata | undefined => {
    const found = registry.get(name);
    return found && (!version || found.version === version) ? { version: found.version, effect: found.effect, approval: found.approval.required, retry: found.retry, resourceKeys: found.resourceAccess.conflictKeys } : undefined;
  };
  const store = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  const accepted = store.accept({ request: { instruction: "Restore devstack to snap-1" }, context }).snapshot;
  store.beginResolution(accepted.operationId);
  let mutations = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => (mutations += 1, { ok: true, value: { id: "devstack", state: "ready" } }) });
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: (actor) => ({
      runtime,
      allowedPolicySources: ["user_ui"],
      approve: async ({ expectation }) => {
        assert.equal(actor, context);
        const recordedAt = Date.now();
        return {
          recordId: "record-real", operationId: expectation.operationId, stepId: expectation.stepId,
          decisionId: expectation.decisionId, decisionClass: expectation.decisionClass as "user_authorization",
          actorId: expectation.actorId, tenantId: expectation.tenantId, sessionId: expectation.sessionId,
          payloadDigest: expectation.payloadDigest, revision: expectation.revision, policySource: "user_ui",
          outcome: "granted", recordedAt, expiresAt: Math.min(recordedAt + 1_000, expectation.expiryBound),
        };
      },
    }),
  });
  const result = await executor.execute({ operationId: accepted.operationId, context, calls: [{ key: "restore", capability: "environment.restore", capabilityVersion: "1.0.0", args: { id: "devstack", snapshot: "snap-1" } }] });
  assert.equal(result.state, "completed", JSON.stringify(result));
  assert.equal(mutations, 1);
  const snapshot = store.load(accepted.operationId);
  assert.equal(snapshot.steps[0]?.state, "succeeded");
  assert.equal(store.recordedDecisions(accepted.operationId)[0]?.usedAt !== undefined, true);
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
    dispatchFor,
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
  const executor = new OperationExecutor({ store, registry: registry(store, [{ outcome: "failed", capability: "write.item", version: "1.0.0", code: "unknown_outcome", error: { code: "unknown_outcome", message: "lost", retryable: false } }]), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor, reconcile: async () => ({ conclusion: "unknown" }) });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item" }) });
  assert.equal(result.state, "reconciling");
  assert.deepEqual(result.unknownOutcomes, ["write"]);
});

test("preserves external version conflicts and exact partial aggregation without rollback", async () => {
  const store = new MemoryStore();
  const executor = new OperationExecutor({ store, registry: registry(store, [
    { outcome: "completed", capability: "write.item", version: "1.0.0", result: { id: "x" } },
    { outcome: "failed", capability: "write.item", version: "1.0.0", code: "revision_conflict", error: { code: "revision_conflict", message: "version changed", retryable: false } },
  ]), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
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
    return { outcome: "failed", capability: name, version: "1.0.0", code: "cancelled", error: { code: "cancelled", message: "aborted before effect", retryable: false, refs: ["adapter:no-effect"] } };
  };
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  const running = executor.execute({ operationId: "op-1", context, signal: controller.signal, calls: calls({ key: "done" }, { key: "running" }, { key: "queued" }) });
  while (dispatches < 2) await new Promise((resolve) => setTimeout(resolve, 0));
  controller.abort();
  const result = await running;
  assert.equal(result.state, "partial");
  assert.deepEqual(result.completedEvidence, ["op-1/done/result"]);
  assert.deepEqual(result.steps.map((step) => step.outcome), ["succeeded", "cancelled", "cancelled"]);
  assert.ok(store.log.includes("cancel:running"));
});

test("an abort race never turns a mutation failure into fabricated cancellation evidence", async () => {
  const store = new MemoryStore();
  const controller = new AbortController();
  const reg = registry(store, []);
  reg.dispatch = async (name, _args, deps) => {
    await new Promise<void>((resolve) => deps.signal?.addEventListener("abort", () => resolve(), { once: true }));
    return { outcome: "failed", capability: name, version: "1.0.0", code: "provider_failure", error: { code: "provider_failure", message: "connection closed", retryable: true } };
  };
  const reconciled: string[] = [];
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor, reconcile: async ({ stepId }) => (reconciled.push(stepId), { conclusion: "unknown" }) });
  const running = executor.execute({ operationId: "op-1", context, signal: controller.signal, calls: calls({ key: "write", capability: "write.item" }) });
  while (!store.log.includes("intent:write")) await new Promise((resolve) => setTimeout(resolve, 0));
  controller.abort();
  const result = await running;
  assert.equal(result.state, "reconciling");
  assert.deepEqual(reconciled, ["write"]);
  assert.equal(store.log.some((entry) => entry.startsWith("cancel:write")), false);
});


test("a read failure racing with abort remains the authoritative failure", async () => {
  const store = new MemoryStore();
  const controller = new AbortController();
  const reg = registry(store, []);
  reg.dispatch = async (name, _args, deps) => {
    await new Promise<void>((resolve) => deps.signal?.addEventListener("abort", () => resolve(), { once: true }));
    return { outcome: "failed", capability: name, version: "1.0.0", code: "provider_failure", error: { code: "provider_failure", message: "connection closed", retryable: false } };
  };
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  const running = executor.execute({ operationId: "op-1", context, signal: controller.signal, calls: calls({ key: "read" }) });
  while (!store.log.includes("intent:read")) await new Promise((resolve) => setTimeout(resolve, 0));
  controller.abort();
  const result = await running;
  assert.equal(result.state, "failed");
  assert.equal(store.steps.get("read")?.error?.code, "provider_failure");
  assert.equal(store.log.some((entry) => entry.startsWith("cancel:read")), false);
});

test("exclusive bench ownership is checked before queueing and every dispatch", async () => {
  const store = new MemoryStore();
  store.owned = false;
  const executor = new OperationExecutor({ store, registry: registry(store, []), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  await assert.rejects(executor.execute({ operationId: "op-1", context, calls: calls({ key: "read" }) }), /not owned/);
  assert.deepEqual(store.log, ["ownership"]);
});

test("validates the complete plan before persisting any step", async () => {
  const store = new MemoryStore();
  const executor = new OperationExecutor({ store, registry: registry(store, []), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  await assert.rejects(executor.execute({ operationId: "op-1", context, calls: calls(
    { key: "a", dependsOn: ["b"] },
    { key: "b", dependsOn: ["a"] },
  ) }), /cycle/);
  assert.deepEqual(store.log, ["ownership"]);
  assert.equal(store.steps.size, 0);
});

test("uses the durable deadline and persists expiry", async () => {
  const store = new MemoryStore();
  store.deadlineAt = Date.now() - 1;
  const reg = registry(store, []);
  reg.dispatch = async (name, _args, deps) => {
    await new Promise<void>((resolve) => deps.signal?.addEventListener("abort", () => resolve(), { once: true }));
    return { outcome: "failed", capability: name, version: "1.0.0", code: "cancelled", error: { code: "cancelled", message: "no effect", retryable: false, refs: ["adapter:no-effect"] } };
  };
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  await assert.rejects(executor.execute({ operationId: "op-1", context, calls: calls({ key: "read" }) }), /deadline has passed/);
  assert.ok(store.log.includes("expire"));
});


test("a running durable deadline records cancellation intent before aborting work", async () => {
  const store = new MemoryStore();
  store.deadlineAt = Date.now() + 20;
  const reg = registry(store, []);
  reg.dispatch = async (name, _args, deps) => {
    await new Promise<void>((resolve) => deps.signal?.addEventListener("abort", () => resolve(), { once: true }));
    return { outcome: "failed", capability: name, version: "1.0.0", code: "provider_failure", error: { code: "provider_failure", message: "connection closed", retryable: true } };
  };
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor, reconcile: async () => ({ conclusion: "unknown" }) });
  await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item" }) });
  assert.ok(store.log.includes("cancel-requested"));
  assert.ok(store.log.indexOf("cancel-requested") < store.log.indexOf("unknown:write"));
});

test("returns the authoritative durable settlement instead of scheduler-local state", async () => {
  const store = new MemoryStore();
  store.settledState = "failed";
  const executor = new OperationExecutor({ store, registry: registry(store, [{ outcome: "completed", capability: "read.item", version: "1.0.0", result: {} }]), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "read" }) });
  assert.equal(result.state, "failed");
});

test("retries idempotent failures only after O05 retryStep authorizes the exact payload", async () => {
  const store = new MemoryStore();
  const reg = registry(store, [
    { outcome: "failed", capability: "read.item", version: "1.0.0", code: "provider_failure", error: { code: "provider_failure", message: "temporary", retryable: true } },
    { outcome: "completed", capability: "read.item", version: "1.0.0", result: { ok: true } },
  ]);
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "read", args: { value: "same" } }) });
  assert.equal(result.state, "completed");
  assert.equal(store.log.filter((entry) => entry.startsWith("dispatch:read.item")).length, 2);
  assert.ok(store.log.indexOf("outcome:read:failed") < store.log.indexOf("retry:read"));
  assert.ok(store.log.indexOf("retry:read") < store.log.lastIndexOf("dispatch:read.item:same"));
});


test("settles the final retryable failure at the trusted attempt limit", async () => {
  const store = new MemoryStore();
  const reg = registry(store, [
    { outcome: "failed", capability: "read.item", version: "1.0.0", code: "provider_failure", error: { code: "provider_failure", message: "temporary one", retryable: true } },
    { outcome: "failed", capability: "read.item", version: "1.0.0", code: "provider_failure", error: { code: "provider_failure", message: "temporary two", retryable: true } },
  ]);
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "read" }) });
  assert.equal(result.state, "failed");
  assert.equal(store.log.filter((entry) => entry.startsWith("dispatch:read.item")).length, 2);
  assert.equal(store.log.filter((entry) => entry === "retry:read").length, 1);
  assert.equal(store.steps.get("read")?.state, "failed");
});

test("recovery expires operations but defers actions that need authoritative arguments or authorization", async () => {
  const store = new MemoryStore();
  store.steps.set("recover", { state: "outcome_unknown", effect: "write" });
  store.steps.set("retry", { state: "failed", effect: "read", error: { code: "provider_failure", message: "temporary", retryable: true } });
  store.steps.set("queued", { state: "queued", effect: "read" });
  const reg = registry(store, [
    { outcome: "completed", capability: "read.item", version: "1.0.0", result: {} },
    { outcome: "completed", capability: "read.item", version: "1.0.0", result: {} },
  ]);
  const reconciled: string[] = [];
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor, reconcile: async ({ stepId }) => (reconciled.push(stepId), { conclusion: "succeeded", evidenceRefs: ["backend:done"] }) });
  const actions: RecoveryAction[] = [
    { kind: "reconcile_step", operationId: "op-1", stepId: "recover", capability: "write.item", capabilityVersion: "1.0.0", effect: "write", retry: { class: "reconcile_required", maxAttempts: 1 }, recorded: true, since: 1, dispatchDigest: "sha256:" + "1".repeat(64) },
    { kind: "retry_candidate", operationId: "op-1", stepId: "retry", capability: "read.item", capabilityVersion: "1.0.0", retry: { class: "idempotent", maxAttempts: 2 }, argDigest: "sha256:" + "2".repeat(64) },
    { kind: "dispatch_step", operationId: "op-1", stepId: "queued", capability: "read.item", capabilityVersion: "1.0.0" },
    { kind: "expire_operation", operationId: "op-1", deadlineAt: 1 },
  ];
  await executor.recover({ operationId: "op-1", context, actions, calls: {
    recover: callForRecovery("recover", "write.item"), retry: callForRecovery("retry"), queued: callForRecovery("queued"),
  } });
  assert.deepEqual(reconciled, []);
  assert.equal(store.log.includes("retry:retry"), false);
  assert.equal(store.log.includes("intent:queued"), false);
  assert.equal(store.log.some((entry) => entry.startsWith("dispatch:")), false);
  assert.ok(store.log.includes("expire"));
  assert.ok(store.log.filter((entry) => entry === "ownership").length >= actions.length);
});

test("resume_abort never replays the original mutation without an abort adapter", async () => {
  const store = new MemoryStore();
  store.steps.set("running", { state: "running", effect: "write" });
  const reg = registry(store, []);
  let dispatches = 0;
  reg.dispatch = async (name) => {
    dispatches += 1;
    return { outcome: "failed", capability: name, version: "1.0.0", code: "cancelled", error: { code: "cancelled", message: "backend confirms no effect", retryable: false, refs: ["backend:abort-confirmed"] } };
  };
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  await executor.recover({
    operationId: "op-1", context,
    actions: [{ kind: "resume_abort", operationId: "op-1", stepId: "running", capability: "write.item", capabilityVersion: "1.0.0", dispatchDigest: "sha256:" + "1".repeat(64) }],
    calls: { running: callForRecovery("running", "write.item") },
  });
  assert.equal(dispatches, 0);
  assert.equal(store.steps.get("running")?.state, "running");
  assert.equal(store.log.includes("cancel:running"), false);
});
