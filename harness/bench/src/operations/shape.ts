/**
 * The one shape engine behind `operations/contracts.ts`.
 *
 * A value is described once, as data, and that description produces both the
 * runtime validation and the JSON Schema the model sees — there is no second,
 * approximate schema to drift from the validator. This module holds shape,
 * bounds, and accounting only; executor policy lives in `contracts.ts`.
 */

export const JSON_LIMITS = {
  depth: 8,
  stringChars: 65_536,
  arrayItems: 256,
  objectKeys: 64,
  totalValues: 4_096,
} as const;

export type JsonLimits = typeof JSON_LIMITS;

/** Default ceiling for one validated document, counted over every string and key. */
export const DEFAULT_MAX_BYTES = 256 * 1024;

/** Longest dictionary key or field name anywhere in a contract value. */
export const KEY_MAX_CHARS = 64;

/** Schemas nest a little deeper than values (properties → items → variants). */
export const SCHEMA_DEPTH = 16;

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
export type JsonObject = { [key: string]: JsonValue };

export type SchemaType = "object" | "array" | "string" | "number" | "integer" | "boolean" | "null";

/** JSON-Schema-shaped data, dependency-free: the shape downstream tasks render. */
export type JsonSchemaLike = {
  type?: SchemaType;
  description?: string;
  required?: string[];
  properties?: Record<string, JsonSchemaLike>;
  items?: JsonSchemaLike;
  enum?: JsonValue[];
  const?: JsonValue;
  additionalProperties?: boolean | JsonSchemaLike;
  propertyNames?: JsonSchemaLike;
  oneOf?: JsonSchemaLike[];
  $ref?: string;
  definitions?: Record<string, JsonSchemaLike>;
  minLength?: number;
  maxLength?: number;
  pattern?: string;
  minimum?: number;
  maximum?: number;
  minItems?: number;
  maxItems?: number;
  minProperties?: number;
  maxProperties?: number;
};

export type IssueCode =
  | "not_object"
  | "unknown_field"
  | "missing_field"
  | "wrong_type"
  | "empty_string"
  | "string_too_long"
  | "too_many_items"
  | "too_few_items"
  | "out_of_range"
  | "bad_syntax"
  | "not_json"
  | "non_finite_number"
  | "too_deep"
  | "too_many_values"
  | "payload_too_large"
  | "forbidden_key"
  | "reserved_key"
  | "invalid_text"
  | "mixed_action"
  | "unsupported_action"
  | "duplicate_key"
  | "unknown_dependency"
  | "binding_collision"
  | "cycle"
  | "forged_approval"
  | "invalid_revision"
  | "invalid_transition"
  | "stale_contract"
  | "validation_failure"
  | "permission_denied"
  | "decision_mismatch"
  | "decision_expired"
  | "decision_replayed";

export type ValidationIssue = { path: string; code: IssueCode; message: string };

export type Validation<T> = { ok: true; value: T } | { ok: false; issues: ValidationIssue[] };

/** Thrown by the `parse*` helpers; `issues` names every rejected field. */
export class ContractViolation extends Error {
  issues: ValidationIssue[];
  constructor(issues: ValidationIssue[]) {
    super(
      issues.length
        ? `${issues[0].path}: ${issues[0].message}${issues.length > 1 ? ` (+${issues.length - 1} more)` : ""}`
        : "contract violation",
    );
    this.name = "ContractViolation";
    this.issues = issues;
  }
}

const FORBIDDEN_KEYS: readonly string[] = ["__proto__", "constructor", "prototype"];

export function isForbiddenKey(key: string): boolean {
  return FORBIDDEN_KEYS.includes(key);
}

/** Own-property lookup: inherited keys are never data (see `selectOutputPath`). */
export function hasOwnKey(value: object, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(value, key);
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Only plain JSON objects carry data: class instances are never contract values. */
export function isPlainRecord(value: unknown): value is Record<string, unknown> {
  if (!isRecord(value)) return false;
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}

export type Ctx = {
  issues: ValidationIssue[];
  bytes: number;
  nodes: number;
  maxBytes: number;
  maxDepth: number;
  maxNodes: number;
  maxStringChars: number;
  /** Reserved `$`-tagged forms, e.g. `$artifact`, with the node that validates them. */
  tags: Record<string, Node>;
  /** While set, approval-looking keys are rejected anywhere inside the value. */
  guardApprovalKeys?: boolean;
};

export function newCtx(maxBytes: number = DEFAULT_MAX_BYTES, tags: Record<string, Node> = {}): Ctx {
  return {
    issues: [],
    bytes: 0,
    nodes: 0,
    maxBytes,
    maxDepth: JSON_LIMITS.depth,
    maxNodes: JSON_LIMITS.totalValues,
    maxStringChars: JSON_LIMITS.stringChars,
    tags,
  };
}

export function fail(ctx: Ctx, path: string, code: IssueCode, message: string): undefined {
  ctx.issues.push({ path, code, message });
  return undefined;
}

export const field = (path: string, key: string): string => (path === "$" ? `$.${key}` : `${path}.${key}`);
export const item = (path: string, index: number): string => `${path}[${index}]`;

export type CustomCheck = (value: unknown, path: string, ctx: Ctx) => Validation<unknown>;

export type Node =
  | { t: "string"; max: number; min?: number; pattern?: RegExp; hint?: string }
  | { t: "int"; min: number; max: number }
  | { t: "bool" }
  | { t: "literal"; value: string }
  | { t: "enum"; values: readonly string[] }
  | { t: "stringList"; min: number; max: number; itemMax: number }
  | { t: "json" }
  | { t: "dict"; minKeys: number; maxKeys: number; of: Node; keyPattern: RegExp; rejectApprovalKeys?: true }
  | { t: "object"; fields: Fields }
  | { t: "array"; min: number; max: number; of: Node }
  | { t: "oneOf"; variants: readonly Node[] }
  | { t: "custom"; schema: JsonSchemaLike; check: CustomCheck };

export type Field = Node & { optional?: true };
export type Fields = Record<string, Field>;

const ORPHANED_MESSAGE = "must be an object";

function checkStringValue(
  value: unknown,
  path: string,
  ctx: Ctx,
  spec: { max: number; min?: number; pattern?: RegExp; hint?: string; label?: string },
): string | undefined {
  const label = spec.label ?? "value";
  if (typeof value !== "string") return fail(ctx, path, "wrong_type", `${label} must be a string`);
  if (spec.min !== undefined && value.length < spec.min) return fail(ctx, path, "empty_string", `${label} must not be empty`);
  if (value.length > spec.max) return fail(ctx, path, "string_too_long", `${label} is limited to ${spec.max} characters`);
  if (spec.pattern && !spec.pattern.test(value)) {
    return fail(ctx, path, "bad_syntax", spec.hint ?? `${label} has an unsupported form`);
  }
  if (hasLoneSurrogate(value)) return fail(ctx, path, "invalid_text", `${label} must be well-formed text`);
  ctx.bytes += value.length;
  return value;
}

function hasLoneSurrogate(value: string): boolean {
  for (let i = 0; i < value.length; i++) {
    const code = value.charCodeAt(i);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = value.charCodeAt(i + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) return true;
      i++;
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return true;
    }
  }
  return false;
}

function checkSize(ctx: Ctx, path: string, count: number, min: number, max: number, label: string): boolean {
  if (count < min) {
    fail(ctx, path, "too_few_items", `${label} must hold at least ${min} entries`);
    return false;
  }
  if (count > max) {
    fail(ctx, path, "too_many_items", `${label} is limited to ${max} entries`);
    return false;
  }
  return true;
}

function checkJson(value: unknown, path: string, ctx: Ctx, depth: number): unknown {
  ctx.nodes += 1;
  if (ctx.nodes > ctx.maxNodes) return fail(ctx, path, "too_many_values", `more than ${ctx.maxNodes} values`);
  if (depth > ctx.maxDepth) return fail(ctx, path, "too_deep", `nested deeper than ${ctx.maxDepth} levels`);
  if (value === null || typeof value === "boolean") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) return fail(ctx, path, "non_finite_number", "numbers must be finite");
    return value;
  }
  if (typeof value === "string") return checkStringValue(value, path, ctx, { max: ctx.maxStringChars });
  if (Array.isArray(value)) {
    if (!checkSize(ctx, path, value.length, 0, JSON_LIMITS.arrayItems, "arrays")) return undefined;
    for (let i = 0; i < value.length; i++) checkJson(value[i], item(path, i), ctx, depth + 1);
    return value;
  }
  if (!isPlainRecord(value)) return fail(ctx, path, "not_json", `not a JSON value (${typeof value})`);
  const keys = Object.keys(value);
  if (!checkSize(ctx, path, keys.length, 0, JSON_LIMITS.objectKeys, "objects")) return undefined;
  if (keys.length === 1) {
    const tag = keys[0];
    // Reserved keys are rejected before tag lookup, and only own registered tags
    // count: `constructor`/`__proto__` must never resolve through the prototype.
    if (isForbiddenKey(tag)) return fail(ctx, field(path, tag), "forbidden_key", `"${tag}" is not a JSON key here`);
    if (hasOwnKey(ctx.tags, tag)) return checkNode(value[tag], ctx.tags[tag], field(path, tag), ctx, depth + 1);
  }
  for (const key of keys) {
    if (isForbiddenKey(key)) {
      fail(ctx, field(path, key), "forbidden_key", `"${key}" is not a JSON key here`);
      continue;
    }
    if (key.startsWith("$")) {
      fail(ctx, field(path, key), "reserved_key", `"${key}" is reserved`);
      continue;
    }
    if (ctx.guardApprovalKeys && APPROVAL_KEY_RE.test(key)) {
      fail(
        ctx,
        field(path, key),
        "forged_approval",
        "additional_input cannot carry approval values; approvals are recorded by the user UI or trusted policy",
      );
      continue;
    }
    ctx.bytes += key.length;
    checkJson(value[key], field(path, key), ctx, depth + 1);
  }
  return value;
}

function checkFields(value: unknown, fields: Fields, path: string, ctx: Ctx, depth: number): Record<string, unknown> | undefined {
  if (!isPlainRecord(value)) return fail(ctx, path, "not_object", ORPHANED_MESSAGE);
  for (const key of Object.keys(value)) {
    if (!Object.prototype.hasOwnProperty.call(fields, key)) {
      fail(ctx, field(path, key), "unknown_field", `unknown field "${key}"`);
    }
  }
  const out: Record<string, unknown> = {};
  for (const [name, spec] of Object.entries(fields)) {
    const raw = value[name];
    if (raw === undefined) {
      if (!spec.optional) fail(ctx, field(path, name), "missing_field", `${name} is required`);
      continue;
    }
    ctx.bytes += name.length;
    const checked = checkNode(raw, spec, field(path, name), ctx, depth + 1);
    if (checked !== undefined) out[name] = checked;
  }
  return out;
}

function checkDict(value: unknown, node: Extract<Node, { t: "dict" }>, path: string, ctx: Ctx, depth: number): unknown {
  if (!isPlainRecord(value)) return fail(ctx, path, "not_object", ORPHANED_MESSAGE);
  const keys = Object.keys(value);
  if (!checkSize(ctx, path, keys.length, node.minKeys, node.maxKeys, "object")) return undefined;
  const out: Record<string, unknown> = {};
  const outerGuard = ctx.guardApprovalKeys;
  if (node.rejectApprovalKeys) ctx.guardApprovalKeys = true;
  try {
    for (const key of keys) {
      if (isForbiddenKey(key)) {
        fail(ctx, field(path, key), "forbidden_key", `"${key}" is not a usable key`);
        continue;
      }
      if (key.length > KEY_MAX_CHARS) {
        fail(ctx, field(path, key), "string_too_long", `keys are limited to ${KEY_MAX_CHARS} characters`);
        continue;
      }
      if (!node.keyPattern.test(key)) {
        fail(ctx, field(path, key), "bad_syntax", "key must be a simple identifier");
        continue;
      }
      if (node.rejectApprovalKeys && APPROVAL_KEY_RE.test(key)) {
        fail(
          ctx,
          field(path, key),
          "forged_approval",
          "additional_input cannot carry approval values; approvals are recorded by the user UI or trusted policy",
        );
        continue;
      }
      ctx.bytes += key.length;
      const checked = checkNode(value[key], node.of, field(path, key), ctx, depth + 1);
      if (checked !== undefined) out[key] = checked;
    }
  } finally {
    ctx.guardApprovalKeys = outerGuard;
  }
  return out;
}

const APPROVAL_KEY_RE =
  /^(approve|approved|approval|grant|granted|deny|denied|allow|allowed|accept|accepted|consent|confirm|confirmed|yes|no|authori[sz]ed|authori[sz]ation|permission|decision)$/i;

/** Validates one value against one node and returns the cleaned value (or `undefined`). */
export function checkNode(value: unknown, node: Node, path: string, ctx: Ctx, depth = 0): unknown {
  ctx.nodes += 1;
  if (ctx.nodes > ctx.maxNodes) return fail(ctx, path, "too_many_values", `more than ${ctx.maxNodes} values`);
  if (depth > ctx.maxDepth) return fail(ctx, path, "too_deep", `nested deeper than ${ctx.maxDepth} levels`);
  switch (node.t) {
    case "string":
      return checkStringValue(value, path, ctx, node);
    case "int": {
      if (typeof value !== "number" || !Number.isInteger(value)) return fail(ctx, path, "wrong_type", "must be an integer");
      if (value < node.min || value > node.max) {
        return fail(ctx, path, "out_of_range", `must be ${node.min}..${node.max}`);
      }
      return value;
    }
    case "bool":
      return typeof value === "boolean" ? value : fail(ctx, path, "wrong_type", "must be a boolean");
    case "literal":
      return value === node.value ? value : fail(ctx, path, "stale_contract", `must be "${node.value}"`);
    case "enum":
      return typeof value === "string" && node.values.includes(value)
        ? value
        : fail(ctx, path, "bad_syntax", `must be one of ${node.values.join(", ")}`);
    case "stringList": {
      if (!Array.isArray(value)) return fail(ctx, path, "wrong_type", "must be an array of strings");
      if (!checkSize(ctx, path, value.length, node.min, node.max, "list")) return undefined;
      const out: string[] = [];
      for (let i = 0; i < value.length; i++) {
        const checked = checkStringValue(value[i], item(path, i), ctx, { max: node.itemMax });
        if (checked !== undefined) out.push(checked);
      }
      return out;
    }
    case "json":
      return checkJson(value, path, ctx, depth);
    case "dict":
      return checkDict(value, node, path, ctx, depth);
    case "object":
      return checkFields(value, node.fields, path, ctx, depth);
    case "array": {
      if (!Array.isArray(value)) return fail(ctx, path, "wrong_type", "must be an array");
      if (!checkSize(ctx, path, value.length, node.min, node.max, "array")) return undefined;
      const out: unknown[] = [];
      for (let i = 0; i < value.length; i++) out.push(checkNode(value[i], node.of, item(path, i), ctx, depth + 1));
      return out;
    }
    case "oneOf": {
      // A branch whose discriminator did not match explains the value far worse than a
      // branch that matched but found missing fields, so report the latter.
      const score = (list: ValidationIssue[]): number =>
        list.reduce((total, entry) => total + (entry.code === "stale_contract" ? 10 : 1), 0);
      let best: ValidationIssue[] | undefined;
      for (const variant of node.variants) {
        const trial: Ctx = { ...ctx, issues: [], bytes: ctx.bytes, nodes: ctx.nodes };
        const checked = checkNode(value, variant, path, trial, depth);
        if (!trial.issues.length) {
          ctx.bytes = trial.bytes;
          ctx.nodes = trial.nodes;
          return checked;
        }
        if (!best || score(trial.issues) < score(best)) best = trial.issues;
      }
      ctx.issues.push(...(best ?? [{ path, code: "bad_syntax" as IssueCode, message: "no allowed form matched" }]));
      return undefined;
    }
    case "custom": {
      const result = node.check(value, path, ctx);
      if (!result.ok) ctx.issues.push(...result.issues);
      return result.ok ? result.value : undefined;
    }
  }
}

/** Runs a node over a fresh context and enforces the total string/key byte budget. */
export function runValidation<T>(
  node: Node,
  value: unknown,
  path = "$",
  opts: { maxBytes?: number; tags?: Record<string, Node> } = {},
): Validation<T> {
  const ctx = newCtx(opts.maxBytes, opts.tags);
  const checked = checkNode(value, node, path, ctx);
  if (!ctx.issues.length && ctx.bytes > ctx.maxBytes) {
    ctx.issues.push({
      path: "$",
      code: "payload_too_large",
      message: `request carries ${ctx.bytes} characters of strings and keys; the limit is ${ctx.maxBytes}`,
    });
  }
  return ctx.issues.length ? { ok: false, issues: ctx.issues } : { ok: true, value: checked as T };
}

// ---------------------------------------------------------------------------
// The same descriptions, emitted as JSON Schema
// ---------------------------------------------------------------------------

export const JSON_VALUE_DEFINITION: JsonSchemaLike = {
  oneOf: [
    { type: "null" },
    { type: "boolean" },
    { type: "number" },
    { type: "string" },
    { type: "array", items: { $ref: "#/definitions/jsonValue" } },
    { type: "object", additionalProperties: { $ref: "#/definitions/jsonValue" } },
  ],
};

export function toJsonSchema(node: Node): JsonSchemaLike {
  switch (node.t) {
    case "string": {
      const schema: JsonSchemaLike = { type: "string" };
      if (node.min !== undefined) schema.minLength = node.min;
      schema.maxLength = node.max;
      if (node.pattern) schema.pattern = node.pattern.source;
      if (node.hint) schema.description = node.hint;
      return schema;
    }
    case "int":
      return { type: "integer", minimum: node.min, maximum: node.max };
    case "bool":
      return { type: "boolean" };
    case "literal":
      return { type: "string", const: node.value };
    case "enum":
      return { type: "string", enum: [...node.values] };
    case "stringList":
      return {
        type: "array",
        minItems: node.min,
        maxItems: node.max,
        items: { type: "string", minLength: 1, maxLength: node.itemMax },
      };
    case "json":
      return { $ref: "#/definitions/jsonValue" };
    case "dict":
      return {
        type: "object",
        minProperties: node.minKeys,
        maxProperties: node.maxKeys,
        propertyNames: { type: "string", pattern: node.keyPattern.source, maxLength: KEY_MAX_CHARS },
        additionalProperties: toJsonSchema(node.of),
      };
    case "object": {
      const properties: Record<string, JsonSchemaLike> = {};
      const required: string[] = [];
      for (const [name, spec] of Object.entries(node.fields)) {
        properties[name] = toJsonSchema(spec);
        if (!spec.optional) required.push(name);
      }
      const schema: JsonSchemaLike = { type: "object", properties, additionalProperties: false };
      if (required.length) schema.required = required;
      return schema;
    }
    case "array":
      return { type: "array", minItems: node.min, maxItems: node.max, items: toJsonSchema(node.of) };
    case "oneOf":
      return { oneOf: node.variants.map(toJsonSchema) };
    case "custom":
      return node.schema;
  }
}

/** Adds shared definitions (`#/definitions/jsonValue`) to a root schema. */
export function withDefinitions(schema: JsonSchemaLike): JsonSchemaLike {
  return { ...schema, definitions: { jsonValue: JSON_VALUE_DEFINITION, ...(schema.definitions ?? {}) } };
}

const SCHEMA_TYPES: readonly string[] = ["object", "array", "string", "number", "integer", "boolean", "null"];

function checkSchemaNode(value: unknown, path: string, issues: ValidationIssue[], depth: number): void {
  if (depth > SCHEMA_DEPTH) {
    issues.push({ path, code: "too_deep", message: `schemas are limited to ${SCHEMA_DEPTH} levels` });
    return;
  }
  if (!isRecord(value)) {
    issues.push({ path, code: "not_object", message: "schema node must be an object" });
    return;
  }
  const allowed = [
    "type",
    "description",
    "required",
    "properties",
    "items",
    "enum",
    "const",
    "additionalProperties",
    "propertyNames",
    "oneOf",
    "$ref",
    "definitions",
    "minLength",
    "maxLength",
    "pattern",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
  ];
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) issues.push({ path: field(path, key), code: "unknown_field", message: `unknown schema keyword "${key}"` });
  }
  if (value.type === undefined && value.oneOf === undefined && value.$ref === undefined) {
    issues.push({ path, code: "missing_field", message: "schema node needs type, oneOf, or $ref" });
  }
  if (value.type !== undefined && (typeof value.type !== "string" || !SCHEMA_TYPES.includes(value.type))) {
    issues.push({ path: field(path, "type"), code: "bad_syntax", message: `type must be one of ${SCHEMA_TYPES.join(", ")}` });
  }
  if (value.description !== undefined && (typeof value.description !== "string" || value.description.length > 500)) {
    issues.push({ path: field(path, "description"), code: "string_too_long", message: "description is limited to 500 characters" });
  }
  if (value.$ref !== undefined && (typeof value.$ref !== "string" || !value.$ref.startsWith("#/"))) {
    issues.push({ path: field(path, "$ref"), code: "bad_syntax", message: "$ref must be a local pointer" });
  }
  for (const keyword of ["minLength", "maxLength", "minItems", "maxItems", "minProperties", "maxProperties"]) {
    const limit = value[keyword];
    if (limit !== undefined && (typeof limit !== "number" || !Number.isInteger(limit) || limit < 0)) {
      issues.push({ path: field(path, keyword), code: "out_of_range", message: `${keyword} must be a non-negative integer` });
    }
  }
  for (const keyword of ["minimum", "maximum"]) {
    const bound = value[keyword];
    if (bound !== undefined && (typeof bound !== "number" || !Number.isFinite(bound))) {
      issues.push({ path: field(path, keyword), code: "wrong_type", message: `${keyword} must be a finite number` });
    }
  }
  if (value.pattern !== undefined) {
    if (typeof value.pattern !== "string") {
      issues.push({ path: field(path, "pattern"), code: "wrong_type", message: "pattern must be a string" });
    } else {
      try {
        new RegExp(value.pattern);
      } catch {
        issues.push({ path: field(path, "pattern"), code: "bad_syntax", message: "pattern must be a valid regular expression" });
      }
    }
  }
  if (value.propertyNames !== undefined) {
    checkSchemaNode(value.propertyNames, field(path, "propertyNames"), issues, depth + 1);
  }
  if (value.required !== undefined) {
    if (!Array.isArray(value.required) || value.required.length > JSON_LIMITS.objectKeys) {
      issues.push({ path: field(path, "required"), code: "too_many_items", message: "required must list at most 64 names" });
    } else {
      for (let i = 0; i < value.required.length; i++) {
        if (typeof value.required[i] !== "string") {
          issues.push({ path: item(field(path, "required"), i), code: "bad_syntax", message: "required name must be a string" });
        }
      }
    }
  }
  if (value.properties !== undefined) {
    if (!isRecord(value.properties) || Object.keys(value.properties).length > JSON_LIMITS.objectKeys) {
      issues.push({ path: field(path, "properties"), code: "too_many_items", message: "properties must hold at most 64 entries" });
    } else {
      for (const key of Object.keys(value.properties)) {
        checkSchemaNode(value.properties[key], field(field(path, "properties"), key), issues, depth + 1);
      }
    }
  }
  if (value.definitions !== undefined) {
    if (!isRecord(value.definitions)) {
      issues.push({ path: field(path, "definitions"), code: "wrong_type", message: "definitions must be an object" });
    } else {
      for (const key of Object.keys(value.definitions)) {
        checkSchemaNode(value.definitions[key], field(field(path, "definitions"), key), issues, depth + 1);
      }
    }
  }
  if (value.items !== undefined) checkSchemaNode(value.items, field(path, "items"), issues, depth + 1);
  if (value.enum !== undefined) {
    if (!Array.isArray(value.enum) || value.enum.length < 1 || value.enum.length > JSON_LIMITS.objectKeys) {
      issues.push({ path: field(path, "enum"), code: "too_many_items", message: "enum must hold 1..64 literals" });
    }
  }
  if (value.additionalProperties !== undefined && typeof value.additionalProperties !== "boolean") {
    checkSchemaNode(value.additionalProperties, field(path, "additionalProperties"), issues, depth + 1);
  }
  if (value.oneOf !== undefined) {
    if (!Array.isArray(value.oneOf) || value.oneOf.length < 1 || value.oneOf.length > SCHEMA_DEPTH) {
      issues.push({ path: field(path, "oneOf"), code: "too_many_items", message: "oneOf must hold 0..8 variants" });
    } else {
      for (let i = 0; i < value.oneOf.length; i++) {
        checkSchemaNode(value.oneOf[i], item(field(path, "oneOf"), i), issues, depth + 1);
      }
    }
  }
}

export function validateJsonSchemaLike(value: unknown, path = "$"): Validation<JsonSchemaLike> {
  const issues: ValidationIssue[] = [];
  checkSchemaNode(value, path, issues, 0);
  return issues.length ? { ok: false, issues } : { ok: true, value: value as JsonSchemaLike };
}

/** Validates a bare JSON value (finite numbers, bounded size/depth, no reserved keys). */
export function validateJsonValue(value: unknown, path = "$"): Validation<JsonValue> {
  return runValidation<JsonValue>({ t: "json" }, value, path);
}
