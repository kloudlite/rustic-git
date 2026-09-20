import { resolveCallArgs, type CapabilityDescriptor, type ExactCall, type JsonSchemaLike, type JsonValue, type ValidationIssue } from "./contracts.ts";

export type ScheduledStepResult =
  | { outcome: "succeeded"; value: JsonValue; evidenceRefs?: string[] }
  | { outcome: "failed"; error: Error }
  | { outcome: "skipped" }
  | { outcome: "cancelled"; evidenceRefs?: string[] }
  | { outcome: "unknown" };

export type ScheduledStep = {
  operationId: string;
  call: ExactCall;
  descriptor: CapabilityDescriptor;
  args: Record<string, JsonValue>;
  signal: AbortSignal;
};

export type ScheduledOperation = {
  operationId: string;
  calls: readonly ExactCall[];
  descriptor(capability: string): CapabilityDescriptor | undefined;
  maxConcurrentReads: number;
  maxConcurrentMutations: number;
  run(step: ScheduledStep): Promise<ScheduledStepResult>;
  signal?: AbortSignal;
  deadlineAt?: number;
};

export type ScheduledStepSummary = { key: string; outcome: ScheduledStepResult["outcome"] };
export type ScheduledOperationResult = {
  operationId: string;
  state: "completed" | "partial" | "failed" | "cancelled" | "reconciling";
  steps: ScheduledStepSummary[];
  outputs: Record<string, JsonValue>;
};

export type ScheduleValidation = { ok: true } | { ok: false; issues: ValidationIssue[] };

export class SchedulerValidationError extends Error {
  readonly code: "validation_failure" | "deadline_exceeded";
  readonly issues?: ValidationIssue[];
  constructor(code: SchedulerValidationError["code"], message: string, issues?: ValidationIssue[]) {
    super(message);
    this.name = "SchedulerValidationError";
    this.code = code;
    this.issues = issues;
  }
}

const issue = (path: string, code: ValidationIssue["code"], message: string): ValidationIssue => ({ path, code, message });

function dereference(schema: JsonSchemaLike, root: JsonSchemaLike): JsonSchemaLike {
  const name = schema.$ref?.startsWith("#/definitions/") ? schema.$ref.slice("#/definitions/".length) : undefined;
  return name ? root.definitions?.[name] ?? schema : schema;
}

function selectedSchema(schema: JsonSchemaLike, select: readonly (string | number)[] | undefined, root: JsonSchemaLike): JsonSchemaLike | undefined {
  let current = dereference(schema, root);
  for (const segment of select ?? []) {
    const variants = current.oneOf ?? [current];
    const selected = variants.map((variant) => {
      const resolved = dereference(variant, root);
      return typeof segment === "number" ? resolved.items : resolved.properties?.[segment];
    });
    if (selected.some((value) => value === undefined)) return undefined;
    current = selected.length === 1 ? selected[0]! : { oneOf: selected as JsonSchemaLike[] };
  }
  return current;
}

function schemaAssignable(source: JsonSchemaLike, target: JsonSchemaLike, sourceRoot: JsonSchemaLike, targetRoot: JsonSchemaLike): boolean {
  const sources = dereference(source, sourceRoot).oneOf ?? [dereference(source, sourceRoot)];
  const targets = dereference(target, targetRoot).oneOf ?? [dereference(target, targetRoot)];
  return sources.every((candidate) => targets.some((expected) => {
    const from = dereference(candidate, sourceRoot);
    const to = dereference(expected, targetRoot);
    if (from.type === "integer" && to.type === "number") return true;
    if (from.type !== undefined && to.type !== undefined && from.type !== to.type) return false;
    if (from.type === "array" && to.type === "array" && from.items && to.items) return schemaAssignable(from.items, to.items, sourceRoot, targetRoot);
    if (from.type === "object" && to.type === "object") {
      return (to.required ?? []).every((name) => from.properties?.[name] && to.properties?.[name] && schemaAssignable(from.properties[name], to.properties[name], sourceRoot, targetRoot));
    }
    return true;
  }));
}

export function validateSchedulePlan(calls: readonly ExactCall[], descriptor: ScheduledOperation["descriptor"]): ScheduleValidation {
  const issues: ValidationIssue[] = [];
  const byKey = new Map<string, ExactCall>();
  for (const [index, call] of calls.entries()) {
    if (byKey.has(call.key)) issues.push(issue(`$.calls[${index}].key`, "duplicate_key", `duplicate step key ${call.key}`));
    byKey.set(call.key, call);
    const target = descriptor(call.capability);
    if (!target || target.version !== call.capabilityVersion) issues.push(issue(`$.calls[${index}].capability`, "validation_failure", `unknown or stale capability ${call.capability}`));
  }
  for (const [index, call] of calls.entries()) {
    const target = descriptor(call.capability);
    const supplied = new Set([...Object.keys(call.args ?? {}), ...Object.keys(call.argsFrom ?? {})]);
    for (const required of target?.inputSchema.required ?? []) {
      if (!supplied.has(required)) issues.push(issue(`$.calls[${index}]`, "missing_field", `required argument ${required} is not supplied`));
    }
    for (const dependency of call.dependsOn ?? []) {
      if (!byKey.has(dependency)) issues.push(issue(`$.calls[${index}].dependsOn`, "unknown_dependency", `unknown dependency ${dependency}`));
    }
    for (const [name, binding] of Object.entries(call.argsFrom ?? {})) {
      const source = byKey.get(binding.from);
      const sourceDescriptor = source && descriptor(source.capability);
      if (!source || !(call.dependsOn ?? []).includes(binding.from)) {
        issues.push(issue(`$.calls[${index}].argsFrom.${name}`, "unknown_dependency", `${binding.from} must be a declared dependency`));
      } else if (Object.prototype.hasOwnProperty.call(call.args ?? {}, name)) {
        issues.push(issue(`$.calls[${index}].argsFrom.${name}`, "binding_collision", `${name} is supplied as both a literal and binding`));
      } else if (sourceDescriptor) {
        const declaredOutput = sourceDescriptor.outputSchema.properties?.[binding.output]
          ?? (typeof sourceDescriptor.outputSchema.additionalProperties === "object" ? sourceDescriptor.outputSchema.additionalProperties : undefined);
        if (!declaredOutput) {
          issues.push(issue(`$.calls[${index}].argsFrom.${name}`, "unknown_dependency", `${binding.from} has no declared output ${binding.output}`));
          continue;
        }
        if (!target) continue;
        const output = selectedSchema(declaredOutput, binding.select, sourceDescriptor.outputSchema);
        const expected = target.inputSchema.properties?.[name] ?? (typeof target.inputSchema.additionalProperties === "object" ? target.inputSchema.additionalProperties : undefined);
        if (!output) issues.push(issue(`$.calls[${index}].argsFrom.${name}`, "unknown_dependency", `selection does not exist in ${binding.from}.${binding.output}`));
        else if (!expected || !schemaAssignable(output, expected, sourceDescriptor.outputSchema, target.inputSchema)) {
          issues.push(issue(`$.calls[${index}].argsFrom.${name}`, "validation_failure", `${binding.from}.${binding.output} is incompatible with ${call.capability}.${name}`));
        }
      }
    }
  }
  const visiting = new Set<string>();
  const visited = new Set<string>();
  const visit = (key: string): void => {
    if (visiting.has(key)) {
      issues.push(issue("$.calls", "cycle", `dependency cycle includes ${key}`));
      return;
    }
    if (visited.has(key)) return;
    visiting.add(key);
    for (const dependency of byKey.get(key)?.dependsOn ?? []) if (byKey.has(dependency)) visit(dependency);
    visiting.delete(key);
    visited.add(key);
  };
  for (const key of byKey.keys()) visit(key);
  return issues.length ? { ok: false, issues } : { ok: true };
}

type Running = { operation: QueuedOperation; call: ExactCall; descriptor: CapabilityDescriptor; controller: AbortController; lanes: string[]; scope: string; exclusive: boolean };
type QueuedOperation = {
  spec: ScheduledOperation;
  outputs: Record<string, JsonValue>;
  results: Map<string, ScheduledStepResult>;
  runningReads: number;
  runningMutations: number;
  resolve(value: ScheduledOperationResult): void;
  abort(): void;
  abortListener?: () => void;
  settled: boolean;
  deadline?: ReturnType<typeof setTimeout>;
};

export class OperationScheduler {
  readonly maxConcurrent: number;
  #operations: QueuedOperation[] = [];
  #running = new Set<Running>();
  #lanes = new Set<string>();
  #cursor = 0;
  #pumping = false;

  constructor(options: { maxConcurrent: number }) {
    if (!Number.isInteger(options.maxConcurrent) || options.maxConcurrent < 1) throw new SchedulerValidationError("validation_failure", "maxConcurrent must be positive");
    this.maxConcurrent = options.maxConcurrent;
  }

  submit(spec: ScheduledOperation): Promise<ScheduledOperationResult> {
    if (![spec.maxConcurrentReads, spec.maxConcurrentMutations].every((limit) => Number.isInteger(limit) && limit > 0)) {
      return Promise.reject(new SchedulerValidationError("validation_failure", "per-operation concurrency limits must be positive integers"));
    }
    const checked = validateSchedulePlan(spec.calls, spec.descriptor);
    if (!checked.ok) return Promise.reject(new SchedulerValidationError("validation_failure", checked.issues.map((entry) => entry.message).join("; "), checked.issues));
    if (spec.deadlineAt !== undefined && spec.deadlineAt <= Date.now()) return Promise.reject(new SchedulerValidationError("deadline_exceeded", "operation deadline has passed"));
    return new Promise((resolve) => {
      const operation: QueuedOperation = {
        spec,
        outputs: {},
        results: new Map(),
        runningReads: 0,
        runningMutations: 0,
        resolve,
        settled: false,
        abort: () => {
          for (const active of this.#running) if (active.operation === operation) active.controller.abort();
          for (const call of spec.calls) if (!operation.results.has(call.key) && ![...this.#running].some((active) => active.operation === operation && active.call.key === call.key)) operation.results.set(call.key, { outcome: "cancelled" });
          this.#finish(operation);
        },
      };
      this.#operations.push(operation);
      if (spec.signal?.aborted) operation.abort();
      else if (spec.signal) {
        operation.abortListener = operation.abort;
        spec.signal.addEventListener("abort", operation.abortListener, { once: true });
      }
      if (spec.deadlineAt !== undefined) operation.deadline = setTimeout(operation.abort, Math.max(0, spec.deadlineAt - Date.now()));
      this.#pump();
    });
  }

  #pump(): void {
    if (this.#pumping) return;
    this.#pumping = true;
    try {
      while (this.#running.size < this.maxConcurrent && this.#operations.length) {
        let selected: { operation: QueuedOperation; call: ExactCall; descriptor: CapabilityDescriptor; lanes: string[] } | undefined;
        for (let offset = 0; offset < this.#operations.length; offset++) {
          const index = (this.#cursor + offset) % this.#operations.length;
          const operation = this.#operations[index];
          selected = this.#ready(operation);
          if (selected) {
            this.#cursor = (index + 1) % this.#operations.length;
            break;
          }
        }
        if (!selected) break;
        this.#start(selected.operation, selected.call, selected.descriptor, selected.lanes);
      }
    } finally {
      this.#pumping = false;
    }
  }

  #ready(operation: QueuedOperation) {
    const activeKeys = new Set([...this.#running].filter((entry) => entry.operation === operation).map((entry) => entry.call.key));
    for (const call of operation.spec.calls) {
      if (operation.results.has(call.key) || activeKeys.has(call.key)) continue;
      const dependencies = call.dependsOn ?? [];
      if (dependencies.some((key) => operation.results.get(key)?.outcome !== "succeeded")) {
        if (dependencies.some((key) => {
          const result = operation.results.get(key);
          return result !== undefined && result.outcome !== "succeeded";
        })) operation.results.set(call.key, { outcome: "skipped" });
        continue;
      }
      const descriptor = operation.spec.descriptor(call.capability)!;
      if (descriptor.effect === "read" ? operation.runningReads >= operation.spec.maxConcurrentReads : operation.runningMutations >= operation.spec.maxConcurrentMutations) continue;
      const lanes = this.#laneKeys(operation.spec.operationId, call, descriptor);
      const scope = call.targetRef ?? operation.spec.operationId;
      if (lanes.some((lane) => this.#lanes.has(lane))) continue;
      if ([...this.#running].some((active) => active.scope === scope && (active.exclusive || descriptor.resourceAccess.exclusive))) continue;
      return { operation, call, descriptor, lanes };
    }
    this.#finish(operation);
    return undefined;
  }

  #laneKeys(operationId: string, call: ExactCall, descriptor: CapabilityDescriptor): string[] {
    const scope = call.targetRef ?? operationId;
    const keys = descriptor.resourceAccess.conflictKeys.map((key) => `${scope}:${key}`);
    if (descriptor.resourceAccess.exclusive) keys.push(`${scope}:exclusive`);
    return keys;
  }

  #start(operation: QueuedOperation, call: ExactCall, descriptor: CapabilityDescriptor, lanes: string[]): void {
    const resolved = resolveCallArgs(call, operation.outputs);
    if (!resolved.ok) {
      operation.results.set(call.key, { outcome: "failed", error: new SchedulerValidationError("validation_failure", resolved.issues.map((entry) => entry.message).join("; "), resolved.issues) });
      this.#finish(operation);
      queueMicrotask(() => this.#pump());
      return;
    }
    const controller = new AbortController();
    const forwardAbort = () => controller.abort();
    operation.spec.signal?.addEventListener("abort", forwardAbort, { once: true });
    const running: Running = { operation, call, descriptor, controller, lanes, scope: call.targetRef ?? operation.spec.operationId, exclusive: descriptor.resourceAccess.exclusive };
    this.#running.add(running);
    for (const lane of lanes) this.#lanes.add(lane);
    if (descriptor.effect === "read") operation.runningReads += 1;
    else operation.runningMutations += 1;
    void operation.spec.run({ operationId: operation.spec.operationId, call, descriptor, args: resolved.value, signal: controller.signal })
      .then((result) => {
        operation.results.set(call.key, result);
        if (result.outcome === "succeeded") operation.outputs[call.key] = result.value;
      }, (error) => operation.results.set(call.key, controller.signal.aborted ? { outcome: "cancelled" } : { outcome: "failed", error: error instanceof Error ? error : new Error(String(error)) }))
      .finally(() => {
        operation.spec.signal?.removeEventListener("abort", forwardAbort);
        this.#running.delete(running);
        for (const lane of lanes) this.#lanes.delete(lane);
        if (descriptor.effect === "read") operation.runningReads -= 1;
        else operation.runningMutations -= 1;
        this.#finish(operation);
        this.#pump();
      });
  }

  #finish(operation: QueuedOperation): void {
    if (operation.settled || operation.results.size !== operation.spec.calls.length || [...this.#running].some((entry) => entry.operation === operation)) return;
    operation.settled = true;
    const index = this.#operations.indexOf(operation);
    if (index >= 0) this.#operations.splice(index, 1);
    if (operation.deadline) clearTimeout(operation.deadline);
    if (operation.abortListener) operation.spec.signal?.removeEventListener("abort", operation.abortListener);
    const steps = operation.spec.calls.map((call) => ({ key: call.key, outcome: operation.results.get(call.key)!.outcome }));
    const outcomes = steps.map((step) => step.outcome);
    const state = outcomes.includes("unknown") ? "reconciling" : outcomes.every((outcome) => outcome === "succeeded") ? "completed" : outcomes.some((outcome) => outcome === "succeeded") ? "partial" : outcomes.every((outcome) => outcome === "cancelled") ? "cancelled" : "failed";
    operation.resolve({ operationId: operation.spec.operationId, state, steps, outputs: operation.outputs });
  }
}
