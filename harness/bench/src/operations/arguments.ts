/**
 * Argument validation for operation capabilities.
 *
 * `shape.ts` describes an argument object once and emits both the validator and
 * the JSON Schema. This module adds what a JSON Schema cannot carry: per-field
 * presence with defaults, the explicit forms that mean "clear this", and the
 * cross-field rules a call must satisfy before dispatch (mutually exclusive
 * creation sources, fields that belong together, action-dependent requirements).
 * Nothing here reaches a handler or an approval bridge — it is pure validation.
 */
import { field, isPlainRecord, runValidation } from "./shape.ts";
import type { JsonValue, Node, Validation, ValidationIssue } from "./shape.ts";
import type { CapabilityArgument, CapabilityEffect, InternalValueState } from "./contracts.ts";

export type ArgumentStates = Record<string, InternalValueState>;

export type ValidatedCapabilityArgs = {
  /** Defaults applied and clear-marked fields removed; states retain clear intent for the adapter. */
  args: Record<string, JsonValue>;
  states: ArgumentStates;
};

export type ArgsCheck = (args: Record<string, JsonValue>, path: string) => ValidationIssue[];

export type ArgumentContract = {
  shape: Node;
  arguments: CapabilityArgument[];
  checks?: readonly ArgsCheck[];
};

/**
 * Whether a value is the argument's declared way of saying "clear this".
 * `null` on a value the contract has no clear form for stays a value and is
 * rejected by the shape, so "false", "0" and "" are never silently a deletion.
 */
function isClearForm(argument: CapabilityArgument, value: unknown): boolean {
  switch (argument.clear) {
    case "clears_when_null":
      return value === null;
    case "clears_when_empty":
      return value === "" || (Array.isArray(value) && value.length === 0) || (isPlainRecord(value) && Object.keys(value).length === 0);
    case "explicit_flag":
      return value === true;
    case "unsupported":
      return false;
  }
}

export function validateCapabilityArgs(contract: ArgumentContract, value: unknown, path = "$"): Validation<ValidatedCapabilityArgs> {
  const states: ArgumentStates = {};
  let document = value;
  if (isPlainRecord(value)) {
    const next: Record<string, unknown> = { ...value };
    for (const argument of contract.arguments) {
      if (!Object.prototype.hasOwnProperty.call(next, argument.name)) continue;
      if (isClearForm(argument, next[argument.name])) {
        states[argument.name] = { kind: "explicitly_clear" };
        delete next[argument.name];
      }
    }
    document = next;
  }
  const shape = runValidation<Record<string, JsonValue>>(contract.shape, document, path);
  if (!shape.ok) return shape;
  const args: Record<string, JsonValue> = { ...shape.value };
  for (const argument of contract.arguments) {
    if (states[argument.name]) continue;
    if (Object.prototype.hasOwnProperty.call(args, argument.name)) {
      states[argument.name] = { kind: "known", value: args[argument.name] };
      continue;
    }
    if (argument.presence === "defaulted" && argument.defaultJson !== undefined) {
      args[argument.name] = argument.defaultJson;
      states[argument.name] = { kind: "known", value: argument.defaultJson };
      continue;
    }
    states[argument.name] = { kind: "unspecified" };
  }
  const issues: ValidationIssue[] = [];
  for (const check of contract.checks ?? []) issues.push(...check(args, path));
  return issues.length ? { ok: false, issues } : { ok: true, value: { args, states } };
}

/**
 * The declared arguments and the executable shape must name the same fields,
 * with the same requiredness; otherwise a capability would validate one thing
 * and publish another. `defineCapability` refuses to build a registry that fails this.
 */
export function argumentDeclarationIssues(shape: Node, declared: readonly CapabilityArgument[], path = "$.arguments"): ValidationIssue[] {
  if (shape.t !== "object") {
    return [{ path, code: "wrong_type", message: "a capability's argument shape must be an object" }];
  }
  const issues: ValidationIssue[] = [];
  const names = new Set(declared.map((argument) => argument.name));
  for (const name of Object.keys(shape.fields)) {
    if (!names.has(name)) issues.push({ path: field(path, name), code: "missing_field", message: `${name} is in the shape but has no argument contract` });
  }
  for (const argument of declared) {
    if (!(argument.name in shape.fields)) {
      issues.push({ path: field(path, argument.name), code: "unknown_field", message: `argument ${argument.name} is not in the shape` });
      continue;
    }
    const required = shape.fields[argument.name].optional !== true;
    if (required !== (argument.presence === "required")) {
      issues.push({
        path: field(path, argument.name),
        code: "validation_failure",
        message: `${argument.name} must be ${required ? "required" : "optional"} in both the shape and the argument contract`,
      });
    }
    if (argument.presence === "defaulted" && required) {
      issues.push({
        path: field(path, argument.name),
        code: "validation_failure",
        message: `${argument.name} has a default, so it cannot be required`,
      });
    }
  }
  return issues;
}

/** At most one of the named groups may be present. An empty group names nothing. */
export function exclusiveGroups(label: string, groups: readonly (readonly string[])[]): ArgsCheck {
  return (args, path) => {
    const present = groups.filter((group) => group.some((name) => args[name] !== undefined));
    if (present.length <= 1) return [];
    const choices = groups.map((group) => group.join(" + ")).join(" or ");
    return [{ path, code: "mixed_action", message: `${label} takes ${choices}, not more than one` }];
  };
}

/** A field that only makes sense beside another one: all or the pair is refused. */
export function requiresField(name: string, required: string, why: string): ArgsCheck {
  return (args, path) =>
    args[name] === undefined || args[required] !== undefined
      ? []
      : [{ path: field(path, name), code: "missing_field", message: `${name} needs ${required}: ${why}` }];
}

/** A workspace is made one way in one call: empty, from repo+branch, or from a snapshot. */
export const WORKSPACE_CREATE_CHECKS: readonly ArgsCheck[] = [
  exclusiveGroups("workspace creation", [["repo", "branch"], ["from_snapshot"]]),
  requiresField("repo", "branch", "a repository source needs a branch"),
  requiresField("branch", "repo", "a branch belongs to a repository"),
];

/** The same rule the registry states, for the registered `kl_workspace_create` handler. */
export function workspaceCreateSourceIssues(args: Record<string, JsonValue>, path = "$"): ValidationIssue[] {
  return WORKSPACE_CREATE_CHECKS.flatMap((check) => check(args, path));
}

export type WorkspaceProcessAction = "list" | "logs" | "watch" | "start" | "stop" | "write";

/**
 * The process tool's actions and what each one needs. One table, used by the
 * registered `process` tool and by the `workspace.process.*` capability
 * contracts, so an action's requirements cannot drift between them.
 */
export const WORKSPACE_PROCESS_ACTIONS: Record<WorkspaceProcessAction, { effect: CapabilityEffect; required: readonly string[]; allowed: readonly string[] }> = {
  list: { effect: "read", required: [], allowed: ["action"] },
  logs: { effect: "read", required: ["id"], allowed: ["action", "id", "since"] },
  watch: { effect: "read", required: ["id"], allowed: ["action", "id", "pattern"] },
  start: { effect: "write", required: ["command"], allowed: ["action", "command", "title"] },
  stop: { effect: "write", required: ["id"], allowed: ["action", "id", "signal"] },
  write: { effect: "write", required: ["id", "data"], allowed: ["action", "id", "data"] },
};

export const PROCESS_READ_ACTIONS = ["list", "logs", "watch"] as const;
export const PROCESS_CONTROL_ACTIONS = ["start", "stop", "write"] as const;

/** The action and its required fields together, before approval and before any tool-server call. */
export function processActionIssues(args: Record<string, JsonValue>, path = "$"): ValidationIssue[] {
  const action = args.action;
  const known = typeof action === "string" && Object.prototype.hasOwnProperty.call(WORKSPACE_PROCESS_ACTIONS, action);
  if (!known) {
    return [{ path: field(path, "action"), code: "unsupported_action", message: `no process action ${typeof action === "string" ? action : "(missing)"}` }];
  }
  const missing = (value: JsonValue): boolean => value === undefined || value === "";
  const spec = WORKSPACE_PROCESS_ACTIONS[action as WorkspaceProcessAction];
  const irrelevant = Object.keys(args).filter((name) => args[name] !== undefined && !spec.allowed.includes(name));
  if (irrelevant.length) {
    return irrelevant.map((name) => ({ path: field(path, name), code: "mixed_action" as const, message: `${name} does not belong to ${action}` }));
  }
  return spec.required
    .filter((name) => missing(args[name]))
    .map((name) => ({ path: field(path, name), code: "missing_field" as const, message: `${action} needs ${name}` }));
}
