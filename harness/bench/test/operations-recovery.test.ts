import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { canonicalDigest, type OperateRequest, type TrustedActorContext } from "../src/operations/contracts.ts";
import { OperationStore, StoreNotOwnedError, type CapabilityMetadata, type RetryPolicy } from "../src/operations/store.ts";
import { planRecovery, pendingReconciliation, type RecoveryPlan, type RecoveryReport } from "../src/operations/recovery.ts";
import { DispatchAuthority } from "../src/operations/dispatch-authority.ts";

const PAYLOAD_A = canonicalDigest({ path: "src/config.ts", revision: 4 });

class OwnershipStub {
  readonly ownerId = "test-bench-owner";
  held = true;
  assertHeld(): void {
    if (!this.held) throw new StoreNotOwnedError("the folder lock was released");
  }
  release(): void {
    this.held = false;
  }
}

function clockFrom(start = 1_760_000_000_000) {
  let now = start;
  return { now: () => now, advance: (ms: number) => void (now += ms) };
}

function context(over: Partial<TrustedActorContext> = {}): TrustedActorContext {
  return {
    actorId: "user-1",
    tenantId: "tenant-1",
    sessionId: "s-1",
    turnId: "turn-1",
    toolCallId: "call-1",
    turnRevision: 1,
    scope: { workspaceId: "ws-api" },
    ...over,
  };
}

function instruction(): OperateRequest {
  return { instruction: "In src/config.ts, change the timeout to 30000." };
}

function metadata(capability: string, retry: RetryPolicy = { class: "none", maxAttempts: 1 }): CapabilityMetadata | undefined {
  if (capability === "file.read") return { version: "1.0.0", effect: "read", approval: "none", retry };
  if (capability === "file.edit") return { version: "1.0.0", effect: "write", approval: "none", retry };
  return undefined;
}

function bench(start = 1_760_000_000_000, retry: RetryPolicy = { class: "none", maxAttempts: 1 }) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "bench-oprecovery-"));
  const clock = clockFrom(start);
  const ownership = new OwnershipStub();
  const store = new OperationStore({ root, ownership, now: clock.now, capabilityMetadata: (capability) => metadata(capability, retry), dispatchAuthority: new DispatchAuthority() });
  return { root, clock, ownership, store };
}

function restart(root: string, clock: ReturnType<typeof clockFrom>, retry: RetryPolicy = { class: "none", maxAttempts: 1 }): OperationStore {
  return new OperationStore({ root, ownership: new OwnershipStub(), now: clock.now, capabilityMetadata: (capability) => metadata(capability, retry), dispatchAuthority: new DispatchAuthority() });
}

function operationFile(root: string, operationId: string): string {
  return path.join(root, "operations", `${operationId}.jsonl`);
}

function planFor(report: RecoveryReport, operationId: string): RecoveryPlan {
  const plan = report.plans.find((entry) => entry.operationId === operationId);
  assert.ok(plan, `no recovery plan for ${operationId}`);
  return plan;
}

function targets(plan: RecoveryPlan, kind: string): string[] {
  return plan.actions.filter((action) => action.kind === kind).map((action) => ("stepId" in action ? action.stepId : action.operationId));
}

test("a crash between dispatch and result yields reconciliation, never a re-dispatch", () => {
  const first = bench();
  const operationId = first.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  first.store.beginResolution(operationId);
  first.store.queueStep(operationId, {
    key: "edit_config",
    capability: "file.edit",
    capabilityVersion: "1.0.0",
    effect: "write",
    resourceKeys: ["workspace.file:src/config.ts"],
  });
  first.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A, idempotencyKey: `${operationId}/step-1` });
  const before = fs.readFileSync(operationFile(first.root, operationId));

  const restarted = restart(first.root, first.clock, { class: "idempotent", maxAttempts: 2 });
  const report = planRecovery(restarted, { now: first.clock.now() });
  const plan = planFor(report, operationId);
  assert.equal(plan.state, "running");
  assert.deepEqual(targets(plan, "dispatch_step"), [], "an unknown mutation is never dispatched again");
  assert.deepEqual(targets(plan, "reconcile_step"), ["step-1"]);
  const reconcile = plan.actions.find((action) => action.kind === "reconcile_step");
  assert.ok(reconcile && reconcile.kind === "reconcile_step");
  assert.equal(reconcile.recorded, false);
  assert.equal(reconcile.idempotencyKey, `${operationId}/step-1`);
  assert.deepEqual(reconcile.retry, { class: "idempotent", maxAttempts: 2 });
  assert.deepEqual(pendingReconciliation(restarted), [{ operationId, stepIds: ["step-1"] }]);

  // Planning is read-only: the durable log is untouched until the scheduler acts.
  assert.equal(before.equals(fs.readFileSync(operationFile(first.root, operationId))), true);
  assert.deepEqual(planRecovery(restarted, { now: first.clock.now() }), planRecovery(restarted, { now: first.clock.now() }));
});

test("dependents wait for a succeeded dependency and for reconciliation", () => {
  const world = bench();
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.beginResolution(operationId);
  world.store.queueStep(operationId, { key: "read_config", capability: "file.read", capabilityVersion: "1.0.0", effect: "read" });
  world.store.queueStep(operationId, {
    key: "edit_config",
    capability: "file.edit",
    capabilityVersion: "1.0.0",
    effect: "write",
    dependencies: ["read_config"],
  });
  assert.deepEqual(targets(planFor(planRecovery(world.store, { now: world.clock.now() }), operationId), "dispatch_step"), ["step-1"]);

  world.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  const whileRunning = planFor(planRecovery(world.store, { now: world.clock.now() }), operationId);
  assert.deepEqual(targets(whileRunning, "dispatch_step"), [], "a dependent never starts before its dependency succeeds");
  assert.deepEqual(targets(whileRunning, "reconcile_step"), ["step-1"]);

  world.store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["read-1"] });
  const afterSuccess = planFor(planRecovery(world.store, { now: world.clock.now() }), operationId);
  assert.deepEqual(targets(afterSuccess, "dispatch_step"), ["step-2"]);
  assert.deepEqual(targets(afterSuccess, "reconcile_step"), []);
});

test("a recorded unknown outcome is reported for reconciliation, not for retry", () => {
  const world = bench();
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.queueStep(operationId, { key: "edit_config", capability: "file.edit", capabilityVersion: "1.0.0", effect: "write" });
  world.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  world.store.markOutcomeUnknown(operationId, "step-1", { summary: "the update did not return" });

  const report = planRecovery(world.store, { now: world.clock.now() });
  const plan = planFor(report, operationId);
  assert.equal(plan.state, "reconciling");
  assert.deepEqual(targets(plan, "dispatch_step"), []);
  assert.deepEqual(targets(plan, "reconcile_step"), ["step-1"]);
  const reconcile = plan.actions[0];
  assert.ok(reconcile.kind === "reconcile_step");
  assert.equal(reconcile.recorded, true);
  assert.equal(reconcile.dispatchDigest, PAYLOAD_A);
  assert.deepEqual(reconcile.retry, { class: "none", maxAttempts: 1 }, "an undeclared retry class is never retried");
  assert.deepEqual(pendingReconciliation(world.store), [{ operationId, stepIds: ["step-1"] }]);
});

test("a retryable failed step is only a non-executable retry candidate after restart", () => {
  const world = bench(1_760_000_000_000, { class: "idempotent", maxAttempts: 2 });
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.queueStep(operationId, { key: "edit_config", capability: "file.edit", capabilityVersion: "1.0.0", effect: "write" });
  world.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  world.store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "try again", retryable: true },
  });

  const restarted = restart(world.root, world.clock, { class: "idempotent", maxAttempts: 2 });
  const plan = planFor(planRecovery(restarted, { now: world.clock.now() }), operationId);
  const retry = plan.actions.find((action) => action.kind === "retry_candidate");
  assert.ok(retry && retry.kind === "retry_candidate");
  assert.equal(retry.stepId, "step-1");
  assert.equal(retry.argDigest, PAYLOAD_A);
  assert.deepEqual(retry.retry, { class: "idempotent", maxAttempts: 2 });
  assert.equal(plan.actions.some((action) => action.kind === "retry_step"), false);
});

test("a retry candidate still requires retryStep to validate current policy and session", () => {
  const world = bench(1_760_000_000_000, { class: "idempotent", maxAttempts: 2 });
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.queueStep(operationId, { key: "edit_config", capability: "file.edit" });
  world.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  world.store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "try again", retryable: true },
  });

  const changedPolicy = restart(world.root, world.clock, { class: "none", maxAttempts: 1 });
  assert.deepEqual(targets(planFor(planRecovery(changedPolicy, { now: world.clock.now() }), operationId), "retry_candidate"), []);

  const eligible = restart(world.root, world.clock, { class: "idempotent", maxAttempts: 2 });
  const candidate = planFor(planRecovery(eligible, { now: world.clock.now() }), operationId).actions.find((action) => action.kind === "retry_candidate");
  assert.ok(candidate && candidate.kind === "retry_candidate");
  assert.throws(
    () => eligible.retryStep(operationId, candidate.stepId, { argDigest: candidate.argDigest, retry: candidate.retry }, context({ sessionId: "other-session" })),
    /current actor, tenant, and session/,
  );
  assert.equal(eligible.load(operationId).steps[0].state, "failed");
});

test("an expired approval after restart cannot turn a retry candidate into dispatch", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "bench-oprecovery-"));
  const clock = clockFrom();
  const approved = (capability: string): CapabilityMetadata | undefined => {
    const found = metadata(capability, { class: "idempotent", maxAttempts: 2 });
    return found ? { ...found, approval: capability === "file.edit" ? "user" : "none" } : undefined;
  };
  const store = new OperationStore({ root, ownership: new OwnershipStub(), now: clock.now, capabilityMetadata: approved, dispatchAuthority: new DispatchAuthority() });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  store.queueStep(operationId, { key: "edit_config", capability: "file.edit" });
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-retry",
    decisionClass: "user_authorization",
    question: "Apply the edit?",
    payloadDigest: PAYLOAD_A,
    ttlMs: 1_000,
  });
  store.recordDecision({
    recordId: "rec-retry",
    operationId,
    stepId: "step-1",
    decisionId: "dec-retry",
    decisionClass: "user_authorization",
    actorId: "user-1",
    tenantId: "tenant-1",
    sessionId: "s-1",
    payloadDigest: PAYLOAD_A,
    revision: waiting.revision,
    policySource: "user_ui",
    outcome: "granted",
    recordedAt: clock.now(),
    expiresAt: clock.now() + 1_000,
  }, context());
  store.resume({ request: { action: "resume", operationId, decisionId: "dec-retry", expectedRevision: waiting.revision, resolution: { kind: "recorded_user_decision", recordId: "rec-retry" } }, context: context() });
  store.recordStepOutcome(operationId, "step-1", { outcome: "failed", error: { code: "execution_failure", message: "retry", retryable: true } });
  clock.advance(2_000);

  const restarted = new OperationStore({ root, ownership: new OwnershipStub(), now: clock.now, capabilityMetadata: approved, dispatchAuthority: new DispatchAuthority() });
  const candidate = planFor(planRecovery(restarted, { now: clock.now() }), operationId).actions.find((action) => action.kind === "retry_candidate");
  assert.ok(candidate && candidate.kind === "retry_candidate");
  assert.throws(
    () => restarted.retryStep(operationId, candidate.stepId, { argDigest: candidate.argDigest, retry: candidate.retry }, context()),
    /expired/,
  );
  assert.equal(restarted.load(operationId).steps[0].state, "failed");
});

test("pending decisions are reported with their deadline and released when expired", () => {
  const world = bench();
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.queueStep(operationId, { key: "edit_config", capability: "file.edit", capabilityVersion: "1.0.0", effect: "write" });
  const waiting = world.store.requireDecision(operationId, "step-1", {
    decisionId: "dec-1",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts?",
    payloadDigest: PAYLOAD_A,
    ttlMs: 1_000,
  });
  const deadline = waiting.pendingDecisions[0].expiresAt;
  const live = planFor(planRecovery(world.store, { now: world.clock.now() }), operationId);
  assert.deepEqual(live.actions.map((action) => action.kind), ["await_decision"]);
  const waitingAction = live.actions[0];
  assert.ok(waitingAction.kind === "await_decision");
  assert.equal(waitingAction.expiresAt, deadline);

  world.clock.advance(5_000);
  const restarted = restart(world.root, world.clock);
  const stale = planFor(planRecovery(restarted, { now: world.clock.now() }), operationId);
  assert.deepEqual(stale.actions.map((action) => action.kind), ["expire_decision"]);
  assert.equal(restarted.expire(operationId).steps[0].state, "cancelled");
});

test("a passed deadline is reported for expiry, and never for unknown effects", () => {
  const clean = bench();
  const cleanId = clean.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  clean.clock.advance(11 * 60 * 1000);
  const expiry = planFor(planRecovery(clean.store, { now: clean.clock.now() }), cleanId);
  assert.deepEqual(expiry.actions.map((action) => action.kind), ["expire_operation"]);
  const expireAction = expiry.actions[0];
  assert.ok(expireAction.kind === "expire_operation");
  assert.equal(expireAction.deadlineAt, clean.store.load(cleanId).deadlineAt);
  assert.equal(clean.store.expire(cleanId).state, "expired");

  const unknown = bench();
  const unknownId = unknown.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  unknown.store.queueStep(unknownId, { key: "edit_config", capability: "file.edit", capabilityVersion: "1.0.0", effect: "write" });
  unknown.store.startStep(unknownId, "step-1", { argDigest: PAYLOAD_A });
  unknown.store.markOutcomeUnknown(unknownId, "step-1");
  unknown.clock.advance(11 * 60 * 1000);
  const plan = planFor(planRecovery(unknown.store, { now: unknown.clock.now() }), unknownId);
  assert.deepEqual(targets(plan, "expire_operation"), []);
  assert.deepEqual(targets(plan, "reconcile_step"), ["step-1"]);
  assert.equal(unknown.store.expire(unknownId).state, "reconciling");
});

test("a passed deadline expires the operation and never re-dispatches queued work behind it", () => {
  const world = bench();
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.queueStep(operationId, { key: "read_config", capability: "file.read", capabilityVersion: "1.0.0", effect: "read" });
  const live = planFor(planRecovery(world.store, { now: world.clock.now() }), operationId);
  assert.deepEqual(targets(live, "dispatch_step"), ["step-1"], "before the deadline queued work is dispatched");

  world.clock.advance(11 * 60 * 1000);
  const restarted = restart(world.root, world.clock);
  const expired = planFor(planRecovery(restarted, { now: world.clock.now() }), operationId);
  assert.deepEqual(targets(expired, "dispatch_step"), [], "a queued step is never dispatched after the deadline");
  assert.deepEqual(expired.actions.map((action) => action.kind), ["expire_operation"]);
  assert.equal(restarted.expire(operationId).state, "expired");
});

test("a finished operation is terminal, so recovery reschedules nothing for it", () => {
  const world = bench();
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.queueStep(operationId, { key: "read_config", capability: "file.read", capabilityVersion: "1.0.0", effect: "read" });
  world.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  const running = planFor(planRecovery(world.store, { now: world.clock.now() }), operationId);
  assert.equal(running.state, "running");

  world.store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["read-1"] });
  const report = planRecovery(world.store, { now: world.clock.now() });
  assert.deepEqual(report.terminal, [operationId]);
  assert.deepEqual(report.plans.map((plan) => plan.operationId), []);
});

test("recovery refuses to plan without the exclusive bench ownership", () => {
  const world = bench();
  world.store.accept({ request: instruction(), context: context() });
  world.ownership.release();
  assert.throws(() => planRecovery(world.store, { now: world.clock.now() }), (error) => error instanceof StoreNotOwnedError);
  world.store.close();
  assert.throws(() => planRecovery(world.store, { now: world.clock.now() }));
});

test("recovery never dispatches queued work or retries while cancellation or a decision wait owns the operation", () => {
  const cancelling = bench();
  const cancelId = cancelling.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  cancelling.store.queueStep(cancelId, { key: "first", capability: "file.read", capabilityVersion: "1.0.0", effect: "read" });
  cancelling.store.queueStep(cancelId, { key: "second", capability: "file.read", capabilityVersion: "1.0.0", effect: "read" });
  cancelling.store.startStep(cancelId, "step-1", { argDigest: PAYLOAD_A });
  cancelling.store.requestCancel(cancelId);
  const cancelPlan = planFor(planRecovery(cancelling.store, { now: cancelling.clock.now() }), cancelId);
  assert.deepEqual(targets(cancelPlan, "dispatch_step"), []);

  const waiting = bench();
  const waitId = waiting.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  waiting.store.queueStep(waitId, { key: "first", capability: "file.read", capabilityVersion: "1.0.0", effect: "read" });
  waiting.store.queueStep(waitId, { key: "second", capability: "file.read", capabilityVersion: "1.0.0", effect: "read" });
  waiting.store.requireDecision(waitId, "step-1", {
    decisionId: "ask-1",
    decisionClass: "additional_input",
    question: "Which source?",
    payloadDigest: PAYLOAD_A,
  });
  const waitPlan = planFor(planRecovery(waiting.store, { now: waiting.clock.now() }), waitId);
  assert.deepEqual(targets(waitPlan, "dispatch_step"), []);
  assert.deepEqual(waitPlan.actions.map((action) => action.kind), ["await_decision"]);
});

test("cancellation durably resumes abort instead of ordinary reconciliation after restart", () => {
  const world = bench();
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.queueStep(operationId, { capability: "file.edit" });
  world.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A, idempotencyKey: "idem-1" });
  world.store.recordBackendOperationId(operationId, "step-1", "backend-1");
  world.store.requestCancel(operationId);

  const restarted = restart(world.root, world.clock);
  const plan = planFor(planRecovery(restarted, { now: world.clock.now() }), operationId);
  assert.deepEqual(plan.actions, [{
    kind: "resume_abort",
    operationId,
    stepId: "step-1",
    capability: "file.edit",
    capabilityVersion: "1.0.0",
    backendOperationId: "backend-1",
    idempotencyKey: "idem-1",
    dispatchDigest: PAYLOAD_A,
  }]);
  assert.deepEqual(pendingReconciliation(restarted), [{ operationId, stepIds: ["step-1"] }]);
});

test("any reconciliation action is an operation-wide dispatch and retry barrier", () => {
  const world = bench(1_760_000_000_000, { class: "idempotent", maxAttempts: 2 });
  const operationId = world.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  world.store.queueStep(operationId, { key: "running", capability: "file.edit" });
  world.store.queueStep(operationId, { key: "retry", capability: "file.edit" });
  world.store.queueStep(operationId, { key: "queued", capability: "file.read" });
  world.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  world.store.startStep(operationId, "step-2", { argDigest: PAYLOAD_A });
  world.store.recordStepOutcome(operationId, "step-2", {
    outcome: "failed",
    error: { code: "execution_failure", message: "retry", retryable: true },
  });

  const plan = planFor(planRecovery(world.store, { now: world.clock.now() }), operationId);
  assert.deepEqual(targets(plan, "reconcile_step"), ["step-1"]);
  assert.deepEqual(targets(plan, "retry_candidate"), []);
  assert.deepEqual(targets(plan, "dispatch_step"), []);
});

test("running reads use trusted recovery policy and are never blindly retried", () => {
  const noRetry = bench();
  const firstId = noRetry.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  noRetry.store.queueStep(firstId, { capability: "file.read" });
  noRetry.store.startStep(firstId, "step-1", { argDigest: PAYLOAD_A });
  const first = planFor(planRecovery(noRetry.store, { now: noRetry.clock.now() }), firstId).actions[0];
  assert.ok(first.kind === "reconcile_step");
  assert.deepEqual(first.retry, { class: "none", maxAttempts: 1 });

  const idempotent = bench(1_760_000_000_000, { class: "idempotent", maxAttempts: 2 });
  const secondId = idempotent.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  idempotent.store.queueStep(secondId, { capability: "file.read" });
  idempotent.store.startStep(secondId, "step-1", { argDigest: PAYLOAD_A });
  const second = planFor(planRecovery(idempotent.store, { now: idempotent.clock.now() }), secondId).actions[0];
  assert.ok(second.kind === "reconcile_step");
  assert.deepEqual(second.retry, { class: "idempotent", maxAttempts: 2 });
  assert.equal(second.dispatchDigest, PAYLOAD_A);
});
