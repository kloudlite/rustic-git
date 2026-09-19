import assert from "node:assert/strict";
import { test } from "node:test";
import {
  OperationScheduler,
  SchedulerValidationError,
  validateSchedulePlan,
  type ScheduledOperation,
} from "../src/operations/scheduler.ts";
import type { CapabilityDescriptor, ExactCall, JsonValue } from "../src/operations/contracts.ts";

const descriptor = (
  capability: string,
  effect: CapabilityDescriptor["effect"] = "read",
  access: Partial<CapabilityDescriptor["resourceAccess"]> = {},
): CapabilityDescriptor => ({
  capability,
  version: "1.0.0",
  title: capability,
  summary: capability,
  guide: capability,
  effect,
  scope: "bench",
  inputSchema: { type: "object", additionalProperties: true },
  outputSchema: { type: "object", additionalProperties: true },
  arguments: [],
  rules: [],
  limits: {},
  examples: [],
  errors: [],
  retry: { class: effect === "read" ? "idempotent" : "reconcile_required", maxAttempts: effect === "read" ? 2 : 1, reconciliation: "none" },
  approval: { required: "none", payloadDigestRequired: false, binds: [] },
  evidence: { success: ["result"], unknownOutcome: [] },
  resourceAccess: {
    reads: access.reads ?? [],
    writes: access.writes ?? [],
    conflictKeys: access.conflictKeys ?? [],
    exclusive: access.exclusive ?? false,
  },
  contractVersion: "v1",
});

const capabilities = new Map([
  ["read.one", { ...descriptor("read.one"), outputSchema: { type: "object", properties: { id: { type: "string" }, count: { type: "integer" } }, additionalProperties: true } }],
  ["read.two", descriptor("read.two")],
  ["read.bound", { ...descriptor("read.bound"), inputSchema: { type: "object", required: ["id"], properties: { id: { type: "string" } }, additionalProperties: false } }],
  ["packages.add", descriptor("packages.add", "write", { writes: ["workspace.packages"], conflictKeys: ["workspace.packages"] })],
  ["packages.rm", descriptor("packages.rm", "write", { writes: ["workspace.packages"], conflictKeys: ["workspace.packages"] })],
  ["services.put", descriptor("services.put", "write", { writes: ["environment.services"], conflictKeys: ["environment.services"] })],
  ["command.run", descriptor("command.run", "write", { writes: ["workspace.process"], conflictKeys: ["workspace.tree"], exclusive: true })],
]);

const call = (key: string, capability = "read.one", extra: Partial<ExactCall> = {}): ExactCall => ({
  key,
  capability,
  capabilityVersion: "1.0.0",
  ...extra,
});

test("plan validation rejects cycles and missing dependencies", () => {
  for (const calls of [
    [call("a", "read.one", { dependsOn: ["b"] }), call("b", "read.two", { dependsOn: ["a"] })],
    [call("a", "read.one", { dependsOn: ["missing"] })],
  ]) {
    const result = validateSchedulePlan(calls, (name) => capabilities.get(name));
    assert.equal(result.ok, false);
    if (result.ok) assert.fail("invalid graph was accepted");
    assert.ok(result.issues.some((issue) => issue.code === "cycle" || issue.code === "unknown_dependency"));
  }
});

test("plan validation rejects undeclared and mistyped output bindings", () => {
  const missing = validateSchedulePlan([
    call("source"),
    call("bound", "read.bound", { dependsOn: ["source"], argsFrom: { id: { from: "source", output: "missing" } } }),
  ], (name) => capabilities.get(name));
  assert.equal(missing.ok, false);

  const typed = validateSchedulePlan([
    call("source"),
    call("bound", "read.bound", { dependsOn: ["source"], argsFrom: { id: { from: "source", output: "count" } } }),
  ], (name) => capabilities.get(name));
  assert.equal(typed.ok, false);
  if (!typed.ok) assert.ok(typed.issues.some((issue) => issue.code === "binding_type"));
});

const deferred = <T>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
};
const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

function operation(id: string, calls: ExactCall[], run: ScheduledOperation["run"], maxReads = 4, maxMutations = 2): ScheduledOperation {
  return { operationId: id, calls, descriptor: (name) => capabilities.get(name), maxConcurrentReads: maxReads, maxConcurrentMutations: maxMutations, run };
}

test("independent reads overlap while dependencies wait for successful outputs", async () => {
  const gates = [deferred<JsonValue>(), deferred<JsonValue>()];
  const started: string[] = [];
  const scheduler = new OperationScheduler({ maxConcurrent: 4 });
  const running = scheduler.submit(operation("op-a", [call("a"), call("b", "read.two"), call("c", "read.bound", { dependsOn: ["a"], argsFrom: { id: { from: "a", output: "id" } } })], async ({ call: step, args }) => {
    started.push(step.key);
    if (step.key === "a") return { outcome: "succeeded", value: await gates[0].promise };
    if (step.key === "b") return { outcome: "succeeded", value: await gates[1].promise };
    assert.deepEqual(args, { id: "ws-1" });
    return { outcome: "succeeded", value: {} };
  }));
  await tick();
  assert.deepEqual(started, ["a", "b"]);
  gates[0].resolve({ id: "ws-1" });
  await tick();
  assert.deepEqual(started, ["a", "b", "c"]);
  gates[1].resolve({});
  assert.equal((await running).state, "completed");
});

test("global and per-effect bounds are fair across operation FIFO order", async () => {
  const gate = deferred<JsonValue>();
  const started: string[] = [];
  const scheduler = new OperationScheduler({ maxConcurrent: 2 });
  const run = (operationId: string) => operation(operationId, [call("r1"), call("r2", "read.two")], async ({ call: step }) => {
    started.push(`${operationId}:${step.key}`);
    return { outcome: "succeeded", value: await gate.promise };
  }, 1);
  const first = scheduler.submit(run("first"));
  const second = scheduler.submit(run("second"));
  await tick();
  assert.deepEqual(started, ["first:r1", "second:r1"]);
  gate.resolve({});
  await Promise.all([first, second]);
  assert.deepEqual(started, ["first:r1", "second:r1", "first:r2", "second:r2"]);
});

test("conflict lanes serialize collection writes and unknown footprints but permit disjoint work", async () => {
  const packages = deferred<JsonValue>();
  const command = deferred<JsonValue>();
  const started: string[] = [];
  const scheduler = new OperationScheduler({ maxConcurrent: 5 });
  const result = scheduler.submit(operation("op-conflicts", [
    call("pkg_add", "packages.add"),
    call("pkg_rm", "packages.rm"),
    call("service", "services.put"),
    call("command", "command.run", { targetRef: "workspace-1" }),
    call("read", "read.one", { targetRef: "workspace-2" }),
  ], async ({ call: step }) => {
    started.push(step.key);
    if (step.key === "pkg_add") return { outcome: "succeeded", value: await packages.promise };
    if (step.key === "command") return { outcome: "succeeded", value: await command.promise };
    return { outcome: "succeeded", value: {} };
  }));
  await tick();
  assert.equal(started[0], "pkg_add");
  assert.deepEqual(new Set(started), new Set(["pkg_add", "service", "command", "read"]));
  packages.resolve({});
  await tick();
  assert.ok(started.includes("pkg_rm"));
  command.resolve({});
  assert.equal((await result).state, "completed");
});

test("deadline and abort cancel queued work and propagate one signal to running steps", async () => {
  const controller = new AbortController();
  let runningSignal: AbortSignal | undefined;
  const scheduler = new OperationScheduler({ maxConcurrent: 1 });
  const result = scheduler.submit({
    ...operation("op-cancel", [call("first"), call("second", "read.two")], async ({ signal }) => {
      runningSignal = signal;
      await new Promise<void>((resolve) => signal.addEventListener("abort", () => resolve(), { once: true }));
      return { outcome: "cancelled", evidenceRefs: ["abort-confirmed"] };
    }),
    signal: controller.signal,
    deadlineAt: Date.now() + 10_000,
  });
  await tick();
  controller.abort();
  const settled = await result;
  assert.equal(runningSignal?.aborted, true);
  assert.equal(settled.state, "cancelled");
  assert.deepEqual(settled.steps.map((step) => step.outcome), ["cancelled", "cancelled"]);

  await assert.rejects(
    scheduler.submit({ ...operation("expired", [call("a")], async () => ({ outcome: "succeeded", value: {} })), deadlineAt: Date.now() - 1 }),
    (error: unknown) => error instanceof SchedulerValidationError && error.code === "deadline_exceeded",
  );
});
