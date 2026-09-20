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
  if (!missing.ok) {
    assert.deepEqual(missing.issues[0], {
      path: "$.calls[1].argsFrom.id",
      code: "unknown_dependency",
      message: "source has no declared output missing",
    });
  }

  const typed = validateSchedulePlan([
    call("source"),
    call("bound", "read.bound", { dependsOn: ["source"], argsFrom: { id: { from: "source", output: "count" } } }),
  ], (name) => capabilities.get(name));
  assert.equal(typed.ok, false);
  if (!typed.ok) {
    assert.ok(typed.issues.some((issue) => issue.code === "validation_failure"));
    assert.ok(typed.issues.every((issue) => issue.path.startsWith("$.calls[")));
  }
});

test("typed open-object outputs support compatible named bindings only", () => {
  const source = {
    ...descriptor("read.open"),
    outputSchema: { type: "object" as const, additionalProperties: { type: "string" as const } },
  };
  const untyped = {
    ...descriptor("read.untyped"),
    outputSchema: { type: "object" as const, additionalProperties: true },
  };
  const stringTarget = {
    ...descriptor("read.string-target"),
    inputSchema: { type: "object" as const, properties: { id: { type: "string" as const } }, required: ["id"], additionalProperties: false },
  };
  const integerTarget = {
    ...descriptor("read.integer-target"),
    inputSchema: { type: "object" as const, properties: { id: { type: "integer" as const } }, required: ["id"], additionalProperties: false },
  };
  const typed = new Map([source, untyped, stringTarget, integerTarget].map((entry) => [entry.capability, entry]));
  const bind = (sourceCapability: string, targetCapability: string) => validateSchedulePlan([
    call("source", sourceCapability),
    call("target", targetCapability, { dependsOn: ["source"], argsFrom: { id: { from: "source", output: "dynamic" } } }),
  ], (name) => typed.get(name));

  assert.equal(bind(source.capability, stringTarget.capability).ok, true);
  assert.equal(bind(source.capability, integerTarget.capability).ok, false);
  assert.equal(bind(untyped.capability, stringTarget.capability).ok, false);
});

test("validates selected nested binding schemas, numeric compatibility, required fields, and collisions", () => {
  const source = {
    ...descriptor("read.structured"),
    outputSchema: {
      type: "object" as const,
      properties: {
        payload: { type: "object" as const, properties: { items: { type: "array" as const, items: { type: "object" as const, properties: { id: { type: "string" as const } }, required: ["id"] } } }, required: ["items"] },
        count: { type: "integer" as const },
        maybe: { oneOf: [{ type: "string" as const }, { type: "null" as const }] },
      },
      required: ["payload", "count", "maybe"],
    },
  };
  const target = {
    ...descriptor("read.typed"),
    inputSchema: { type: "object" as const, properties: { id: { type: "string" as const }, amount: { type: "number" as const } }, required: ["id", "amount"], additionalProperties: false },
  };
  const typed = new Map([[source.capability, source], [target.capability, target]]);
  const valid = validateSchedulePlan([
    call("source", source.capability),
    call("target", target.capability, { dependsOn: ["source"], argsFrom: {
      id: { from: "source", output: "payload", select: ["items", 0, "id"] },
      amount: { from: "source", output: "count" },
    } }),
  ], (name) => typed.get(name));
  assert.equal(valid.ok, true, valid.ok ? "" : JSON.stringify(valid.issues));

  for (const invalid of [
    call("target", target.capability, { dependsOn: ["source"], argsFrom: { id: { from: "source", output: "maybe" }, amount: { from: "source", output: "count" } } }),
    call("target", target.capability, { dependsOn: ["source"], args: { id: "literal" }, argsFrom: { id: { from: "source", output: "payload", select: ["items", 0, "id"] }, amount: { from: "source", output: "count" } } }),
    call("target", target.capability, { dependsOn: ["source"], argsFrom: { id: { from: "source", output: "payload", select: ["items", 0, "id"] } } }),
  ]) {
    const checked = validateSchedulePlan([call("source", source.capability), invalid], (name) => typed.get(name));
    assert.equal(checked.ok, false);
  }
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
    call("same_scope", "read.one", { targetRef: "workspace-1" }),
    call("read", "read.one", { targetRef: "workspace-2" }),
  ], async ({ call: step }) => {
    started.push(step.key);
    if (step.key === "pkg_add") return { outcome: "succeeded", value: await packages.promise };
    if (step.key === "command") return { outcome: "succeeded", value: await command.promise };
    return { outcome: "succeeded", value: {} };
  }, 4, 3));
  await tick();
  assert.equal(started[0], "pkg_add");
  assert.deepEqual(new Set(started), new Set(["pkg_add", "service", "command", "read"]));
  assert.equal(started.includes("same_scope"), false);
  packages.resolve({});
  await tick();
  assert.ok(started.includes("pkg_rm"));
  command.resolve({});
  await tick();
  assert.equal(started.includes("same_scope"), true);
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

test("rejects invalid per-operation concurrency limits", async () => {
  const scheduler = new OperationScheduler({ maxConcurrent: 2 });
  for (const [maxConcurrentReads, maxConcurrentMutations] of [[0, 1], [1, -1], [1.5, 1], [Number.POSITIVE_INFINITY, 1]]) {
    await assert.rejects(
      scheduler.submit(operation("invalid-limits", [call("a")], async () => ({ outcome: "succeeded", value: {} }), maxConcurrentReads, maxConcurrentMutations)),
      (error: unknown) => error instanceof SchedulerValidationError && error.code === "validation_failure",
    );
  }
});

test("pre-aborted submissions settle without queue residue and completion removes abort listeners", async () => {
  const scheduler = new OperationScheduler({ maxConcurrent: 1 });
  const aborted = new AbortController();
  aborted.abort();
  const cancelled = await scheduler.submit({ ...operation("pre-aborted", [call("a")], async () => assert.fail("pre-aborted work ran")), signal: aborted.signal });
  assert.equal(cancelled.state, "cancelled");

  let adds = 0;
  let removes = 0;
  const signal = { aborted: false, addEventListener: () => { adds += 1; }, removeEventListener: () => { removes += 1; } } as unknown as AbortSignal;
  const completed = await scheduler.submit({ ...operation("after", [call("a")], async () => ({ outcome: "succeeded", value: {} })), signal });
  assert.equal(completed.state, "completed");
  assert.equal(removes, adds);
});

test("a blocked early step cannot starve later ready steps in the same operation", async () => {
  const scheduler = new OperationScheduler({ maxConcurrent: 2 });
  const held = deferred<JsonValue>();
  const started: string[] = [];
  const blocker = scheduler.submit(operation("blocker", [call("hold", "packages.add", { targetRef: "workspace-shared" })], async () => ({ outcome: "succeeded", value: await held.promise }), 2, 1));
  await tick();
  const candidate = scheduler.submit(operation("candidate", [
    call("blocked", "packages.rm", { targetRef: "workspace-shared" }),
    call("later-a", "read.one", { targetRef: "workspace-a" }),
    call("later-b", "read.one", { targetRef: "workspace-b" }),
  ], async ({ call: step }) => (started.push(step.key), { outcome: "succeeded", value: {} }), 2, 1));
  await tick();
  assert.deepEqual(new Set(started), new Set(["later-a", "later-b"]));
  assert.equal(started.includes("blocked"), false);
  held.resolve({});
  assert.equal((await blocker).state, "completed");
  assert.equal((await candidate).state, "completed");
  assert.equal(started.at(-1), "blocked");
});

test("a failed dependency skips its whole chain whatever the plan order (C-3 T6)", async () => {
  // Calls are listed [C dependsOn B, B dependsOn A, A]: the chain's root is LAST in the
  // list. A single skip-propagation pass over this order only catches B (A's direct
  // dependent) on the tick it fails; C never gets a result and submit() never resolved
  // before the fixpoint loop in #ready.
  const scheduler = new OperationScheduler({ maxConcurrent: 3 });
  const plan = operation(
    "chain",
    [
      call("c", "read.one", { dependsOn: ["b"] }),
      call("b", "read.two", { dependsOn: ["a"] }),
      call("a", "read.one"),
    ],
    async ({ call: step }) => (step.key === "a" ? { outcome: "failed", error: new Error("a failed") } : assert.fail(`${step.key} must never run`)),
  );
  const result = await Promise.race([
    scheduler.submit(plan),
    new Promise<never>((_, reject) => setTimeout(() => reject(new Error("submit() did not resolve within 1s")), 1_000)),
  ]);
  assert.equal(result.state, "failed");
  assert.deepEqual(result.steps.map((step) => [step.key, step.outcome]), [
    ["c", "skipped"],
    ["b", "skipped"],
    ["a", "failed"],
  ]);
});
