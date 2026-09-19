import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  CAPABILITY_CONTRACTS,
  CapabilityRegistry,
  INITIAL_READ_CAPABILITIES,
  approvalPrompt,
  capabilityRegistry,
  createBenchCapabilityRuntime,
} from "../src/operations/capabilities.ts";
import type { CapabilityDefinition } from "../src/operations/capabilities.ts";
import { validateCapabilityDescriptor } from "../src/operations/contracts.ts";
import type { RecordedDecision, ResumeExpectation } from "../src/operations/contracts.ts";
import { toJsonSchema } from "../src/operations/shape.ts";
import type { Validation } from "../src/operations/shape.ts";
import { WORKSPACE_PROCESS_ACTIONS, validateCapabilityArgs } from "../src/operations/arguments.ts";
import type { ValidatedCapabilityArgs } from "../src/operations/arguments.ts";
import * as kloudliteExtension from "../../pi/kloudlite.ts";
import type { DispatchOutcome, ToolResult } from "../../pi/kloudlite.ts";
import { DECLINED, dispatchWithPolicy, isOwnBench, toolResultOf } from "../../pi/kloudlite.ts";
import * as workspaceToolsExtension from "../../pi/workspace-tools.ts";
import { forbidden, mutates } from "../../pi/workspace-tools.ts";
import { TOOLS, gated, question } from "../../pi/catalog.ts";
import { createPlatformAdapters } from "../src/operations/adapters.ts";
import { Bench } from "../src/bench.ts";
import { capabilityRuntime } from "./operations-runtime-fixture.ts";

/**
 * `tsc --module nodenext` types the default import of a CommonJS-format module as the module
 * namespace, so the two extension entry points are read out of it explicitly. Node's ESM loader
 * exposes the same `default`, and a missing one fails here instead of calling nothing.
 */
function defaultExport<T>(module: unknown, name: string): T {
  const entry = (module as unknown as { default?: unknown }).default;
  if (typeof entry !== "function") throw new Error(`${name} does not export a default function`);
  return entry as T;
}
const kloudlite = defaultExport<(pi: any) => unknown>(kloudliteExtension, "kloudlite");
const workspaceTools = defaultExport<(pi: any) => unknown>(workspaceToolsExtension, "workspace-tools");

const text = (t: string, isError?: boolean): ToolResult => ({ content: [{ type: "text", text: t }], ...(isError ? { isError } : {}) });

const definition = (capability: string): CapabilityDefinition => {
  const found = CAPABILITY_CONTRACTS.find((entry) => entry.descriptor.capability === capability);
  assert.ok(found, `no contract for ${capability}`);
  return found;
};
const validateArgs = (capability: string, args: unknown): Validation<ValidatedCapabilityArgs> => {
  const entry = definition(capability);
  return validateCapabilityArgs({ shape: entry.shape, arguments: entry.descriptor.arguments, checks: entry.checks }, args);
};
function expectValid(result: Validation<ValidatedCapabilityArgs>, message?: string): ValidatedCapabilityArgs {
  if (!result.ok) assert.fail(`${message ?? "expected valid args"}: ${JSON.stringify(result.issues)}`);
  return result.value;
}
const codes = (result: Validation<unknown>): string[] => (result.ok ? [] : result.issues.map((issue) => issue.code));
function expectRefused<T extends { outcome: string }>(result: T): Extract<T, { outcome: "refused" }> {
  if (result.outcome !== "refused") assert.fail(`expected a refusal, got ${result.outcome}`);
  return result as Extract<T, { outcome: "refused" }>;
}

/** The same shape `bench-tools.test.ts` uses: just enough pi to see what an extension registers. */
type Registered = { name: string; description: string; parameters: any; execute: (...a: any[]) => Promise<any> };
function fakePi() {
  const tools: Registered[] = [];
  const hooks: Record<string, ((ev: any) => Promise<any>)[]> = {};
  const pi = {
    registerTool: (t: Registered) => tools.push(t),
    on: (name: string, fn: (ev: any) => Promise<any>) => ((hooks[name] ??= []).push(fn), undefined),
    getAllTools: () => tools,
  } as any;
  return { pi, tools };
}

const withEnv = (vars: Record<string, string | undefined>) => {
  const saved = Object.fromEntries(Object.keys(vars).map((k) => [k, process.env[k]]));
  // Per key: `process.env.X = undefined` writes the string "undefined", which is truthy and would
  // put the extension in the wrong mode. An absent value is a deletion, never an assignment.
  for (const [k, v] of Object.entries(vars)) if (v === undefined) delete process.env[k];
  else process.env[k] = v;
  return () => {
    for (const [k, v] of Object.entries(saved)) if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  };
};

const deferred = <T>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
};
const tick = () => new Promise((r) => setTimeout(r, 0));
const decisionExpectation = (overrides: Partial<ResumeExpectation> = {}): Omit<ResumeExpectation, "payloadDigest" | "now"> & { now?: number } => ({
  actorId: "actor-1", tenantId: "tenant-1", sessionId: "session-1", operationId: "op-1", stepId: "step-1", decisionId: "decision-1", decisionClass: "user_authorization", revision: 1, expiryBound: 2_000, now: 1_000, ...overrides,
});
const grantedDecision = (expectation: ResumeExpectation, overrides: Partial<RecordedDecision> = {}): RecordedDecision => ({
  recordId: "record-1", operationId: expectation.operationId, stepId: expectation.stepId, decisionId: expectation.decisionId, decisionClass: "user_authorization", actorId: expectation.actorId, tenantId: expectation.tenantId, sessionId: expectation.sessionId, payloadDigest: expectation.payloadDigest, revision: expectation.revision, policySource: "user_ui", outcome: "granted", recordedAt: expectation.now - 1, expiresAt: expectation.expiryBound, ...overrides,
});
const approvedDispatch = async (
  registry: CapabilityRegistry,
  capability: string,
  args: unknown,
  runtime: ReturnType<typeof capabilityRuntime>,
  overrides: Partial<RecordedDecision> = {},
  allowedPolicySources: readonly ("user_ui" | "trusted_policy")[] = ["user_ui"],
) => {
  const deps = { runtime, decision: decisionExpectation(), allowedPolicySources };
  const prepared = registry.prepare(capability, args, deps);
  assert.equal(prepared.ok, true);
  if (!prepared.ok || !prepared.approval) assert.fail("expected an approval request");
  const record = grantedDecision(prepared.approval.expectation, overrides);
  return registry.dispatch(capability, args, { runtime, allowedPolicySources, approval: { record, expectation: prepared.approval.expectation } });
};

test("every shipped contract is valid, and its published schema is its executable shape", () => {
  assert.ok(CAPABILITY_CONTRACTS.length >= 15);
  assert.deepEqual(
    capabilityRegistry.descriptors().map((descriptor) => descriptor.capability).sort(),
    CAPABILITY_CONTRACTS.map((entry) => entry.descriptor.capability).sort(),
  );
  for (const entry of CAPABILITY_CONTRACTS) {
    const result = validateCapabilityDescriptor(entry.descriptor);
    assert.ok(result.ok, `${entry.descriptor.capability}: ${result.ok ? "" : JSON.stringify(result.issues)}`);
    assert.deepEqual(entry.descriptor.inputSchema, toJsonSchema(entry.shape), entry.descriptor.capability);
    assert.ok(Object.isFrozen(entry.descriptor), `${entry.descriptor.capability} is frozen`);
  }
});

test("capability output schemas expose capability-specific structured values", () => {
  for (const entry of CAPABILITY_CONTRACTS) {
    const schema = entry.descriptor.outputSchema;
    assert.equal(schema.properties?.content, undefined, entry.descriptor.capability);
    assert.ok(schema.type === "object" || schema.type === "array" || schema.type === "string" || (schema.oneOf?.length ?? 0) > 0, entry.descriptor.capability);
  }
  assert.equal(definition("workspace.list").descriptor.outputSchema.type, "array");
  assert.equal(definition("workspace.inspect").descriptor.outputSchema.properties?.id?.type, "string");
  assert.equal(definition("bench.process.list").descriptor.outputSchema.items?.properties?.command?.type, "string");
});

test("process actions reject fields that do not belong to the selected action", () => {
  for (const args of [
    { action: "list", id: "p1" },
    { action: "logs", pattern: "ready", id: "p1" },
    { action: "watch", since: 1, id: "p1" },
    { action: "start", id: "p1", command: "npm run dev" },
    { action: "stop", data: "x", id: "p1" },
    { action: "write", signal: "TERM", id: "p1", data: "x" },
  ]) {
    const capability = ["start", "stop", "write"].includes(args.action) ? "workspace.process.control" : "workspace.process.read";
    const result = validateArgs(capability, args);
    assert.equal(result.ok, false, JSON.stringify(args));
    assert.ok(codes(result).includes("mixed_action"), JSON.stringify(args));
  }
});

test("trusted runtime adapters receive clear state separately from ordinary arguments", async () => {
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.intercept"]);
  const seen: unknown[] = [];
  const runtime = capabilityRuntime({
      "environment.intercept": async (input) => (seen.push(input), { ok: true, value: { id: "devstack", service: "postgres", cleared: true } }),
  });
  const result = await approvedDispatch(registry, "environment.intercept", { id: "devstack", service: "postgres", workspace: null }, runtime);
  assert.equal(result.outcome, "completed");
  assert.deepEqual(seen, [{ args: { id: "devstack", service: "postgres" }, states: { id: { kind: "known", value: "devstack" }, service: { kind: "known", value: "postgres" }, workspace: { kind: "explicitly_clear" }, ports: { kind: "unspecified" } }, signal: undefined }]);
});

test("an omitted intercept target is not dispatched as a clear", async () => {
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.intercept"]);
  const runtime = capabilityRuntime({ "environment.intercept": async () => ({ ok: true, value: {} }) });
  const refused = expectRefused(await registry.dispatch("environment.intercept", { id: "devstack", service: "postgres" }, { runtime }));
  assert.equal(refused.code, "invalid_args");
});

test("mutations accept only a matching, live, unused RecordedDecision", async () => {
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"]);
  let runs = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => (runs++, { ok: true, value: { id: "devstack" } }) });
  const attempt = (overrides: Partial<RecordedDecision> = {}) => approvedDispatch(registry, "environment.restore", { id: "devstack", snapshot: "snap-1" }, runtime, overrides);
  for (const forged of [
    { actorId: "actor-2" }, { sessionId: "session-2" }, { stepId: "step-2" }, { payloadDigest: "sha256:" + "0".repeat(64) }, { revision: 2 }, { expiresAt: 3_000 }, { expiresAt: 1_000 }, { usedAt: 999 }, { outcome: "denied" as const },
  ]) {
    const refused = expectRefused(await attempt(forged));
    assert.ok(["decision_mismatch", "validation_failure", "decision_expired", "decision_replayed", "permission_denied"].includes(refused.code));
  }
  assert.equal(runs, 0);
  assert.equal((await attempt()).outcome, "completed");
  assert.equal(runs, 1);
});

test("bench process adapter filters ended and workspace rows, orders them, and caps output", async () => {
  const processRows = Array.from({ length: 205 }, (_, i) => ({ id: `p${i}`, session: "s1", workspace: i % 2 ? "api" : "web", name: `job-${i}`, command: `run ${i}`, started: i, ...(i % 3 === 0 ? { ended: i + 1, code: 0 } : {}) }));
  const runtime = createBenchCapabilityRuntime({ all: () => processRows }, {});
  const running = await capabilityRegistry.dispatch("bench.process.list", { workspace: "api" }, { runtime });
  assert.equal(running.outcome, "completed");
  if (running.outcome !== "completed" || !Array.isArray(running.result)) assert.fail("expected process rows");
  assert.ok(running.result.length <= 200);
  assert.ok(running.result.every((row: any) => row.workspace === "api" && row.ended === undefined));
  assert.ok((running.result[0] as any).started > (running.result.at(-1) as any).started);
});

test("a capability runtime is opaque and resolves only reviewed capability adapters", () => {
  const runtime = capabilityRuntime({ "workspace.list": async () => ({ ok: true, value: [] }) });
  assert.equal(runtime.resolve("workspace.list") instanceof Function, true);
  assert.equal(runtime.resolve("kl_workspaces"), undefined, "legacy tool names are not dispatch keys");
  assert.equal((runtime as any).tools, undefined, "raw tool maps are not exposed");
  assert.equal(Object.isFrozen(runtime), true);
});

test("every enabled read is wired in the real Bench runtime and returns schema-valid structured output", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cap-bench-runtime-"));
  const bench = new Bench({
    dir, readOnly: false, model: "test",
    platform: async (_method, route) => route.startsWith("/v1/workspaces/ws-1")
      ? { status: 200, data: { id: "ws-1", name: "api", state: "running", packages: [] } }
      : { status: 200, data: [{ id: "ws-1", name: "api", state: "running" }, { id: "bench-deadbeef", name: "hidden", kind: "bench" }] },
  });
  try {
    const args: Record<string, unknown> = {
      "bench.process.list": {}, "skill.read": {}, "workspace.inspect": { id: "api" }, "workspace.list": {}, "workspace.progress": { id: "ws-1" },
    };
    for (const capability of INITIAL_READ_CAPABILITIES) {
      assert.ok(bench.capabilityRuntime.resolve(capability), capability);
      const result = await capabilityRegistry.dispatch(capability, args[capability], { runtime: bench.capabilityRuntime });
      assert.equal(result.outcome, "completed", `${capability}: ${JSON.stringify(result)}`);
      if (capability === "workspace.list" && result.outcome === "completed") assert.deepEqual(result.result, [{ id: "ws-1", name: "api", state: "running" }]);
    }
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("runtime rejects adapter output that does not satisfy the descriptor schema", async () => {
  const runtime = capabilityRuntime({ "workspace.list": async () => ({ ok: true, value: "not rows" }) });
  const result = await capabilityRegistry.dispatch("workspace.list", {}, { runtime });
  assert.equal(result.outcome, "failed");
  if (result.outcome === "failed") assert.equal(result.code, "validation_failure");
});

test("runtime rejects missing required output fields and non-JSON output", async () => {
  for (const value of [{ name: "api" }, { id: "ws-1", name: "api", state: "running", bad: undefined }]) {
    const runtime = capabilityRuntime({ "workspace.inspect": async () => ({ ok: true, value: value as any }) });
    const result = await capabilityRegistry.dispatch("workspace.inspect", { id: "ws-1" }, { runtime });
    assert.equal(result.outcome, "failed", JSON.stringify(value));
    if (result.outcome === "failed") assert.equal(result.code, "validation_failure");
  }
});

test("a definitive HTTP 5xx is provider failure; only a thrown mutation transport is unknown", async () => {
  const responseRuntime = capabilityRuntime(createPlatformAdapters(async () => ({ status: 503, data: { error: "unavailable" } })));
  const transportRuntime = capabilityRuntime(createPlatformAdapters(async () => { throw new Error("connection reset after send"); }));
  for (const [runtime, code] of [[responseRuntime, "provider_failure"], [transportRuntime, "unknown_outcome"]] as const) {
    const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"]);
    const result = await approvedDispatch(registry, "environment.restore", { id: "env-1", snapshot: "snap-1" }, runtime);
    assert.equal(result.outcome, "failed");
    if (result.outcome === "failed") assert.equal(result.code, code);
  }
});

test("trusted policy decisions run only when the trusted dispatch context allows that source", async () => {
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"]);
  let runs = 0;
  const runtime = capabilityRuntime({ "environment.restore": async () => (runs++, { ok: true, value: { id: "env-1" } }) });
  const dispatch = (allowedPolicySources: readonly ("user_ui" | "trusted_policy")[]) => approvedDispatch(registry, "environment.restore", { id: "env-1", snapshot: "snap-1" }, runtime, { policySource: "trusted_policy" }, allowedPolicySources);
  const allowed = await dispatch(["trusted_policy"]);
  assert.equal(allowed.outcome, "completed");
  const downgraded = expectRefused(await dispatch(["user_ui"]));
  assert.equal(downgraded.code, "forged_approval");
  assert.equal(runs, 1);
});

test("malformed decisions are refused from the shaped record before binding checks", async () => {
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"]);
  const runtime = capabilityRuntime({ "environment.restore": async () => ({ ok: true, value: { id: "env-1" } }) });
  const refused = expectRefused(await approvedDispatch(registry, "environment.restore", { id: "env-1", snapshot: "snap-1" }, runtime, { expiresAt: "later" as any }));
  assert.equal(refused.code, "forged_approval");
});

test("shared mutation adapters preserve collections and use explicit clear transport", async () => {
  const calls: Array<{ method: string; route: string; body?: unknown }> = [];
  const platform = async (method: string, route: string, body?: unknown) => {
    calls.push({ method, route, body });
    if (method === "GET" && route.startsWith("/v1/environments/")) return { status: 200, data: { services: [{ name: "db", image: "postgres", x: { keep: true } }, { name: "api", image: "old", command: ["old"] }] } };
    if (method === "GET" && route.startsWith("/v1/workspaces/")) return { status: 200, data: { packages: ["go", "nodejs@20"] } };
    if (method === "PATCH" && route.startsWith("/v1/workspaces/")) return { status: 200, data: (body as any).packages };
    return { status: 200, data: body ?? { done: true } };
  };
  const runtime = capabilityRuntime(createPlatformAdapters(platform));
  const dispatch = async (capability: string, args: unknown) => {
    const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, [capability]);
    return approvedDispatch(registry, capability, args, runtime);
  };
  assert.equal((await dispatch("environment.intercept", { id: "env-1", service: "db", workspace: null })).outcome, "completed");
  assert.deepEqual(calls.at(-1), { method: "DELETE", route: "/v1/environments/env-1/intercepts/db", body: undefined });
  assert.equal((await dispatch("environment.service.put", { id: "env-1", service: { name: "api", image: "new" } })).outcome, "completed");
  assert.deepEqual((calls.at(-1)?.body as any).services, [{ name: "db", image: "postgres", x: { keep: true } }, { name: "api", image: "new" }]);
  assert.equal((await dispatch("environment.service.rm", { id: "env-1", name: "api" })).outcome, "completed");
  assert.deepEqual((calls.at(-1)?.body as any).services, [{ name: "db", image: "postgres", x: { keep: true } }]);
  assert.equal((await dispatch("workspace.packages.add", { workspace: "ws-1", packages: ["nodejs@22", "rustc"] })).outcome, "completed");
  assert.deepEqual((calls.at(-1)?.body as any).packages, ["go", "nodejs@22", "rustc"]);
  assert.equal((await dispatch("workspace.packages.rm", { workspace: "ws-1", packages: ["nodejs"] })).outcome, "completed");
  assert.deepEqual((calls.at(-1)?.body as any).packages, ["go"]);
});

test("shared mutation adapters preserve restore/create routes and unknown outcomes", async () => {
  const calls: string[] = [];
  const runtime = capabilityRuntime(createPlatformAdapters(async (method, route) => {
    calls.push(`${method} ${route}`);
    if (method !== "GET") throw new Error("connection reset after send");
    return { status: 200, data: [] };
  }));
  for (const [capability, args] of [
    ["environment.restore", { id: "env-1", snapshot: "snap-1" }],
    ["workspace.create", { name: "api", repo: "org/repo", branch: "main" }],
    ["environment.create", { name: "dev", from_snapshot: "snap-2", services: [] }],
  ] as const) {
    const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, [capability]);
    const result = await approvedDispatch(registry, capability, args, runtime);
    assert.equal(result.outcome, "failed");
    if (result.outcome === "failed") assert.equal(result.code, "unknown_outcome");
  }
  assert.deepEqual(calls, ["POST /v1/environments/env-1/restore-in-place", "POST /v1/workspaces", "POST /v1/environments/restore"]);
});

test("the pilot allowlist is reads that cannot reach a workspace's hands", () => {
  assert.deepEqual(
    [...INITIAL_READ_CAPABILITIES].sort(),
    ["bench.process.list", "skill.read", "workspace.inspect", "workspace.list", "workspace.progress"],
  );
  const hands = new Set(["read", "write", "edit", "patch", "bash", "process", "grep", "find", "ls"]);
  for (const name of INITIAL_READ_CAPABILITIES) {
    const entry = definition(name);
    assert.equal(entry.descriptor.effect, "read", name);
    assert.ok(entry.descriptor.scope === "bench" || entry.descriptor.scope === "platform", name);
    assert.ok(!hands.has(String(entry.target)), `${name} must not target a workspace hand (${entry.target})`);
    assert.equal(entry.disabled, undefined, name);
    assert.equal(capabilityRegistry.isEnabled(name), true, name);
  }
  // The workspace/environment contracts exist for review; none of them is dispatchable in the pilot.
  for (const entry of CAPABILITY_CONTRACTS) {
    if (entry.descriptor.scope !== "workspace" && entry.descriptor.scope !== "environment") continue;
    assert.equal(capabilityRegistry.isEnabled(entry.descriptor.capability), false, entry.descriptor.capability);
    assert.ok(entry.disabled, `${entry.descriptor.capability} must say why it is not enabled`);
  }
});

test("describe is a deterministic lookup: guide by default, schema on request, no mutating contracts", () => {
  const guide = capabilityRegistry.describe({ action: "describe", capability: "workspace.inspect" });
  if (!guide.ok) assert.fail(guide.message);
  assert.equal(guide.detail, "guide");
  assert.equal(guide.entries.length, 1);
  assert.equal(guide.entries[0].inputSchema, undefined);
  assert.equal(guide.entries[0].rules, undefined);
  assert.ok(guide.entries[0].guide.length > 0);

  const schema = capabilityRegistry.describe({ action: "describe", capability: "workspace.inspect", detail: "schema" });
  if (!schema.ok) assert.fail(schema.message);
  assert.equal(schema.detail, "schema");
  assert.deepEqual(schema.entries[0].inputSchema, toJsonSchema(definition("workspace.inspect").shape));
  assert.ok((schema.entries[0].rules ?? []).length > 0);
  assert.deepEqual(schema, capabilityRegistry.describe({ action: "describe", capability: "workspace.inspect", detail: "schema" }));

  const index = capabilityRegistry.describe({ action: "describe" });
  if (!index.ok) assert.fail(index.message);
  assert.deepEqual(index.entries.map((entry) => entry.capability), [...INITIAL_READ_CAPABILITIES].sort());

  const disabled = capabilityRegistry.describe({ action: "describe", capability: "environment.restore" });
  if (disabled.ok) assert.fail("a disabled capability must not be described");
  assert.equal(disabled.code, "unsupported_capability");
});

test("invalid arguments are refused before approval and before the handler", async () => {
  const calls: string[] = [];
  const deps = {
    runtime: capabilityRuntime({
      "workspace.inspect": async () => {
        calls.push("run");
        return { ok: true as const, value: { id: "ws-1" } };
      },
    }),
  };
  const unknownField = expectRefused(await capabilityRegistry.dispatch("workspace.inspect", { id: "api", extra: 1 }, deps));
  assert.equal(unknownField.code, "invalid_args");
  assert.ok(unknownField.issues?.some((issue) => issue.path === "$.extra"));
  const missing = expectRefused(await capabilityRegistry.dispatch("workspace.inspect", {}, deps));
  assert.equal(missing.code, "invalid_args");
  const wrongType = expectRefused(await capabilityRegistry.dispatch("workspace.inspect", { id: 7 }, deps));
  assert.equal(wrongType.code, "invalid_args");
  assert.deepEqual(calls, [], "nothing ran and nobody was asked");
});

test("an enabled read runs through its wired handler; a refusal or an error is never completed", async () => {
  const seen: unknown[] = [];
  const runtime = (adapter: any) => capabilityRuntime({ "workspace.list": adapter });
  const done = await capabilityRegistry.dispatch("workspace.list", { team: "acme" }, {
    runtime: runtime(async ({ args }: any) => (seen.push(args), { ok: true, value: [{ id: "api", name: "api", state: "running" }, { id: "web", name: "web", state: "stopped" }] })),
  });
  assert.equal(done.outcome, "completed");
  assert.deepEqual(seen, [{ team: "acme" }]);

  const unwired = expectRefused(await capabilityRegistry.dispatch("workspace.list", {}, {}));
  assert.equal(unwired.code, "unsupported_capability");

  const threw = await capabilityRegistry.dispatch("workspace.list", {}, { runtime: runtime(async () => { throw new Error("the platform is down"); }) });
  assert.equal(threw.outcome, "failed");
  const errored = await capabilityRegistry.dispatch("workspace.list", {}, { runtime: runtime(async () => ({ ok: false, error: { code: "permission_denied", message: "you are not allowed", retryable: false } })) });
  assert.equal(errored.outcome, "failed", "a handler that reports an error is not a success");

  const stale = expectRefused(await capabilityRegistry.dispatch("workspace.list", {}, { runtime: runtime(async () => ({ ok: true, value: [] })) }, "2.0.0"));
  assert.equal(stale.code, "stale_contract");
});

test("a prepared denial refuses the step and runs nothing; a prepared grant runs it once", async () => {
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"]);
  const calls: string[] = [];
  const runtime = capabilityRuntime({ "environment.restore": async ({ args }) => (calls.push(`run:${String(args.id)}`), { ok: true, value: { id: args.id } }) });
  const args = { id: "devstack", snapshot: "snap-1" };
  const prepared = registry.prepare("environment.restore", args, { runtime, decision: decisionExpectation() });
  assert.equal(prepared.ok, true);
  if (!prepared.ok || !prepared.approval) assert.fail("expected an approval request");
  assert.equal(prepared.approval.prompt, "Restore snapshot snap-1 into environment devstack, in place");
  assert.match(prepared.approval.payloadDigest, /^sha256:[0-9a-f]{64}$/);

  const denied = expectRefused(await registry.dispatch("environment.restore", args, { runtime, approval: {
    record: grantedDecision(prepared.approval.expectation, { outcome: "denied" }),
    expectation: prepared.approval.expectation,
  } }));
  assert.equal(denied.code, "permission_denied");
  assert.equal(denied.reason, DECLINED);
  assert.equal(calls.filter((call) => call.startsWith("run:")).length, 0, "a denial never reaches the handler");

  const granted = await registry.dispatch("environment.restore", args, { runtime, approval: {
    record: grantedDecision(prepared.approval.expectation),
    expectation: prepared.approval.expectation,
  } });
  assert.equal(granted.outcome, "completed");
  assert.deepEqual(calls.filter((call) => call.startsWith("run:")), ["run:devstack"]);
});

test("a mutating plan with no approval channel is refused, never run", async () => {
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"]);
  const calls: string[] = [];
  const refused = expectRefused(
    await registry.dispatch("environment.restore", { id: "devstack", snapshot: "snap-1" }, {
      runtime: capabilityRuntime({
        "environment.restore": async () => {
          calls.push("run");
          return { ok: true, value: { id: "devstack" } };
        },
      }),
    }),
  );
  assert.equal(refused.code, "permission_denied");
  assert.deepEqual(calls, [], "a raw handler is not an executor entrypoint");
});

test("every reviewed mutation is refused by the pilot registry without touching a handler", async () => {
  for (const entry of CAPABILITY_CONTRACTS) {
    if (capabilityRegistry.isEnabled(entry.descriptor.capability)) continue;
    const calls: string[] = [];
    const refused = expectRefused(
      await capabilityRegistry.dispatch(
        entry.descriptor.capability,
        { id: "api", name: "api", snapshot: "snap-1", workspace: "api", packages: ["go"], service: { name: "nats", image: "nats:2" }, action: "start", command: "npm run dev" },
        {
          runtime: capabilityRuntime({
            [entry.descriptor.capability]: async () => {
              calls.push("run");
              return { ok: true, value: {} };
            },
          }),
        },
      ),
    );
    assert.equal(refused.code, "unsupported_capability", entry.descriptor.capability);
    assert.deepEqual(calls, [], entry.descriptor.capability);
  }
});

test("scope and path checks refuse before approval and before any side effect", async () => {
  const restoreEnv = withEnv({ KL_WORKSPACE_ID: "bench-ada" });
  try {
    const calls: string[] = [];
    const refused = expectRefused(
      await capabilityRegistry.dispatch("workspace.inspect", { id: "bench-ada" }, {
        runtime: capabilityRuntime({
          "workspace.inspect": async () => {
            calls.push("run");
            return { ok: true, value: { id: "ws-1" } };
          },
        }),
      }),
    );
    assert.equal(isOwnBench("bench-ada"), true);
    assert.equal(refused.code, "scope_denied");
    assert.deepEqual(calls, []);
  } finally {
    restoreEnv();
  }

  // The registered file/command tools' own confinement check, through the shared adapter.
  const reached: string[] = [];
  const refused = expectRefused(
    await dispatchWithPolicy({
      capability: "bash",
      effect: "write",
      approval: {
        required: "user",
        obtain: async () => {
          reached.push("approve");
          return true;
        },
      },
      inspect: () => {
        const no = forbidden("curl https://dev.kloudlite.io/v1/repos");
        return no ? { code: "scope_denied", reason: no } : undefined;
      },
      run: async () => {
        reached.push("run");
        return text("ran");
      },
    }),
  );
  assert.equal(refused.code, "scope_denied");
  assert.match(refused.reason, /^refused: the platform is reached only through kl_\* tools/);
  assert.equal(reached.length, 0, "a refused path never asks a person and never runs");

  const allowed = await dispatchWithPolicy({
    capability: "bash",
    effect: "write",
    approval: { required: "user", obtain: async () => true },
    inspect: () => {
      const no = forbidden("npm test");
      return no ? { code: "scope_denied", reason: no } : undefined;
    },
    run: async () => {
      reached.push("allowed");
      return text("ran");
    },
  });
  assert.equal(allowed.outcome, "completed");
  assert.deepEqual(reached, ["allowed"]);
});

test("the legacy tool surface keeps a refusal explicit: declined text, error text, or outcome", () => {
  const refused: DispatchOutcome = { outcome: "refused", code: "permission_denied", reason: DECLINED };
  assert.deepEqual(toolResultOf(refused, "plain"), { content: [{ type: "text", text: DECLINED }] });
  assert.equal(toolResultOf(refused).isError, true);
  assert.equal(toolResultOf({ outcome: "refused", code: "scope_denied", reason: "no" }, "plain").isError, true);
  assert.equal(toolResultOf({ outcome: "completed", result: text("ok") }).isError, undefined);
  assert.equal(toolResultOf({ outcome: "failed", code: "execution_failure", result: text("broken") }).isError, true);
});

test("the registered tools and the capability contracts agree on names, effects and approval", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cap-registry-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restoreEnv = withEnv({
    KL_WORKSPACE_ID: "bench-ada",
    KL_TEAM: "acme",
    KL_TOOLS_WORKSPACE: undefined,
    KL_FORK: undefined,
    KL_API_URL: "http://127.0.0.1:1",
    KL_TOOL_TOKEN_FILE: path.join(dir, "token"),
    KL_TOOLS_ADDRESS: "127.0.0.1:1",
  });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    workspaceTools(pi);
    const registered = new Map(tools.map((tool) => [tool.name, tool]));
    const propertiesOf = (name: string): string[] => Object.keys(registered.get(name)?.parameters?.properties ?? {}).sort();

    for (const name of INITIAL_READ_CAPABILITIES) {
      const entry = definition(name);
      // Bench-owned metadata names its own boundary; O08 wires it. Everything else must be a tool
      // this session actually registers, so a fixture that went into the wrong mode fails loudly.
      if (entry.target === entry.descriptor.capability) continue;
      assert.ok(registered.has(String(entry.target)), `${name} targets the unregistered ${entry.target}`);
      const spec = TOOLS.find((t) => t.name === entry.target);
      assert.ok(spec, String(entry.target));
      assert.equal(entry.descriptor.effect, spec.effect, name);
      assert.equal(entry.descriptor.approval.required === "none", !gated(String(entry.target)), name);
      assert.deepEqual(Object.keys(entry.descriptor.inputSchema.properties ?? {}).sort(), propertiesOf(String(entry.target)), name);
    }

    const mutations: [string, string][] = [
      ["workspace.create", "kl_workspace_create"],
      ["environment.create", "kl_environment_create"],
      ["environment.restore", "kl_environment_restore"],
      ["environment.intercept", "kl_intercept"],
      ["environment.service.put", "kl_environment_service_add"],
      ["environment.service.rm", "kl_environment_service_rm"],
    ];
    for (const [capability, target] of mutations) {
      const entry = definition(capability);
      assert.ok(registered.has(target), `${target} is not registered in the bench fixture`);
      const spec = TOOLS.find((t) => t.name === target);
      assert.ok(spec, target);
      assert.equal(entry.descriptor.effect, spec.effect, capability);
      assert.equal(gated(target), true, target);
      assert.equal(entry.descriptor.approval.required, "user", capability);
      assert.deepEqual(Object.keys(entry.descriptor.inputSchema.properties ?? {}).sort(), propertiesOf(target), capability);
    }

    // The process tool is one registered tool with two contracts; their union is its parameter set.
    assert.ok(registered.has("process"), "the process tool is registered");
    const processKeys = [
      ...new Set([
        ...Object.keys(definition("workspace.process.read").descriptor.inputSchema.properties ?? {}),
        ...Object.keys(definition("workspace.process.control").descriptor.inputSchema.properties ?? {}),
      ]),
    ].sort();
    assert.deepEqual(processKeys, propertiesOf("process"));
    for (const [action, spec] of Object.entries(WORKSPACE_PROCESS_ACTIONS)) {
      assert.equal(spec.effect === "write", mutates("process", { action }), action);
    }

    // Package changes are writes and must use the same approval boundary as their contracts.
    assert.equal(gated("kl_pkg_add"), true);
    assert.equal(gated("kl_pkg_rm"), true);
    assert.ok(registered.has("kl_pkg_add"), "kl_pkg_add is registered");
    for (const capability of ["workspace.packages.add", "workspace.packages.rm"]) {
      const entry = definition(capability);
      assert.equal(entry.descriptor.approval.required, "user");
      assert.ok(entry.descriptor.rules.some((rule) => /same approval card/.test(rule)), capability);
      assert.deepEqual(Object.keys(entry.descriptor.inputSchema.properties ?? {}).sort(), propertiesOf("kl_pkg_add"), capability);
    }
  } finally {
    restoreEnv();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("an incomplete process action is refused before the card and before the tool server", async () => {
  const seen: string[] = [];
  const cards: string[] = [];
  const answerYes = http.createServer((req, res) => {
    let body = "";
    req.on("data", (d) => (body += d));
    req.on("end", () => {
      if (req.url?.startsWith("/proposals/")) {
        res.writeHead(200, { "content-type": "application/json" });
        return res.end(JSON.stringify({ answer: "yes" }));
      }
      seen.push(`${req.url} ${body}`);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(req.url?.endsWith("/process_list") ? { processes: [] } : { id: "p1" }));
    });
  });
  await new Promise<void>((r) => answerYes.listen(0, "127.0.0.1", r));
  const at = `127.0.0.1:${(answerYes.address() as { port: number }).port}`;
  const restoreEnv = withEnv({ KL_TOOLS_ADDRESS: at, KL_TOOLS_WORKSPACE: "api", KL_BENCH_URL: `http://${at}` });
  const ctx = { ui: { setWidget: (key: string, lines: string[]) => void cards.push(`${key} ${lines.join("")}`) } };
  try {
    const { pi, tools } = fakePi();
    workspaceTools(pi);
    const process = tools.find((tool) => tool.name === "process")!;

    const noCommand = await process.execute("c1", { action: "start" }, undefined, undefined, ctx);
    assert.equal(noCommand.isError, true);
    assert.match(noCommand.content[0].text, /start needs command/);
    const noId = await process.execute("c2", { action: "stop" }, undefined, undefined, ctx);
    assert.match(noId.content[0].text, /stop needs id/);
    const noData = await process.execute("c3", { action: "write", id: "p1" }, undefined, undefined, ctx);
    assert.match(noData.content[0].text, /write needs data/);
    assert.equal(seen.length, 0, "an incomplete action never reaches the tool server");
    assert.equal(cards.length, 0, "an incomplete action never draws a card");

    // The same tool with the fields its action needs still asks and then runs.
    const started = await process.execute("c4", { action: "start", command: "npm run dev" }, undefined, undefined, ctx);
    assert.ok(!started.isError, started.content[0].text);
    assert.deepEqual(seen.map((line) => line.split(" ")[0]), ["/tools/exec", "/tools/process_list"]);
    assert.ok(cards.some((card) => card.startsWith("harness:proposal")));
  } finally {
    restoreEnv();
    await new Promise<void>((r) => answerYes.close(() => r()));
  }
});

test("the restore contract and its catalog entry say in place, by id and snapshot", () => {
  const entry = definition("environment.restore");
  assert.deepEqual(Object.keys(entry.descriptor.inputSchema.properties ?? {}).sort(), ["id", "snapshot"]);
  const ask = question("kl_environment_restore", { id: "devstack", snapshot: "snap-1" });
  assert.equal(ask, "Restore snapshot snap-1 into environment devstack, in place");
  assert.doesNotMatch(ask, /new environment/i);
  assert.equal(approvalPrompt(entry, { id: "devstack", snapshot: "snap-1" }), ask);
  assert.ok(entry.descriptor.rules.some((rule) => /in place/.test(rule) && /does not create/.test(rule)));
  assert.ok(entry.descriptor.rules.some((rule) => /reconciled/.test(rule)));
  // The drift this replaced: `name` and `snapshot_id` are not this capability's arguments.
  const drifted = validateArgs("environment.restore", { name: "devstack", snapshot_id: "snap-1" });
  assert.equal(drifted.ok, false);
  assert.ok(codes(drifted).includes("unknown_field"));
  const missing = validateArgs("environment.restore", { id: "devstack" });
  assert.equal(missing.ok, false);
  assert.ok(codes(missing).includes("missing_field"));
});

/** A /v1 that records every call and answers a proposal with yes, like `bench-tools.test.ts`'s. */
async function fakeV1(route: (method: string, url: string) => unknown = () => ({})) {
  const seen: { method: string; url: string; body: any }[] = [];
  const srv = http.createServer((req, res) => {
    let raw = "";
    req.on("data", (d) => (raw += d));
    req.on("end", () => {
      seen.push({ method: req.method!, url: req.url!, body: raw ? JSON.parse(raw) : undefined });
      res.writeHead(200, { "content-type": "application/json" });
      if (req.url!.startsWith("/proposals")) return void res.end(JSON.stringify({ answer: "yes" }));
      res.end(JSON.stringify(route(req.method!, req.url!) ?? {}));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const base = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;
  return { seen, base, close: () => new Promise<void>((r) => srv.close(() => r())) };
}

test("the registered restore handler posts in place, with the id from the call and its snapshot", async () => {
  const api = await fakeV1((method, url) => method === "GET" && url === "/v1/environments"
    ? [{ id: "devstack", name: "devstack" }]
    : { id: "devstack", name: "devstack", state: "running" });
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cap-restore-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restoreEnv = withEnv({
    KL_TOOL_TOKEN_FILE: path.join(dir, "token"),
    KL_API_URL: api.base,
    KL_BENCH_URL: api.base,
    KL_WORKSPACE_ID: "bench-ada",
    KL_TEAM: "acme",
    KL_TOOLS_WORKSPACE: undefined,
    KL_FORK: undefined,
  });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const restore = tools.find((tool) => tool.name === "kl_environment_restore")!;
    const answer = await restore.execute("c1", { id: "devstack", snapshot: "snap-1" }, undefined, undefined, undefined);
    assert.ok(!answer.isError, answer.content[0].text);
    const posted = api.seen.find((call) => call.method === "POST");
    assert.equal(posted?.url, "/v1/environments/devstack/restore-in-place");
    assert.deepEqual(posted?.body, { snapshot_id: "snap-1" });
    assert.equal(api.seen.filter((call) => call.method === "POST").length, 1, "one restore, in place");
  } finally {
    restoreEnv();
    fs.rmSync(dir, { recursive: true, force: true });
    await api.close();
  }
});

test("a read dispatched through the registry answers exactly what the registered tool answers", async () => {
  const api = await fakeV1((method, url) => {
    if (method === "GET" && url === "/v1/workspaces") return [{ id: "ws-1", name: "api", state: "running" }];
    return {};
  });
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cap-parity-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restoreEnv = withEnv({
    KL_TOOL_TOKEN_FILE: path.join(dir, "token"),
    KL_API_URL: api.base,
    KL_BENCH_URL: api.base,
    KL_WORKSPACE_ID: "bench-ada",
    KL_TEAM: "acme",
    KL_TOOLS_WORKSPACE: undefined,
    KL_FORK: undefined,
  });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const legacy = tools.find((tool) => tool.name === "kl_workspaces")!;
    const direct = await legacy.execute("c1", {}, undefined, undefined, undefined);
    const runtime = capabilityRuntime(createPlatformAdapters(async (method, route, body) => {
      const response = await fetch(`${api.base}${route}`, { method, headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
      return { status: response.status, data: await response.json() };
    }, "bench-ada"));
    const viaRegistry = await capabilityRegistry.dispatch("workspace.list", {}, { runtime });
    assert.equal(viaRegistry.outcome, "completed");
    assert.equal(viaRegistry.outcome === "completed" ? JSON.stringify(viaRegistry.result, null, 2) : "", direct.content[0].text);
    assert.equal(direct.isError, undefined);
  } finally {
    restoreEnv();
    fs.rmSync(dir, { recursive: true, force: true });
    await api.close();
  }
});

test("a workspace is created one way, and the registered tool refuses two sources before the api", async () => {
  const both = validateArgs("workspace.create", { name: "backend", repo: "kloudlite/rustic-git", branch: "main", from_snapshot: "snap-1" });
  assert.equal(both.ok, false);
  assert.ok(codes(both).includes("mixed_action"));
  const branchOnly = validateArgs("workspace.create", { name: "backend", branch: "main" });
  assert.equal(branchOnly.ok, false);
  assert.ok(codes(branchOnly).includes("missing_field"));
  const repoOnly = validateArgs("workspace.create", { name: "backend", repo: "kloudlite/rustic-git" });
  assert.equal(repoOnly.ok, false);
  assert.ok(codes(repoOnly).includes("missing_field"));
  expectValid(validateArgs("workspace.create", { name: "backend", from_snapshot: "snap-1" }));
  expectValid(validateArgs("workspace.create", { name: "backend", repo: "kloudlite/rustic-git", branch: "main" }));

  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cap-create-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restoreEnv = withEnv({
    KL_WORKSPACE_ID: "bench-ada",
    KL_TEAM: "acme",
    KL_TOOLS_WORKSPACE: undefined,
    KL_FORK: undefined,
    KL_API_URL: "http://127.0.0.1:1",
    KL_TOOL_TOKEN_FILE: path.join(dir, "token"),
  });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const create = tools.find((tool) => tool.name === "kl_workspace_create")!;
    const cards: string[] = [];
    const ctx = { ui: { setWidget: (key: string, lines: string[]) => void cards.push(`${key} ${lines.join("")}`) } };
    const refused = await create.execute("c1", { name: "backend", repo: "kloudlite/rustic-git", from_snapshot: "snap-1" }, undefined, undefined, ctx);
    assert.equal(refused.isError, true);
    assert.match(refused.content[0].text, /workspace creation takes repo \+ branch or from_snapshot, not more than one/);
    const branchOnlyRefused = await create.execute("c2", { name: "backend", branch: "main" }, undefined, undefined, ctx);
    assert.equal(branchOnlyRefused.isError, true);
    assert.match(branchOnlyRefused.content[0].text, /branch needs repo/);
    assert.deepEqual(cards, [], "an impossible combination never asks a person");
  } finally {
    restoreEnv();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("intercept says set, clear or the one-to-one default explicitly", () => {
  const entry = definition("environment.intercept");
  const set = expectValid(validateArgs("environment.intercept", { id: "devstack", service: "postgres", workspace: "api" }));
  assert.equal(set.states.workspace.kind, "known");
  assert.equal(set.states.ports.kind, "unspecified");

  const omitted = expectValid(validateArgs("environment.intercept", { id: "devstack", service: "postgres" }));
  assert.equal(omitted.states.workspace.kind, "unspecified");
  assert.equal("workspace" in omitted.args, false);

  const cleared = expectValid(validateArgs("environment.intercept", { id: "devstack", service: "postgres", workspace: null }));
  assert.equal(cleared.states.workspace.kind, "explicitly_clear");
  assert.equal("workspace" in cleared.args, false, "the clear form never reaches the handler as a value");

  const portsWithoutWorkspace = validateArgs("environment.intercept", { id: "devstack", service: "postgres", ports: [{ service: 5432, workspace: 5432 }] });
  assert.equal(portsWithoutWorkspace.ok, false);
  assert.ok(codes(portsWithoutWorkspace).includes("missing_field"));
  const emptyPorts = validateArgs("environment.intercept", { id: "devstack", service: "postgres", workspace: "api", ports: [] });
  assert.equal(emptyPorts.ok, false, "an empty list is not the one-to-one default");
  assert.ok(entry.descriptor.rules.some((rule) => /one-to-one/.test(rule) || /one to one/.test(rule)));
  const schema = entry.descriptor.inputSchema.properties?.workspace;
  assert.ok(schema?.oneOf?.some((variant) => variant.type === "null"), "the generated schema must publish null as the clear form");
});

test("approval cannot mutate the payload that the handler executes", async () => {
  const registry = new CapabilityRegistry(CAPABILITY_CONTRACTS, ["environment.restore"]);
  let executed: Record<string, unknown> | undefined;
  const runtime = capabilityRuntime({ "environment.restore": async ({ args }) => {
    executed = args;
    return { ok: true, value: { id: args.id } };
  } });
  const args = { id: "devstack", snapshot: "snap-1" };
  const prepared = registry.prepare("environment.restore", args, { runtime, decision: decisionExpectation() });
  assert.equal(prepared.ok, true);
  if (!prepared.ok || !prepared.approval) assert.fail("expected an approval request");
  (prepared.approval.args as Record<string, unknown>).snapshot = "other-snapshot";
  const result = await registry.dispatch("environment.restore", args, { runtime, approval: { record: grantedDecision(prepared.approval.expectation), expectation: prepared.approval.expectation } });
  assert.equal(result.outcome, "completed");
  assert.deepEqual(executed, { id: "devstack", snapshot: "snap-1" });
});

test("workspace inspect resolves a unique workspace name before fetching it", async () => {
  const api = await fakeV1((method, url) => {
    if (method === "GET" && url === "/v1/workspaces") return [{ id: "ws-1", name: "api" }];
    if (method === "GET" && url === "/v1/workspaces/ws-1") return { id: "ws-1", name: "api" };
    return {};
  });
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cap-inspect-"));
  fs.writeFileSync(path.join(dir, "token"), "t");
  const restoreEnv = withEnv({ KL_TOOL_TOKEN_FILE: path.join(dir, "token"), KL_API_URL: api.base, KL_BENCH_URL: api.base, KL_WORKSPACE_ID: "bench-ada" });
  try {
    const { pi, tools } = fakePi();
    kloudlite(pi);
    const inspect = tools.find((tool) => tool.name === "kl_workspace")!;
    const answer = await inspect.execute("c1", { id: "api" }, undefined, undefined, undefined);
    assert.ok(!answer.isError, answer.content[0].text);
    assert.ok(api.seen.some((call) => call.url === "/v1/workspaces"));
    assert.ok(api.seen.some((call) => call.url === "/v1/workspaces/ws-1"));
  } finally {
    restoreEnv();
    fs.rmSync(dir, { recursive: true, force: true });
    await api.close();
  }
});

test("defaults and clear forms are the only values nobody stated", () => {
  const empty = expectValid(validateArgs("bench.process.list", {}));
  assert.equal(empty.args.includeEnded, false);
  assert.equal(empty.states.includeEnded.kind, "known");
  assert.equal(empty.states.workspace.kind, "unspecified");
  assert.equal(validateArgs("bench.process.list", { includeEnded: "yes" }).ok, false);
  expectValid(validateArgs("bench.process.list", { includeEnded: true }));
});
