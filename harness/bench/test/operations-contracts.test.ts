import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  APPROVAL_BINDINGS,
  ARTIFACT_TAG,
  BINDING_RESOLUTION_PHASE,
  BUDGET_NODE,
  CONTRACT_VERSION,
  DEFAULT_BUDGETS,
  DEFAULT_INSTRUCTION_DESCRIPTION,
  DESCRIPTION_PAGE_SIZE,
  EXACT_PLAN_RULES,
  JSON_LIMITS,
  MAX_ARTIFACT_BYTES,
  OPERATION_TRANSITIONS,
  OPERATE_REQUEST_SCHEMA,
  PENDING_STEP_STATES,
  REQUEST_LIMITS,
  RETRYABLE_STEP_STATES,
  STEP_TRANSITIONS,
  TERMINAL_OPERATION_STATES,
  TRUSTED_CONTEXT_SOURCES,
  assertOperationTransition,
  buildDescriptionIndex,
  callsWithUnresolvedBindings,
  canonicalDigest,
  canTransitionOperation,
  canTransitionStep,
  checkResumeAgainstRecord,
  decisionClassesForResolution,
  deriveDeduplicationKey,
  describeCapability,
  freezeCapabilityDescriptor,
  isTerminalOperationState,
  isTerminalStepState,
  operationTransition,
  parseOperateRequest,
  publicErrorClass,
  requestDigest,
  resolutionCanResolve,
  resolveCallArgs,
  selectOutputPath,
  stableStringify,
  stepTransition,
  validateBudgets,
  validateCapabilityArgument,
  validateCapabilityDescriptor,
  validateCompactOperationResult,
  validateEventSequence,
  validateJsonSchemaLike,
  validateJsonValue,
  validateOperateRequest,
  validateOperationError,
  validateOperationEvent,
  validateOperationSnapshot,
  validatePendingDecision,
  validateRecordedDecision,
  validateStepRecord,
  validateTransitionTables,
  validateTrustedActorContext,
} from "../src/operations/contracts.ts";
import type {
  CapabilityDescriptor,
  ContextRef,
  ExactCall,
  IntentRequest,
  OperationEvent,
  RecordedDecision,
  ResumeExpectation,
  ResumeRequest,
  TrustedActorContext,
} from "../src/operations/contracts.ts";
import { OperationStore } from "../src/operations/store.ts";
import { DispatchAuthority } from "../src/operations/dispatch-authority.ts";

const FIXTURES = path.join(path.dirname(fileURLToPath(import.meta.url)), "fixtures", "operations");
const DIGEST = `sha256:${"0123456789abcdef".repeat(4)}`;
const OTHER_DIGEST = `sha256:${"fedcba9876543210".repeat(4)}`;

type ResultLike = { ok: true } | { ok: false; issues: Array<{ path: string; code: string }> };
type Manifest = { contractVersion: string; files: Array<{ path: string; kind: string }> };
const codesOf = (result: ResultLike): string[] => (result.ok ? [] : result.issues.map((i) => i.code));
const pathsOf = (result: ResultLike): string[] => (result.ok ? [] : result.issues.map((i) => i.path));
const read = <T>(relative: string): T => JSON.parse(fs.readFileSync(path.join(FIXTURES, relative), "utf8")) as T;

test("the default call is one instruction and needs nothing else", () => {
  const result = validateOperateRequest({ instruction: "In src/config.ts, change the timeout to 30000." });
  assert.equal(result.ok, true);
  assert.deepEqual(result.ok ? result.value : null, { instruction: "In src/config.ts, change the timeout to 30000." });
});

test("instruction text arrives exactly as written", () => {
  const instruction = "  In src/auth.ts, reject expired tokens.\r\n  Keep the public API — 🔒  ";
  const parsed = parseOperateRequest({ instruction }) as IntentRequest;
  assert.equal(parsed.instruction, instruction);
  assert.equal(parsed.instruction.length, instruction.length);
  assert.deepEqual(Object.keys(parsed), ["instruction"]);
});

test("optional fields are omitted when absent and validated when present", () => {
  const minimal = parseOperateRequest({ instruction: "x" }) as IntentRequest;
  assert.equal("contextRefs" in minimal, false);
  assert.equal("inputs" in minimal, false);
  assert.equal("constraints" in minimal, false);
  assert.equal(validateOperateRequest({ instruction: "x", constraints: [] }).ok, false);
  assert.equal(validateOperateRequest({ instruction: "x", inputs: {} }).ok, false);
  const full = validateOperateRequest({
    instruction: "x",
    constraints: ["keep the public API"],
    expectedResults: ["expired tokens stop before the user loads"],
    inputs: { timeoutMs: 30000 },
  });
  assert.equal(full.ok, true);
});

test("unknown fields are rejected and identity never comes from the request", () => {
  const rejected = [
    { instruction: "x", action: "cancel" },
    { instruction: "x", operationId: "op-1" },
    { instruction: "x", actorId: "user-1" },
    { instruction: "x", tenantId: "tenant-1" },
    { instruction: "x", approved: true },
    { action: "cancel", operationId: "op-1", scope: { workspaceId: "ws" } },
    { action: "inspect", operationId: "op-1", actorId: "someone" },
    { action: "describe", detail: "schema", authorize: true },
  ];
  for (const request of rejected) {
    const result = validateOperateRequest(request);
    assert.equal(result.ok, false, JSON.stringify(request));
    assert.ok(
      codesOf(result).some((code) => code === "unknown_field" || code === "mixed_action"),
      `${JSON.stringify(request)} -> ${codesOf(result).join(",")}`,
    );
  }
  assert.deepEqual(codesOf(validateOperateRequest({ instruction: "x", action: "cancel" })), ["mixed_action"]);
});

test("malformed requests are named precisely, including stale revisions", () => {
  assert.ok(codesOf(validateOperateRequest({})).includes("missing_field"));
  assert.equal(validateOperateRequest({ instruction: "" }).ok, false);
  assert.equal(validateOperateRequest({ instruction: 7 }).ok, false);
  assert.equal(validateOperateRequest({ instruction: "x".repeat(REQUEST_LIMITS.instructionChars + 1) }).ok, false);
  assert.equal(validateOperateRequest("nope").ok, false);
  assert.equal(validateOperateRequest(null).ok, false);
  assert.deepEqual(codesOf(validateOperateRequest({ action: "delete", operationId: "op-1" })), ["unsupported_action"]);
  assert.deepEqual(codesOf(validateOperateRequest({ action: "cancel" })), ["missing_field"]);
  assert.equal(validateOperateRequest({ action: "exact" }).ok, false);

  for (const value of [0, -1, 1.5, "3"]) {
    const result = validateOperateRequest({
      action: "resume",
      operationId: "op-1",
      decisionId: "dec-1",
      expectedRevision: value,
      resolution: { kind: "recorded_user_decision", recordId: "decision-1" },
    });
    assert.equal(result.ok, false, String(value));
    assert.deepEqual(codesOf(result), ["invalid_revision"]);
    assert.deepEqual(pathsOf(result), ["$.expectedRevision"]);
  }
});

test("JSON values stay finite, bounded, and prototype-free", () => {
  assert.equal(validateJsonValue({ a: 1, b: [true, null, "x"] }).ok, true);
  assert.deepEqual(codesOf(validateJsonValue({ a: Number.NaN })), ["non_finite_number"]);
  assert.deepEqual(codesOf(validateJsonValue({ a: Infinity })), ["non_finite_number"]);
  for (const value of [{ a: undefined }, { a: () => 1 }, { a: new Date() }, 1n, new Map()]) {
    assert.equal(validateJsonValue(value).ok, false, String(value));
  }
  let deep: unknown = 1;
  for (let i = 0; i < JSON_LIMITS.depth + 2; i++) deep = [deep];
  assert.ok(codesOf(validateJsonValue(deep)).includes("too_deep"));
  assert.ok(codesOf(validateJsonValue(new Array(JSON_LIMITS.arrayItems + 1).fill(0))).includes("too_many_items"));
  assert.ok(codesOf(validateJsonValue("x".repeat(JSON_LIMITS.stringChars + 1))).includes("string_too_long"));

  const parsed = JSON.parse('{"__proto__":{"polluted":true},"constructor":1,"prototype":2,"safe":true}');
  const result = validateJsonValue(parsed);
  assert.equal(result.ok, false);
  assert.equal(codesOf(result).filter((code) => code === "forbidden_key").length, 3);
  assert.equal(Object.keys(parsed).includes("__proto__"), true);
  assert.equal(({} as Record<string, unknown>).polluted, undefined);

  // Reserved keys must not slip in through request dictionaries either.
  const protoInputs = validateOperateRequest({ instruction: "x", inputs: JSON.parse('{"__proto__":{"polluted":true}}') });
  assert.equal(protoInputs.ok, false);
  assert.ok(codesOf(protoInputs).includes("forbidden_key"));
  const protoArgs = validateOperateRequest({
    action: "exact",
    request: {
      objective: "o",
      calls: [{ key: "a", capability: "file.read", capabilityVersion: "1.0.0", args: JSON.parse('{"constructor":1}') }],
    },
  });
  assert.equal(protoArgs.ok, false);
  assert.ok(codesOf(protoArgs).includes("forbidden_key"));
  assert.equal(({} as Record<string, unknown>).polluted, undefined);

  // A one-key object must never resolve a reserved name through the prototype chain.
  const oneKey = validateJsonValue(JSON.parse('{"constructor":{"polluted":true}}'));
  assert.equal(oneKey.ok, false);
  assert.deepEqual(codesOf(oneKey), ["forbidden_key"]);
  const nested = validateOperateRequest({ instruction: "x", inputs: { patch: JSON.parse('{"__proto__":1}') } });
  assert.equal(nested.ok, false);
  assert.ok(codesOf(nested).includes("forbidden_key"));
  assert.equal(({} as Record<string, unknown>).polluted, undefined);
  const tagged = validateOperateRequest({
    instruction: "x",
    inputs: { patch: JSON.parse(`{"$artifact":{"artifactId":"art-9","digest":"${DIGEST}","byteLength":1}}`) },
  });
  assert.equal(tagged.ok, true, "the registered $artifact tag still validates");
});

test("context references are validated as references, not as permissions", () => {
  const refs: ContextRef[] = [
    { kind: "message", messageId: "msg-7" },
    { kind: "evidence", operationId: "op-42", evidenceId: "ev-3" },
    { kind: "artifact", artifactId: "art-9", digest: DIGEST, byteLength: 1024, mediaType: "text/plain" },
  ];
  assert.equal(validateOperateRequest({ instruction: "x", contextRefs: refs }).ok, true);
  const rejected = [
    { instruction: "x", contextRefs: [] },
    { instruction: "x", contextRefs: [{ kind: "url", href: "https://example.test/a" }] },
    { instruction: "x", contextRefs: [{ kind: "message", messageId: "msg-7", authorized: true }] },
    { instruction: "x", contextRefs: [{ kind: "artifact", artifactId: "art-9", digest: "md5:abc", byteLength: 1 }] },
    { instruction: "x", contextRefs: [{ kind: "artifact", artifactId: "art-9", digest: DIGEST, byteLength: MAX_ARTIFACT_BYTES + 1 }] },
    { instruction: "x", contextRefs: [{ kind: "artifact", artifactId: "art-9", digest: DIGEST, byteLength: 1, path: "/etc/passwd" }] },
  ];
  for (const request of rejected) assert.equal(validateOperateRequest(request).ok, false, JSON.stringify(request));
});

test("exact content travels as an artifact reference, not as inline bytes", () => {
  const artifact = { [ARTIFACT_TAG]: { artifactId: "art-9", digest: DIGEST, byteLength: 12, mediaType: "text/plain" } };
  assert.equal(validateOperateRequest({ instruction: "x", inputs: { patch: artifact } }).ok, true);
  assert.equal(validateOperateRequest({ instruction: "x", inputs: { patch: { ...artifact, extra: 1 } } }).ok, false);
  assert.equal(validateOperateRequest({ instruction: "x", inputs: { patch: { $unknown: 1 } } }).ok, false);
  assert.equal(
    validateOperateRequest({ instruction: "x", inputs: { patch: { [ARTIFACT_TAG]: { artifactId: "art-9", digest: "nope", byteLength: 1 } } } }).ok,
    false,
  );
});

test("resume cites a recorded decision; additional input is separate and cannot approve", () => {
  const base = { action: "resume", operationId: "op-42", decisionId: "dec-42", expectedRevision: 3 };
  const cited = { ...base, resolution: { kind: "recorded_user_decision", recordId: "decision-7" } };
  const provided = { ...base, resolution: { kind: "additional_input", inputs: { timeoutMs: 30000 } } };
  assert.equal(validateOperateRequest(cited).ok, true);
  assert.equal(validateOperateRequest(provided).ok, true);

  const forged = [
    { ...base, resolution: { kind: "additional_input", inputs: { approve: true } } },
    { ...base, resolution: { kind: "additional_input", inputs: { granted: true } } },
    { ...base, resolution: { kind: "additional_input", inputs: { nested: { approval: "yes" } } } },
    { ...base, resolution: { kind: "additional_input", inputs: { list: [{ decision: "allow" }] } } },
    { ...base, resolution: { kind: "recorded_user_decision", recordId: "decision-7", grant: true } },
    { ...base, resolution: { kind: "recorded_user_decision", recordId: "decision-7", outcome: "granted" } },
    { ...base, resolution: { kind: "recorded_user_decision", recordId: "decision-7" }, approved: true },
  ];
  for (const request of forged) {
    const result = validateOperateRequest(request);
    assert.equal(result.ok, false, JSON.stringify(request));
    assert.ok(
      codesOf(result).some((code) => code === "forged_approval" || code === "unknown_field"),
      `${JSON.stringify(request)} -> ${codesOf(result).join(",")}`,
    );
  }

  const citedResolution = (parseOperateRequest(cited) as ResumeRequest).resolution;
  const providedResolution = (parseOperateRequest(provided) as ResumeRequest).resolution;
  assert.deepEqual(decisionClassesForResolution(citedResolution), ["user_authorization", "user_preference"]);
  assert.deepEqual(decisionClassesForResolution(providedResolution), ["additional_input"]);
  assert.equal(resolutionCanResolve(providedResolution, "user_authorization"), false);
  assert.equal(resolutionCanResolve(providedResolution, "user_preference"), false);
  assert.equal(resolutionCanResolve(citedResolution, "additional_input"), false);
  assert.equal(resolutionCanResolve(citedResolution, "user_authorization"), true);
});

test("a recorded decision binds actor, session, payload, revision, and expiry", () => {
  const record: RecordedDecision = {
    recordId: "decision-7",
    operationId: "op-42",
    stepId: "step-2",
    decisionId: "dec-42",
    decisionClass: "user_authorization",
    actorId: "user-1",
    tenantId: "tenant-1",
    sessionId: "s-1",
    payloadDigest: DIGEST,
    revision: 3,
    policySource: "user_ui",
    outcome: "granted",
    recordedAt: 1760000002000,
    expiresAt: 1760000600000,
  };
  assert.equal(validateRecordedDecision(record).ok, true);
  assert.equal(validateRecordedDecision({ ...record, decisionClass: "additional_input" }).ok, false);
  assert.equal(validateRecordedDecision({ ...record, outcome: "approved" }).ok, false);
  assert.equal(validateRecordedDecision({ ...record, model: "flash" }).ok, false);

  const expectation: ResumeExpectation = {
    actorId: "user-1",
    tenantId: "tenant-1",
    sessionId: "s-1",
    operationId: "op-42",
    stepId: "step-2",
    decisionId: "dec-42",
    decisionClass: "user_authorization",
    payloadDigest: DIGEST,
    revision: 3,
    now: 1760000001000,
    expiryBound: 1760000600000,
  };
  const granted = checkResumeAgainstRecord(record, expectation);
  assert.equal(granted.ok, true);
  if (granted.ok) {
    assert.equal(granted.value.outcome, "granted");
    assert.equal(granted.value.dispatchAuthorized, true);
    assert.equal(granted.value.requiredAction, "dispatch");
  }
  const denied = checkResumeAgainstRecord({ ...record, outcome: "denied" }, expectation);
  assert.equal(denied.ok, true);
  if (denied.ok) {
    assert.equal(denied.value.outcome, "denied");
    assert.equal(denied.value.dispatchAuthorized, false, "a recorded denial is never permission to run");
    assert.equal(denied.value.requiredAction, "refuse_step");
  }
  assert.ok(codesOf(validateRecordedDecision({ ...record, expiresAt: undefined })).includes("missing_field"));
  assert.ok(codesOf(validateRecordedDecision({ ...record, expiresAt: record.recordedAt })).includes("out_of_range"));
  assert.ok(codesOf(validateRecordedDecision({ ...record, expiresAt: record.recordedAt - 1 })).includes("out_of_range"));
  const mismatches: Array<[Partial<ResumeExpectation>, string]> = [
    [{ sessionId: "s-2" }, "decision_mismatch"],
    [{ actorId: "user-2" }, "decision_mismatch"],
    [{ tenantId: "tenant-2" }, "decision_mismatch"],
    [{ operationId: "op-99" }, "decision_mismatch"],
    [{ stepId: "step-9" }, "decision_mismatch"],
    [{ decisionId: "dec-99" }, "decision_mismatch"],
    [{ decisionClass: "user_preference" }, "decision_mismatch"],
    [{ decisionClass: "additional_input" }, "decision_mismatch"],
    [{ payloadDigest: OTHER_DIGEST }, "validation_failure"],
    [{ revision: 4 }, "invalid_revision"],
    [{ expiryBound: 1760000300000 }, "permission_denied"],
    [{ now: record.expiresAt }, "decision_expired"],
  ];
  for (const [patch, code] of mismatches) {
    const result = checkResumeAgainstRecord(record, { ...expectation, ...patch });
    assert.equal(result.ok, false, JSON.stringify(patch));
    assert.ok(codesOf(result).includes(code), `${code} not in ${codesOf(result).join(",")}`);
  }
  assert.ok(codesOf(checkResumeAgainstRecord({ ...record, usedAt: 1760000003000 }, expectation)).includes("decision_replayed"));
  assert.ok(codesOf(checkResumeAgainstRecord({ ...record, expiresAt: 1760000000000 }, expectation)).includes("decision_expired"));
});

test("advanced exact calls pin versions, declare dependencies, and stay acyclic", () => {
  const plan = (calls: unknown[]) => ({ action: "exact", request: { objective: "Read two files", calls } });
  const readA = { key: "read_a", capability: "file.read", capabilityVersion: "1.0.0", targetRef: "ws-api", args: { path: "a.ts" } };
  const readB = {
    key: "read_b",
    capability: "file.read",
    capabilityVersion: "1.0.0",
    dependsOn: ["read_a"],
    argsFrom: { cursor: { from: "read_a", output: "file", select: ["revision"] } },
  };
  assert.equal(validateOperateRequest(plan([readA, readB])).ok, true);

  const rejected: Array<[unknown, string]> = [
    [[{ ...readA, capabilityVersion: "latest" }], "bad_syntax"],
    [[{ ...readA, key: "Read_A" }], "bad_syntax"],
    [[{ ...readA, capability: "file read" }], "bad_syntax"],
    [[{ ...readA, targetRef: "/etc/passwd" }], "bad_syntax"],
    [[readA, { ...readB, dependsOn: ["nope"] }], "unknown_dependency"],
    [[readA, { ...readB, dependsOn: undefined }], "unknown_dependency"],
    [[readA, { ...readB, args: { cursor: "literal" } }], "binding_collision"],
    [[readA, { ...readB, argsFrom: { cursor: { from: "read_a", output: "file", select: [] } } }], "too_few_items"],
    [[readA, { ...readB, argsFrom: { cursor: { from: "read_a", output: "file", extra: 1 } } }], "unknown_field"],
    [[{ ...readA, key: "same" }, { ...readA, key: "same" }], "duplicate_key"],
    [[{ ...readA, key: "a", dependsOn: ["b"] }, { ...readA, key: "b", dependsOn: ["a"] }], "cycle"],
    [[], "too_many_items"],
    [new Array(REQUEST_LIMITS.exactCalls + 1).fill(readA), "too_many_items"],
    [null, "wrong_type"],
  ];
  for (const [calls, code] of rejected) {
    const result = validateOperateRequest(plan(calls as unknown[]));
    assert.equal(result.ok, false, JSON.stringify(calls).slice(0, 120));
    assert.ok(codesOf(result).includes(code), `${code} not in ${codesOf(result).join(",")}`);
  }

  // Binding syntax is a separate channel: an object that merely looks like a binding stays literal JSON.
  const literal = { key: "literal", capability: "file.read", capabilityVersion: "1.0.0", args: { from: "read_a", output: "file" } };
  assert.equal(validateOperateRequest(plan([literal])).ok, true);
});

test("bindings resolve at dispatch time from own properties only", () => {
  assert.equal(BINDING_RESOLUTION_PHASE, "dispatch_time");
  const call: ExactCall = {
    key: "read_b",
    capability: "file.read",
    capabilityVersion: "1.0.0",
    dependsOn: ["read_a"],
    args: { path: "b.ts" },
    argsFrom: { cursor: { from: "read_a", output: "file", select: ["revision"] } },
  };
  assert.equal(callsWithUnresolvedBindings({ objective: "o", calls: [call] }).length, 1);
  const resolved = resolveCallArgs(call, { read_a: { file: { revision: "rev-1", text: "hello" } } });
  assert.equal(resolved.ok, true);
  assert.deepEqual(resolved.ok ? resolved.value : null, { path: "b.ts", cursor: "rev-1" });
  assert.equal(resolveCallArgs(call, {}).ok, false);
  assert.equal(resolveCallArgs(call, { read_a: {} }).ok, false);
  assert.equal(resolveCallArgs(call, { read_a: { file: {} } }).ok, false);
  assert.equal(selectOutputPath({}, { from: "a", output: "x" }).ok, false);
  assert.equal(selectOutputPath({ a: {} }, { from: "a", output: "toString" }).ok, false);
  assert.equal(selectOutputPath({ a: { x: {} } }, { from: "a", output: "x", select: ["hasOwnProperty"] }).ok, false);
  assert.equal(selectOutputPath({ a: { x: {} } }, { from: "a", output: "x", select: ["constructor"] }).ok, false);
  assert.equal(selectOutputPath({ a: { x: [1, 2] } }, { from: "a", output: "x", select: [5] }).ok, false);
  assert.equal(selectOutputPath({ a: { x: { b: 7 } } }, { from: "a", output: "x", select: ["b"] }).ok, true);
  assert.equal(selectOutputPath({ a: { x: 1 } }, { from: "a", output: "x" }).ok, true);
});

test("the transition tables are complete and trigger-checked", () => {
  const table = validateTransitionTables();
  assert.equal(table.ok, true);
  for (const transition of OPERATION_TRANSITIONS) {
    assert.equal(canTransitionOperation(transition.from, transition.to, transition.trigger), true, `${transition.from}->${transition.to}`);
  }
  for (const transition of STEP_TRANSITIONS) {
    assert.equal(canTransitionStep(transition.from, transition.to, transition.trigger), true, `${transition.from}->${transition.to}`);
  }

  assert.equal(canTransitionOperation("needs_input", "running", "decision_supplied"), true);
  assert.equal(canTransitionOperation("needs_input", "running", "cancel_requested"), false);
  assert.equal(canTransitionOperation("accepted", "completed", "effects_committed"), false);
  assert.equal(canTransitionOperation("running", "reconciling", "unknown_outcome"), true);
  assert.equal(canTransitionOperation("running", "reconciling", "effects_failed"), false);
  assert.equal(canTransitionOperation("reconciling", "failed", "reconcile_conclusive"), true);
  assert.equal(canTransitionOperation("reconciling", "failed", "effects_failed"), false);
  assert.equal(canTransitionOperation("reconciling", "expired", "deadline_reached"), false, "unknown effects cannot expire away");
  assert.equal(canTransitionOperation("awaiting_approval", "running", "approval_recorded"), true);
  assert.equal(canTransitionOperation("awaiting_approval", "running", "resolve_started"), false);
  assert.throws(() => assertOperationTransition("completed", "running", "dispatch_started"), /not permitted/);
  assert.equal(isTerminalOperationState("partial"), true);
  for (const terminal of TERMINAL_OPERATION_STATES) {
    for (const to of ["running", "completed", "partial", "reconciling"] as const) {
      for (const trigger of ["dispatch_started", "effects_committed", "unknown_outcome"] as const) {
        assert.equal(canTransitionOperation(terminal, to, trigger), false, `${terminal}->${to} by ${trigger}`);
      }
    }
  }
  assert.equal(operationTransition("partial", "needs_input", "decision_required"), undefined);
  assert.equal(operationTransition("running", "completed", "effects_committed")?.trigger, "effects_committed");

  assert.equal(canTransitionStep("running", "outcome_unknown", "outcome_unknown"), true);
  assert.equal(canTransitionStep("outcome_unknown", "succeeded", "reconcile_conclusive"), true);
  assert.equal(canTransitionStep("outcome_unknown", "failed", "reconcile_conclusive"), true);
  assert.equal(canTransitionStep("outcome_unknown", "cancelled", "cancel_requested"), false);
  assert.equal(canTransitionStep("outcome_unknown", "running", "retry_allowed"), false);
  assert.equal(canTransitionStep("running", "outcome_unknown", "result_failed"), false);
  assert.equal(canTransitionStep("running", "cancelled", "cancel_requested"), false, "an abort request is not an outcome");
  assert.equal(canTransitionStep("running", "cancelled", "cancel_confirmed"), true);
  assert.equal(canTransitionStep("queued", "cancelled", "cancel_requested"), true);
  assert.equal(canTransitionStep("awaiting_approval", "cancelled", "cancel_requested"), true);
  assert.equal(canTransitionStep("failed", "running", "retry_allowed"), true);
  assert.equal(canTransitionStep("failed", "running", "dispatch_started"), false);
  assert.equal(canTransitionStep("failed", "succeeded", "result_observed"), false);
  assert.equal(isTerminalStepState("failed"), false, "a declared retry class may restart a failed step");
  assert.equal(isTerminalStepState("succeeded"), true);
  assert.equal(isTerminalStepState("skipped"), true);
  assert.ok(RETRYABLE_STEP_STATES.includes("failed"));
  assert.equal(stepTransition("running", "succeeded", "result_observed")?.trigger, "result_observed");
  assert.ok(PENDING_STEP_STATES.includes("outcome_unknown"));
});

test("events replay monotonically and results stay free of trusted detail", () => {
  const event: OperationEvent = {
    operationId: "op-42",
    sequence: 3,
    at: 1760000000000,
    phase: "queued",
    revision: 2,
    stepId: "step-2",
    capability: "file.read",
    summary: "Waiting for step-1 before reading src/config.ts.",
    queueReason: "depends on step-1",
    dependencies: ["read_a"],
  };
  assert.equal(validateOperationEvent(event).ok, true);
  assert.equal(validateOperationEvent({ ...event, phase: "thinking" }).ok, false);
  assert.equal(validateOperationEvent({ ...event, reasoning: "because" }).ok, false);
  assert.equal(validateEventSequence(2, { ...event, sequence: 3 }).ok, true);
  assert.equal(validateEventSequence(3, { ...event, sequence: 3 }).ok, false);
  assert.equal(validateEventSequence(4, { ...event, sequence: 3 }).ok, false);

  const result = { operationId: "op-42", revision: 4, state: "completed", summary: "Updated the timeout in src/config.ts." };
  assert.equal(validateCompactOperationResult({ ...result, evidenceRefs: ["change-42"], changed: true }).ok, true);
  assert.equal(validateCompactOperationResult({ ...result, actor: { actorId: "user-1" } }).ok, false);
  assert.equal(validateCompactOperationResult({ ...result, scope: { workspaceId: "ws-api" } }).ok, false);
  assert.equal(validateCompactOperationResult({ ...result, state: "flying" }).ok, false);
  assert.equal(validateCompactOperationResult({ ...result, summary: "" }).ok, false);

  assert.equal(validateOperationError({ code: "unknown_outcome", message: "no result", retryable: false }).ok, true);
  assert.equal(validateOperationError({ code: "made_up", message: "no result", retryable: false }).ok, false);
  assert.equal(publicErrorClass("unknown_outcome"), "unknown_outcome");
  assert.equal(publicErrorClass("ambiguous_target"), "ambiguous");
  assert.equal(publicErrorClass("scope_denied"), "permission_denied");
  assert.equal(publicErrorClass("decision_replayed"), "permission_denied");
});

test("snapshots carry trusted scope, and partial is terminal and honest", () => {
  const snapshot = {
    contractVersion: "v1",
    operationId: "op-42",
    revision: 3,
    state: "partial",
    createdAt: 1,
    updatedAt: 2,
    actor: { actorId: "user-1", tenantId: "tenant-1", sessionId: "s-1", turnId: "turn-4" },
    scope: { workspaceId: "ws-api" },
    request: { instruction: "In src/config.ts, change the timeout to 30000." },
    requestDigest: DIGEST,
    dedupeKey: "tool-call-1",
    budgets: { ...DEFAULT_BUDGETS },
    steps: [
      { stepId: "step-1", state: "succeeded", capability: "file.read", capabilityVersion: "1.0.0", effect: "read", attempts: 1 },
      { stepId: "step-2", state: "failed", capability: "file.edit", capabilityVersion: "1.0.0", effect: "write", attempts: 2 },
    ],
    pendingDecisions: [],
    unknownOutcomes: [],
    usage: { steps: 2, selectionRounds: 1, generationCalls: 0, attempts: 3 },
    lastSequence: 5,
  };
  assert.equal(validateOperationSnapshot(snapshot).ok, true);
  const pendingDecision = {
    decisionId: "dec-1",
    operationId: "op-42",
    stepId: "step-2",
    decisionClass: "additional_input",
    question: "Which file should change?",
    createdAt: 1,
    expiresAt: 5_000,
    revision: 3,
  };
  const unknownOutcome = { stepId: "step-2", capability: "file.edit", since: 3 };
  assert.ok(codesOf(validateOperationSnapshot({ ...snapshot, unknownOutcomes: [unknownOutcome] })).includes("invalid_transition"));
  assert.ok(codesOf(validateOperationSnapshot({ ...snapshot, pendingDecisions: [pendingDecision] })).includes("invalid_transition"));
  assert.ok(
    codesOf(validateOperationSnapshot({ ...snapshot, steps: [{ ...snapshot.steps[0], state: "running" }] })).includes("invalid_transition"),
  );

  // Terminal truthfulness applies everywhere, not only to partial.
  const succeeded = snapshot.steps[0];
  const failed = snapshot.steps[1];
  assert.equal(validateOperationSnapshot({ ...snapshot, state: "cancelled" }).ok, true);
  assert.ok(
    codesOf(validateOperationSnapshot({ ...snapshot, state: "cancelled", unknownOutcomes: [unknownOutcome] })).includes("invalid_transition"),
  );
  assert.ok(
    codesOf(validateOperationSnapshot({ ...snapshot, state: "expired", steps: [{ ...succeeded, state: "running" }] })).includes("invalid_transition"),
  );
  assert.ok(
    codesOf(validateOperationSnapshot({ ...snapshot, state: "failed", pendingDecisions: [pendingDecision] })).includes("invalid_transition"),
  );
  assert.ok(codesOf(validateOperationSnapshot({ ...snapshot, state: "completed" })).includes("invalid_transition"));
  assert.equal(validateOperationSnapshot({ ...snapshot, state: "completed", steps: [succeeded] }).ok, true);
  assert.ok(
    codesOf(validateOperationSnapshot({ ...snapshot, state: "completed", steps: [{ ...succeeded, state: "skipped" }] })).includes("invalid_transition"),
  );
  assert.ok(
    codesOf(validateOperationSnapshot({ ...snapshot, state: "partial", steps: [succeeded] })).includes("invalid_transition"),
    "partial needs genuinely mixed settled results",
  );
  assert.equal(validateOperationSnapshot({ ...snapshot, steps: [succeeded, failed] }).ok, true);
  assert.equal(validateOperationSnapshot({ ...snapshot, contractVersion: "v2" }).ok, false);
  assert.equal(validateOperationSnapshot({ ...snapshot, request: { instruction: "x", actorId: "user-2" } }).ok, false);
  assert.equal(validateOperationSnapshot({ ...snapshot, budgets: { ...DEFAULT_BUDGETS, maxSteps: 50 } }).ok, false);
  assert.equal(validateOperationSnapshot({ ...snapshot, scope: { workspaceId: "ws", tenantId: "tenant-2" } }).ok, false);

  assert.equal(
    validateStepRecord({ stepId: "step-1", state: "queued", capability: "file.read", capabilityVersion: "1.0.0", effect: "read", attempts: 0 }).ok,
    true,
  );
  assert.equal(validatePendingDecision(pendingDecision).ok, true);
  assert.equal(validatePendingDecision({ ...pendingDecision, decisionClass: "approval" }).ok, false);
  assert.ok(codesOf(validatePendingDecision({ ...pendingDecision, expiresAt: undefined })).includes("missing_field"));
  assert.ok(codesOf(validatePendingDecision({ ...pendingDecision, expiresAt: pendingDecision.createdAt })).includes("out_of_range"));
  assert.equal(
    validatePendingDecision({ ...pendingDecision, decisionClass: "user_authorization", expiresAt: 9_000 }).ok,
    true,
    "a pending user decision is finite like every other pending decision",
  );
});

test("budgets narrow the policy ceiling but never raise it", () => {
  assert.equal(validateBudgets(DEFAULT_BUDGETS).ok, true);
  assert.equal(validateBudgets({ ...DEFAULT_BUDGETS, maxSteps: 6 }).ok, true);
  assert.ok(codesOf(validateBudgets({ ...DEFAULT_BUDGETS, maxSteps: DEFAULT_BUDGETS.maxSteps + 1 })).includes("out_of_range"));
  assert.ok(codesOf(validateBudgets({ ...DEFAULT_BUDGETS, maxConcurrentMutations: 8 }, { ...DEFAULT_BUDGETS, maxConcurrentMutations: 2 })).includes("out_of_range"));
  assert.equal(validateBudgets({ ...DEFAULT_BUDGETS, maxSteps: 0 }).ok, false);
  assert.equal(validateBudgets({ ...DEFAULT_BUDGETS, unnamed: 1 }).ok, false);
});

test("capability descriptors stay precise and immutable", () => {
  const descriptor = read<CapabilityDescriptor>("capabilities/file-edit.json");
  assert.equal(validateCapabilityDescriptor(descriptor).ok, true);
  const described = describeCapability(descriptor);
  assert.equal(described.version, "1.0.0");
  assert.equal(described.inputSchema, undefined);
  assert.equal(describeCapability(descriptor, "schema").inputSchema?.type, "object");

  assert.ok(
    codesOf(validateCapabilityDescriptor({ ...descriptor, approval: { ...descriptor.approval, payloadDigestRequired: false } })).includes("bad_syntax"),
  );
  assert.ok(
    codesOf(validateCapabilityDescriptor({ ...descriptor, retry: { class: "idempotent", maxAttempts: 2, reconciliation: "none" } })).includes(
      "bad_syntax",
    ),
  );
  assert.ok(
    codesOf(validateCapabilityDescriptor({ ...descriptor, arguments: [descriptor.arguments[0], { ...descriptor.arguments[0] }] })).includes(
      "duplicate_key",
    ),
  );
  assert.equal(validateCapabilityDescriptor({ ...descriptor, version: "latest" }).ok, false);
  assert.equal(validateCapabilityDescriptor({ ...descriptor, errors: ["not_a_code"] }).ok, false);
  assert.equal(validateCapabilityDescriptor({ ...descriptor, approval: { ...descriptor.approval, binds: ["actor"] } }).ok, false);
  assert.equal(validateCapabilityDescriptor({ ...descriptor, extra: true }).ok, false);
  assert.ok(APPROVAL_BINDINGS.every((binding) => descriptor.approval.binds.includes(binding)));

  const frozen = freezeCapabilityDescriptor(read<CapabilityDescriptor>("capabilities/file-edit.json"));
  assert.equal(Object.isFrozen(frozen), true);
  assert.equal(Object.isFrozen(frozen.arguments[0]), true);
  assert.throws(() => {
    (frozen as { version: string }).version = "2.0.0";
  }, TypeError);

  assert.ok(
    codesOf(
      validateCapabilityArgument({ name: "ports", presence: "defaulted", schema: { type: "array" }, provenance: ["default"], clear: "unsupported" }),
    ).includes("missing_field"),
  );
  assert.equal(
    validateCapabilityArgument({
      name: "ports",
      presence: "optional",
      schema: { type: "array", items: { type: "number" } },
      provenance: ["user_value"],
      clear: "clears_when_null",
    }).ok,
    true,
  );

  const many = Array.from({ length: DESCRIPTION_PAGE_SIZE + 1 }, (_, index) => ({
    ...descriptor,
    capability: `cap.${String(index).padStart(2, "0")}`,
  }));
  const page = buildDescriptionIndex(many);
  assert.equal(page.entries.length, DESCRIPTION_PAGE_SIZE);
  assert.equal(typeof page.nextCursor, "string");
  assert.equal(buildDescriptionIndex(many, page.nextCursor).entries.length, 1);
});

test("total request size is bounded, not just each individual string", () => {
  const manyStrings: Record<string, string> = {};
  for (let i = 0; i < REQUEST_LIMITS.inputs; i++) manyStrings[`key_${i}`] = "x".repeat(9_000);
  const result = validateOperateRequest({ instruction: "x", inputs: manyStrings });
  assert.equal(result.ok, false);
  assert.deepEqual(codesOf(result), ["payload_too_large"]);

  const longKey = "k".repeat(REQUEST_LIMITS.keyChars + 1);
  assert.ok(codesOf(validateOperateRequest({ instruction: "x", inputs: { [longKey]: 1 } })).includes("string_too_long"));

  const args: Record<string, string> = {};
  for (let i = 0; i < REQUEST_LIMITS.argsPerCall; i++) args[`k${i}`] = "y".repeat(9_000);
  const exact = validateOperateRequest({
    action: "exact",
    request: { objective: "big", calls: [{ key: "a", capability: "file.read", capabilityVersion: "1.0.0", args }] },
  });
  assert.equal(exact.ok, false);
  assert.deepEqual(codesOf(exact), ["payload_too_large"]);
});

test("durable identity is canonical and collision-free", () => {
  assert.equal(stableStringify({ b: 1, a: [1, 2] }), '{"a":[1,2],"b":1}');
  assert.equal(canonicalDigest({ b: 1, a: 2 }), canonicalDigest({ a: 2, b: 1 }));
  assert.notEqual(canonicalDigest({ a: "1" }), canonicalDigest({ a: 1 }));
  assert.match(canonicalDigest({ a: 1 }), /^sha256:[0-9a-f]{64}$/);

  const request = parseOperateRequest({ instruction: "In src/config.ts, change the timeout to 30000." });
  assert.match(requestDigest(request), /^sha256:[0-9a-f]{64}$/);
  assert.equal(requestDigest(request), requestDigest(parseOperateRequest({ instruction: "In src/config.ts, change the timeout to 30000." })));
  assert.notEqual(requestDigest(request), requestDigest(parseOperateRequest({ instruction: "In src/config.ts, change the timeout to 30001." })));

  const base = {
    actorId: "user-1",
    tenantId: "tenant-1",
    sessionId: "s-1",
    turnId: "turn-4",
    toolCallId: "call-9",
    turnRevision: 2,
    scope: { workspaceId: "ws-api" },
  };
  const context = validateTrustedActorContext(base);
  assert.equal(context.ok, true);
  // A ":"-joined dedup key would make these two tuples collide; hashing the tuple must not.
  const left = validateTrustedActorContext({ ...base, sessionId: "s-1:t", turnId: "4" });
  const right = validateTrustedActorContext({ ...base, sessionId: "s-1", turnId: "t:4" });
  assert.equal(left.ok && right.ok, true);
  if (context.ok && left.ok && right.ok) {
    assert.notEqual(deriveDeduplicationKey(context.value), deriveDeduplicationKey(left.value));
    assert.notEqual(deriveDeduplicationKey(left.value), deriveDeduplicationKey(right.value));
    assert.equal(deriveDeduplicationKey(context.value), deriveDeduplicationKey({ ...context.value }));
    assert.notEqual(deriveDeduplicationKey(left.value), deriveDeduplicationKey(right.value));
  }
  assert.equal(validateTrustedActorContext({ actorId: "user-1" }).ok, false);
  assert.equal(validateTrustedActorContext({ ...base, model: "flash" }).ok, false);
  assert.ok(TRUSTED_CONTEXT_SOURCES.turnRevision.includes("turn"));
});

test("canonical digest is deterministic without mutable runtime installation", () => {
  assert.equal(canonicalDigest({ b: 1, a: 2 }), "sha256:d3626ac30a87e6f7a6428233b3c68299976865fa5508e4267c5415c76af7a772");
});

test("describe, inspect, cancel, and resume are the control surface", () => {
  assert.equal(validateOperateRequest({ action: "describe" }).ok, true);
  assert.equal(validateOperateRequest({ action: "describe", capability: "file.edit", detail: "schema" }).ok, true);
  assert.equal(validateOperateRequest({ action: "describe", detail: "full" }).ok, false);
  assert.equal(validateOperateRequest({ action: "describe", capability: "File.Edit" }).ok, false);
  assert.equal(validateOperateRequest({ action: "inspect", operationId: "op-42", afterSequence: 7 }).ok, true);
  assert.equal(validateOperateRequest({ action: "inspect", operationId: "op-42", afterSequence: -1 }).ok, false);
  assert.equal(validateOperateRequest({ action: "inspect", operationId: "op-42", afterSequence: 1.5 }).ok, false);
  assert.equal(validateOperateRequest({ action: "cancel", operationId: "op-42" }).ok, true);
});

test("the published schema is the same description the validator uses", () => {
  const schema = validateJsonSchemaLike(OPERATE_REQUEST_SCHEMA);
  assert.equal(schema.ok, true, schema.ok ? "" : JSON.stringify(schema.issues));
  assert.equal(OPERATE_REQUEST_SCHEMA.oneOf?.length, 6);
  assert.ok(OPERATE_REQUEST_SCHEMA.definitions?.jsonValue);

  const intentBranch = OPERATE_REQUEST_SCHEMA.oneOf?.find((branch) => branch.required?.includes("instruction"));
  assert.ok(intentBranch);
  assert.deepEqual(intentBranch.required, ["instruction"]);
  assert.equal(intentBranch.additionalProperties, false);
  assert.ok(intentBranch.properties?.inputs);
  assert.equal(intentBranch.properties?.instruction?.minLength, 1);
  assert.equal(intentBranch.properties?.instruction?.maxLength, REQUEST_LIMITS.instructionChars);
  assert.equal(intentBranch.properties?.contextRefs?.maxItems, REQUEST_LIMITS.contextRefs);
  assert.equal(intentBranch.properties?.constraints?.maxItems, REQUEST_LIMITS.constraints);
  assert.equal(intentBranch.properties?.constraints?.items?.maxLength, REQUEST_LIMITS.constraintChars);
  const inputValues = intentBranch.properties?.inputs?.additionalProperties;
  assert.equal(typeof inputValues === "object" ? inputValues.$ref : undefined, "#/definitions/jsonValue");

  const describeBranch = OPERATE_REQUEST_SCHEMA.oneOf?.find((branch) => branch.properties?.action?.const === "describe");
  assert.ok(describeBranch);
  assert.deepEqual(describeBranch.properties?.detail?.enum, ["guide", "schema"]);
  assert.equal(validateOperateRequest({ action: "describe" }).ok, true);

  const exactBranch = OPERATE_REQUEST_SCHEMA.oneOf?.find((branch) => branch.properties?.action?.const === "exact");
  const calls = exactBranch?.properties?.request?.properties?.calls;
  assert.equal(calls?.minItems, 1);
  assert.equal(calls?.maxItems, REQUEST_LIMITS.exactCalls);
  assert.equal(calls?.items?.properties?.key?.pattern, "^[a-z][a-z0-9_]*$");
  assert.equal(calls?.items?.properties?.capabilityVersion?.pattern, "^\\d+\\.\\d+(\\.\\d+)?$");
  assert.equal(calls?.items?.properties?.dependsOn?.maxItems, REQUEST_LIMITS.dependsOnPerCall);
  assert.equal(calls?.items?.properties?.argsFrom?.maxProperties, REQUEST_LIMITS.bindingsPerCall);
  const argValues = calls?.items?.properties?.args?.additionalProperties;
  assert.equal(typeof argValues === "object" ? argValues.$ref : undefined, "#/definitions/jsonValue");
  assert.ok(EXACT_PLAN_RULES.some((rule) => /acyclic/i.test(rule)));
  assert.ok((calls?.description ?? "").includes("acyclic"), "cross-field DAG rules travel with the schema");

  const resumeBranch = OPERATE_REQUEST_SCHEMA.oneOf?.find((branch) => branch.properties?.action?.const === "resume");
  assert.equal(resumeBranch?.properties?.expectedRevision?.minimum, 1);

  assert.equal(DEFAULT_INSTRUCTION_DESCRIPTION.startsWith("Give one bounded instruction"), true);
  assert.equal(CONTRACT_VERSION, "v1");
});

test("every fixture validates against its declared kind and none is unlisted", () => {
  const manifest = read<Manifest>("manifest.json");
  assert.equal(manifest.contractVersion, CONTRACT_VERSION);
  const validators: Record<string, (value: unknown) => ResultLike> = {
    operate_request: validateOperateRequest,
    compact_result: validateCompactOperationResult,
    operation_snapshot: validateOperationSnapshot,
    operation_event: validateOperationEvent,
    capability_descriptor: validateCapabilityDescriptor,
    recorded_decision: validateRecordedDecision,
    pending_decision: validatePendingDecision,
  };
  const kinds = [...new Set(manifest.files.map((entry) => entry.kind))].sort();
  assert.deepEqual(Object.keys(validators).sort(), kinds);
  for (const entry of manifest.files) {
    const validate = validators[entry.kind];
    assert.ok(validate, `no validator for kind ${entry.kind}`);
    const result = validate(read<unknown>(entry.path));
    assert.equal(result.ok, true, `${entry.path} -> ${codesOf(result).join(",")}`);
  }
  const listed = new Set(manifest.files.map((entry) => entry.path));
  const onDisk = (fs.readdirSync(FIXTURES, { recursive: true }) as string[])
    .filter((name) => name.endsWith(".json") && name !== "manifest.json")
    .map((name) => name.split(path.sep).join("/"));
  for (const name of onDisk) assert.ok(listed.has(name), `fixture ${name} is missing from the manifest`);
  assert.equal(listed.size, manifest.files.length);
});

test("an operation deadline can never overflow a timer", () => {
  const operationDeadlineMs = BUDGET_NODE.fields.operationDeadlineMs;
  if (operationDeadlineMs.t !== "int") throw new Error("operationDeadlineMs is expected to be an int field");
  // Every deadline-derived delay in the executor and scheduler relies on this: Node
  // clamps a `setTimeout` delay above 2^31-1 ms (24.8 days) to fire almost immediately,
  // so a timer built from an operation deadline is only ever safe because the budget
  // schema's own ceiling stays under that bound.
  assert.ok(operationDeadlineMs.max < 2 ** 31 - 1);

  // The ceiling is enforced, not just declared: accept() refuses a request whose budget
  // asks for one millisecond more than the schema's own maximum.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "operation-deadline-ceiling-"));
  const ownership = { ownerId: "ceiling-test", assertHeld: () => {} };
  const store = new OperationStore({ root, ownership, now: Date.now, capabilityMetadata: () => undefined, dispatchAuthority: new DispatchAuthority() });
  const context: TrustedActorContext = {
    actorId: "actor-1", tenantId: "tenant-1", sessionId: "session-1", turnId: "turn-1", toolCallId: "call-1", turnRevision: 1, scope: { workspaceId: "ws-1" },
  };
  assert.throws(() => store.accept({
    request: { instruction: "Restore devstack to snap-1" },
    context,
    budgets: { operationDeadlineMs: operationDeadlineMs.max + 1 },
  }));
});
