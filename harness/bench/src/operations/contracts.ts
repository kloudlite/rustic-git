/**
 * O01 — frozen shared contracts for the harness operation executor (contract version `v1`).
 *
 * One source of truth for what the main model may send to `operate(...)`, what the
 * executor may report back, and how O02–O09 describe capabilities, arguments, budgets,
 * transitions, and recovery. Shapes are declared once as data in `./shape.ts`, which
 * produces both the runtime validation and the published JSON Schema.
 *
 * Trust boundary: model input arrives as `unknown` and must pass `validateOperateRequest`.
 * Actor, tenant, session, turn revision, and scope come from the harness
 * (`TrustedActorContext`), never from the request. A well-formed reference is not a
 * permission grant — validation here is shape only; the store and dispatch adapter decide.
 */
import { createHash } from "node:crypto";
import {
  ContractViolation,
  DEFAULT_MAX_BYTES,
  KEY_MAX_CHARS,
  checkNode,
  field,
  hasOwnKey,
  isForbiddenKey,
  isRecord,
  item,
  runValidation,
  toJsonSchema,
  validateJsonSchemaLike,
  withDefinitions,
} from "./shape.ts";
import type { IssueCode, JsonSchemaLike, JsonValue, Node, Validation, ValidationIssue } from "./shape.ts";

export { ContractViolation, JSON_LIMITS, KEY_MAX_CHARS, validateJsonSchemaLike, validateJsonValue } from "./shape.ts";
export type {
  Ctx,
  IssueCode,
  JsonLimits,
  JsonObject,
  JsonSchemaLike,
  JsonValue,
  Validation,
  ValidationIssue,
} from "./shape.ts";

export const CONTRACT_VERSION = "v1";
export const OPERATE_TOOL_NAME = "operate";

/** Exact-DAG output bindings stay unresolved until dispatch (see `resolveCallArgs`). */
export const BINDING_RESOLUTION_PHASE = "dispatch_time";

/** Reserved key that marks an artifact reference inside otherwise-literal JSON args. */
export const ARTIFACT_TAG = "$artifact";

/** The startup description that teaches the whole common path (design §5, protocol §6). */
export const DEFAULT_INSTRUCTION_DESCRIPTION =
  "Give one bounded instruction describing the intended outcome, naming the file or resource when needed. " +
  "The executor resolves tool arguments and performs the work. For edits, describe the desired change and " +
  "constraints; the executor reads the source and calculates the patch. Do not repeat source text or build a " +
  "patch unless its exact content is essential. Results arrive with an operation ID; inspect or cancel through " +
  "this tool when needed.";

export const REQUEST_LIMITS = {
  /** Total characters of strings and keys in one request, counted across the whole document. */
  requestChars: DEFAULT_MAX_BYTES,
  keyChars: KEY_MAX_CHARS,
  instructionChars: 8_000,
  objectiveChars: 2_000,
  contextRefs: 16,
  inputs: 32,
  constraints: 16,
  constraintChars: 500,
  expectedResults: 16,
  expectedResultChars: 500,
  exactCalls: 12,
  dependsOnPerCall: 11,
  bindingsPerCall: 32,
  argsPerCall: 32,
  callKeyChars: 64,
  capabilityChars: 128,
  versionChars: 64,
  targetRefChars: 256,
  operationIdChars: 128,
  decisionIdChars: 128,
  selectPathSegments: 8,
  evidenceRefs: 16,
  evidenceRefChars: 256,
  summaryChars: 2_000,
  messageChars: 1_000,
} as const;

/** Largest artifact (read snapshot, generated payload, exact patch) the contract admits. */
export const MAX_ARTIFACT_BYTES = 8 * 1024 * 1024;

const ID_RE = /^[A-Za-z0-9][A-Za-z0-9._:-]*$/;
const DIGEST_RE = /^sha256:[0-9a-f]{64}$/;
const MEDIA_TYPE_RE = /^[a-z]+\/[A-Za-z0-9.+-]+$/;
const CAPABILITY_RE = /^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$/;
const PINNED_VERSION_RE = /^\d+\.\d+(\.\d+)?$/;
const CALL_KEY_RE = /^[a-z][a-z0-9_]*$/;
const INPUT_KEY_RE = /^[A-Za-z_][A-Za-z0-9_.-]*$/;
const OUTPUT_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

const issue = (path: string, code: IssueCode, message: string): ValidationIssue => ({ path, code, message });

// ---------------------------------------------------------------------------
// Frozen request types
// ---------------------------------------------------------------------------

/**
 * Authorized context: a message, operation evidence, or a workspace artifact.
 * Shape validation only — a well-formed reference to another tenant's artifact must
 * still be refused by the durable store and dispatch adapter.
 */
export type ContextRef =
  | { kind: "message"; messageId: string }
  | { kind: "evidence"; operationId: string; evidenceId: string }
  | { kind: "artifact"; artifactId: string; digest: string; byteLength: number; mediaType?: string };

/** Exact content/source supplied inline, under `$artifact` inside JSON args. */
export type ArtifactRef = { artifactId: string; digest: string; byteLength: number; mediaType?: string };

export type IntentRequest = {
  instruction: string;
  contextRefs?: ContextRef[];
  inputs?: Record<string, JsonValue>;
  constraints?: string[];
  expectedResults?: string[];
};

export type DescribeRequest = {
  action: "describe";
  capability?: string;
  cursor?: string;
  detail?: "guide" | "schema";
};

/** Typed dependency binding between steps; distinct from literal JSON in `args`. */
export type OutputBinding = { from: string; output: string; select?: Array<string | number> };

export type ExactCall = {
  key: string;
  capability: string;
  capabilityVersion: string;
  targetRef?: string;
  args?: Record<string, JsonValue>;
  argsFrom?: Record<string, OutputBinding>;
  dependsOn?: string[];
};

export type ExactRequest = { objective: string; contextRefs?: ContextRef[]; calls: ExactCall[] };
export type ExactOperateRequest = { action: "exact"; request: ExactRequest };
export type InspectRequest = { action: "inspect"; operationId: string; afterSequence?: number };

export type DecisionResolution =
  | { kind: "recorded_user_decision"; recordId: string }
  | { kind: "additional_input"; inputs: Record<string, JsonValue>; contextRefs?: ContextRef[] };

export type ResumeRequest = {
  action: "resume";
  operationId: string;
  decisionId: string;
  /** The operation revision the caller believes it is answering; stale values are refused. */
  expectedRevision: number;
  resolution: DecisionResolution;
};

export type CancelRequest = { action: "cancel"; operationId: string };

export type OperateRequest =
  | IntentRequest
  | DescribeRequest
  | ExactOperateRequest
  | InspectRequest
  | ResumeRequest
  | CancelRequest;

export type RequestAction = DescribeRequest["action"] | ExactOperateRequest["action"] | InspectRequest["action"] | ResumeRequest["action"] | CancelRequest["action"];

const REQUEST_ACTIONS: readonly string[] = ["describe", "exact", "inspect", "resume", "cancel"];

// ---------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------

const id = (max: number, hint = "must be an opaque identifier"): Node => ({ t: "string", min: 1, max, pattern: ID_RE, hint });
const capabilityName: Node = { t: "string", min: 1, max: REQUEST_LIMITS.capabilityChars, pattern: CAPABILITY_RE, hint: "must be a dotted lowercase name" };
const pinnedVersion: Node = { t: "string", min: 1, max: REQUEST_LIMITS.versionChars, pattern: PINNED_VERSION_RE, hint: "must pin an exact version such as 1.0.0" };
const operationId: Node = id(REQUEST_LIMITS.operationIdChars);
const digest: Node = { t: "string", min: 1, max: 80, pattern: DIGEST_RE, hint: "must be sha256:<64 hex>" };
const json: Node = { t: "json" };

/** Resume revisions carry their own code: a stale or malformed revision is a resume failure. */
const expectedRevision: Node = {
  t: "custom",
  schema: { type: "integer", minimum: 1, description: "Operation revision the caller is answering; stale values are refused." },
  check: (value, revisionPath) =>
    typeof value === "number" && Number.isInteger(value) && value >= 1
      ? { ok: true, value }
      : { ok: false, issues: [issue(revisionPath, "invalid_revision", "expectedRevision must be an integer >= 1")] },
};

const ARTIFACT_REF: Extract<Node, { t: "object" }> = {
  t: "object",
  fields: {
    artifactId: id(REQUEST_LIMITS.targetRefChars, "must be an artifact identifier"),
    digest,
    byteLength: { t: "int", min: 0, max: MAX_ARTIFACT_BYTES },
    mediaType: { t: "string", min: 1, max: 128, pattern: MEDIA_TYPE_RE, hint: "must be a lowercase type/subtype", optional: true },
  },
};

const withKind = (kind: string, spec: Extract<Node, { t: "object" }>): Node => ({
  t: "object",
  fields: { kind: { t: "literal", value: kind }, ...spec.fields },
});

const CONTEXT_REF: Node = {
  t: "oneOf",
  variants: [
    withKind("message", { t: "object", fields: { messageId: id(REQUEST_LIMITS.decisionIdChars) } }),
    withKind("evidence", {
      t: "object",
      fields: { operationId, evidenceId: id(REQUEST_LIMITS.evidenceRefChars, "must be an evidence identifier") },
    }),
    withKind("artifact", ARTIFACT_REF),
  ],
};

const JSON_TAGS: Record<string, Node> = { [ARTIFACT_TAG]: ARTIFACT_REF };

const contextRefs: Node = { t: "array", min: 1, max: REQUEST_LIMITS.contextRefs, of: CONTEXT_REF };
const dataInputs = (rejectApprovalKeys: boolean): Node => ({
  t: "dict",
  minKeys: 1,
  maxKeys: REQUEST_LIMITS.inputs,
  of: json,
  keyPattern: INPUT_KEY_RE,
  ...(rejectApprovalKeys ? { rejectApprovalKeys: true as const } : {}),
});

const INTENT_FORM: Node = {
  t: "object",
  fields: {
    instruction: { t: "string", min: 1, max: REQUEST_LIMITS.instructionChars },
    contextRefs: { ...contextRefs, optional: true },
    inputs: { ...dataInputs(false), optional: true },
    constraints: { t: "stringList", min: 1, max: REQUEST_LIMITS.constraints, itemMax: REQUEST_LIMITS.constraintChars, optional: true },
    expectedResults: {
      t: "stringList",
      min: 1,
      max: REQUEST_LIMITS.expectedResults,
      itemMax: REQUEST_LIMITS.expectedResultChars,
      optional: true,
    },
  },
};

const DESCRIBE_FORM: Node = {
  t: "object",
  fields: {
    action: { t: "literal", value: "describe" },
    capability: { ...capabilityName, optional: true },
    cursor: { ...id(REQUEST_LIMITS.capabilityChars, "must be an opaque cursor"), optional: true },
    detail: { t: "enum", values: ["guide", "schema"], optional: true },
  },
};

const OUTPUT_BINDING: Node = {
  t: "object",
  fields: {
    from: { t: "string", min: 1, max: REQUEST_LIMITS.callKeyChars, pattern: CALL_KEY_RE, hint: "must be a step key" },
    output: { t: "string", min: 1, max: REQUEST_LIMITS.callKeyChars, pattern: OUTPUT_NAME_RE, hint: "must name a declared capability output" },
    select: {
      t: "array",
      min: 1,
      max: REQUEST_LIMITS.selectPathSegments,
      of: {
        t: "oneOf",
        variants: [
          { t: "string", min: 1, max: 64, pattern: OUTPUT_NAME_RE },
          { t: "int", min: 0, max: 10_000 },
        ],
      },
      optional: true,
    },
  },
};

const EXACT_CALL: Node = {
  t: "object",
  fields: {
    key: { t: "string", min: 1, max: REQUEST_LIMITS.callKeyChars, pattern: CALL_KEY_RE, hint: "must be a lowercase step key" },
    capability: capabilityName,
    capabilityVersion: pinnedVersion,
    targetRef: { ...id(REQUEST_LIMITS.targetRefChars, "must be an opaque scoped reference"), optional: true },
    args: { t: "dict", minKeys: 0, maxKeys: REQUEST_LIMITS.argsPerCall, of: json, keyPattern: INPUT_KEY_RE, optional: true },
    argsFrom: {
      t: "dict",
      minKeys: 1,
      maxKeys: REQUEST_LIMITS.bindingsPerCall,
      of: OUTPUT_BINDING,
      keyPattern: INPUT_KEY_RE,
      optional: true,
    },
    dependsOn: {
      t: "array",
      min: 1,
      max: REQUEST_LIMITS.dependsOnPerCall,
      of: { t: "string", min: 1, max: REQUEST_LIMITS.callKeyChars, pattern: CALL_KEY_RE, hint: "must be a step key" },
      optional: true,
    },
  },
};

function findCycle(nodes: readonly string[], deps: Map<string, readonly string[]>): string[] | undefined {
  const state = new Map<string, 0 | 1 | 2>();
  const stack: string[] = [];
  let found: string[] | undefined;
  const visit = (node: string): void => {
    if (found) return;
    const seen = state.get(node) ?? 0;
    if (seen === 2) return;
    if (seen === 1) {
      found = [...stack.slice(stack.indexOf(node)), node];
      return;
    }
    state.set(node, 1);
    stack.push(node);
    for (const dep of deps.get(node) ?? []) visit(dep);
    stack.pop();
    state.set(node, 2);
  };
  for (const node of nodes) visit(node);
  return found;
}

export const EXACT_PLAN_RULES: readonly string[] = [
  "step keys are unique",
  "every dependsOn entry names another step in the same plan",
  "every argsFrom.from appears in that step's dependsOn",
  "an argument is either literal (args) or bound (argsFrom), never both",
  "the dependency graph is acyclic",
  "bound values resolve at dispatch and are re-checked against the capability input schema",
];

/**
 * Validates the step list and the invariants a single field cannot see — exactly the
 * rules published in `EXACT_PLAN_RULES`, which JSON Schema cannot express.
 */
const EXACT_CALLS: Node = {
  t: "custom",
  schema: {
    type: "array",
    minItems: 1,
    maxItems: REQUEST_LIMITS.exactCalls,
    items: toJsonSchema(EXACT_CALL),
    description: EXACT_PLAN_RULES.join("; "),
  },
  check: (value, path, ctx) => {
    if (!Array.isArray(value)) return { ok: false, issues: [issue(path, "wrong_type", "calls must be an array")] };
    if (value.length < 1 || value.length > REQUEST_LIMITS.exactCalls) {
      return { ok: false, issues: [issue(path, "too_many_items", `calls must hold 1..${REQUEST_LIMITS.exactCalls} steps`)] };
    }
    const calls: ExactCall[] = [];
    const keys = new Set<string>();
    for (let i = 0; i < value.length; i++) {
      const callPath = item(path, i);
      const before = ctx.issues.length;
      const checked = checkNode(value[i], EXACT_CALL, callPath, ctx) as Record<string, unknown> | undefined;
      if (!checked || ctx.issues.length !== before) continue;
      const call = checked as unknown as ExactCall;
      if (keys.has(call.key)) {
        ctx.issues.push(issue(field(callPath, "key"), "duplicate_key", `duplicate step key "${call.key}"`));
        continue;
      }
      keys.add(call.key);
      const deps = call.dependsOn ?? [];
      if (new Set(deps).size !== deps.length) {
        ctx.issues.push(issue(field(callPath, "dependsOn"), "duplicate_key", "dependsOn repeats a step key"));
      }
      for (const [name, binding] of Object.entries(call.argsFrom ?? {})) {
        const bindingPath = field(field(callPath, "argsFrom"), name);
        if (Object.prototype.hasOwnProperty.call(call.args ?? {}, name)) {
          ctx.issues.push(issue(bindingPath, "binding_collision", `"${name}" is supplied as both a literal and a binding`));
        }
        if (!deps.includes(binding.from)) {
          ctx.issues.push(issue(field(bindingPath, "from"), "unknown_dependency", `"${binding.from}" must also appear in dependsOn`));
        }
      }
      calls.push(call);
    }
    for (let i = 0; i < calls.length; i++) {
      for (const dep of calls[i].dependsOn ?? []) {
        if (!keys.has(dep)) {
          ctx.issues.push(issue(item(path, i), "unknown_dependency", `dependency "${dep}" is not a step in this request`));
        }
      }
    }
    const cycle = findCycle(calls.map((call) => call.key), new Map(calls.map((call) => [call.key, call.dependsOn ?? []])));
    if (cycle) ctx.issues.push(issue(path, "cycle", `dependency cycle: ${cycle.join(" -> ")}`));
    return { ok: true, value: calls };
  },
};

const EXACT_FORM: Node = {
  t: "object",
  fields: {
    action: { t: "literal", value: "exact" },
    request: {
      t: "object",
      fields: {
        objective: { t: "string", min: 1, max: REQUEST_LIMITS.objectiveChars },
        contextRefs: { ...contextRefs, optional: true },
        calls: EXACT_CALLS,
      },
    },
  },
};

const INSPECT_FORM: Node = {
  t: "object",
  fields: {
    action: { t: "literal", value: "inspect" },
    operationId,
    afterSequence: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
  },
};

const DECISION_RESOLUTION: Node = {
  t: "oneOf",
  variants: [
    {
      t: "object",
      fields: { kind: { t: "literal", value: "recorded_user_decision" }, recordId: id(REQUEST_LIMITS.decisionIdChars, "must be a decision record identifier") },
    },
    {
      t: "object",
      fields: {
        kind: { t: "literal", value: "additional_input" },
        inputs: dataInputs(true),
        contextRefs: { ...contextRefs, optional: true },
      },
    },
  ],
};

const RESUME_FORM: Node = {
  t: "object",
  fields: {
    action: { t: "literal", value: "resume" },
    operationId,
    decisionId: id(REQUEST_LIMITS.decisionIdChars),
    expectedRevision,
    resolution: DECISION_RESOLUTION,
  },
};

const CANCEL_FORM: Node = {
  t: "object",
  fields: { action: { t: "literal", value: "cancel" }, operationId },
};

const REQUEST_FORMS: readonly Node[] = [INTENT_FORM, DESCRIBE_FORM, EXACT_FORM, INSPECT_FORM, RESUME_FORM, CANCEL_FORM];
const OPERATE_REQUEST: Node = { t: "oneOf", variants: REQUEST_FORMS };

const VALIDATION_OPTIONS = { maxBytes: REQUEST_LIMITS.requestChars, tags: JSON_TAGS };

/**
 * Runtime validation of everything a model may send to `operate`. Shape and bounds
 * only — never authority, and never a permission grant.
 */
export function validateOperateRequest(input: unknown, path = "$"): Validation<OperateRequest> {
  if (isRecord(input) && input.action !== undefined) {
    if (input.instruction !== undefined) {
      return { ok: false, issues: [issue(field(path, "instruction"), "mixed_action", "an instruction request must not carry an action")] };
    }
    if (typeof input.action !== "string") {
      return { ok: false, issues: [issue(field(path, "action"), "wrong_type", "action must be a string")] };
    }
    if (!REQUEST_ACTIONS.includes(input.action)) {
      return { ok: false, issues: [issue(field(path, "action"), "unsupported_action", `unsupported action "${input.action}"`)] };
    }
  }
  return runValidation<OperateRequest>(OPERATE_REQUEST, input, path, VALIDATION_OPTIONS);
}

/** Same as `validateOperateRequest`, but throws `ContractViolation` with every issue. */
export function parseOperateRequest(input: unknown): OperateRequest {
  const result = validateOperateRequest(input);
  if (!result.ok) throw new ContractViolation(result.issues);
  return result.value;
}

/** JSON-safe body check plus request validation; the two are the same contract. */
export function validateOperateRequestJson(input: unknown): Validation<OperateRequest> {
  return validateOperateRequest(input);
}

// ---------------------------------------------------------------------------
// Dependency bindings
// ---------------------------------------------------------------------------

/** Steps whose args must be re-validated against the capability schema after bindings resolve. */
export function callsWithUnresolvedBindings(plan: ExactRequest): ExactCall[] {
  return plan.calls.filter((call) => Object.keys(call.argsFrom ?? {}).length > 0);
}

/**
 * Resolves one `argsFrom` binding: the named output of the dependency step, then an
 * optional path into it. Own properties only — inherited members (`toString`,
 * `constructor`) are not data. Runtime use is at dispatch time (O06); a missing step,
 * output, or path is `binding_unresolved`, never a silent literal.
 */
export function selectOutputPath(outputs: Record<string, JsonValue>, binding: OutputBinding): Validation<JsonValue> {
  const missing = (path: string, code: IssueCode, message: string): Validation<JsonValue> => ({
    ok: false,
    issues: [issue(path, code, message)],
  });
  const base = `$.${binding.from}`;
  if (!hasOwnKey(outputs, binding.from)) return missing(base, "unknown_dependency", `step "${binding.from}" has no output yet`);
  const stepOutput = outputs[binding.from];
  if (!isRecord(stepOutput) || !hasOwnKey(stepOutput, binding.output)) {
    return missing(`${base}.${binding.output}`, "unknown_dependency", `step "${binding.from}" has no output named "${binding.output}"`);
  }
  let current = stepOutput[binding.output] as JsonValue;
  const segments = binding.select ?? [];
  for (let i = 0; i < segments.length; i++) {
    const segment = segments[i];
    const path = `${base}.${binding.output}[${i}]`;
    if (typeof segment === "number") {
      if (!Array.isArray(current) || segment >= current.length) {
        return missing(path, "unknown_dependency", `no element ${segment} in bound output`);
      }
      current = current[segment];
      continue;
    }
    if (isForbiddenKey(segment)) {
      return missing(path, "forbidden_key", `"${segment}" is not a usable output key`);
    }
    if (!isRecord(current) || !hasOwnKey(current, segment)) {
      return missing(path, "unknown_dependency", `no own key "${segment}" in bound output`);
    }
    current = current[segment] as JsonValue;
  }
  return { ok: true, value: current };
}

/** Literal args plus resolved bindings, ready for capability input validation. */
export function resolveCallArgs(call: ExactCall, outputs: Record<string, JsonValue>): Validation<Record<string, JsonValue>> {
  const args: Record<string, JsonValue> = { ...(call.args ?? {}) };
  const issues: ValidationIssue[] = [];
  for (const [name, binding] of Object.entries(call.argsFrom ?? {})) {
    const selected = selectOutputPath(outputs, binding);
    if (!selected.ok) issues.push(...selected.issues);
    else args[name] = selected.value;
  }
  return issues.length ? { ok: false, issues } : { ok: true, value: args };
}

// ---------------------------------------------------------------------------
// Lifecycle: states and the permitted transition table O05 consumes
// ---------------------------------------------------------------------------

export const OPERATION_STATES = [
  "accepted",
  "resolving",
  "awaiting_approval",
  "running",
  "needs_input",
  "reconciling",
  "cancel_requested",
  "completed",
  "partial",
  "failed",
  "cancelled",
  "expired",
] as const;

export type OperationState = (typeof OPERATION_STATES)[number];

/** `partial` is terminal and truthful: effects remain, nothing is still in flight. */
export const TERMINAL_OPERATION_STATES = ["completed", "failed", "cancelled", "expired", "partial"] as const;
export type TerminalOperationState = (typeof TERMINAL_OPERATION_STATES)[number];

export type OperationTransitionTrigger =
  | "accept"
  | "resolve_started"
  | "dispatch_started"
  | "approval_required"
  | "approval_recorded"
  | "decision_required"
  | "decision_supplied"
  | "effects_committed"
  | "effects_partial"
  | "effects_failed"
  | "unknown_outcome"
  | "reconcile_conclusive"
  | "cancel_requested"
  | "cancel_settled"
  | "deadline_reached";

export type OperationTransition = { from: OperationState; to: OperationState; trigger: OperationTransitionTrigger };

/**
 * Every permitted operation transition, keyed by (from, to, trigger). Terminal states
 * have no outgoing edge. `reconciling` holds unknown mutation outcomes until an
 * authoritative result exists: an unresolved write never becomes `failed` or
 * `cancelled`, and is never retried — so an operation holding unknown effects cannot
 * expire away either; it stays observable until reconciliation is conclusive or a
 * decision is recorded. `cancel_settled` asserts the cancellation finished with
 * definitive evidence and no unresolved effects. `partial` is terminal and means
 * effects remain with nothing pending.
 */
export const OPERATION_TRANSITIONS: readonly OperationTransition[] = [
  { from: "accepted", to: "resolving", trigger: "resolve_started" },
  { from: "accepted", to: "awaiting_approval", trigger: "approval_required" },
  { from: "accepted", to: "running", trigger: "dispatch_started" },
  { from: "accepted", to: "needs_input", trigger: "decision_required" },
  { from: "accepted", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "accepted", to: "failed", trigger: "effects_failed" },
  { from: "accepted", to: "expired", trigger: "deadline_reached" },

  { from: "resolving", to: "running", trigger: "dispatch_started" },
  { from: "resolving", to: "awaiting_approval", trigger: "approval_required" },
  { from: "resolving", to: "needs_input", trigger: "decision_required" },
  { from: "resolving", to: "reconciling", trigger: "unknown_outcome" },
  { from: "resolving", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "resolving", to: "failed", trigger: "effects_failed" },
  { from: "resolving", to: "expired", trigger: "deadline_reached" },

  { from: "awaiting_approval", to: "resolving", trigger: "resolve_started" },
  { from: "awaiting_approval", to: "running", trigger: "approval_recorded" },
  { from: "awaiting_approval", to: "needs_input", trigger: "decision_required" },
  { from: "awaiting_approval", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "awaiting_approval", to: "failed", trigger: "effects_failed" },
  { from: "awaiting_approval", to: "expired", trigger: "deadline_reached" },

  { from: "running", to: "awaiting_approval", trigger: "approval_required" },
  { from: "running", to: "needs_input", trigger: "decision_required" },
  { from: "running", to: "reconciling", trigger: "unknown_outcome" },
  { from: "running", to: "completed", trigger: "effects_committed" },
  { from: "running", to: "partial", trigger: "effects_partial" },
  { from: "running", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "running", to: "failed", trigger: "effects_failed" },
  { from: "running", to: "expired", trigger: "deadline_reached" },

  { from: "needs_input", to: "resolving", trigger: "resolve_started" },
  { from: "needs_input", to: "running", trigger: "decision_supplied" },
  { from: "needs_input", to: "awaiting_approval", trigger: "approval_required" },
  { from: "needs_input", to: "partial", trigger: "effects_partial" },
  { from: "needs_input", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "needs_input", to: "failed", trigger: "effects_failed" },
  { from: "needs_input", to: "expired", trigger: "deadline_reached" },

  { from: "reconciling", to: "running", trigger: "dispatch_started" },
  { from: "reconciling", to: "needs_input", trigger: "decision_required" },
  { from: "reconciling", to: "completed", trigger: "reconcile_conclusive" },
  { from: "reconciling", to: "partial", trigger: "effects_partial" },
  { from: "reconciling", to: "cancel_requested", trigger: "cancel_requested" },
  { from: "reconciling", to: "failed", trigger: "reconcile_conclusive" },

  { from: "cancel_requested", to: "cancelled", trigger: "cancel_settled" },
  { from: "cancel_requested", to: "partial", trigger: "effects_partial" },
  { from: "cancel_requested", to: "reconciling", trigger: "unknown_outcome" },
  { from: "cancel_requested", to: "failed", trigger: "effects_failed" },
];

export const STEP_STATES = [
  "queued",
  "awaiting_approval",
  "running",
  "succeeded",
  "failed",
  "skipped",
  "cancelled",
  "outcome_unknown",
] as const;

export type StepState = (typeof STEP_STATES)[number];

export const TERMINAL_STEP_STATES = ["succeeded", "skipped", "cancelled"] as const;
export type TerminalStepState = (typeof TERMINAL_STEP_STATES)[number];

/** A failed step is settled, but a declared retry class may restart it once more. */
export const RETRYABLE_STEP_STATES = ["failed"] as const;
export type RetryableStepState = (typeof RETRYABLE_STEP_STATES)[number];

/** Step states that mean work is still owed (never allowed in a terminal snapshot). */
export const PENDING_STEP_STATES = ["queued", "awaiting_approval", "running", "outcome_unknown"] as const;

export type StepTransitionTrigger =
  | "approval_required"
  | "approval_recorded"
  | "approval_denied"
  | "dependency_failed"
  | "dispatch_started"
  | "result_observed"
  | "result_failed"
  | "outcome_unknown"
  | "reconcile_conclusive"
  | "cancel_confirmed"
  | "retry_allowed"
  | "cancel_requested";

export type StepTransition = { from: StepState; to: StepState; trigger: StepTransitionTrigger };

/**
 * Every permitted step transition, keyed by (from, to, trigger). `outcome_unknown`
 * leaves only through `reconcile_conclusive`: an uncertain mutation is never replayed
 * and never cancelled away. A running step reaches `cancelled` only through
 * `cancel_confirmed` — evidence that no effect was applied — because a mere abort
 * request is not an outcome. `failed -> running` exists only for declared retry classes.
 */
export const STEP_TRANSITIONS: readonly StepTransition[] = [
  { from: "queued", to: "awaiting_approval", trigger: "approval_required" },
  { from: "queued", to: "running", trigger: "dispatch_started" },
  { from: "queued", to: "skipped", trigger: "dependency_failed" },
  { from: "queued", to: "cancelled", trigger: "cancel_requested" },

  { from: "awaiting_approval", to: "running", trigger: "approval_recorded" },
  { from: "awaiting_approval", to: "skipped", trigger: "approval_denied" },
  { from: "awaiting_approval", to: "cancelled", trigger: "cancel_requested" },

  { from: "running", to: "succeeded", trigger: "result_observed" },
  { from: "running", to: "failed", trigger: "result_failed" },
  { from: "running", to: "outcome_unknown", trigger: "outcome_unknown" },
  { from: "running", to: "cancelled", trigger: "cancel_confirmed" },

  { from: "failed", to: "running", trigger: "retry_allowed" },

  { from: "outcome_unknown", to: "succeeded", trigger: "reconcile_conclusive" },
  { from: "outcome_unknown", to: "failed", trigger: "reconcile_conclusive" },
];

const operationIndex = new Map(OPERATION_TRANSITIONS.map((t) => [`${t.from}|${t.to}|${t.trigger}`, t]));
const stepIndex = new Map(STEP_TRANSITIONS.map((t) => [`${t.from}|${t.to}|${t.trigger}`, t]));

/** The table entry for a triple, or undefined. Trigger is part of the contract. */
export function operationTransition(
  from: OperationState,
  to: OperationState,
  trigger: OperationTransitionTrigger,
): OperationTransition | undefined {
  return operationIndex.get(`${from}|${to}|${trigger}`);
}

export function stepTransition(from: StepState, to: StepState, trigger: StepTransitionTrigger): StepTransition | undefined {
  return stepIndex.get(`${from}|${to}|${trigger}`);
}

export function canTransitionOperation(from: OperationState, to: OperationState, trigger: OperationTransitionTrigger): boolean {
  return operationIndex.has(`${from}|${to}|${trigger}`);
}

export function canTransitionStep(from: StepState, to: StepState, trigger: StepTransitionTrigger): boolean {
  return stepIndex.has(`${from}|${to}|${trigger}`);
}

export function isTerminalOperationState(state: OperationState): boolean {
  return (TERMINAL_OPERATION_STATES as readonly string[]).includes(state);
}

export function isTerminalStepState(state: StepState): boolean {
  return (TERMINAL_STEP_STATES as readonly string[]).includes(state);
}

/** Throws unless the exact (from, to, trigger) triple is permitted. O05 applies records through this. */
export function assertOperationTransition(from: OperationState, to: OperationState, trigger: OperationTransitionTrigger): void {
  if (!canTransitionOperation(from, to, trigger)) {
    throw new ContractViolation([issue("$.state", "invalid_transition", `"${from}" -> "${to}" is not permitted by "${trigger}"`)]);
  }
}

/** Self-check over the frozen tables: known states, unique triples, no edge out of terminal. */
export function validateTransitionTables(): Validation<{ operations: number; steps: number }> {
  const issues: ValidationIssue[] = [];
  const states: readonly string[] = OPERATION_STATES;
  const steps: readonly string[] = STEP_STATES;
  const seen = new Set<string>();
  for (const t of OPERATION_TRANSITIONS) {
    const key = `${t.from}|${t.to}|${t.trigger}`;
    if (!states.includes(t.from) || !states.includes(t.to)) issues.push(issue(`$.OPERATION_TRANSITIONS[${key}]`, "invalid_transition", "unknown state"));
    if (seen.has(key)) issues.push(issue(`$.OPERATION_TRANSITIONS[${key}]`, "duplicate_key", "duplicate transition"));
    seen.add(key);
    if (isTerminalOperationState(t.from)) {
      issues.push(issue(`$.OPERATION_TRANSITIONS[${key}]`, "invalid_transition", `terminal state "${t.from}" has an outgoing transition`));
    }
  }
  const seenSteps = new Set<string>();
  for (const t of STEP_TRANSITIONS) {
    const key = `${t.from}|${t.to}|${t.trigger}`;
    if (!steps.includes(t.from) || !steps.includes(t.to)) issues.push(issue(`$.STEP_TRANSITIONS[${key}]`, "invalid_transition", "unknown state"));
    if (seenSteps.has(key)) issues.push(issue(`$.STEP_TRANSITIONS[${key}]`, "duplicate_key", "duplicate transition"));
    seenSteps.add(key);
    if (isTerminalStepState(t.from)) {
      issues.push(issue(`$.STEP_TRANSITIONS[${key}]`, "invalid_transition", `terminal step state "${t.from}" has an outgoing transition`));
    }
    if ((RETRYABLE_STEP_STATES as readonly string[]).includes(t.from) && t.trigger !== "retry_allowed") {
      issues.push(issue(`$.STEP_TRANSITIONS[${key}]`, "invalid_transition", 'a failed step leaves only through "retry_allowed"'));
    }
  }
  return issues.length
    ? { ok: false, issues }
    : { ok: true, value: { operations: OPERATION_TRANSITIONS.length, steps: STEP_TRANSITIONS.length } };
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

export const OPERATION_ERROR_CODES = [
  "unsupported_request",
  "unsupported_capability",
  "invalid_args",
  "validation_failure",
  "ambiguous_target",
  "no_match",
  "ambiguous_match",
  "missing_fact",
  "permission_denied",
  "scope_denied",
  "stale_contract",
  "revision_conflict",
  "snapshot_expired",
  "payload_too_large",
  "binding_unresolved",
  "budget_exceeded",
  "deadline_exceeded",
  "provider_failure",
  "execution_failure",
  "unknown_outcome",
  "decision_mismatch",
  "decision_expired",
  "decision_replayed",
  "forged_approval",
  "invalid_transition",
  "cancelled",
] as const;

export type OperationErrorCode = (typeof OPERATION_ERROR_CODES)[number];

/** Classes a public error must fall into; the model sees one of these, not internals. */
export const PUBLIC_ERROR_CLASSES = [
  "unsupported",
  "ambiguous",
  "missing_information",
  "permission_denied",
  "validation",
  "provider",
  "execution",
  "unknown_outcome",
  "conflict",
  "lifecycle",
] as const;

export type PublicErrorClass = (typeof PUBLIC_ERROR_CLASSES)[number];

export const ERROR_CLASS_OF: Record<OperationErrorCode, PublicErrorClass> = {
  unsupported_request: "unsupported",
  unsupported_capability: "unsupported",
  invalid_args: "validation",
  validation_failure: "validation",
  ambiguous_target: "ambiguous",
  no_match: "missing_information",
  ambiguous_match: "ambiguous",
  missing_fact: "missing_information",
  permission_denied: "permission_denied",
  scope_denied: "permission_denied",
  stale_contract: "conflict",
  revision_conflict: "conflict",
  snapshot_expired: "conflict",
  payload_too_large: "validation",
  binding_unresolved: "execution",
  budget_exceeded: "lifecycle",
  deadline_exceeded: "lifecycle",
  provider_failure: "provider",
  execution_failure: "execution",
  unknown_outcome: "unknown_outcome",
  decision_mismatch: "permission_denied",
  decision_expired: "permission_denied",
  decision_replayed: "permission_denied",
  forged_approval: "permission_denied",
  invalid_transition: "lifecycle",
  cancelled: "lifecycle",
};

export type OperationError = {
  code: OperationErrorCode;
  message: string;
  retryable: boolean;
  missing?: string[];
  refs?: string[];
};

const ERROR_NODE: Node = {
  t: "object",
  fields: {
    code: { t: "enum", values: OPERATION_ERROR_CODES },
    message: { t: "string", min: 1, max: REQUEST_LIMITS.messageChars },
    retryable: { t: "bool" },
    missing: { t: "stringList", min: 1, max: 16, itemMax: 256, optional: true },
    refs: { t: "stringList", min: 1, max: REQUEST_LIMITS.evidenceRefs, itemMax: REQUEST_LIMITS.evidenceRefChars, optional: true },
  },
};

export function validateOperationError(value: unknown, path = "$"): Validation<OperationError> {
  return runValidation<OperationError>(ERROR_NODE, value, path);
}

export function publicErrorClass(code: OperationErrorCode): PublicErrorClass {
  return ERROR_CLASS_OF[code];
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

export type OperationBudgets = {
  maxSteps: number;
  maxSelectionRounds: number;
  maxGenerationCalls: number;
  maxConcurrentReads: number;
  maxConcurrentMutations: number;
  operationDeadlineMs: number;
  handleWithinMs: number;
  providerTimeoutMs: number;
  maxGeneratedPayloadBytes: number;
  maxTextFileBytes: number;
  maxReadSnapshotBytes: number;
};

/** Starting policy values from the design (§7); longer work needs an explicit recipe budget. */
export const DEFAULT_BUDGETS: OperationBudgets = {
  maxSteps: 12,
  maxSelectionRounds: 3,
  maxGenerationCalls: 2,
  maxConcurrentReads: 4,
  maxConcurrentMutations: 2,
  operationDeadlineMs: 10 * 60 * 1000,
  handleWithinMs: 2_000,
  providerTimeoutMs: 30_000,
  maxGeneratedPayloadBytes: 64 * 1024,
  maxTextFileBytes: 1024 * 1024,
  maxReadSnapshotBytes: 4 * 1024 * 1024,
};

const BUDGET_NODE: Node = {
  t: "object",
  fields: {
    maxSteps: { t: "int", min: 1, max: 1_000 },
    maxSelectionRounds: { t: "int", min: 0, max: 100 },
    maxGenerationCalls: { t: "int", min: 0, max: 100 },
    maxConcurrentReads: { t: "int", min: 1, max: 64 },
    maxConcurrentMutations: { t: "int", min: 1, max: 16 },
    operationDeadlineMs: { t: "int", min: 1_000, max: 24 * 60 * 60 * 1000 },
    handleWithinMs: { t: "int", min: 100, max: 60_000 },
    providerTimeoutMs: { t: "int", min: 100, max: 10 * 60 * 1000 },
    maxGeneratedPayloadBytes: { t: "int", min: 1, max: MAX_ARTIFACT_BYTES },
    maxTextFileBytes: { t: "int", min: 1, max: MAX_ARTIFACT_BYTES },
    maxReadSnapshotBytes: { t: "int", min: 1, max: MAX_ARTIFACT_BYTES },
  },
};

/** Budgets may narrow the policy ceiling but never raise it. */
export function validateBudgets(
  value: unknown,
  ceiling: OperationBudgets = DEFAULT_BUDGETS,
  path = "$",
): Validation<OperationBudgets> {
  const result = runValidation<OperationBudgets>(BUDGET_NODE, value, path);
  if (!result.ok) return result;
  const issues: ValidationIssue[] = [];
  for (const key of Object.keys(DEFAULT_BUDGETS) as Array<keyof OperationBudgets>) {
    if (result.value[key] > ceiling[key]) {
      issues.push(issue(field(path, key), "out_of_range", `${key} exceeds the policy ceiling of ${ceiling[key]}`));
    }
  }
  return issues.length ? { ok: false, issues } : result;
}

// ---------------------------------------------------------------------------
// Trusted context
// ---------------------------------------------------------------------------

/** Execution scope of an operation. Harness-supplied; never taken from the request. */
export type OperationScope = { workspaceId?: string; treeId?: string; repositoryId?: string };

/**
 * Supplied by the authenticated bench/extension boundary, never by the model.
 * `TRUSTED_CONTEXT_SOURCES` records each field's origin so O08 cannot read one out
 * of tool arguments.
 */
export type TrustedActorContext = {
  actorId: string;
  tenantId: string;
  sessionId: string;
  turnId: string;
  toolCallId: string;
  /** Monotonic revision of the current user turn; stale resumes are refused against it. */
  turnRevision: number;
  scope: OperationScope;
};

export const TRUSTED_CONTEXT_SOURCES: Record<keyof TrustedActorContext, string> = {
  actorId: "authenticated bench session owner",
  tenantId: "authenticated bench session tenant",
  sessionId: "bench session record",
  turnId: "current exchange/turn record",
  toolCallId: "the tool call being dispatched",
  turnRevision: "monotonic revision of the current user turn",
  scope: "workspace/tree/repository binding of the bench session",
};

const TRUSTED_CONTEXT_NODE: Node = {
  t: "object",
  fields: {
    actorId: id(256),
    tenantId: id(256),
    sessionId: id(256),
    turnId: id(256),
    toolCallId: id(256),
    turnRevision: { t: "int", min: 1, max: Number.MAX_SAFE_INTEGER },
    scope: {
      t: "object",
      fields: {
        workspaceId: { ...id(256), optional: true },
        treeId: { ...id(256), optional: true },
        repositoryId: { ...id(256), optional: true },
      },
    },
  },
};

export function validateTrustedActorContext(value: unknown, path = "$"): Validation<TrustedActorContext> {
  return runValidation<TrustedActorContext>(TRUSTED_CONTEXT_NODE, value, path);
}

/**
 * Stable deduplication key for durable acceptance (O05). The tuple is hashed, not
 * joined: identifiers may contain ":", so joining would let two different tool calls
 * collide. Same key plus a different canonical request digest is rejected; different
 * tool-call IDs are never treated as duplicates.
 */
export function deriveDeduplicationKey(context: TrustedActorContext): string {
  return canonicalDigest({
    actorId: context.actorId,
    sessionId: context.sessionId,
    turnId: context.turnId,
    toolCallId: context.toolCallId,
  });
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

export type DecisionClass = "user_authorization" | "user_preference" | "additional_input";

/** Which pending decision classes a resolution may legally answer. */
export function decisionClassesForResolution(resolution: DecisionResolution): readonly DecisionClass[] {
  return resolution.kind === "recorded_user_decision"
    ? ["user_authorization", "user_preference"]
    : ["additional_input"];
}

export function resolutionCanResolve(resolution: DecisionResolution, decisionClass: DecisionClass): boolean {
  return decisionClassesForResolution(resolution).includes(decisionClass);
}

export type PendingDecision = {
  decisionId: string;
  operationId: string;
  stepId: string;
  decisionClass: DecisionClass;
  question: string;
  createdAt: number;
  expiresAt?: number;
  revision: number;
};

const PENDING_DECISION_NODE: Node = {
  t: "object",
  fields: {
    decisionId: id(REQUEST_LIMITS.decisionIdChars),
    operationId,
    stepId: id(128),
    decisionClass: { t: "enum", values: ["user_authorization", "user_preference", "additional_input"] },
    question: { t: "string", min: 1, max: 2_000 },
    createdAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER },
    expiresAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
    revision: { t: "int", min: 1, max: Number.MAX_SAFE_INTEGER },
  },
};

export function validatePendingDecision(value: unknown, path = "$"): Validation<PendingDecision> {
  return runValidation<PendingDecision>(PENDING_DECISION_NODE, value, path);
}

/**
 * A decision recorded by the authenticated user UI or the trusted policy adapter.
 * A model never constructs this; `resume` may only cite its `recordId`. Recorded
 * decisions are authorizations or preferences — never additional input.
 */
export type RecordedDecision = {
  recordId: string;
  operationId: string;
  stepId: string;
  decisionId: string;
  decisionClass: "user_authorization" | "user_preference";
  actorId: string;
  tenantId: string;
  sessionId: string;
  payloadDigest: string;
  revision: number;
  policySource: "user_ui" | "trusted_policy";
  outcome: "granted" | "denied";
  recordedAt: number;
  expiresAt?: number;
  usedAt?: number;
};

const RECORDED_DECISION_NODE: Node = {
  t: "object",
  fields: {
    recordId: id(REQUEST_LIMITS.decisionIdChars, "must be a decision record identifier"),
    operationId,
    stepId: id(128),
    decisionId: id(REQUEST_LIMITS.decisionIdChars),
    decisionClass: { t: "enum", values: ["user_authorization", "user_preference"] },
    actorId: id(256),
    tenantId: id(256),
    sessionId: id(256),
    payloadDigest: digest,
    revision: { t: "int", min: 1, max: Number.MAX_SAFE_INTEGER },
    policySource: { t: "enum", values: ["user_ui", "trusted_policy"] },
    outcome: { t: "enum", values: ["granted", "denied"] },
    recordedAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER },
    expiresAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
    usedAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
  },
};

export function validateRecordedDecision(value: unknown, path = "$"): Validation<RecordedDecision> {
  return runValidation<RecordedDecision>(RECORDED_DECISION_NODE, value, path);
}

export type ResumeExpectation = {
  actorId: string;
  tenantId: string;
  sessionId: string;
  operationId: string;
  stepId: string;
  decisionId: string;
  /** Class of the pending decision being answered; a record of another class never answers it. */
  decisionClass: DecisionClass;
  payloadDigest: string;
  revision: number;
  now: number;
};

/**
 * The verified meaning of a cited decision. `ok` means the citation checked out, not
 * that work may run: a validly recorded denial is evidence and never permission, so
 * O05/O06 must refuse the step unless `requiredAction` is `dispatch` (which only a
 * matching `granted` record yields).
 */
export type ResumeVerdict = {
  record: RecordedDecision;
  outcome: "granted" | "denied";
  dispatchAuthorized: boolean;
  requiredAction: "dispatch" | "refuse_step";
};

/**
 * Cross-checks a cited decision against the operation, step, decision class, and
 * current step payload, so forged, stale, expired, replayed, cross-session, and
 * wrong-step citations fail explicitly. O05 calls this before resuming; O08 records
 * the decision in the first place.
 */
export function checkResumeAgainstRecord(
  record: RecordedDecision,
  expectation: ResumeExpectation,
): Validation<ResumeVerdict> {
  const issues: ValidationIssue[] = [];
  const mismatch = (code: IssueCode, message: string) => issues.push(issue("$.resolution.recordId", code, message));
  if (record.operationId !== expectation.operationId) mismatch("decision_mismatch", "decision belongs to another operation");
  if (record.stepId !== expectation.stepId) mismatch("decision_mismatch", "decision was recorded for another step");
  if (record.decisionId !== expectation.decisionId) mismatch("decision_mismatch", "decision does not answer the cited decisionId");
  if (record.decisionClass !== expectation.decisionClass) {
    mismatch("decision_mismatch", "decision class does not answer the pending decision");
  }
  if (record.actorId !== expectation.actorId || record.tenantId !== expectation.tenantId) {
    mismatch("decision_mismatch", "decision was recorded for another actor or tenant");
  }
  if (record.sessionId !== expectation.sessionId) mismatch("decision_mismatch", "decision was recorded in another session");
  if (record.payloadDigest !== expectation.payloadDigest) {
    mismatch("validation_failure", "the approved payload changed since the decision was recorded");
  }
  if (record.revision !== expectation.revision) mismatch("invalid_revision", "decision was recorded against another revision");
  if (record.usedAt !== undefined) mismatch("decision_replayed", "decision has already been consumed");
  if (record.expiresAt !== undefined && record.expiresAt <= expectation.now) {
    mismatch("decision_expired", "decision has expired");
  }
  if (issues.length) return { ok: false, issues };
  const dispatchAuthorized = record.outcome === "granted";
  return {
    ok: true,
    value: {
      record,
      outcome: record.outcome,
      dispatchAuthorized,
      requiredAction: dispatchAuthorized ? "dispatch" : "refuse_step",
    },
  };
}

// ---------------------------------------------------------------------------
// Events, results, snapshots
// ---------------------------------------------------------------------------

export const OPERATION_EVENT_PHASES = [
  "accepted",
  "resolving",
  "resolved",
  "approval_required",
  "decision_recorded",
  "queued",
  "dispatched",
  "progress",
  "succeeded",
  "failed",
  "unknown_outcome",
  "reconciled",
  "needs_input",
  "completed",
  "partial",
  "cancelled",
  "expired",
] as const;

export type OperationEventPhase = (typeof OPERATION_EVENT_PHASES)[number];
export type OperationEventModel = { provider: string; model: string; version: string };

/**
 * Durable event envelope (O05 storage, O09 replay). Redacted preview only: never
 * source, patches, secrets, or chain-of-thought — a concise decision code and the
 * approved argument digest are the maximum.
 */
export type OperationEvent = {
  operationId: string;
  /** Strictly increasing per operation, starting at 1. */
  sequence: number;
  at: number;
  phase: OperationEventPhase;
  revision: number;
  stepId?: string;
  capability?: string;
  summary: string;
  decisionCode?: string;
  queueReason?: string;
  dependencies?: string[];
  model?: OperationEventModel;
  retryCount?: number;
  elapsedMs?: number;
  evidenceRefs?: string[];
  argDigest?: string;
  argPreview?: string;
};

const EVENT_NODE: Node = {
  t: "object",
  fields: {
    operationId,
    sequence: { t: "int", min: 1, max: Number.MAX_SAFE_INTEGER },
    at: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER },
    phase: { t: "enum", values: OPERATION_EVENT_PHASES },
    revision: { t: "int", min: 1, max: Number.MAX_SAFE_INTEGER },
    stepId: { ...id(128), optional: true },
    capability: { ...capabilityName, optional: true },
    summary: { t: "string", min: 1, max: REQUEST_LIMITS.summaryChars },
    decisionCode: { t: "string", min: 1, max: 128, optional: true },
    queueReason: { t: "string", min: 1, max: 256, optional: true },
    dependencies: {
      t: "array",
      min: 1,
      max: REQUEST_LIMITS.exactCalls,
      of: { t: "string", min: 1, max: 64 },
      optional: true,
    },
    model: {
      t: "object",
      fields: {
        provider: { t: "string", min: 1, max: 64 },
        model: { t: "string", min: 1, max: 128 },
        version: { t: "string", min: 1, max: 64 },
      },
      optional: true,
    },
    retryCount: { t: "int", min: 0, max: 100, optional: true },
    elapsedMs: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
    evidenceRefs: {
      t: "stringList",
      min: 1,
      max: REQUEST_LIMITS.evidenceRefs,
      itemMax: REQUEST_LIMITS.evidenceRefChars,
      optional: true,
    },
    argDigest: { ...digest, optional: true },
    argPreview: { t: "string", min: 1, max: 512, optional: true },
  },
};

export function validateOperationEvent(value: unknown, path = "$"): Validation<OperationEvent> {
  return runValidation<OperationEvent>(EVENT_NODE, value, path, VALIDATION_OPTIONS);
}

/** Replay guard: events for one operation arrive in strictly increasing sequence order. */
export function validateEventSequence(
  previousSequence: number | undefined,
  event: OperationEvent,
): Validation<OperationEvent> {
  if (previousSequence !== undefined && event.sequence <= previousSequence) {
    return {
      ok: false,
      issues: [
        issue("$.sequence", "out_of_range", `sequence ${event.sequence} does not follow ${previousSequence}; replay must be monotonic`),
      ],
    };
  }
  return { ok: true, value: event };
}

export type StepRecord = {
  stepId: string;
  key?: string;
  state: StepState;
  capability: string;
  capabilityVersion: string;
  targetRef?: string;
  effect: CapabilityEffect;
  resourceKeys?: string[];
  dependencies?: string[];
  attempts: number;
  queuedReason?: string;
  startedAt?: number;
  endedAt?: number;
  backendOperationId?: string;
  idempotencyKey?: string;
  evidenceRefs?: string[];
  error?: OperationError;
};

const STEP_RECORD_NODE: Node = {
  t: "object",
  fields: {
    stepId: id(128),
    key: { t: "string", min: 1, max: REQUEST_LIMITS.callKeyChars, pattern: CALL_KEY_RE, optional: true },
    state: { t: "enum", values: STEP_STATES },
    capability: capabilityName,
    capabilityVersion: pinnedVersion,
    targetRef: { ...id(REQUEST_LIMITS.targetRefChars), optional: true },
    effect: { t: "enum", values: ["read", "write", "destroy"] },
    resourceKeys: { t: "stringList", min: 1, max: 16, itemMax: 128, optional: true },
    dependencies: { t: "stringList", min: 1, max: REQUEST_LIMITS.dependsOnPerCall, itemMax: 64, optional: true },
    attempts: { t: "int", min: 0, max: 100 },
    queuedReason: { t: "string", min: 1, max: 256, optional: true },
    startedAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
    endedAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
    backendOperationId: { t: "string", min: 1, max: 256, optional: true },
    idempotencyKey: { t: "string", min: 1, max: 256, optional: true },
    evidenceRefs: {
      t: "stringList",
      min: 1,
      max: REQUEST_LIMITS.evidenceRefs,
      itemMax: REQUEST_LIMITS.evidenceRefChars,
      optional: true,
    },
    error: { ...ERROR_NODE, optional: true },
  },
};

export function validateStepRecord(value: unknown, path = "$"): Validation<StepRecord> {
  return runValidation<StepRecord>(STEP_RECORD_NODE, value, path, VALIDATION_OPTIONS);
}

export type UnknownOutcome = {
  stepId: string;
  capability: string;
  /** Digest of the approved args that were dispatched; reconciliation uses it, never a retry. */
  dispatchDigest?: string;
  since: number;
};

const UNKNOWN_OUTCOME_NODE: Node = {
  t: "object",
  fields: {
    stepId: id(128),
    capability: capabilityName,
    dispatchDigest: { ...digest, optional: true },
    since: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER },
  },
};

export function validateUnknownOutcome(value: unknown, path = "$"): Validation<UnknownOutcome> {
  return runValidation<UnknownOutcome>(UNKNOWN_OUTCOME_NODE, value, path, VALIDATION_OPTIONS);
}

/**
 * The compact result is the only shape returned to the main model. It carries no
 * source, patches, internal schemas, actor identity, or scope.
 */
export type CompactOperationResult = {
  operationId: string;
  revision: number;
  state: OperationState;
  summary: string;
  evidenceRefs?: string[];
  changed?: boolean;
  decision?: { decisionId: string; decisionClass: DecisionClass; question: string; expiresAt?: number };
  error?: OperationError;
  unknownOutcomes?: string[];
};

const COMPACT_RESULT_NODE: Node = {
  t: "object",
  fields: {
    operationId,
    revision: { t: "int", min: 1, max: Number.MAX_SAFE_INTEGER },
    state: { t: "enum", values: OPERATION_STATES },
    summary: { t: "string", min: 1, max: REQUEST_LIMITS.summaryChars },
    evidenceRefs: {
      t: "stringList",
      min: 1,
      max: REQUEST_LIMITS.evidenceRefs,
      itemMax: REQUEST_LIMITS.evidenceRefChars,
      optional: true,
    },
    changed: { t: "bool", optional: true },
    decision: {
      t: "object",
      fields: {
        decisionId: id(REQUEST_LIMITS.decisionIdChars),
        decisionClass: { t: "enum", values: ["user_authorization", "user_preference", "additional_input"] },
        question: { t: "string", min: 1, max: 2_000 },
        expiresAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
      },
      optional: true,
    },
    error: { ...ERROR_NODE, optional: true },
    unknownOutcomes: { t: "stringList", min: 1, max: REQUEST_LIMITS.exactCalls, itemMax: 128, optional: true },
  },
};

export function validateCompactOperationResult(value: unknown, path = "$"): Validation<CompactOperationResult> {
  return runValidation<CompactOperationResult>(COMPACT_RESULT_NODE, value, path, VALIDATION_OPTIONS);
}

/**
 * Full `inspect` snapshot: internal and trusted. Only projections (compact result,
 * redacted events) cross back to the model. Persistence is O05's job; this freezes the
 * record shape, the trust split, and the terminal-partial invariant.
 */
export type OperationSnapshot = {
  contractVersion: string;
  operationId: string;
  revision: number;
  state: OperationState;
  createdAt: number;
  updatedAt: number;
  deadlineAt?: number;
  actor: { actorId: string; tenantId: string; sessionId: string; turnId: string };
  scope: OperationScope;
  request: OperateRequest;
  requestDigest: string;
  dedupeKey: string;
  budgets: OperationBudgets;
  steps: StepRecord[];
  pendingDecisions: PendingDecision[];
  unknownOutcomes: UnknownOutcome[];
  usage: { steps: number; selectionRounds: number; generationCalls: number; attempts: number };
  lastSequence: number;
  result?: CompactOperationResult;
};

const SNAPSHOT_NODE: Node = {
  t: "object",
  fields: {
    contractVersion: { t: "literal", value: CONTRACT_VERSION },
    operationId,
    revision: { t: "int", min: 1, max: Number.MAX_SAFE_INTEGER },
    state: { t: "enum", values: OPERATION_STATES },
    createdAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER },
    updatedAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER },
    deadlineAt: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER, optional: true },
    actor: {
      t: "object",
      fields: { actorId: id(256), tenantId: id(256), sessionId: id(256), turnId: id(256) },
    },
    scope: {
      t: "object",
      fields: {
        workspaceId: { ...id(256), optional: true },
        treeId: { ...id(256), optional: true },
        repositoryId: { ...id(256), optional: true },
      },
    },
    request: OPERATE_REQUEST,
    requestDigest: digest,
    dedupeKey: { t: "string", min: 1, max: 512 },
    budgets: {
      t: "custom",
      schema: toJsonSchema(BUDGET_NODE),
      check: (value, budgetPath) => validateBudgets(value, DEFAULT_BUDGETS, budgetPath),
    },
    steps: { t: "array", min: 0, max: REQUEST_LIMITS.exactCalls, of: STEP_RECORD_NODE },
    pendingDecisions: { t: "array", min: 0, max: 16, of: PENDING_DECISION_NODE },
    unknownOutcomes: { t: "array", min: 0, max: REQUEST_LIMITS.exactCalls, of: UNKNOWN_OUTCOME_NODE },
    usage: {
      t: "object",
      fields: {
        steps: { t: "int", min: 0, max: 10_000 },
        selectionRounds: { t: "int", min: 0, max: 1_000 },
        generationCalls: { t: "int", min: 0, max: 1_000 },
        attempts: { t: "int", min: 0, max: 10_000 },
      },
    },
    lastSequence: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER },
    result: { ...COMPACT_RESULT_NODE, optional: true },
  },
};

/**
 * Terminal states must be truthful. None of them may retain unknown effects or work
 * still owed; `completed` holds only successful/skipped steps with at least one
 * success; `partial` reports genuinely mixed settled results.
 */
function terminalSnapshotIssues(snapshot: OperationSnapshot, path: string): ValidationIssue[] {
  if (!isTerminalOperationState(snapshot.state)) return [];
  const issues: ValidationIssue[] = [];
  if (snapshot.unknownOutcomes.length) {
    issues.push(issue(field(path, "unknownOutcomes"), "invalid_transition", `${snapshot.state} cannot retain unknown outcomes`));
  }
  if (snapshot.pendingDecisions.length) {
    issues.push(issue(field(path, "pendingDecisions"), "invalid_transition", `${snapshot.state} cannot carry pending decisions`));
  }
  snapshot.steps.forEach((step, index) => {
    if ((PENDING_STEP_STATES as readonly string[]).includes(step.state)) {
      issues.push(issue(item(field(path, "steps"), index), "invalid_transition", `${snapshot.state} cannot carry a "${step.state}" step`));
    }
  });
  const states = snapshot.steps.map((step) => step.state);
  if (snapshot.state === "completed") {
    if (!states.includes("succeeded")) {
      issues.push(issue(field(path, "steps"), "invalid_transition", "completed needs at least one succeeded step"));
    }
    if (states.some((state) => state === "failed" || state === "cancelled")) {
      issues.push(issue(field(path, "steps"), "invalid_transition", "completed cannot carry failed or cancelled steps"));
    }
  }
  if (snapshot.state === "partial") {
    if (!states.includes("succeeded")) {
      issues.push(issue(field(path, "steps"), "invalid_transition", "partial keeps completed effects, so it needs a succeeded step"));
    }
    if (!states.some((state) => state === "failed" || state === "skipped" || state === "cancelled")) {
      issues.push(issue(field(path, "steps"), "invalid_transition", "partial needs a settled failed, skipped, or cancelled step"));
    }
  }
  return issues;
}

export function validateOperationSnapshot(value: unknown, path = "$"): Validation<OperationSnapshot> {
  const result = runValidation<OperationSnapshot>(SNAPSHOT_NODE, value, path, VALIDATION_OPTIONS);
  if (!result.ok) return result;
  const issues = terminalSnapshotIssues(result.value, path);
  return issues.length ? { ok: false, issues } : result;
}

// ---------------------------------------------------------------------------
// Capability descriptors, provenance
// ---------------------------------------------------------------------------

export type CapabilityEffect = "read" | "write" | "destroy";
export type CapabilityScope = "bench" | "workspace" | "platform" | "environment" | "code";

/** Internal value states; omission is never silently promoted to clear (design §6). */
export type InternalValueState =
  | { kind: "known"; value: JsonValue }
  | { kind: "unspecified" }
  | { kind: "explicitly_clear" }
  | { kind: "ambiguous"; candidates?: string[] }
  | { kind: "unsupported"; reason?: string };

export type ArgumentProvenance =
  | "user_value"
  | "runtime_fact"
  | "resource_candidate"
  | "enum"
  | "set"
  | "generated_content"
  | "default";

export const PROVENANCE_SOURCES: readonly ArgumentProvenance[] = [
  "user_value",
  "runtime_fact",
  "resource_candidate",
  "enum",
  "set",
  "generated_content",
  "default",
];

export type ArgumentPresence = "required" | "optional" | "defaulted";
export type ClearSemantics = "unsupported" | "clears_when_null" | "clears_when_empty" | "explicit_flag";

export type CapabilityArgument = {
  name: string;
  presence: ArgumentPresence;
  schema: JsonSchemaLike;
  provenance: ArgumentProvenance[];
  clear: ClearSemantics;
  defaultJson?: JsonValue;
  description?: string;
};

export type CapabilityApproval = {
  required: "none" | "user" | "policy";
  payloadDigestRequired: boolean;
  /** Fields every recorded decision binds; O05/O08 must persist all of them. */
  binds: readonly string[];
};

export const APPROVAL_BINDINGS = [
  "actor",
  "tenant",
  "session",
  "operation",
  "step",
  "payloadDigest",
  "revision",
  "policySource",
  "expiry",
] as const;

export type CapabilityRetry = {
  class: "none" | "idempotent" | "reconcile_required";
  maxAttempts: number;
  reconciliation: "none" | "backend_operation_id" | "preconditions";
};

export type CapabilityResourceAccess = {
  reads: string[];
  writes: string[];
  /** Lanes that must never overlap, e.g. "workspace.packages". */
  conflictKeys: string[];
  /** Unknown footprint (an unclassified command) takes one exclusive lane. */
  exclusive: boolean;
};

export type CapabilityEvidence = {
  /** Evidence required before a step may report success. */
  success: string[];
  /** Evidence used to reconcile an unknown outcome instead of retrying. */
  unknownOutcome: string[];
};

export type CapabilityDescriptor = {
  capability: string;
  version: string;
  title: string;
  summary: string;
  /** Short instruction guide returned by `describe` without `detail: "schema"`. */
  guide: string;
  effect: CapabilityEffect;
  scope: CapabilityScope;
  group?: string;
  inputSchema: JsonSchemaLike;
  outputSchema: JsonSchemaLike;
  arguments: CapabilityArgument[];
  rules: string[];
  limits: Record<string, number>;
  examples: Array<{ instruction: string; args?: Record<string, JsonValue> }>;
  errors: OperationErrorCode[];
  retry: CapabilityRetry;
  approval: CapabilityApproval;
  evidence: CapabilityEvidence;
  resourceAccess: CapabilityResourceAccess;
  contractVersion: string;
};

const SCHEMA_NODE: Node = {
  t: "custom",
  schema: { type: "object" },
  check: (value, schemaPath) => validateJsonSchemaLike(value, schemaPath),
};

const ARGUMENT_NODE: Node = {
  t: "object",
  fields: {
    name: { t: "string", min: 1, max: 64, pattern: INPUT_KEY_RE, hint: "must be a simple argument name" },
    presence: { t: "enum", values: ["required", "optional", "defaulted"] },
    schema: SCHEMA_NODE,
    provenance: {
      t: "array",
      min: 1,
      max: PROVENANCE_SOURCES.length,
      of: { t: "enum", values: PROVENANCE_SOURCES },
    },
    clear: { t: "enum", values: ["unsupported", "clears_when_null", "clears_when_empty", "explicit_flag"] },
    defaultJson: { ...json, optional: true },
    description: { t: "string", min: 1, max: 500, optional: true },
  },
};

export function validateCapabilityArgument(value: unknown, path = "$"): Validation<CapabilityArgument> {
  const result = runValidation<CapabilityArgument>(ARGUMENT_NODE, value, path, VALIDATION_OPTIONS);
  if (!result.ok) return result;
  if (result.value.presence === "defaulted" && result.value.defaultJson === undefined) {
    return { ok: false, issues: [issue(field(path, "defaultJson"), "missing_field", "a defaulted argument must declare defaultJson")] };
  }
  return result;
}

const DESCRIPTOR_NODE: Node = {
  t: "object",
  fields: {
    capability: capabilityName,
    version: pinnedVersion,
    title: { t: "string", min: 1, max: 120 },
    summary: { t: "string", min: 1, max: 240 },
    guide: { t: "string", min: 1, max: 2_000 },
    effect: { t: "enum", values: ["read", "write", "destroy"] },
    scope: { t: "enum", values: ["bench", "workspace", "platform", "environment", "code"] },
    group: { t: "string", min: 1, max: 64, optional: true },
    inputSchema: SCHEMA_NODE,
    outputSchema: SCHEMA_NODE,
    arguments: { t: "array", min: 0, max: 64, of: ARGUMENT_NODE },
    rules: { t: "stringList", min: 0, max: 32, itemMax: 500 },
    limits: { t: "dict", minKeys: 0, maxKeys: 32, of: { t: "int", min: 0, max: Number.MAX_SAFE_INTEGER }, keyPattern: INPUT_KEY_RE },
    examples: {
      t: "array",
      min: 0,
      max: 8,
      of: {
        t: "object",
        fields: {
          instruction: { t: "string", min: 1, max: REQUEST_LIMITS.instructionChars },
          args: { t: "dict", minKeys: 0, maxKeys: REQUEST_LIMITS.argsPerCall, of: json, keyPattern: INPUT_KEY_RE, optional: true },
        },
      },
    },
    errors: { t: "array", min: 1, max: 32, of: { t: "enum", values: OPERATION_ERROR_CODES } },
    retry: {
      t: "object",
      fields: {
        class: { t: "enum", values: ["none", "idempotent", "reconcile_required"] },
        maxAttempts: { t: "int", min: 1, max: 10 },
        reconciliation: { t: "enum", values: ["none", "backend_operation_id", "preconditions"] },
      },
    },
    approval: {
      t: "object",
      fields: {
        required: { t: "enum", values: ["none", "user", "policy"] },
        payloadDigestRequired: { t: "bool" },
        binds: {
          t: "array",
          min: APPROVAL_BINDINGS.length,
          max: APPROVAL_BINDINGS.length,
          of: { t: "enum", values: APPROVAL_BINDINGS },
        },
      },
    },
    evidence: {
      t: "object",
      fields: {
        success: { t: "stringList", min: 1, max: 16, itemMax: 128 },
        unknownOutcome: { t: "stringList", min: 0, max: 16, itemMax: 128 },
      },
    },
    resourceAccess: {
      t: "object",
      fields: {
        reads: { t: "stringList", min: 0, max: 16, itemMax: 128 },
        writes: { t: "stringList", min: 0, max: 16, itemMax: 128 },
        conflictKeys: { t: "stringList", min: 0, max: 16, itemMax: 128 },
        exclusive: { t: "bool" },
      },
    },
    contractVersion: { t: "literal", value: CONTRACT_VERSION },
  },
};

export function validateCapabilityDescriptor(value: unknown, path = "$"): Validation<CapabilityDescriptor> {
  const result = runValidation<CapabilityDescriptor>(DESCRIPTOR_NODE, value, path, VALIDATION_OPTIONS);
  if (!result.ok) return result;
  const descriptor = result.value;
  const issues: ValidationIssue[] = [];
  const names = new Set<string>();
  descriptor.arguments.forEach((argument, index) => {
    if (names.has(argument.name)) {
      issues.push(issue(item(field(path, "arguments"), index), "duplicate_key", `duplicate argument "${argument.name}"`));
    }
    names.add(argument.name);
  });
  const bindingSet = new Set(descriptor.approval.binds);
  for (const binding of APPROVAL_BINDINGS) {
    if (!bindingSet.has(binding)) {
      issues.push(issue(field(field(path, "approval"), "binds"), "missing_field", `binds must include "${binding}"`));
    }
  }
  if (descriptor.effect !== "read" && !descriptor.approval.payloadDigestRequired) {
    issues.push(issue(field(path, "approval"), "bad_syntax", "a mutating capability must bind approval to the payload digest"));
  }
  if (descriptor.effect !== "read" && descriptor.retry.class !== "none" && descriptor.retry.reconciliation === "none") {
    issues.push(issue(field(path, "retry"), "bad_syntax", "a retryable mutation must declare how its outcome is reconciled"));
  }
  return issues.length ? { ok: false, issues } : result;
}

export type CapabilityDescription = {
  capability: string;
  version: string;
  title: string;
  effect: CapabilityEffect;
  guide: string;
  rules?: string[];
  limits?: Record<string, number>;
  inputSchema?: JsonSchemaLike;
};

/** Deterministic registry lookup; never activates a tool and never calls a model. */
export function describeCapability(
  descriptor: CapabilityDescriptor,
  detail: "guide" | "schema" = "guide",
): CapabilityDescription {
  const description: CapabilityDescription = {
    capability: descriptor.capability,
    version: descriptor.version,
    title: descriptor.title,
    effect: descriptor.effect,
    guide: descriptor.guide,
  };
  if (detail === "schema") {
    description.rules = [...descriptor.rules];
    description.limits = { ...descriptor.limits };
    description.inputSchema = descriptor.inputSchema;
  }
  return description;
}

export const DESCRIPTION_PAGE_SIZE = 20;

/** Bounded permitted index when `describe` names no capability. */
export function buildDescriptionIndex(
  descriptors: readonly CapabilityDescriptor[],
  cursor?: string,
): { entries: CapabilityDescription[]; nextCursor?: string } {
  const sorted = [...descriptors].sort((a, b) => (a.capability < b.capability ? -1 : a.capability > b.capability ? 1 : 0));
  const start = cursor ? sorted.findIndex((descriptor) => descriptor.capability > cursor) : 0;
  const from = start < 0 ? sorted.length : start;
  const page = sorted.slice(from, from + DESCRIPTION_PAGE_SIZE);
  const result: { entries: CapabilityDescription[]; nextCursor?: string } = {
    entries: page.map((descriptor) => describeCapability(descriptor, "guide")),
  };
  if (page.length && from + page.length < sorted.length) result.nextCursor = page[page.length - 1].capability;
  return result;
}

/** Descriptors are frozen: later tasks read them, none mutate them in place. */
export function freezeCapabilityDescriptor<T extends CapabilityDescriptor>(descriptor: T): T {
  const deepFreeze = (value: unknown): void => {
    if (value === null || typeof value !== "object" || Object.isFrozen(value)) return;
    for (const child of Object.values(value)) deepFreeze(child);
    Object.freeze(value);
  };
  deepFreeze(descriptor);
  return descriptor;
}

// ---------------------------------------------------------------------------
// Judgment and generation adapters (interfaces only; O03/O04 implement them)
// ---------------------------------------------------------------------------

export type JudgmentModel = { provider: "typesafe"; model: string; version: string };

export type ChoiceJudgmentRequest = {
  question: string;
  questionVersion: string;
  options: Array<{ id: string; label: string }>;
  context?: string;
  timeoutMs?: number;
  allowAbstain?: boolean;
};

export type ScoreJudgmentRequest = {
  question: string;
  questionVersion: string;
  rubric: string[];
  context?: string;
  timeoutMs?: number;
};

export type NoulJudgmentRequest = { question: string; questionVersion: string; context?: string; timeoutMs?: number };

export type ChoiceJudgmentResult =
  | { outcome: "selected"; optionId: string; confidence: number }
  | { outcome: "ambiguous" }
  | { outcome: "no_match" }
  | { outcome: "provider_failure"; retryable: boolean; message: string };

export type ScoreJudgmentResult =
  | { outcome: "scored"; scores: Array<{ criterion: string; score: number }>; modelVersion: string }
  | { outcome: "ambiguous" }
  | { outcome: "provider_failure"; retryable: boolean; message: string };

export type NoulJudgmentResult =
  | { outcome: "yes"; modelVersion: string }
  | { outcome: "no"; modelVersion: string }
  | { outcome: "unknown" }
  | { outcome: "provider_failure"; retryable: boolean; message: string };

/** A judgment selects or scores; it never grants approval and never expands scope. */
export interface JudgmentAdapter {
  readonly model: JudgmentModel;
  choice(request: ChoiceJudgmentRequest, signal?: AbortSignal): Promise<ChoiceJudgmentResult>;
  score(request: ScoreJudgmentRequest, signal?: AbortSignal): Promise<ScoreJudgmentResult>;
  noul(request: NoulJudgmentRequest, signal?: AbortSignal): Promise<NoulJudgmentResult>;
}

export type GenerationRole = "edit_content" | "command_draft" | "prose" | "query";
export type GenerationModel = { provider: "deepseek"; model: "flash"; version: string };

/**
 * Provider input must already be classified as eligible by trusted policy: read access
 * alone is not enough, and a model can never classify its own input.
 */
export type GenerationInput = { ref: ContextRef; digest: string; classification: "provider_eligible" };

export type GenerationRequest = {
  role: GenerationRole;
  instruction: string;
  constraints?: string[];
  inputRefs: GenerationInput[];
  outputSchema: JsonSchemaLike;
  maxOutputBytes: number;
  maxTokens: number;
  timeoutMs: number;
};

export type GenerationResult =
  | { outcome: "proposed"; content: JsonValue; model: GenerationModel; usage: { inputTokens: number; outputTokens: number } }
  | { outcome: "unsupported"; reason: string }
  | { outcome: "invalid_output"; issues: string[] }
  | { outcome: "provider_failure"; retryable: boolean; message: string };

/** Generation proposes content only: no tools, no credentials, no recursive `operate`. */
export interface GenerationAdapter {
  readonly model: GenerationModel;
  generate(request: GenerationRequest, signal?: AbortSignal): Promise<GenerationResult>;
}

// ---------------------------------------------------------------------------
// Canonical identity (O05)
// ---------------------------------------------------------------------------

/** Deterministic JSON with sorted keys; string contents are preserved exactly. */
export function stableStringify(value: JsonValue): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  const keys = Object.keys(value).sort();
  return `{${keys.map((key) => `${JSON.stringify(key)}:${stableStringify(value[key])}`).join(",")}}`;
}

/** sha256 over the canonical form; key order and formatting never change the digest. */
export function canonicalDigest(value: JsonValue): string {
  return `sha256:${createHash("sha256").update(stableStringify(value), "utf8").digest("hex")}`;
}

/** Digest of a validated request; O05 stores it beside the deduplication key. */
export function requestDigest(request: OperateRequest): string {
  return canonicalDigest(request as unknown as JsonValue);
}

// ---------------------------------------------------------------------------
// The model-facing schema, emitted from the same descriptions
// ---------------------------------------------------------------------------

export const OPERATE_REQUEST_SCHEMA: JsonSchemaLike = withDefinitions({ oneOf: REQUEST_FORMS.map(toJsonSchema) });
