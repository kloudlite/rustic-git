/**
 * The reviewed capability contracts for the operation executor, and the one
 * dispatch that reaches a handler through policy.
 *
 * The registry is data: every entry derives its published `inputSchema` from the
 * same shape `validateCapabilityArgs` runs, so the schema a person or model reads
 * cannot drift from the validator. `describe` is a deterministic lookup over
 * frozen descriptors — no model call, no tool activation, no operation.
 *
 * The bench pilot enables READS only (see `INITIAL_READ_CAPABILITIES`). Contracts
 * for the mutations O02 had to pin down (creation sources, in-place restore,
 * intercept clear/default, collection preservation, process actions) are present
 * so their semantics are reviewable and testable, but dispatch refuses them until
 * a later, explicitly gated stage enables each one.
 */
import {
  APPROVAL_BINDINGS,
  CONTRACT_VERSION,
  buildDescriptionIndex,
  canonicalDigest,
  describeCapability,
  freezeCapabilityDescriptor,
  validateCapabilityDescriptor,
  validateJsonSchemaLike,
  validateJsonValue,
} from "./contracts.ts";
import type {
  CapabilityApproval,
  CapabilityDescription,
  CapabilityDescriptor,
  CapabilityEffect,
  CapabilityRetry,
  DescribeRequest,
  JsonSchemaLike,
  OperationErrorCode,
  RecordedDecision,
  ResumeExpectation,
} from "./contracts.ts";
import { DispatchAuthority, type DispatchToken } from "./dispatch-authority.ts";
import { toJsonSchema } from "./shape.ts";
import type { Fields, JsonValue, Node, ValidationIssue } from "./shape.ts";
import {
  PROCESS_CONTROL_ACTIONS,
  PROCESS_READ_ACTIONS,
  WORKSPACE_CREATE_CHECKS,
  argumentDeclarationIssues,
  processActionIssues,
  requiresField,
  validateCapabilityArgs,
} from "./arguments.ts";
import type { ArgsCheck, ArgumentStates } from "./arguments.ts";
import { NOT_A_WORKSPACE, SKILLS, dispatchWithPolicy, isOwnBench } from "../../../pi/kloudlite.ts";
import type { ApprovalRequirement } from "../../../pi/kloudlite.ts";
import { question } from "../../../pi/catalog.ts";
import type { Adapter, AdapterInput, AdapterResult } from "./adapters.ts";

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

export type CapabilityValue = JsonValue;
export type CapabilityError = { code: OperationErrorCode; message: string; retryable: boolean };
export type CapabilityAdapterResult = AdapterResult;
export type CapabilityAdapterInput = AdapterInput;
export type CapabilityAdapter = Adapter;

const RUNTIME = Symbol.for("kloudlite.operations.capability-runtime");
export type CapabilityRuntime = { readonly [RUNTIME]: true; readonly resolve: (capability: string) => CapabilityAdapter | undefined };

export type BenchProcessSource = { all(): Array<{ id: string; session: string; workspace?: string; name: string; command: string; started: number; ended?: number; code?: number | null; lost?: true }> };
export function createBenchCapabilityRuntime(procs: BenchProcessSource, adapters: Readonly<Record<string, CapabilityAdapter>>): CapabilityRuntime {
  const benchProcessList: CapabilityAdapter = async ({ args }) => ({ ok: true, value: procs.all()
    .filter((row) => (args.includeEnded === true || row.ended === undefined) && (args.workspace === undefined || row.workspace === args.workspace))
    .sort((a, b) => b.started - a.started || a.id.localeCompare(b.id))
    .slice(0, 200)
    .map(({ id, workspace, name, command, started, ended, code, lost }) => ({ id, ...(workspace ? { workspace } : {}), name, command, started, ...(ended === undefined ? {} : { ended }), ...(code === undefined ? {} : { code }), ...(lost ? { lost } : {}) })) });
  const registered = new Map<string, CapabilityAdapter>(Object.entries({
      ...adapters,
      "bench.process.list": benchProcessList,
  }));
  return Object.freeze({ [RUNTIME]: true as const, resolve: (capability: string) => registered.get(capability) });
}

export type CapabilityApprovalRequest = {
  capability: string;
  version: string;
  effect: CapabilityEffect;
  /** The line the person reads; the bridge renders it and records the bindings. */
  prompt: string;
  args: Record<string, JsonValue>;
  payloadDigest: string;
  expectation: ResumeExpectation;
};

export type CapabilityDispatchDeps = {
  runtime?: CapabilityRuntime;
  /** The executor owns obtaining and persisting this record. */
  approve?: (request: CapabilityApprovalRequest) => Promise<RecordedDecision>;
  decision?: Omit<ResumeExpectation, "payloadDigest" | "now"> & { now?: number };
  /** O05-issued, one-shot authorization for this exact dispatch attempt. */
  dispatchToken?: DispatchToken;
  dispatchOperationId?: string;
  dispatchStepId?: string;
  dispatchAttempt?: number;
  /** Trusted dispatch policy, never model input. An unlisted source cannot authorize this call. */
  allowedPolicySources?: readonly RecordedDecision["policySource"][];
  signal?: AbortSignal;
};

export type CapabilityDefinition = {
  descriptor: CapabilityDescriptor;
  shape: Node;
  checks: readonly ArgsCheck[];
  /** Registered tool/boundary name this capability dispatches to. */
  target?: string;
  /** Approval wording for a capability the catalogue does not already describe. */
  ask?: (args: Record<string, JsonValue>) => string;
  /** A refusal that belongs to the capability's scope, decided before approval and before the handler. */
  scopeCheck?: (args: Record<string, JsonValue>) => { code: OperationErrorCode; reason: string } | undefined;
  /** Why this contract is not in the pilot allowlist yet. */
  disabled?: string;
};

type CapabilityInput = Omit<CapabilityDescriptor, "inputSchema" | "contractVersion"> & {
  shape: Node;
  checks?: readonly ArgsCheck[];
  target?: string;
  ask?: CapabilityDefinition["ask"];
  scopeCheck?: CapabilityDefinition["scopeCheck"];
  disabled?: string;
};

const str = (hint: string, max = 256): Node => ({ t: "string", min: 1, max, hint });
const nullValue: Node = {
  t: "custom",
  schema: { type: "null" },
  check: (value, path) => value === null ? { ok: true, value } : { ok: false, issues: [{ path, code: "wrong_type", message: "expected null" }] },
};
const optional = <T extends Node>(node: T): T & { optional: true } => Object.assign({}, node, { optional: true as const });
const argsObject = (fields: Fields): Node => ({ t: "object", fields });
const stringArray = (min: number, max: number, hint: string, itemMax = 128): Node => ({ t: "array", min, max, of: str(hint, itemMax) });

const openDocument = (description: string): JsonSchemaLike => ({ type: "object", description, additionalProperties: true });
const rows = (description: string, properties: Record<string, JsonSchemaLike>): JsonSchemaLike => ({
  type: "array", description, items: { type: "object", additionalProperties: true, properties, required: Object.keys(properties) },
});
const textOutput = (description: string): JsonSchemaLike => ({ type: "string", description });

const cloneJson = <T extends JsonValue>(value: T): T => JSON.parse(JSON.stringify(value)) as T;
const cloneStates = (states: ArgumentStates): ArgumentStates => JSON.parse(JSON.stringify(states)) as ArgumentStates;

/** Validates a descriptor against one executable shape and freezes both into a definition. */
export function defineCapability(input: CapabilityInput): CapabilityDefinition {
  const { shape, checks, target, ask, scopeCheck, disabled, ...fields } = input;
  const descriptor = freezeCapabilityDescriptor({
    ...fields,
    inputSchema: toJsonSchema(shape),
    contractVersion: CONTRACT_VERSION,
  });
  const issues: ValidationIssue[] = [...argumentDeclarationIssues(shape, descriptor.arguments)];
  const described = validateCapabilityDescriptor(descriptor);
  if (!described.ok) issues.push(...described.issues);
  if (issues.length) throw new Error(`${descriptor.capability}: ${issues.map((issue) => `${issue.path} ${issue.message}`).join("; ")}`);
  return { descriptor, shape, checks: checks ?? [], target, ask, scopeCheck, disabled };
}

/**
 * The line a person reads before a changing capability runs. It comes from the
 * catalogue entry for the same registered tool, so the card and the contract
 * cannot describe different actions.
 */
export function approvalPrompt(definition: CapabilityDefinition, args: Record<string, JsonValue>): string | undefined {
  if (definition.descriptor.approval.required === "none") return undefined;
  if (definition.ask) return definition.ask(args);
  return definition.target ? question(definition.target, args as Record<string, any>) : undefined;
}

const USER_APPROVAL: CapabilityApproval = { required: "user", payloadDigestRequired: true, binds: APPROVAL_BINDINGS };
const NO_APPROVAL: CapabilityApproval = { required: "none", payloadDigestRequired: false, binds: APPROVAL_BINDINGS };
const IDEMPOTENT: CapabilityRetry = { class: "idempotent", maxAttempts: 2, reconciliation: "none" };
/** A create or a whole-collection write is not retried blind: an unknown outcome is reconciled first. */
const RECONCILE_FIRST: CapabilityRetry = { class: "reconcile_required", maxAttempts: 1, reconciliation: "preconditions" };

const SERVICE: Node = {
  t: "object",
  fields: {
    name: str("service name in the environment", 64),
    image: str("container image", 256),
    command: optional(stringArray(0, 64, "one argv element", 256)),
    env: optional({ t: "dict", minKeys: 0, maxKeys: 64, of: str("environment value", 512), keyPattern: /^[A-Za-z_][A-Za-z0-9_]*$/ }),
    mounts: optional({
      t: "array",
      min: 0,
      max: 32,
      of: { t: "object", fields: { folder: str("one safe segment of the environment's volume", 128), path: str("where it is mounted in the container", 256) } },
    }),
    ports: optional({ t: "array", min: 0, max: 32, of: { t: "int", min: 0, max: 65_535 } }),
  },
};

// ---------------------------------------------------------------------------
// The contracts
// ---------------------------------------------------------------------------

const benchProcessList = defineCapability({
  capability: "bench.process.list",
  version: "1.0.0",
  title: "Bench process metadata",
  summary: "Read the bench's own process table: id, workspace, command, start, and exit code.",
  guide: "Reads what this bench recorded about processes it started. includeEnded keeps rows that have exited; workspace narrows the rows a workspace published. It never reaches a workspace tool server, so workspace logs, files and commands stay outside the bench pilot.",
  outputSchema: {
    type: "array",
    description: "recorded processes",
    items: {
      type: "object",
      additionalProperties: true,
      required: ["id", "command", "started"],
      properties: { id: { type: "string" }, workspace: { type: "string" }, command: { type: "string" }, started: { type: "integer" }, ended: { type: "integer" }, code: { oneOf: [{ type: "integer" }, { type: "null" }] } },
    },
  },
  effect: "read",
  scope: "bench",
  group: "bench",
  shape: argsObject({ includeEnded: optional({ t: "bool" }), workspace: optional(str("only rows published by this workspace name", 128)) }),
  arguments: [
    {
      name: "includeEnded",
      presence: "defaulted",
      schema: { type: "boolean", description: "keep rows that have exited; default false" },
      provenance: ["default"],
      clear: "unsupported",
      defaultJson: false,
    },
    {
      name: "workspace",
      presence: "optional",
      schema: { type: "string", description: "only rows published by this workspace name" },
      provenance: ["user_value", "resource_candidate"],
      clear: "unsupported",
    },
  ],
  rules: [
    "reads only rows the bench recorded; it never starts a process and never calls a workspace tool server",
    "a workspace filter narrows the rows that were published; it grants no access to that workspace",
    "a lost row stays lost, and an exit code is reported only when the bench observed it",
  ],
  limits: { maxRows: 200 },
  examples: [{ instruction: "What is still running in this bench?" }],
  errors: ["scope_denied", "execution_failure"],
  retry: IDEMPOTENT,
  approval: NO_APPROVAL,
  evidence: { success: ["bench.process.rows"], unknownOutcome: [] },
  resourceAccess: { reads: ["bench.process.metadata"], writes: [], conflictKeys: [], exclusive: false },
  target: "bench.process.list",
});

const workspaceList = defineCapability({
  capability: "workspace.list",
  version: "1.0.0",
  title: "List visible workspaces",
  summary: "Read the workspaces the signed-in person can see, optionally narrowed to one team.",
  guide: "Lists workspaces by name and id. Pass team to narrow the listing to a team slug. The listing is the person's own permitted set: benches never appear in it, and a team slug never widens access.",
  outputSchema: rows("visible workspaces", { id: { type: "string" }, name: { type: "string" }, state: { type: "string" } }),
  effect: "read",
  scope: "platform",
  group: "workspace",
  shape: argsObject({ team: optional(str("team slug; absent lists everything visible", 128)) }),
  arguments: [
    {
      name: "team",
      presence: "optional",
      schema: { type: "string", description: "team slug; absent lists everything visible" },
      provenance: ["user_value", "resource_candidate"],
      clear: "unsupported",
    },
  ],
  rules: ["the listing is the caller's own permitted set; benches are filtered out of it", "a team slug narrows the listing and never widens access"],
  limits: {},
  examples: [{ instruction: "Which workspaces do I have?" }],
  errors: ["permission_denied", "scope_denied", "execution_failure"],
  retry: IDEMPOTENT,
  approval: NO_APPROVAL,
  evidence: { success: ["workspace.rows"], unknownOutcome: [] },
  resourceAccess: { reads: ["platform.workspaces"], writes: [], conflictKeys: [], exclusive: false },
  target: "kl_workspaces",
});

const workspaceInspect = defineCapability({
  capability: "workspace.inspect",
  version: "1.0.0",
  title: "Inspect one workspace",
  summary: "Read one workspace in full: state, node, packages, and its space's environment.",
  guide: "Give a workspace id or its unique name. The answer is the workspace's own document; the bench's own id and bench-shaped ids are refused before the platform is called, and a name two workspaces answer to is reported as an ambiguity rather than guessed.",
  outputSchema: { ...openDocument("the workspace's own document"), required: ["id"], properties: { id: { type: "string" }, name: { type: "string" }, state: { type: "string" }, packages: { type: "array", items: { type: "string" } } } },
  effect: "read",
  scope: "platform",
  group: "workspace",
  shape: argsObject({ id: str("workspace id or its unique name", 256) }),
  arguments: [
    {
      name: "id",
      presence: "required",
      schema: { type: "string", description: "workspace id or its unique name" },
      provenance: ["user_value", "resource_candidate"],
      clear: "unsupported",
    },
  ],
  rules: [
    "id is a workspace id or a unique name; two workspaces with one name is an ambiguity, never a guess",
    "the bench's own id and bench-shaped ids are refused before the platform is called",
  ],
  limits: {},
  examples: [{ instruction: "Show me workspace api.", args: { id: "api" } }],
  errors: ["no_match", "ambiguous_match", "permission_denied", "scope_denied", "execution_failure"],
  retry: IDEMPOTENT,
  approval: NO_APPROVAL,
  evidence: { success: ["workspace.document"], unknownOutcome: [] },
  resourceAccess: { reads: ["platform.workspace"], writes: [], conflictKeys: [], exclusive: false },
  target: "kl_workspace",
  scopeCheck: (args) => (typeof args.id === "string" && isOwnBench(args.id) ? { code: "scope_denied", reason: NOT_A_WORKSPACE } : undefined),
});

const workspaceProgress = defineCapability({
  capability: "workspace.progress",
  version: "1.0.0",
  title: "Workspace progress",
  summary: "Read what a workspace's own session has been asked, said, and is running.",
  guide: "Give a workspace by name or id. The bench answers from its own exchange, message, and process tables — it never asks the workspace's session to act and never calls its tool server. Outstanding asks are reported once, with the note that a reply arrives on its own.",
  outputSchema: { ...openDocument("workspace progress"), required: ["asks", "processes", "messages"], properties: { asks: { type: "array", items: { type: "object" } }, processes: { type: "array", items: { type: "object" } }, messages: { type: "array", items: { type: "object" } } } },
  effect: "read",
  scope: "bench",
  group: "workspace",
  shape: argsObject({ id: str("workspace id or name", 256) }),
  arguments: [
    {
      name: "id",
      presence: "required",
      schema: { type: "string", description: "workspace id or name" },
      provenance: ["user_value", "resource_candidate"],
      clear: "unsupported",
    },
  ],
  rules: [
    "reads the bench's recorded exchanges, recent messages and process rows for that workspace",
    "it never asks the workspace's session to do anything and never spawns an agent",
    "the workspace may be named or given by id; the bench's own id is refused",
  ],
  limits: { maxMessages: 10 },
  examples: [{ instruction: "How is the api workspace getting on?", args: { id: "api" } }],
  errors: ["no_match", "ambiguous_match", "scope_denied", "execution_failure"],
  retry: IDEMPOTENT,
  approval: NO_APPROVAL,
  evidence: { success: ["workspace.progress.exchanges", "workspace.progress.processes"], unknownOutcome: [] },
  resourceAccess: { reads: ["bench.exchanges", "bench.messages", "bench.process.metadata"], writes: [], conflictKeys: [], exclusive: false },
  target: "kl_workspace_progress",
  scopeCheck: (args) => (typeof args.id === "string" && isOwnBench(args.id) ? { code: "scope_denied", reason: NOT_A_WORKSPACE } : undefined),
});

const skillRead = defineCapability({
  capability: "skill.read",
  version: "1.0.0",
  title: "Read a skill",
  summary: "List the installed skills, or read one skill's own text.",
  guide: "With no name, lists the installed skills and what each is for. With a name, returns that skill's text. This is a deterministic read of files installed beside the extension: it never activates a tool, changes the active tool set, or calls a model.",
  outputSchema: { oneOf: [textOutput("the skill's own text"), { type: "array", items: { type: "object", required: ["name", "description"], properties: { name: { type: "string" }, description: { type: "string" } }, additionalProperties: false }, description: "installed skills and their descriptions" }] },
  effect: "read",
  scope: "bench",
  group: "platform",
  shape: argsObject({ name: optional({ t: "enum", values: SKILLS }) }),
  arguments: [
    {
      name: "name",
      presence: "optional",
      schema: { type: "string", enum: [...SKILLS], description: "an installed skill; absent lists them" },
      provenance: ["user_value", "resource_candidate"],
      clear: "unsupported",
    },
  ],
  rules: [
    "no name lists the installed skills; a name returns that skill's own text",
    "read-only: it never activates a tool, never changes a session's active tools, and never calls a model",
    "names are the canonical ones; the registered `skill` tool additionally normalizes case and a trailing .md",
  ],
  limits: {},
  examples: [{ instruction: "What does the environments skill say?", args: { name: "environments" } }],
  errors: ["invalid_args", "execution_failure"],
  retry: IDEMPOTENT,
  approval: NO_APPROVAL,
  evidence: { success: ["skill.text"], unknownOutcome: [] },
  resourceAccess: { reads: ["bench.skills"], writes: [], conflictKeys: [], exclusive: false },
  target: "skill",
});

const workspaceCreate = defineCapability({
  capability: "workspace.create",
  version: "1.0.0",
  title: "Create a workspace",
  summary: "Create a workspace empty, from a repository and branch, or from a snapshot.",
  guide: "Name the workspace. Add repo and branch to start from a repository, or from_snapshot to restore one of this bench's snapshots into a new workspace — never both in one call, because the snapshot would silently win and the repository would never be cloned. packages are nixpkgs attributes installed at creation.",
  outputSchema: openDocument("the created workspace's document"),
  effect: "write",
  scope: "workspace",
  group: "workspace",
   shape: argsObject({
    name: str("workspace name", 64),
    repo: optional(str("repository as owner/name", 256)),
    branch: optional(str("branch to check out", 128)),
    packages: optional(stringArray(1, 64, "nixpkgs attribute, optionally attr@version", 128)),
    from_snapshot: optional(str("snapshot id to restore into a new workspace", 256)),
  }),
  arguments: [
    { name: "name", presence: "required", schema: { type: "string" }, provenance: ["user_value"], clear: "unsupported" },
    { name: "repo", presence: "optional", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    { name: "branch", presence: "optional", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    { name: "packages", presence: "optional", schema: { type: "array", items: { type: "string" } }, provenance: ["user_value", "set"], clear: "unsupported" },
    { name: "from_snapshot", presence: "optional", schema: { type: "string" }, provenance: ["resource_candidate"], clear: "unsupported" },
  ],
  checks: WORKSPACE_CREATE_CHECKS,
  rules: [
    "a workspace is made one way in one call: empty, from repo+branch, or from a snapshot",
    "from_snapshot restores into a NEW workspace; putting an existing environment back is environment.restore, which is in place",
    "packages are nixpkgs attributes installed at creation; unrelated packages on other workspaces are untouched",
    "a create is not retried automatically: an unknown outcome is reconciled from the listing before anything else runs",
  ],
  limits: { maxPackages: 64 },
  examples: [{ instruction: "Make a workspace called backend-debug from kloudlite/rustic-git on main." }],
  errors: ["invalid_args", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["workspace.id", "workspace.state"], unknownOutcome: ["workspace.listing", "workspace.name"] },
  resourceAccess: { reads: ["platform.workspaces"], writes: ["platform.workspaces"], conflictKeys: ["platform.workspaces"], exclusive: false },
  target: "kl_workspace_create",
  disabled: "creating a workspace is a mutation; the bench pilot enables reads only",
});

const environmentCreate = defineCapability({
  capability: "environment.create",
  version: "1.0.0",
  title: "Create an environment",
  summary: "Create a new environment from a services list, from a snapshot, or empty.",
  guide: "Name the environment and give its services, or name from_snapshot to create a new environment from a snapshot. When both are given the api's restore receives the services as an override; the bench never drops either input on its own.",
  outputSchema: openDocument("the created environment's document"),
  effect: "write",
  scope: "environment",
  group: "environment",
  shape: argsObject({
    name: str("environment name", 64),
    services: optional({ t: "array", min: 0, max: 32, of: SERVICE }),
    from_snapshot: optional(str("snapshot id to create the new environment from", 256)),
  }),
  arguments: [
    { name: "name", presence: "required", schema: { type: "string" }, provenance: ["user_value"], clear: "unsupported" },
    { name: "services", presence: "optional", schema: { type: "array", items: { type: "object" } }, provenance: ["user_value", "set"], clear: "unsupported" },
    { name: "from_snapshot", presence: "optional", schema: { type: "string" }, provenance: ["resource_candidate"], clear: "unsupported" },
  ],
  rules: [
    "from_snapshot creates a NEW environment from a snapshot; a services list given with it is passed to the restore as an override",
    "services are sent whole: a service's omitted command, env, mounts and ports are filled empty by the api, never inherited from another row",
    "an unknown outcome is reconciled from the environment listing before anything else runs",
  ],
  limits: { maxServices: 32 },
  examples: [{ instruction: "Create an environment devstack with postgres and redis." }],
  errors: ["invalid_args", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["environment.id", "environment.state"], unknownOutcome: ["environment.listing", "environment.name"] },
  resourceAccess: { reads: ["platform.environments"], writes: ["platform.environments"], conflictKeys: ["platform.environments"], exclusive: false },
  target: "kl_environment_create",
  disabled: "creating an environment is a mutation; the bench pilot enables reads only",
});

const environmentRestore = defineCapability({
  capability: "environment.restore",
  version: "1.0.0",
  title: "Restore an environment in place",
  summary: "Put an environment back to one of its own snapshots, in place.",
  guide: "Give the environment and the snapshot to go back to. The environment named by id is restored in place — this never creates a new environment. Both the id and the snapshot are required, and the snapshot must belong to that environment's volume.",
  outputSchema: openDocument("the restored environment's document"),
  effect: "write",
  scope: "environment",
  group: "environment",
  shape: argsObject({ id: str("environment id or name", 256), snapshot: str("snapshot id to go back to", 256) }),
  arguments: [
    { name: "id", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    { name: "snapshot", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
  ],
  rules: [
    "restores the named snapshot INTO the environment named by id, in place; it does not create an environment",
    "the snapshot belongs to that environment's volume; a snapshot id from elsewhere is refused by the api",
    "an unknown outcome is reconciled by reading the environment's state and snapshot list, not by restoring again",
  ],
  limits: {},
  examples: [{ instruction: "Put devstack back to snapshot snap-1234.", args: { id: "devstack", snapshot: "snap-1234" } }],
  errors: ["invalid_args", "no_match", "ambiguous_match", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["environment.state"], unknownOutcome: ["environment.state", "environment.snapshots"] },
  resourceAccess: { reads: ["environment.snapshots"], writes: ["environment.services"], conflictKeys: ["environment.services", "environment"], exclusive: false },
  target: "kl_environment_restore",
  disabled: "restoring is a mutation; the bench pilot enables reads only",
});

const environmentIntercept = defineCapability({
  capability: "environment.intercept",
  version: "1.0.0",
  title: "Point a service's traffic at a workspace",
  summary: "Deliver one environment service's traffic to a workspace, or clear that intercept.",
  guide: "Give the environment and service, then the workspace that should receive its traffic. Explicit null CLEARS the intercept; omission is not a clear request. Omit ports to forward every port one to one; give remaps as {service, workspace} pairs when the ports differ.",
  outputSchema: openDocument("the intercept state"),
  effect: "write",
  scope: "environment",
  group: "environment",
  shape: argsObject({
    id: str("environment id or name", 256),
    service: str("service name in the environment", 128),
      workspace: optional({ t: "oneOf", variants: [str("workspace that should receive the traffic", 256), nullValue] }),
    ports: optional({
      t: "array",
      min: 1,
      max: 32,
      of: { t: "object", fields: { service: { t: "int", min: 0, max: 65_535 }, workspace: { t: "int", min: 0, max: 65_535 } } },
    }),
  }),
  arguments: [
    { name: "id", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    { name: "service", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    {
      name: "workspace",
      presence: "optional",
        schema: { oneOf: [{ type: "string" }, { type: "null" }], description: "null clears the intercept; omission makes no change" },
      provenance: ["user_value", "resource_candidate"],
      clear: "clears_when_null",
      description: "null clears the intercept; omission makes no change",
    },
    {
      name: "ports",
      presence: "optional",
      schema: { type: "array", items: { type: "object" } },
      provenance: ["user_value", "set"],
      clear: "unsupported",
      description: "absent forwards every port one to one; an empty list is not that default and is refused",
    },
  ],
  checks: [requiresField("ports", "workspace", "a port remap needs a workspace to deliver to")],
  rules: [
    "workspace names where the traffic goes; explicit null clears the intercept and omission makes no change",
    "omitting ports keeps the one-to-one mapping the api already applies; an empty list is refused instead of being read as that default",
    "the choice belongs to the environment named by id and applies to the whole space",
  ],
  limits: { maxPortMaps: 32 },
  examples: [
    { instruction: "Send devstack's postgres traffic to workspace api.", args: { id: "devstack", service: "postgres", workspace: "api" } },
    { instruction: "Stop intercepting devstack's postgres.", args: { id: "devstack", service: "postgres", workspace: null } },
  ],
  errors: ["invalid_args", "no_match", "ambiguous_match", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["environment.intercept"], unknownOutcome: ["environment.intercepts"] },
  resourceAccess: { reads: ["environment.services"], writes: ["environment.intercepts"], conflictKeys: ["environment.intercepts"], exclusive: false },
  target: "kl_intercept",
  disabled: "intercepting changes a space; the bench pilot enables reads only",
});

const environmentServicePut = defineCapability({
  capability: "environment.service.put",
  version: "1.0.0",
  title: "Add or replace a service",
  summary: "Add a service to an environment, or replace the one with the same name whole.",
  guide: "Give the environment and the full service. The service list is written as a whole: every service not named here is passed through verbatim, including fields this schema does not model. A service with the same name is replaced whole — it does not inherit the command, env, mounts or ports you leave out.",
  outputSchema: openDocument("environment and service state"),
  effect: "write",
  scope: "environment",
  group: "environment",
  shape: argsObject({ id: str("environment id or name", 256), service: SERVICE }),
  arguments: [
    { name: "id", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    { name: "service", presence: "required", schema: { type: "object" }, provenance: ["user_value", "generated_content"], clear: "unsupported" },
  ],
  rules: [
    "the services list is replaced whole: every other service is passed through verbatim, including fields this schema does not model",
    "a service of the same name is replaced whole; omitted command, env, mounts and ports are not inherited from the row it replaces",
    "an unknown outcome is reconciled by reading the environment's services before writing again",
  ],
  limits: { maxServices: 32 },
  examples: [{ instruction: "Add nats:2 to devstack as service nats." }],
  errors: ["invalid_args", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["environment.services", "environment.serviceStatus"], unknownOutcome: ["environment.services"] },
  resourceAccess: { reads: ["environment.services"], writes: ["environment.services"], conflictKeys: ["environment.services", "environment"], exclusive: false },
  target: "kl_environment_service_add",
  disabled: "changing a service is a mutation; the bench pilot enables reads only",
});

const environmentServiceRemove = defineCapability({
  capability: "environment.service.rm",
  version: "1.0.0",
  title: "Remove a service",
  summary: "Remove one service from an environment; its files stay on the volume.",
  guide: "Give the environment and the service name. The service's workload goes; its files stay on the volume. The services list is written whole, so every other service is passed through verbatim.",
  outputSchema: openDocument("environment and service state"),
  effect: "destroy",
  scope: "environment",
  group: "environment",
  shape: argsObject({ id: str("environment id or name", 256), name: str("the service to remove", 64) }),
  arguments: [
    { name: "id", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    { name: "name", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
  ],
  rules: [
    "only the named service is removed; every other service is passed through verbatim",
    "the service's files stay on the environment's volume; only its workload goes",
    "removing a service that is not there is reported, and nothing is written",
  ],
  limits: { maxServices: 32 },
  examples: [{ instruction: "Remove nats from devstack." }],
  errors: ["invalid_args", "no_match", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["environment.services"], unknownOutcome: ["environment.services"] },
  resourceAccess: { reads: ["environment.services"], writes: ["environment.services"], conflictKeys: ["environment.services", "environment"], exclusive: false },
  target: "kl_environment_service_rm",
  disabled: "removing a service is a mutation; the bench pilot enables reads only",
});

const workspacePackageAdd = defineCapability({
  capability: "workspace.packages.add",
  version: "1.0.0",
  title: "Add workspace packages",
  summary: "Add nixpkgs attributes to a workspace; every other package stays as it is.",
  guide: "Name the workspace and the nixpkgs attributes to add (attr@version pins one). The current list is read and written back whole, so every package not named here is preserved exactly. A re-pin replaces the entry with the same attribute rather than adding a second one.",
  outputSchema: { type: "array", items: { type: "string" }, description: "workspace packages" },
  effect: "write",
  scope: "workspace",
  group: "workspace",
  shape: argsObject({ workspace: str("the workspace to act on, by name or id", 256), packages: stringArray(1, 64, "nixpkgs attribute, optionally attr@version", 128) }),
  arguments: [
    { name: "workspace", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    { name: "packages", presence: "required", schema: { type: "array", items: { type: "string" } }, provenance: ["user_value", "set"], clear: "unsupported" },
  ],
  rules: [
    "reads the current package list and writes the whole list back; unrelated packages are preserved exactly",
    "a re-pin replaces the entry with the same attribute instead of adding a second one",
    "the registered tool uses the same approval card as every other workspace mutation before packages change",
  ],
  limits: { maxPackages: 64 },
  examples: [{ instruction: "Add rustc and cargo to workspace api." }],
  errors: ["invalid_args", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["workspace.packages"], unknownOutcome: ["workspace.packages"] },
  resourceAccess: { reads: ["platform.workspace_specs"], writes: ["workspace.packages"], conflictKeys: ["workspace.packages"], exclusive: false },
  target: "kl_pkg_add",
  disabled: "changing packages is a mutation; the bench pilot enables reads only",
});

const workspacePackageRemove = defineCapability({
  capability: "workspace.packages.rm",
  version: "1.0.0",
  title: "Remove workspace packages",
  summary: "Remove nixpkgs attributes from a workspace; every other package stays as it is.",
  guide: "Name the workspace and the attributes to remove. The list is read and written back whole, so every package not named here is preserved exactly. Removing something that is not installed is reported and nothing is written.",
  outputSchema: { type: "array", items: { type: "string" }, description: "workspace packages" },
  effect: "write",
  scope: "workspace",
  group: "workspace",
  shape: argsObject({ workspace: str("the workspace to act on, by name or id", 256), packages: stringArray(1, 64, "nixpkgs attribute, optionally attr@version", 128) }),
  arguments: [
    { name: "workspace", presence: "required", schema: { type: "string" }, provenance: ["user_value", "resource_candidate"], clear: "unsupported" },
    { name: "packages", presence: "required", schema: { type: "array", items: { type: "string" } }, provenance: ["user_value", "set"], clear: "unsupported" },
  ],
  rules: [
    "reads the current package list and writes the whole list back; unrelated packages are preserved exactly",
    "an attr removes every pin of that attribute, which is what a person means by removing nodejs",
    "the registered tool uses the same approval card as every other workspace mutation before packages change",
  ],
  limits: { maxPackages: 64 },
  examples: [{ instruction: "Remove nodejs from workspace api." }],
  errors: ["invalid_args", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["workspace.packages"], unknownOutcome: ["workspace.packages"] },
  resourceAccess: { reads: ["platform.workspace_specs"], writes: ["workspace.packages"], conflictKeys: ["workspace.packages"], exclusive: false },
  target: "kl_pkg_rm",
  disabled: "changing packages is a mutation; the bench pilot enables reads only",
});

const workspaceProcessRead = defineCapability({
  capability: "workspace.process.read",
  version: "1.0.0",
  title: "Read a workspace's processes",
  summary: "List a workspace's processes, read one's logs, or watch one for a pattern.",
  guide: "action=list reads the tool server's process table; logs pages by the cursor a previous read answered with; watch registers a standing pattern. All three only read, and a watch answers through messages rather than polling.",
  outputSchema: openDocument("workspace process rows or output"),
  effect: "read",
  scope: "workspace",
  group: "workspace",
  shape: argsObject({
    action: { t: "enum", values: PROCESS_READ_ACTIONS },
    id: optional(str("the process, for logs and watch", 128)),
    since: optional({ t: "int", min: 0, max: Number.MAX_SAFE_INTEGER }),
    pattern: optional(str("a regular expression watch matches", 256)),
  }),
  arguments: [
    { name: "action", presence: "required", schema: { type: "string", enum: [...PROCESS_READ_ACTIONS] }, provenance: ["enum"], clear: "unsupported" },
    { name: "id", presence: "optional", schema: { type: "string" }, provenance: ["resource_candidate"], clear: "unsupported", description: "required by logs and watch" },
    { name: "since", presence: "optional", schema: { type: "integer" }, provenance: ["runtime_fact"], clear: "unsupported" },
    { name: "pattern", presence: "optional", schema: { type: "string" }, provenance: ["user_value"], clear: "unsupported" },
  ],
  checks: [processActionIssues],
  rules: [
    "list is the tool server's own table; logs returns only what follows the cursor it was given; watch registers a standing pattern",
    "all three actions read; they never start, signal or write to a process",
    "logs and watch need an id, and watch needs the workspace's own session to receive matching lines",
  ],
  limits: {},
  examples: [{ instruction: "What is running in the api workspace?", args: { action: "list" } }],
  errors: ["invalid_args", "no_match", "scope_denied", "execution_failure"],
  retry: IDEMPOTENT,
  approval: NO_APPROVAL,
  evidence: { success: ["workspace.process.rows", "workspace.process.output"], unknownOutcome: [] },
  resourceAccess: { reads: ["workspace.process.metadata", "workspace.process.output"], writes: [], conflictKeys: [], exclusive: false },
  target: "process",
  disabled: "the bench pilot has no workspace adapter yet; workspace reads wait for that scoped step",
});

const workspaceProcessControl = defineCapability({
  capability: "workspace.process.control",
  version: "1.0.0",
  title: "Start or signal a workspace process",
  summary: "Start a background process in a workspace, stop one, or write to its stdin.",
  guide: "action=start runs a command in the background and answers with a process id; stop signals one; write sends data to its stdin. start needs command, and stop and write each need id. The command's footprint is unknown, so the step takes the workspace's exclusive execution lane.",
  outputSchema: openDocument("workspace process id and state"),
  effect: "write",
  scope: "workspace",
  group: "workspace",
  shape: argsObject({
    action: { t: "enum", values: PROCESS_CONTROL_ACTIONS },
    command: optional(str("the command to run in the background", 2_000)),
    id: optional(str("the process, for stop and write", 128)),
    data: optional(str("what to write to the process's stdin", 4_096)),
    title: optional(str("a short name a person will see", 60)),
    signal: optional({ t: "enum", values: ["TERM", "KILL"] }),
  }),
  arguments: [
    { name: "action", presence: "required", schema: { type: "string", enum: [...PROCESS_CONTROL_ACTIONS] }, provenance: ["enum"], clear: "unsupported" },
    { name: "command", presence: "optional", schema: { type: "string" }, provenance: ["generated_content", "user_value"], clear: "unsupported", description: "required by start" },
    { name: "id", presence: "optional", schema: { type: "string" }, provenance: ["resource_candidate"], clear: "unsupported", description: "required by stop and write" },
    { name: "data", presence: "optional", schema: { type: "string" }, provenance: ["generated_content", "user_value"], clear: "unsupported", description: "required by write" },
    { name: "title", presence: "optional", schema: { type: "string" }, provenance: ["generated_content"], clear: "unsupported" },
    { name: "signal", presence: "optional", schema: { type: "string", enum: ["TERM", "KILL"] }, provenance: ["enum"], clear: "unsupported" },
  ],
  checks: [processActionIssues],
  rules: [
    "start runs the command in the background and answers a process id; stop signals one; write sends stdin",
    "an action and its required fields are validated together, before approval and before the tool server is called",
    "the command's footprint is unknown, so the step takes one exclusive workspace/tree lane rather than guessing at conflicts",
  ],
  limits: { maxDataChars: 4_096, maxCommandChars: 2_000 },
  examples: [{ instruction: "Start npm run dev in the api workspace." }],
  errors: ["invalid_args", "no_match", "permission_denied", "scope_denied", "execution_failure", "unknown_outcome"],
  retry: RECONCILE_FIRST,
  approval: USER_APPROVAL,
  evidence: { success: ["workspace.process.id", "workspace.process.state"], unknownOutcome: ["workspace.process.rows"] },
  resourceAccess: { reads: ["workspace.process.metadata"], writes: ["workspace.process"], conflictKeys: ["workspace.process", "workspace.tree"], exclusive: true },
  target: "process",
  ask: (args) => (args.action === "start" ? `Run in the workspace: ${String(args.command ?? "").split("\n")[0].slice(0, 120)}` : args.action === "write" ? `Write to process ${args.id}` : `Stop process ${args.id}`),
  disabled: "the bench pilot has no workspace adapter yet; workspace mutations wait for that scoped step",
});

/** Every reviewed capability. Order is not significant; `describe` sorts by name. */
export const CAPABILITY_CONTRACTS: readonly CapabilityDefinition[] = [
  benchProcessList,
  skillRead,
  workspaceCreate,
  workspaceInspect,
  workspaceList,
  workspacePackageAdd,
  workspacePackageRemove,
  workspaceProcessControl,
  workspaceProcessRead,
  workspaceProgress,
  environmentCreate,
  environmentIntercept,
  environmentRestore,
  environmentServicePut,
  environmentServiceRemove,
];

/**
 * The capabilities the bench pilot may dispatch. Reads only, and only ones that
 * do not need the scoped workspace adapter: the bench's own process metadata,
 * workspace listing/inspection/progress, and skill text.
 */
export const INITIAL_READ_CAPABILITIES = [
  "bench.process.list",
  "skill.read",
  "workspace.inspect",
  "workspace.list",
  "workspace.progress",
] as const;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

export type CapabilityRefusal = {
  outcome: "refused";
  capability: string;
  version: string;
  code: OperationErrorCode;
  reason: string;
  /** Every rejected argument path, when the refusal was a validation failure. */
  issues?: ValidationIssue[];
};

export type CapabilityDispatchResult =
  | { outcome: "completed"; capability: string; version: string; result: CapabilityValue }
  | { outcome: "failed"; capability: string; version: string; code: OperationErrorCode; error: CapabilityError }
  | CapabilityRefusal;

const approvalFailure = (error: unknown): OperationErrorCode => {
  const code = String((error as Error)?.message ?? "");
  return (["decision_expired", "decision_replayed", "forged_approval", "decision_mismatch"] as const).includes(code as any)
    ? code as OperationErrorCode
    : "permission_denied";
};

function validateOutputAgainstSchema(value: JsonValue, schema: JsonSchemaLike, path = "$result"): ValidationIssue[] {
  const issues: ValidationIssue[] = [];
  const fail = (message: string) => issues.push({ path, code: "validation_failure", message });
  if (schema.oneOf) {
    if (!schema.oneOf.some((variant) => validateOutputAgainstSchema(value, variant, path).length === 0)) fail("result matches no output variant");
    return issues;
  }
  if (schema.type === "array") {
    if (!Array.isArray(value)) return fail("expected an array"), issues;
    if (schema.items) value.forEach((item, index) => issues.push(...validateOutputAgainstSchema(item, schema.items!, `${path}[${index}]`)));
  } else if (schema.type === "object") {
    if (typeof value !== "object" || value === null || Array.isArray(value)) return fail("expected an object"), issues;
    for (const name of schema.required ?? []) if (!(name in value)) issues.push({ path: `${path}.${name}`, code: "validation_failure", message: "required output is missing" });
    for (const [name, child] of Object.entries(schema.properties ?? {})) if (name in value) issues.push(...validateOutputAgainstSchema((value as any)[name], child, `${path}.${name}`));
  } else if (schema.type === "string" && typeof value !== "string") fail("expected a string");
  else if (schema.type === "integer" && (!Number.isInteger(value))) fail("expected an integer");
  else if (schema.type === "boolean" && typeof value !== "boolean") fail("expected a boolean");
  else if (schema.type === "null" && value !== null) fail("expected null");
  return issues;
}

function validateOutput(value: unknown, schema: JsonSchemaLike): ValidationIssue[] {
  const schemaShape = validateJsonSchemaLike(schema, "$outputSchema");
  if (!schemaShape.ok) return schemaShape.issues;
  const json = validateJsonValue(value, "$result");
  if (!json.ok) return json.issues;
  return validateOutputAgainstSchema(json.value, schemaShape.value);
}

export type CapabilityDescribeResult =
  | { ok: true; detail: "guide" | "schema"; entries: CapabilityDescription[]; nextCursor?: string }
  | { ok: false; code: OperationErrorCode; message: string };

export type CapabilityPrepareResult =
  | { ok: true; approval?: CapabilityApprovalRequest }
  | { ok: false; result: CapabilityDispatchResult };

export class CapabilityRegistry {
  private readonly byName: Map<string, CapabilityDefinition>;
  private readonly enabled: Set<string>;
  private readonly dispatchAuthority: DispatchAuthority;

  constructor(definitions: readonly CapabilityDefinition[], enabled: readonly string[], dispatchAuthority: DispatchAuthority) {
    this.byName = new Map(definitions.map((definition) => [definition.descriptor.capability, definition]));
    if (this.byName.size !== definitions.length) throw new Error("two capability definitions share a name");
    this.enabled = new Set(enabled);
    this.dispatchAuthority = dispatchAuthority;
    for (const name of this.enabled) {
      if (!this.byName.has(name)) throw new Error(`enabled capability ${name} is not in the registry`);
    }
  }

  descriptors(): CapabilityDescriptor[] {
    return [...this.byName.values()].map((definition) => definition.descriptor);
  }

  get(capability: string): CapabilityDescriptor | undefined {
    return this.byName.get(capability)?.descriptor;
  }

  isEnabled(capability: string): boolean {
    return this.enabled.has(capability) && this.byName.has(capability);
  }

  enabledDescriptors(): CapabilityDescriptor[] {
    return [...this.enabled].flatMap((name) => {
      const definition = this.byName.get(name);
      return definition ? [definition.descriptor] : [];
    });
  }

  /** Deterministic registry lookup: a short guide by default, the versioned schema on request. */
  describe(request: DescribeRequest): CapabilityDescribeResult {
    const detail = request.detail ?? "guide";
    if (request.capability) {
      const definition = this.byName.get(request.capability);
      if (!definition || !this.enabled.has(request.capability)) {
        return { ok: false, code: "unsupported_capability", message: `${request.capability} is not an enabled capability` };
      }
      return { ok: true, detail, entries: [describeCapability(definition.descriptor, detail)] };
    }
    const index = buildDescriptionIndex(this.enabledDescriptors(), request.cursor);
    return { ok: true, detail: "guide", entries: index.entries, ...(index.nextCursor ? { nextCursor: index.nextCursor } : {}) };
  }

  prepare(capability: string, args: unknown, deps: CapabilityDispatchDeps = {}, version?: string): CapabilityPrepareResult {
    const definition = this.byName.get(capability);
    if (!definition) return { ok: false, result: { outcome: "refused", capability, version: version ?? "", code: "unsupported_capability", reason: `no capability ${capability}` } };
    const { descriptor } = definition;
    if (!this.enabled.has(capability)) return { ok: false, result: { outcome: "refused", capability, version: descriptor.version, code: "unsupported_capability", reason: `${capability} is not enabled here${definition.disabled ? `: ${definition.disabled}` : ""}` } };
    if (version !== undefined && version !== descriptor.version) return { ok: false, result: { outcome: "refused", capability, version: descriptor.version, code: "stale_contract", reason: `${capability} ${version} is not the reviewed ${descriptor.version} contract` } };
    const valid = validateCapabilityArgs({ shape: definition.shape, arguments: descriptor.arguments, checks: definition.checks }, args);
    if (!valid.ok) return { ok: false, result: { outcome: "refused", capability, version: descriptor.version, code: "invalid_args", reason: valid.issues.map((issue) => `${issue.path}: ${issue.message}`).join("; "), issues: valid.issues } };
    if (capability === "environment.intercept" && valid.value.states.workspace.kind === "unspecified") return { ok: false, result: { outcome: "refused", capability, version: descriptor.version, code: "invalid_args", reason: "workspace must name a target or be explicit null to clear the intercept" } };
    if (!definition.target) return { ok: false, result: { outcome: "refused", capability, version: descriptor.version, code: "unsupported_capability", reason: `${capability} has no handler boundary` } };
    if (!deps.runtime?.resolve(capability)) return { ok: false, result: { outcome: "refused", capability, version: descriptor.version, code: "unsupported_capability", reason: `no trusted adapter is wired for ${capability}` } };
    if (descriptor.approval.required === "none") return { ok: true };
    if (!deps.decision) return { ok: false, result: { outcome: "refused", capability, version: descriptor.version, code: "permission_denied", reason: "the dispatch has no durable decision expectation" } };
    const approvedArgs = cloneJson(valid.value.args);
    const payloadDigest = canonicalDigest(approvedArgs);
    const expectation: ResumeExpectation = { ...deps.decision, payloadDigest, now: deps.decision.now ?? Date.now() };
    return { ok: true, approval: { capability, version: descriptor.version, effect: descriptor.effect, prompt: approvalPrompt(definition, valid.value.args) ?? `${capability} ${descriptor.version}`, args: approvedArgs, payloadDigest, expectation } };
  }

  /**
   * The only way a capability reaches its handler. Arguments are validated and
   * the capability's scope is checked before an approval-required dispatch consumes
   * its one-shot authorization. An unauthorized handler is never invoked.
   */
  async dispatch(capability: string, args: unknown, deps: CapabilityDispatchDeps = {}, version?: string): Promise<CapabilityDispatchResult> {
    const definition = this.byName.get(capability);
    if (!definition) return { outcome: "refused", capability, version: version ?? "", code: "unsupported_capability", reason: `no capability ${capability}` };
    const { descriptor } = definition;
    if (!this.enabled.has(capability)) {
      return {
        outcome: "refused",
        capability,
        version: descriptor.version,
        code: "unsupported_capability",
        reason: `${capability} is not enabled here${definition.disabled ? `: ${definition.disabled}` : ""}`,
      };
    }
    if (version !== undefined && version !== descriptor.version) {
      return { outcome: "refused", capability, version: descriptor.version, code: "stale_contract", reason: `${capability} ${version} is not the reviewed ${descriptor.version} contract` };
    }
    const valid = validateCapabilityArgs({ shape: definition.shape, arguments: descriptor.arguments, checks: definition.checks }, args);
    if (!valid.ok) {
      return {
        outcome: "refused",
        capability,
        version: descriptor.version,
        code: "invalid_args",
        reason: valid.issues.map((issue) => `${issue.path}: ${issue.message}`).join("; "),
        issues: valid.issues,
      };
    }
    if (capability === "environment.intercept" && valid.value.states.workspace.kind === "unspecified") {
      return { outcome: "refused", capability, version: descriptor.version, code: "invalid_args", reason: "workspace must name a target or be explicit null to clear the intercept" };
    }
    if (!definition.target) {
      return { outcome: "refused", capability, version: descriptor.version, code: "unsupported_capability", reason: `${capability} has no handler boundary` };
    }
    const handler = deps.runtime?.resolve(capability);
    if (!handler) {
      return { outcome: "refused", capability, version: descriptor.version, code: "unsupported_capability", reason: `no trusted adapter is wired for ${capability}` };
    }
    const approvalRequired = descriptor.approval.required;
    const executionArgs = cloneJson(valid.value.args);
    let outcome;
    try {
      outcome = await dispatchWithPolicy({
      capability,
      effect: descriptor.effect,
      approval: { required: approvalRequired, ...(approvalRequired === "none" ? {} : { obtain: async () => deps.dispatchToken !== undefined && deps.dispatchOperationId !== undefined && deps.dispatchStepId !== undefined && deps.dispatchAttempt !== undefined && this.dispatchAuthority.consume(deps.dispatchToken, { operationId: deps.dispatchOperationId, stepId: deps.dispatchStepId, capability, version: descriptor.version, payloadDigest: canonicalDigest(valid.value.args), attempt: deps.dispatchAttempt }) }) },
      inspect: () => definition.scopeCheck?.(valid.value.args),
      run: async () => handler({ args: executionArgs, states: cloneStates(valid.value.states), signal: deps.signal }),
      failed: (result) => result.ok ? undefined : result.error.code,
      });
    } catch (error) {
      if (descriptor.approval.required === "none") return { outcome: "failed", capability, version: descriptor.version, code: "provider_failure", error: { code: "provider_failure", message: String((error as Error)?.message ?? error), retryable: true } };
      return { outcome: "refused", capability, version: descriptor.version, code: approvalFailure(error), reason: "the recorded decision did not authorize this dispatch" };
    }
    if (outcome.outcome === "completed") {
      if (!outcome.result.ok) return { outcome: "failed", capability, version: descriptor.version, code: outcome.result.error.code, error: outcome.result.error };
      const outputIssues = validateOutput(outcome.result.value, descriptor.outputSchema);
      if (outputIssues.length) return { outcome: "failed", capability, version: descriptor.version, code: "validation_failure", error: { code: "validation_failure", message: outputIssues.map((issue) => `${issue.path}: ${issue.message}`).join("; "), retryable: false } };
      return { outcome: "completed", capability, version: descriptor.version, result: outcome.result.value };
    }
    if (outcome.outcome === "failed") return { outcome: "failed", capability, version: descriptor.version, code: outcome.code, error: outcome.result.ok ? { code: outcome.code, message: "capability failed", retryable: false } : outcome.result.error };
    return { ...outcome, capability, version: descriptor.version };
  }

}

/** The bench pilot's registry. Nothing may enable a capability that is not a read yet. */
export const capabilityRegistry = new CapabilityRegistry(CAPABILITY_CONTRACTS, INITIAL_READ_CAPABILITIES, new DispatchAuthority());

for (const name of INITIAL_READ_CAPABILITIES) {
  const descriptor = capabilityRegistry.get(name);
  if (!descriptor) throw new Error(`the pilot allowlist names an unknown capability ${name}`);
  if (descriptor.effect !== "read") throw new Error(`${name} is a ${descriptor.effect}; the bench pilot enables reads only`);
}
