/**
 * Real-store coverage of the four paths whose correctness depends on the store
 * accepting or refusing a transition, not merely on which calls the executor made
 * (`RecordingStore` below records call order and validates nothing):
 *  - approval: "an approval survives an independent step settling first (C-2)"
 *  - failure: "a failed dependency leaves a terminal durable operation (C-3 T7)" and
 *    "an exhausted retry keeps the provider's error (C-3 T8)"
 *  - deadline and approval expiry: "C-5 T1: an approval nobody answers ends when the
 *    operation is aborted", "C-5 T2: an approval nobody answers ends at its expiry",
 *    "C-5 T3: an approval that arrives after the abort dispatches nothing", "C-5 T4: a
 *    deadline far away does not fire at once"
 *  - cancellation and tokens: "C-4 T1: a cancel before dispatch revokes the token",
 *    "C-4 T2: a refused outcome call leaves the token alive", "C-4 T2b: cancelStep with
 *    no evidence leaves the token alive", "C-4 T3: a store that cannot record a cancel
 *    still aborts the work, with no uncaught exception", "C-4 T4: a handler that throws
 *    mid-write is unknown_outcome, not a refusal; a forged token is still refused",
 *    "C-4 T5: a bench that lost the folder does not dispatch"
 *  - recovery: "after a restart a running step is reported for reconciliation and left
 *    untouched"
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import { OperationExecutor, type ExecutorStore, type ReconciliationResult } from "../src/operations/executor.ts";
import { planRecovery } from "../src/operations/recovery.ts";
import { OperationScheduler } from "../src/operations/scheduler.ts";
import { canonicalDigest, isTerminalOperationState, selectOutputPath, type CapabilityDescriptor, type ExactCall, type JsonValue, type OperationError, type OperationSnapshot, type RecordedDecision, type TrustedActorContext } from "../src/operations/contracts.ts";
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

// Records the calls the executor makes and validates nothing. Use it ONLY to assert
// call order or the absence of calls; NEVER for behaviour whose correctness depends on
// the store accepting or refusing a transition — those tests build a real
// `OperationStore` on `fs.mkdtempSync` (see the C-2, C-3 and C-4 tests below).
class RecordingStore implements ExecutorStore {
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
  skipStep(_operationId: string, stepId: string): OperationSnapshot { this.log.push(`skip:${stepId}`); Object.assign(this.steps.get(stepId)!, { state: "skipped" }); return this.snapshot(); }
  requireDecision(operationId: string, stepId: string, input: { decisionId: string; decisionClass: "user_authorization" | "user_preference"; question: string }): OperationSnapshot { this.log.push(`decision:${stepId}`); this.revision += 1; this.steps.get(stepId)!.state = "awaiting_approval"; this.pendingDecisions = [{ decisionId: input.decisionId, operationId, stepId, decisionClass: input.decisionClass, question: input.question, createdAt: 1, expiresAt: 100_000, revision: this.revision }]; return this.snapshot(); }
  recordDecision(_record: RecordedDecision): OperationSnapshot { this.log.push("decision-recorded"); return this.snapshot(); }
  resume(input: { request: { operationId: string; decisionId: string; resolution: { recordId: string } } }): { outcome: "dispatch"; snapshot: OperationSnapshot; dispatchToken: DispatchToken } { const pending = this.pendingDecisions.find((entry) => entry.decisionId === input.request.decisionId)!; this.log.push(`resume:${pending.stepId}`); this.pendingDecisions = []; const step = this.steps.get(pending.stepId)!; step.state = "running"; step.attempts = (step.attempts ?? 0) + 1; return { outcome: "dispatch", snapshot: this.snapshot(), dispatchToken: this.authority.issue({ operationId: input.request.operationId, stepId: pending.stepId, capability: "write.item", version: "1.0.0", payloadDigest: "sha256:" + "1".repeat(64), attempt: step.attempts }) }; }
  requestCancel(_operationId: string): OperationSnapshot { this.log.push("cancel-requested"); for (const step of this.steps.values()) if (step.state === "queued") step.state = "cancelled"; return this.snapshot(); }
  expire(): OperationSnapshot { this.log.push("expire"); this.state = "expired"; return this.snapshot(); }
  settle(_operationId?: string, _options?: { settleFailed?: boolean }): { snapshot: OperationSnapshot } { if (this.settledState) this.state = this.settledState; return { snapshot: this.snapshot() }; }
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

function registry(store: RecordingStore, outcomes: CapabilityDispatchResult[], descriptors: Record<string, CapabilityDescriptor> = { "read.item": READ, "write.item": WRITE }): Pick<CapabilityRegistry, "get" | "prepare" | "dispatch"> {
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

function approvedRegistry(store: RecordingStore): Pick<CapabilityRegistry, "get" | "prepare" | "dispatch"> {
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
  const store = new RecordingStore();
  let trustedContext: TrustedActorContext | undefined;
  const executor = new OperationExecutor({ store, registry: registry(store, [{ outcome: "completed", capability: "write.item", version: "1.0.0", result: { id: "x" } }]), scheduler: new OperationScheduler({ maxConcurrent: 2 }), dispatchFor: (actor) => (trustedContext = actor, {}) });
  await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item", args: { value: "x" } }) });
  assert.equal(trustedContext, context);
  assert.deepEqual(store.log.slice(0, 4), ["ownership", "queue:write", "ownership", "intent:write"]);
  assert.ok(store.log.indexOf("intent:write") < store.log.indexOf("dispatch:write.item:x"));
});

test("approval-required mutations enter running only through the recorded O05 resume", async () => {
  const store = new RecordingStore();
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

test("an approval survives an independent step settling first (C-2)", async () => {
  const authority = new DispatchAuthority();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore", "bench.process.list"], authority);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "operation-executor-c2-"));
  const ownership = { ownerId: "executor-test", assertHeld: () => {} };
  const metadata = (name: string, version: string): CapabilityMetadata | undefined => {
    const found = registry.get(name);
    return found && (!version || found.version === version) ? { version: found.version, effect: found.effect, approval: found.approval.required, retry: found.retry, resourceKeys: found.resourceAccess.conflictKeys } : undefined;
  };
  const store = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  const accepted = store.accept({ request: { instruction: "Restore devstack to snap-1 and list bench processes" }, context }).snapshot;
  store.beginResolution(accepted.operationId);
  let mutations = 0;
  let reads = 0;
  let resolveReadSettled!: () => void;
  const readSettledSignal = new Promise<void>((resolve) => {
    resolveReadSettled = resolve;
  });
  const runtime = capabilityRuntime({
    "environment.restore": async () => (mutations += 1, { ok: true, value: { id: "devstack", state: "ready" } }),
    "bench.process.list": async () => (reads += 1, resolveReadSettled(), { ok: true, value: [] }),
  });
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 2 }),
    dispatchFor: () => ({
      runtime,
      allowedPolicySources: ["user_ui"],
      // The prompt is raised (and the pending decision's revision fixed) before the read
      // even starts, but the approval itself resolves only AFTER the independent read has
      // settled — a sibling commit lands on the operation between the prompt and the
      // answer, exactly the C-2 scenario. Without the fix, resume would refuse the
      // now-stale revision and the write would never dispatch.
      approve: async ({ expectation }) => {
        await readSettledSignal;
        const recordedAt = Date.now();
        return {
          recordId: "record-c2", operationId: expectation.operationId, stepId: expectation.stepId,
          decisionId: expectation.decisionId, decisionClass: expectation.decisionClass as "user_authorization",
          actorId: expectation.actorId, tenantId: expectation.tenantId, sessionId: expectation.sessionId,
          payloadDigest: expectation.payloadDigest, revision: expectation.revision, policySource: "user_ui",
          outcome: "granted", recordedAt, expiresAt: Math.min(recordedAt + 60_000, expectation.expiryBound),
        };
      },
    }),
  });
  // "list" is offered first: once the operation raises the "restore" prompt it moves to
  // awaiting_approval, which blocks any OTHER step from starting (`operationChange`
  // refuses "running" from "awaiting_approval"). The read must therefore already be
  // running (and settle) before the prompt is raised for the sibling commit to land
  // between the prompt and the answer, which is the C-2 scenario: a step that already
  // started (and here, finished) while approval was pending, not one starting after.
  const result = await executor.execute({
    operationId: accepted.operationId,
    context,
    calls: [
      { key: "list", capability: "bench.process.list", capabilityVersion: "1.0.0", args: {} },
      { key: "restore", capability: "environment.restore", capabilityVersion: "1.0.0", args: { id: "devstack", snapshot: "snap-1" } },
    ],
  });
  assert.equal(result.state, "completed", JSON.stringify(result));
  assert.equal(mutations, 1);
  assert.equal(reads, 1);
  const snapshot = store.load(accepted.operationId);
  assert.equal(snapshot.steps.find((step) => step.key === "restore")?.state, "succeeded");
  assert.equal(snapshot.steps.find((step) => step.key === "list")?.state, "succeeded");

  // A new store on the same directory constructs and loads a terminal operation: the
  // concurrent progress never poisoned the log.
  const reopened = new OperationStore({ root, ownership: { ownerId: "executor-test-2", assertHeld: () => {} }, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  assert.equal(reopened.load(accepted.operationId).state, "completed");
});

test("reconciles unknown mutation outcomes before retrying or dispatching dependents", async () => {
  const store = new RecordingStore();
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
  const store = new RecordingStore();
  const executor = new OperationExecutor({ store, registry: registry(store, [{ outcome: "failed", capability: "write.item", version: "1.0.0", code: "unknown_outcome", error: { code: "unknown_outcome", message: "lost", retryable: false } }]), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor, reconcile: async () => ({ conclusion: "unknown" }) });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "write", capability: "write.item" }) });
  assert.equal(result.state, "reconciling");
  assert.deepEqual(result.unknownOutcomes, ["write"]);
});

test("preserves external version conflicts and exact partial aggregation without rollback", async () => {
  const store = new RecordingStore();
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
  const store = new RecordingStore();
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
  const store = new RecordingStore();
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
  const store = new RecordingStore();
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
  const store = new RecordingStore();
  store.owned = false;
  const executor = new OperationExecutor({ store, registry: registry(store, []), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  await assert.rejects(executor.execute({ operationId: "op-1", context, calls: calls({ key: "read" }) }), /not owned/);
  assert.deepEqual(store.log, ["ownership"]);
});

test("validates the complete plan before persisting any step", async () => {
  const store = new RecordingStore();
  const executor = new OperationExecutor({ store, registry: registry(store, []), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  await assert.rejects(executor.execute({ operationId: "op-1", context, calls: calls(
    { key: "a", dependsOn: ["b"] },
    { key: "b", dependsOn: ["a"] },
  ) }), /cycle/);
  assert.deepEqual(store.log, ["ownership"]);
  assert.equal(store.steps.size, 0);
});

test("uses the durable deadline and persists expiry", async () => {
  const store = new RecordingStore();
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
  const store = new RecordingStore();
  // The race this test hit under load (flaky at --test-concurrency=4): `deadlineAt` is
  // read once in `execute()` (`deadlineAt > Date.now()`) before the timer is armed. A
  // margin as tight as 20ms could already have elapsed by the time that check runs under
  // event-loop contention, so the timer never arms and `cancel-requested` never happens —
  // not a bug in the code under test, a fixed-margin-vs-real-timer race in the test's own
  // setup. A wider margin makes the "still ahead of Date.now() when execute() reads it"
  // check reliable without polling anything (there is nothing to poll: the assertion
  // below only inspects the log after execute() has already fully resolved).
  store.deadlineAt = Date.now() + 300;
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
  const store = new RecordingStore();
  store.settledState = "failed";
  const executor = new OperationExecutor({ store, registry: registry(store, [{ outcome: "completed", capability: "read.item", version: "1.0.0", result: {} }]), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  const result = await executor.execute({ operationId: "op-1", context, calls: calls({ key: "read" }) });
  assert.equal(result.state, "failed");
});

test("retries idempotent failures only after O05 retryStep authorizes the exact payload", async () => {
  const store = new RecordingStore();
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
  const store = new RecordingStore();
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
  const store = new RecordingStore();
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

test("a failed dependency leaves a terminal durable operation (C-3 T7)", async () => {
  const authority = new DispatchAuthority();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["bench.process.list", "workspace.list"], authority);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "operation-executor-c3-t7-"));
  const ownership = { ownerId: "executor-test", assertHeld: () => {} };
  const metadata = (name: string, version: string): CapabilityMetadata | undefined => {
    const found = registry.get(name);
    return found && (!version || found.version === version) ? { version: found.version, effect: found.effect, approval: found.approval.required, retry: found.retry, resourceKeys: found.resourceAccess.conflictKeys } : undefined;
  };
  const store = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  const accepted = store.accept({ request: { instruction: "List bench processes then workspaces" }, context }).snapshot;
  const runtime = capabilityRuntime({
    "bench.process.list": async () => ({ ok: false, error: { code: "execution_failure", message: "backend unavailable", retryable: false } }),
    "workspace.list": async () => assert.fail("workspace.list must never run: its dependency failed"),
  });
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 2 }),
    dispatchFor: () => ({ runtime }),
  });
  const result = await executor.execute({
    operationId: accepted.operationId,
    context,
    calls: [
      { key: "list", capability: "bench.process.list", capabilityVersion: "1.0.0", args: {} },
      { key: "workspaces", capability: "workspace.list", capabilityVersion: "1.0.0", args: {}, dependsOn: ["list"] },
    ],
  });
  assert.equal(isTerminalOperationState(result.state as never), true, JSON.stringify(result));
  const snapshot = store.load(accepted.operationId);
  assert.equal(isTerminalOperationState(snapshot.state), true);
  assert.equal(snapshot.steps.find((step) => step.key === "list")?.state, "failed");
  assert.equal(snapshot.steps.find((step) => step.key === "workspaces")?.state, "skipped");

  const reopenedStore = new OperationStore({ root, ownership: { ownerId: "executor-test-2", assertHeld: () => {} }, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  assert.equal(reopenedStore.load(accepted.operationId).state, snapshot.state);
});

test("an exhausted retry keeps the provider's error (C-3 T8)", async () => {
  const authority = new DispatchAuthority();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["bench.process.list"], authority);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "operation-executor-c3-t8-"));
  const ownership = { ownerId: "executor-test", assertHeld: () => {} };
  const metadata = (name: string, version: string): CapabilityMetadata | undefined => {
    const found = registry.get(name);
    return found && (!version || found.version === version) ? { version: found.version, effect: found.effect, approval: found.approval.required, retry: found.retry, resourceKeys: found.resourceAccess.conflictKeys } : undefined;
  };
  const store = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  // Spy on recordStepOutcome directly: this is what would detect a third call for the
  // step's terminal attempt even if the resulting invalid_transition throw were later
  // swallowed somewhere upstream (the durable step's own error field is unaffected by a
  // throw either way, since a rejected write never lands).
  let recordStepOutcomeCalls = 0;
  const realRecordStepOutcome = store.recordStepOutcome.bind(store);
  store.recordStepOutcome = ((...args: Parameters<typeof realRecordStepOutcome>) => {
    recordStepOutcomeCalls += 1;
    return realRecordStepOutcome(...args);
  }) as typeof store.recordStepOutcome;
  const accepted = store.accept({ request: { instruction: "List bench processes" }, context }).snapshot;
  let dispatches = 0;
  const runtime = capabilityRuntime({
    "bench.process.list": async () => {
      dispatches += 1;
      return { ok: false, error: { code: "execution_failure", message: `provider says no (attempt ${dispatches})`, retryable: true } };
    },
  });
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: () => ({ runtime }),
  });
  const result = await executor.execute({
    operationId: accepted.operationId,
    context,
    calls: [{ key: "list", capability: "bench.process.list", capabilityVersion: "1.0.0", args: {} }],
  });
  // bench.process.list's declared retry is idempotent/maxAttempts 2: one dispatch, one
  // retry, both fail, then the loop returns directly instead of falling into #record.
  assert.equal(dispatches, 2);
  const failure = result.steps.find((step) => step.key === "list");
  assert.equal(failure?.outcome, "failed");
  assert.equal(result.failures.length, 1, JSON.stringify(result.failures));
  assert.equal(result.failures[0]?.message, "provider says no (attempt 2)");

  const snapshot = store.load(accepted.operationId);
  const step = snapshot.steps.find((entry) => entry.key === "list");
  assert.equal(step?.state, "failed");
  assert.equal(step?.error?.message, "provider says no (attempt 2)");
  assert.equal(isTerminalOperationState(snapshot.state), true);

  // recordStepOutcome was called exactly twice for the two attempts (once per failure),
  // never a third time for the same terminal attempt — a duplicate call throws
  // invalid_transition (the real store has only running -> failed), which #record used
  // to trigger by always recording the loop's own already-recorded last failure again.
  assert.equal(recordStepOutcomeCalls, 2);
  const failedEvents = store.events(accepted.operationId).filter((event) => event.stepId === step?.stepId && event.phase === "failed");
  assert.equal(failedEvents.length, 2);
});

test("resume_abort never replays the original mutation without an abort adapter", async () => {
  const store = new RecordingStore();
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

// C-4: cancellation revokes dispatch authority; validation comes before revocation.
// Real OperationStore + real CapabilityRegistry sharing one DispatchAuthority, same
// pattern as the C-2 test above — never RecordingStore for these.
function realStoreForC4() {
  const authority = new DispatchAuthority();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"], authority);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "operation-executor-c4-"));
  const ownership = { ownerId: "executor-test-c4", owned: true, assertHeld() { if (!this.owned) throw new Error("not owned"); } };
  const metadata = (name: string, version: string): CapabilityMetadata | undefined => {
    const found = registry.get(name);
    return found && (!version || found.version === version) ? { version: found.version, effect: found.effect, approval: found.approval.required, retry: found.retry, resourceKeys: found.resourceAccess.conflictKeys } : undefined;
  };
  const store = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  return { authority, registry, store, ownership };
}

const RESTORE_ARGS = { id: "devstack", snapshot: "snap-1" };
const RESTORE_DIGEST = canonicalDigest(RESTORE_ARGS);

// Drives an operation from accept through a resumed (token-issued) approval-required
// "restore" step without dispatching it, so the test can act on the live token itself.
function acceptAndResumeC4(store: OperationStore, decisionId = "decision-c4") {
  const callContext: TrustedActorContext = { ...context, toolCallId: decisionId };
  const accepted = store.accept({ request: { instruction: "Restore devstack to snap-1" }, context: callContext }).snapshot;
  store.beginResolution(accepted.operationId);
  const queued = store.queueStep(accepted.operationId, { key: "restore", capability: "environment.restore", capabilityVersion: "1.0.0", args: RESTORE_ARGS });
  const stepId = queued.steps[0]!.stepId;
  store.requireDecision(accepted.operationId, stepId, { decisionId, decisionClass: "user_authorization", question: "Restore devstack to snap-1?", payloadDigest: RESTORE_DIGEST });
  const pending = store.load(accepted.operationId).pendingDecisions[0]!;
  const recordedAt = Date.now();
  const record: RecordedDecision = {
    recordId: `record-${decisionId}`, operationId: accepted.operationId, stepId, decisionId: pending.decisionId,
    decisionClass: "user_authorization", actorId: context.actorId, tenantId: context.tenantId, sessionId: context.sessionId,
    payloadDigest: RESTORE_DIGEST, revision: pending.revision, policySource: "user_ui",
    outcome: "granted", recordedAt, expiresAt: recordedAt + 60_000,
  };
  store.recordDecision(record, context);
  const resumed = store.resume({
    request: { action: "resume", operationId: accepted.operationId, decisionId: pending.decisionId, expectedRevision: pending.revision, resolution: { kind: "recorded_user_decision", recordId: record.recordId } },
    context,
  });
  return { operationId: accepted.operationId, stepId, dispatchToken: resumed.dispatchToken };
}

// The `decision` a `registry.dispatch` call needs to authorize an approval-required
// capability: the same shape `dispatchWithPolicy`'s `obtain` closure expects.
function decisionFor(operationId: string, stepId: string, decisionId: string, revision: number) {
  return {
    operationId, stepId, decisionId, decisionClass: "user_authorization" as const,
    actorId: context.actorId, tenantId: context.tenantId, sessionId: context.sessionId,
    turnId: context.turnId, toolCallId: context.toolCallId, turnRevision: 1,
    revision, expiryBound: Date.now() + 60_000,
  };
}

test("C-4 T1: a cancel before dispatch revokes the token", async () => {
  const { registry, store } = realStoreForC4();
  let handled = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => (handled += 1, { ok: true, value: { id: "devstack", state: "ready" } }) });
  const { operationId, stepId, dispatchToken } = acceptAndResumeC4(store);
  store.requestCancel(operationId, { reason: "test" });
  const result = await registry.dispatch(
    "environment.restore",
    RESTORE_ARGS,
    { runtime, decision: decisionFor(operationId, stepId, "decision-c4", store.load(operationId).revision), allowedPolicySources: ["user_ui"], dispatchToken, dispatchOperationId: operationId, dispatchStepId: stepId, dispatchAttempt: 1 },
    "1.0.0",
  );
  assert.equal(result.outcome, "refused", JSON.stringify(result));
  assert.equal(handled, 0);
});

test("C-4 T2: a refused outcome call leaves the token alive", async () => {
  const { registry, store } = realStoreForC4();
  let handled = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => (handled += 1, { ok: true, value: { id: "devstack", state: "ready" } }) });
  const { operationId, stepId, dispatchToken } = acceptAndResumeC4(store);
  assert.throws(() => store.recordStepOutcome(operationId, stepId, { outcome: "succeeded" }));
  const result = await registry.dispatch(
    "environment.restore",
    RESTORE_ARGS,
    { runtime, decision: decisionFor(operationId, stepId, "decision-c4", store.load(operationId).revision), allowedPolicySources: ["user_ui"], dispatchToken, dispatchOperationId: operationId, dispatchStepId: stepId, dispatchAttempt: 1 },
    "1.0.0",
  );
  assert.equal(result.outcome, "completed", JSON.stringify(result));
  assert.equal(handled, 1);
});

test("C-4 T2b: cancelStep with no evidence leaves the token alive", async () => {
  const { registry, store } = realStoreForC4();
  let handled = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => (handled += 1, { ok: true, value: { id: "devstack", state: "ready" } }) });
  const { operationId, stepId, dispatchToken } = acceptAndResumeC4(store);
  assert.throws(() => store.cancelStep(operationId, stepId, { evidenceRefs: [] }));
  const result = await registry.dispatch(
    "environment.restore",
    RESTORE_ARGS,
    { runtime, decision: decisionFor(operationId, stepId, "decision-c4", store.load(operationId).revision), allowedPolicySources: ["user_ui"], dispatchToken, dispatchOperationId: operationId, dispatchStepId: stepId, dispatchAttempt: 1 },
    "1.0.0",
  );
  assert.equal(result.outcome, "completed", JSON.stringify(result));
  assert.equal(handled, 1);
});

test("C-4 T3: a store that cannot record a cancel still aborts the work, with no uncaught exception", async () => {
  const { authority, store } = realStoreForC4();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["bench.process.list"], authority);
  // A plain delegating wrapper, not a Proxy: OperationStore's private (#) fields
  // require `this` to be the real instance, which a Proxy's receiver is not.
  const throwingStore: ExecutorStore = {
    assertOwnership: () => store.assertOwnership(),
    load: (operationId) => store.load(operationId),
    queueStep: (...args) => store.queueStep(...args),
    startStep: (...args) => store.startStep(...args),
    retryStep: (...args) => store.retryStep(...args),
    recordStepOutcome: (...args) => store.recordStepOutcome(...args),
    markOutcomeUnknown: (...args) => store.markOutcomeUnknown(...args),
    reconcileStep: (...args) => store.reconcileStep(...args),
    cancelStep: (...args) => store.cancelStep(...args),
    skipStep: (...args) => store.skipStep(...args),
    requireDecision: (...args) => store.requireDecision(...args),
    recordDecision: (...args) => store.recordDecision(...args),
    resume: (...args) => store.resume(...args),
    requestCancel: () => { throw new Error("cannot write"); },
    expire: (...args) => store.expire(...args),
    settle: (...args) => store.settle(...args),
  };
  const accepted = store.accept({ request: { instruction: "Two independent reads" }, context }).snapshot;
  store.beginResolution(accepted.operationId);
  let secondRan = 0;
  let firstStarted!: () => void;
  const firstStartedSignal = new Promise<void>((resolve) => (firstStarted = resolve));
  const runtime = capabilityRuntime({
    "bench.process.list": async (input) => {
      firstStarted();
      await new Promise<void>((resolve) => input.signal?.addEventListener("abort", () => resolve(), { once: true }));
      return { ok: true, value: [] };
    },
  });
  const executor = new OperationExecutor({
    store: throwingStore,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: () => ({ runtime }),
  });

  const uncaught: unknown[] = [];
  const guard = (error: unknown) => uncaught.push(error);
  process.once("uncaughtException", guard);
  try {
    const controller = new AbortController();
    const execPromise = executor.execute({
      operationId: accepted.operationId,
      context,
      signal: controller.signal,
      calls: [
        { key: "first", capability: "bench.process.list", capabilityVersion: "1.0.0", args: {} },
        { key: "second", capability: "bench.process.list", capabilityVersion: "1.0.0", args: {}, dependsOn: ["first"] },
      ],
    });
    await firstStartedSignal;
    controller.abort();
    const result = await Promise.race([
      execPromise,
      new Promise<never>((_, reject) => setTimeout(() => reject(new Error("execute() did not settle within 3s after abort")), 3000)),
    ]);
    const secondStep = result.steps.find((step) => step.key === "second");
    if (secondStep?.outcome === "succeeded") secondRan = 1;
    assert.equal(secondRan, 0);
    assert.deepEqual(uncaught, []);
  } finally {
    process.removeListener("uncaughtException", guard);
  }
});

test("C-4 T4: a handler that throws mid-write is unknown_outcome, not a refusal; a forged token is still refused", async () => {
  const { registry, store } = realStoreForC4();
  let entered = 0;
  const runtime = capabilityRuntime({
    "environment.restore": async () => {
      entered += 1;
      throw new Error("write started then the connection dropped");
    },
  });
  const { operationId, stepId, dispatchToken } = acceptAndResumeC4(store);
  const decision = decisionFor(operationId, stepId, "decision-c4", store.load(operationId).revision);
  const result = await registry.dispatch(
    "environment.restore",
    RESTORE_ARGS,
    { runtime, decision, allowedPolicySources: ["user_ui"], dispatchToken, dispatchOperationId: operationId, dispatchStepId: stepId, dispatchAttempt: 1 },
    "1.0.0",
  );
  assert.equal(entered, 1);
  assert.equal(result.outcome, "failed", JSON.stringify(result));
  assert.equal((result as any).code, "unknown_outcome", JSON.stringify(result));

  // Mirror: a wrong/forged token never reaches the handler, and is still refused.
  const { operationId: opId2, stepId: stepId2 } = acceptAndResumeC4(store, "decision-c4-2");
  const forgedToken = {} as DispatchToken;
  const decision2 = decisionFor(opId2, stepId2, "decision-c4-2", store.load(opId2).revision);
  const result2 = await registry.dispatch(
    "environment.restore",
    RESTORE_ARGS,
    { runtime, decision: decision2, allowedPolicySources: ["user_ui"], dispatchToken: forgedToken, dispatchOperationId: opId2, dispatchStepId: stepId2, dispatchAttempt: 1 },
    "1.0.0",
  );
  assert.equal(entered, 1, "the handler must not be entered for a forged token");
  assert.equal(result2.outcome, "refused", JSON.stringify(result2));
});

test("C-4 T5: a bench that lost the folder does not dispatch", async () => {
  const { authority, store, ownership } = realStoreForC4();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["bench.process.list"], authority);
  const accepted = store.accept({ request: { instruction: "List bench processes" }, context }).snapshot;
  store.beginResolution(accepted.operationId);
  let handled = 0;
  const runtime = capabilityRuntime({ "bench.process.list": async () => (handled += 1, { ok: true, value: [] }) });
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: () => ({ runtime }),
  });
  // `queueStep` (called from inside `execute`, before dispatch) is the step that loses
  // ownership here: flip it false as soon as the queue is observed, so the first
  // `assertOwnership()` right before dispatch is the one that throws.
  const originalQueueStep = store.queueStep.bind(store);
  store.queueStep = ((...args: Parameters<typeof store.queueStep>) => {
    const result = originalQueueStep(...args);
    ownership.owned = false;
    return result;
  }) as typeof store.queueStep;
  const executePromise = executor.execute({ operationId: accepted.operationId, context, calls: [{ key: "list", capability: "bench.process.list", capabilityVersion: "1.0.0", args: {} }] });
  // Ownership is lost right after queueStep, so the pre-dispatch `assertOwnership()`
  // throws inside the scheduler's run callback; the handler must never have been
  // entered regardless of how that throw ultimately surfaces from execute().
  await assert.rejects(() => Promise.race([
    executePromise,
    new Promise<never>((_, reject) => setTimeout(() => reject(new Error("execute() did not settle within 3s")), 3000)),
  ]));
  assert.equal(handled, 0);
});

// C-5: bounded waits in the executor. Real OperationStore + real CapabilityRegistry
// sharing one DispatchAuthority, same pattern as the C-4 block above.
function realStoreForC5(budgets?: Partial<{ operationDeadlineMs: number }>) {
  const authority = new DispatchAuthority();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"], authority);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "operation-executor-c5-"));
  const ownership = { ownerId: "executor-test-c5", assertHeld: () => {} };
  const metadata = (name: string, version: string): CapabilityMetadata | undefined => {
    const found = registry.get(name);
    return found && (!version || found.version === version) ? { version: found.version, effect: found.effect, approval: found.approval.required, retry: found.retry, resourceKeys: found.resourceAccess.conflictKeys } : undefined;
  };
  const store = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  const callContext: TrustedActorContext = { ...context, toolCallId: `c5-${Math.random()}` };
  const accepted = store.accept({ request: { instruction: "Restore devstack to snap-1" }, context: callContext, ...(budgets ? { budgets } : {}) }).snapshot;
  store.beginResolution(accepted.operationId);
  return { authority, registry, store, callContext, operationId: accepted.operationId };
}

// Bounds a promise that must settle for the test itself to make progress (never the
// production code under test): a hang here fails the test instead of the whole run.
function bounded<T>(promise: Promise<T>, label: string, ms = 3000): Promise<T> {
  return Promise.race([
    promise,
    new Promise<never>((_, reject) => setTimeout(() => reject(new Error(`${label} did not settle within ${ms}ms`)), ms)),
  ]);
}

test("C-5 T1: an approval nobody answers ends when the operation is aborted", async () => {
  const { registry, store, callContext, operationId } = realStoreForC5();
  let handled = 0;
  let approveSignal: AbortSignal | undefined;
  const runtime = capabilityRuntime({ "environment.restore": async () => (handled += 1, { ok: true, value: { id: "devstack", state: "ready" } }) });
  const controller = new AbortController();
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: () => ({
      runtime,
      allowedPolicySources: ["user_ui"],
      approve: (_approval, options) => { approveSignal = options?.signal; return new Promise<RecordedDecision>(() => {}); },
    }),
  });
  const execPromise = executor.execute({ operationId, context: callContext, signal: controller.signal, calls: [{ key: "restore", capability: "environment.restore", capabilityVersion: "1.0.0", args: { id: "devstack", snapshot: "snap-1" } }] });
  setTimeout(() => controller.abort(), 50);
  const result = await bounded(execPromise, "execute()");
  assert.equal(result.state, "cancelled", JSON.stringify(result));
  assert.equal(result.steps.find((step) => step.key === "restore")?.outcome, "cancelled");
  assert.equal(handled, 0);
  assert.equal(approveSignal?.aborted, true);
  const snapshot = store.load(operationId);
  assert.equal(isTerminalOperationState(snapshot.state), true);
});

test("C-5 T2: an approval nobody answers ends at its expiry", async () => {
  const { registry, store, callContext, operationId } = realStoreForC5({ operationDeadlineMs: 1_000 });
  // The operation's deadline (1000ms floor, the schema minimum) outlives the decision's
  // clamp: requireDecision clamps expiresAt to min(now+ttl, deadlineAt), and ttl here is
  // the store's 10-minute default, so the deadline is what actually bounds this wait —
  // set the deadline near-term relative to the 2s guard below.
  let handled = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => (handled += 1, { ok: true, value: { id: "devstack", state: "ready" } }) });
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: () => ({
      runtime,
      allowedPolicySources: ["user_ui"],
      approve: () => new Promise<RecordedDecision>(() => {}),
    }),
  });
  const result = await bounded(
    executor.execute({ operationId, context: callContext, calls: [{ key: "restore", capability: "environment.restore", capabilityVersion: "1.0.0", args: { id: "devstack", snapshot: "snap-1" } }] }),
    "execute()",
    2_000,
  );
  assert.equal(result.steps.find((step) => step.key === "restore")?.outcome, "cancelled", JSON.stringify(result));
  assert.equal(handled, 0);
  const snapshot = store.load(operationId);
  assert.equal(isTerminalOperationState(snapshot.state), true);
});

test("C-5 T3: an approval that arrives after the abort dispatches nothing", async () => {
  const { registry, store, callContext, operationId } = realStoreForC5();
  let handled = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => (handled += 1, { ok: true, value: { id: "devstack", state: "ready" } }) });
  const controller = new AbortController();
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: () => ({
      runtime,
      allowedPolicySources: ["user_ui"],
      approve: async ({ expectation }) => {
        await new Promise((resolve) => setTimeout(resolve, 200));
        const recordedAt = Date.now();
        return {
          recordId: "record-c5-t3", operationId: expectation.operationId, stepId: expectation.stepId,
          decisionId: expectation.decisionId, decisionClass: expectation.decisionClass as "user_authorization",
          actorId: expectation.actorId, tenantId: expectation.tenantId, sessionId: expectation.sessionId,
          payloadDigest: expectation.payloadDigest, revision: expectation.revision, policySource: "user_ui",
          outcome: "granted", recordedAt, expiresAt: recordedAt + 60_000,
        };
      },
    }),
  });
  const execPromise = executor.execute({ operationId, context: callContext, signal: controller.signal, calls: [{ key: "restore", capability: "environment.restore", capabilityVersion: "1.0.0", args: { id: "devstack", snapshot: "snap-1" } }] });
  setTimeout(() => controller.abort(), 50);
  const result = await bounded(execPromise, "execute()");
  assert.equal(result.steps.find((step) => step.key === "restore")?.outcome, "cancelled", JSON.stringify(result));
  assert.equal(handled, 0);
  // The late-arriving approval must never have been recorded: the operation's decision
  // list stays empty after the cancel resolved, even though `approve` resolves 150ms
  // after execute() has already returned.
  await new Promise((resolve) => setTimeout(resolve, 250));
  assert.equal(handled, 0);
  assert.equal(store.recordedDecisions(operationId).length, 0);
});

test("C-5 T4: a deadline far away does not fire at once", async () => {
  // `accept`'s own policy ceiling (store.ts, DEFAULT_BUDGETS.operationDeadlineMs) caps a
  // real operation's deadline at 10 minutes regardless of the wider schema bound, so the
  // real store cannot be given a literal 30-day deadline; this integration test uses the
  // store's maximum allowed deadline to prove the executor's own deadline timer does not
  // fire early on a far-out but reachable value. The deadline can never overflow a timer
  // in the first place (see "an operation deadline can never overflow a timer",
  // operations-contracts.test.ts) since the budget schema caps it at 24h, far below the
  // 2^31-1 ms Node clamps at.
  const { registry, store, callContext, operationId } = realStoreForC5({ operationDeadlineMs: 10 * 60 * 1000 });
  let ran = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => { ran += 1; await new Promise((resolve) => setTimeout(resolve, 100)); return { ok: true, value: { id: "devstack", state: "ready" } }; } });
  const executor = new OperationExecutor({
    store,
    registry,
    scheduler: new OperationScheduler({ maxConcurrent: 1 }),
    dispatchFor: () => ({
      runtime,
      allowedPolicySources: ["user_ui"],
      approve: async ({ expectation }) => {
        const recordedAt = Date.now();
        return {
          recordId: "record-c5-t4", operationId: expectation.operationId, stepId: expectation.stepId,
          decisionId: expectation.decisionId, decisionClass: expectation.decisionClass as "user_authorization",
          actorId: expectation.actorId, tenantId: expectation.tenantId, sessionId: expectation.sessionId,
          payloadDigest: expectation.payloadDigest, revision: expectation.revision, policySource: "user_ui",
          outcome: "granted", recordedAt, expiresAt: recordedAt + 60_000,
        };
      },
    }),
  });
  const result = await bounded(
    executor.execute({ operationId, context: callContext, calls: [{ key: "restore", capability: "environment.restore", capabilityVersion: "1.0.0", args: { id: "devstack", snapshot: "snap-1" } }] }),
    "execute()",
  );
  assert.equal(result.state, "completed", JSON.stringify(result));
  assert.equal(result.steps.find((step) => step.key === "restore")?.outcome, "succeeded");
  assert.equal(ran, 1);
});

test("C-5 T5: a negative or fractional output index is a missing dependency", async () => {
  const outputs = { list: { items: [1, 2, 3] } };
  const negative = selectOutputPath(outputs, { from: "list", output: "items", select: [-1] });
  assert.equal(negative.ok, false);
  assert.equal(negative.ok ? undefined : negative.issues[0]?.code, "unknown_dependency");
  const fractional = selectOutputPath(outputs, { from: "list", output: "items", select: [1.5] });
  assert.equal(fractional.ok, false);
  assert.equal(fractional.ok ? undefined : fractional.issues[0]?.code, "unknown_dependency");
});

// C-6: recovery reports what it deferred instead of dropping it silently. The executor
// team removed recovery dispatch on purpose (unsafe); this task only makes the silence
// visible, so the fake store here is acceptable ONLY because these tests assert the
// ABSENCE of store calls, the same reasoning the two pre-existing recover tests use.
test("C-6 T1: recovery reports every action it deferred", async () => {
  const store = new RecordingStore();
  store.steps.set("recover", { state: "outcome_unknown", effect: "write" });
  store.steps.set("retry", { state: "failed", effect: "read", error: { code: "provider_failure", message: "temporary", retryable: true } });
  store.steps.set("queued", { state: "queued", effect: "read" });
  store.steps.set("resuming", { state: "running", effect: "write" });
  store.steps.set("awaiting", { state: "awaiting_approval", effect: "write" });
  const reg = registry(store, []);
  const reconciled: string[] = [];
  const executor = new OperationExecutor({ store, registry: reg, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor, reconcile: async ({ stepId }) => (reconciled.push(stepId), { conclusion: "succeeded", evidenceRefs: ["backend:done"] }) });
  const actions: RecoveryAction[] = [
    { kind: "reconcile_step", operationId: "op-1", stepId: "recover", capability: "write.item", capabilityVersion: "1.0.0", effect: "write", retry: { class: "reconcile_required", maxAttempts: 1 }, recorded: true, since: 1, dispatchDigest: "sha256:" + "1".repeat(64) },
    { kind: "dispatch_step", operationId: "op-1", stepId: "queued", capability: "read.item", capabilityVersion: "1.0.0" },
    { kind: "resume_abort", operationId: "op-1", stepId: "resuming", capability: "write.item", capabilityVersion: "1.0.0", dispatchDigest: "sha256:" + "3".repeat(64) },
    { kind: "retry_candidate", operationId: "op-1", stepId: "retry", capability: "read.item", capabilityVersion: "1.0.0", retry: { class: "idempotent", maxAttempts: 2 }, argDigest: "sha256:" + "2".repeat(64) },
    { kind: "await_decision", operationId: "op-1", decisionId: "decision-1", stepId: "awaiting", expiresAt: 1_000 },
    { kind: "expire_decision", operationId: "op-1", decisionId: "decision-2", stepId: "awaiting", expiresAt: 1_000 },
    { kind: "expire_operation", operationId: "op-1", deadlineAt: 1 },
  ];
  const outcome = await executor.recover({ operationId: "op-1", context, actions, calls: {
    recover: callForRecovery("recover", "write.item"), retry: callForRecovery("retry"), queued: callForRecovery("queued"),
    resuming: callForRecovery("resuming", "write.item"), awaiting: callForRecovery("awaiting", "write.item"),
  } });
  assert.deepEqual(outcome.handled, [actions[5], actions[6]]);
  assert.deepEqual(outcome.deferred, [actions[0], actions[1], actions[2], actions[3], actions[4]]);
  assert.deepEqual(reconciled, []);
  assert.equal(store.log.includes("retry:retry"), false);
  assert.equal(store.log.includes("intent:queued"), false);
  assert.equal(store.log.some((entry) => entry.startsWith("dispatch:")), false);
  assert.equal(store.log.some((entry) => entry.startsWith("unknown:")), false);
  assert.equal(store.log.some((entry) => entry.startsWith("cancel:")), false);
  assert.equal(store.log.filter((entry) => entry === "expire").length, 2);
});

test("C-6 T2: an action for another operation still throws", async () => {
  const store = new RecordingStore();
  const executor = new OperationExecutor({ store, registry: registry(store, []), scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor });
  await assert.rejects(
    () => executor.recover({ operationId: "op-1", context, actions: [{ kind: "expire_operation", operationId: "op-2", deadlineAt: 1 }], calls: {} }),
    /recovery action belongs to op-2/,
  );
});

test("the recording store is frozen: new executor tests use the real store", () => {
  // N counted after the RecordingStore rename and before adding anything else. Lower it
  // when a test moves to the real store; never raise it.
  const N = 19;
  const source = fs.readFileSync(new URL(import.meta.url), "utf8");
  // Built from two literals so this line does not count itself.
  const needle = "new Recording" + "Store(";
  const occurrences = source.split(needle).length - 1;
  assert.ok(occurrences <= N, `expected at most ${N} occurrences of the needle, found ${occurrences}`);
});

test("after a restart a running step is reported for reconciliation and left untouched", async () => {
  const authority = new DispatchAuthority();
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["bench.process.list"], authority);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "operation-executor-c7-recovery-"));
  const ownership = { ownerId: "executor-test-c7", assertHeld: () => {} };
  const metadata = (name: string, version: string): CapabilityMetadata | undefined => {
    const found = registry.get(name);
    return found && (!version || found.version === version) ? { version: found.version, effect: found.effect, approval: found.approval.required, retry: found.retry, resourceKeys: found.resourceAccess.conflictKeys } : undefined;
  };
  const store = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  const accepted = store.accept({ request: { instruction: "List bench processes" }, context }).snapshot;
  store.beginResolution(accepted.operationId);
  const queued = store.queueStep(accepted.operationId, { key: "list", capability: "bench.process.list", capabilityVersion: "1.0.0" });
  const stepId = queued.steps[0]!.stepId;
  store.startStep(accepted.operationId, stepId, { argDigest: canonicalDigest({}), idempotencyKey: `${accepted.operationId}/${stepId}` });

  // The restart: a new OperationStore on the same directory, never the same instance.
  const newStore = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: metadata, dispatchAuthority: authority });
  const report = planRecovery(newStore, { now: Date.now() });
  const plan = report.plans.find((entry) => entry.operationId === accepted.operationId)!;
  const reconcile = plan.actions.find((action) => action.kind === "reconcile_step" && action.stepId === stepId);
  assert.ok(reconcile, `expected a reconcile_step action for ${stepId}, got ${JSON.stringify(plan.actions)}`);
  assert.equal(plan.actions.some((action) => action.kind === "dispatch_step"), false);

  const executor = new OperationExecutor({ store: newStore, registry, scheduler: new OperationScheduler({ maxConcurrent: 1 }), dispatchFor: () => ({}) });
  const outcome = await executor.recover({
    operationId: accepted.operationId,
    context,
    actions: plan.actions,
    calls: { [stepId]: { key: "list", capability: "bench.process.list", capabilityVersion: "1.0.0" } },
  });
  assert.deepEqual(outcome.handled, []);
  assert.equal(outcome.deferred.length, plan.actions.length);
  assert.ok(outcome.deferred.some((action) => action.kind === "reconcile_step" && action.stepId === stepId));

  const snapshot = newStore.load(accepted.operationId);
  assert.equal(snapshot.steps.find((step) => step.stepId === stepId)?.state, "running");
  assert.equal(isTerminalOperationState(snapshot.state), false);
});
