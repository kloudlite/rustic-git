/**
 * O05 — durable operation store.
 *
 * One append-only log per operation, named from an internally generated operation id:
 * `<bench folder>/operations/<operationId>.jsonl`. Every line is one atomic commit —
 * the full validated snapshot plus the events it published — written with a single
 * `write` and one `fsync`, so a crash loses at most the record being written and never
 * presents half a state change. A newly created file is fsynced before its directory.
 *
 * Trust: acceptance takes the request and the actor context from the caller. Identity
 * is never read out of the request (`validateOperateRequest` rejects unknown fields);
 * the deduplication key comes from `deriveDeduplicationKey(TrustedActorContext)`. The
 * operation id is not proof of a backend exactly-once effect anywhere.
 *
 * Corruption: a final record that never finished writing is a torn tail — reported and
 * repaired on the next append. Anything else malformed (an LF-terminated bad line, a
 * snapshot that fails `validateOperationSnapshot`, a sequence that does not follow, a
 * digest that does not match its request) refuses to load and names the file, because
 * silently dropping operation history would hide unresolved effects.
 */
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import {
  CONTRACT_VERSION,
  OPERATION_TRANSITIONS,
  STEP_TRANSITIONS,
  ContractViolation,
  DEFAULT_BUDGETS,
  canTransitionOperation,
  checkResumeAgainstRecord,
  deriveDeduplicationKey,
  isTerminalOperationState,
  requestDigest,
  canonicalDigest,
  resolutionCanResolve,
  validateBudgets,
  validateEventSequence,
  validateOperateRequest,
  validateOperationEvent,
  validateOperationError,
  validateOperationSnapshot,
  validatePendingDecision,
  validateRecordedDecision,
  validateStepRecord,
  validateTrustedActorContext,
  type CapabilityEffect,
  type CompactOperationResult,
  type DecisionResolution,
  type IssueCode,
  type JsonValue,
  type OperateRequest,
  type OperationBudgets,
  type OperationError,
  type OperationErrorCode,
  type OperationEvent,
  type OperationSnapshot,
  type OperationState,
  type OperationTransitionTrigger,
  type PendingDecision,
  type RecordedDecision,
  type ResumeExpectation,
  type ResumeRequest,
  type StepRecord,
  type TrustedActorContext,
  type UnknownOutcome,
} from "./contracts.ts";
import { isRecord, type Validation } from "./shape.ts";
import {
  applyTransition,
  compactView,
  settledState,
  type NewEvent,
  type OperationChange,
  type StepChange,
  type TransitionInput,
} from "./state.ts";

export const COMMIT_FORMAT_VERSION = 1;
export const FRAME_FORMAT_VERSION = 2;
const FRAME_MAGIC = "O05v2\n";
export const LOG_SUFFIX = ".jsonl";
/** Longest default a decision may stay pending before the store expires it. */
export const DEFAULT_DECISION_TTL_MS = 10 * 60 * 1000;

const SAFE_OPERATION_ID = /^op-[a-z0-9]+(?:-[a-z0-9]+)*$/;
const DIGEST_RE = /^sha256:[0-9a-f]{64}$/;

/**
 * One durable change: the validated snapshot plus everything published with it.
 *
 * `turnRevision` is the trusted user-turn watermark (from `TrustedActorContext`, never
 * from a request). The frozen `OperationSnapshot` has no field for it, so O05 keeps it
 * in its own commit state: it never decreases, and a resume whose trusted current turn
 * is older than the watermark is refused as superseded.
 */
export type CommitRecord = {
  v: typeof COMMIT_FORMAT_VERSION;
  kind: "commit";
  at: number;
  turnRevision: number;
  snapshot: OperationSnapshot;
  events: OperationEvent[];
  decisions?: RecordedDecision[];
  resolutions?: Array<{ decisionId: string; resolution: DecisionResolution }>;
  /** O05-private cancellation intent; absent from the frozen public snapshot. */
  abortRequestedStepIds?: string[];
};

export interface StoreFs {
  mkdirSync(target: string, options?: { recursive?: boolean; mode?: number }): void;
  readdirSync(target: string): string[];
  existsSync(target: string): boolean;
  readFileSync(target: string): Buffer;
  openSync(target: string, flags: string | number, mode?: number): number;
  writeSync(fd: number, data: string | Buffer): number;
  fsyncSync(fd: number): void;
  closeSync(fd: number): void;
  truncateSync(target: string, length: number): void;
  unlinkSync(target: string): void;
  renameSync(from: string, to: string): void;
  fstatSync(fd: number): fs.Stats;
  lstatSync(target: string): fs.Stats;
  chmodSync(target: string, mode: number): void;
  readSync(fd: number, buffer: Buffer, offset: number, length: number, position: number | null): number;
}

export const nodeStoreFs: StoreFs = {
  mkdirSync: (target, options) => {
    fs.mkdirSync(target, options ?? { recursive: true });
  },
  readdirSync: (target) => fs.readdirSync(target),
  existsSync: (target) => fs.existsSync(target),
  readFileSync: (target) => fs.readFileSync(target),
  openSync: (target, flags, mode) => fs.openSync(target, flags, mode),
  writeSync: (fd, data) => (typeof data === "string" ? fs.writeSync(fd, data) : fs.writeSync(fd, data)),
  fsyncSync: (fd) => fs.fsyncSync(fd),
  closeSync: (fd) => fs.closeSync(fd),
  truncateSync: (target, length) => fs.truncateSync(target, length),
  unlinkSync: (target) => fs.unlinkSync(target),
  renameSync: (from, to) => fs.renameSync(from, to),
  fstatSync: (fd) => fs.fstatSync(fd),
  lstatSync: (target) => fs.lstatSync(target),
  chmodSync: (target, mode) => fs.chmodSync(target, mode),
  readSync: (fd, buffer, offset, length, position) => fs.readSync(fd, buffer, offset, length, position),
};


/**
 * Exclusive bench ownership. Recovery and every durable write assert it; the store
 * never dispatches or records work for a folder another harness holds.
 */
export interface ExclusiveOwnership {
  readonly ownerId: string;
  /** Throws `StoreNotOwnedError` when this process no longer holds the folder. */
  assertHeld(): void;
}

/** What approval a capability currently requires, from the trusted registry. */
export type ApprovalRequirement = "none" | "user" | "policy";
export type ApprovalPolicySource = (capability: string, version: string) => ApprovalRequirement;
/** Deny by default: an unknown capability is asked for user approval, never auto-approved. */
export const USER_APPROVAL_POLICY: ApprovalPolicySource = () => "user";
export type RetryPolicy = { class: "none" | "idempotent" | "reconcile_required"; maxAttempts: number };
export type RetryPolicySource = (capability: string, version: string) => RetryPolicy;
/** Unknown capabilities never retry. */
export const NO_RETRY_POLICY: RetryPolicySource = () => ({ class: "none", maxAttempts: 1 });
export type CapabilityMetadata = {
  version: string;
  effect: CapabilityEffect;
  resourceKeys?: string[];
  approval: ApprovalRequirement;
  retry: RetryPolicy;
};
export type CapabilityMetadataSource = (capability: string, version: string) => CapabilityMetadata | undefined;

export class OperationStoreError extends Error {
  readonly code: OperationErrorCode;
  readonly missing?: string[];
  readonly refs?: string[];
  constructor(code: OperationErrorCode, message: string, extra: { missing?: string[]; refs?: string[] } = {}) {
    super(message);
    this.name = "OperationStoreError";
    this.code = code;
    if (extra.missing) this.missing = extra.missing;
    if (extra.refs) this.refs = extra.refs;
  }
}

export class StoreNotOwnedError extends OperationStoreError {
  constructor(detail: string) {
    super("permission_denied", `this process does not own the bench folder: ${detail}`);
    this.name = "StoreNotOwnedError";
  }
}

export class OperationNotFoundError extends OperationStoreError {
  constructor(operationId: string) {
    super("unsupported_request", `no durable operation ${operationId}`);
    this.name = "OperationNotFoundError";
  }
}

export class InvalidOperationIdError extends OperationStoreError {
  constructor(operationId: string) {
    super("validation_failure", `"${operationId}" is not a generated operation id`);
    this.name = "InvalidOperationIdError";
  }
}

export class DuplicateRequestConflictError extends OperationStoreError {
  constructor(dedupeKey: string, operationId: string) {
    super("revision_conflict", `tool call ${dedupeKey} was already accepted as ${operationId} with a different request`);
    this.name = "DuplicateRequestConflictError";
  }
}

export class OperationLogCorruptError extends OperationStoreError {
  readonly file: string;
  readonly operationId?: string;
  constructor(file: string, detail: string, operationId?: string) {
    super("validation_failure", `${file}: ${detail}`);
    this.name = "OperationLogCorruptError";
    this.file = file;
    this.operationId = operationId;
  }
}

export class StoreClosedError extends OperationStoreError {
  constructor() {
    super("execution_failure", "the operation store is closed");
    this.name = "StoreClosedError";
  }
}

export type LoadedOperation = {
  snapshot: OperationSnapshot;
  file: string;
  /** Highest trusted user-turn revision recorded for this operation. */
  turnRevision: number;
  events: OperationEvent[];
  decisions: Map<string, RecordedDecision>;
  resolutions: Map<string, DecisionResolution>;
  /** Last recorded intent digest per step, used to bind approvals to a payload. */
  payloadDigests: Map<string, string>;
  abortRequestedStepIds: Set<string>;
  format: "v1" | "v2";
  records: CommitRecord[];
};

type RetainedOperation = Omit<LoadedOperation, "snapshot"> & { snapshot?: undefined };
type StoredOperation = LoadedOperation | RetainedOperation;

export type OperationStoreOptions = {
  /** Bench folder; the store owns `<root>/operations`. */
  root: string;
  ownership: ExclusiveOwnership;
  /** Trusted O02 seam. Every persisted security property comes from this descriptor. */
  capabilityMetadata: CapabilityMetadataSource;
  fs?: StoreFs;
  now?: () => number;
  /** Test seam; the id is still validated before it can name a file. */
  generateOperationId?: () => string;
};

export type AcceptInput = {
  request: OperateRequest;
  context: TrustedActorContext;
  /** May narrow the frozen policy ceilings, never raise them. */
  budgets?: Partial<OperationBudgets>;
};

export type AcceptResult = { snapshot: OperationSnapshot; replayed: boolean };

export type QueueStepInput = {
  key?: string;
  capability: string;
  targetRef?: string;
  dependencies?: string[];
  queuedReason?: string;
  summary?: string;
};

export type StartStepInput = {
  /** `canonicalDigest` of the resolved args; the intent recorded before dispatch. */
  argDigest: string;
  idempotencyKey?: string;
  /** Backend-side identity, when the backend assigns one; reconciliation uses it. */
  backendOperationId?: string;
  summary?: string;
};

export type StepOutcomeInput = {
  outcome: "succeeded" | "failed";
  evidenceRefs?: string[];
  error?: OperationError;
  summary?: string;
  elapsedMs?: number;
};

export type ReconcileStepInput = {
  conclusion: "succeeded" | "failed";
  evidenceRefs?: string[];
  error?: OperationError;
  summary?: string;
};

export type RetryStepInput = StartStepInput & {
  /** Claimed descriptor policy; it must exactly match the store's trusted source. */
  retry: RetryPolicy;
};

export type CancelStepInput = { evidenceRefs: string[]; summary?: string };

export type RequireDecisionInput = {
  decisionId: string;
  decisionClass: PendingDecision["decisionClass"];
  question: string;
  /** Digest of the exact payload the decision authorizes; re-proposing changes it. */
  payloadDigest: string;
  ttlMs?: number;
  summary?: string;
};

export type ResumeInput = { request: ResumeRequest; context: TrustedActorContext };

export type ResumeResult =
  | { outcome: "dispatch"; snapshot: OperationSnapshot; stepId: string; decisionId: string; recordId: string; dispatchDigest: string }
  | { outcome: "supply_input"; snapshot: OperationSnapshot; decisionId: string; resolution: Extract<DecisionResolution, { kind: "additional_input" }> }
  | { outcome: "refuse_step"; snapshot: OperationSnapshot; stepId: string; decisionId: string; recordId: string };

export type InspectResult = {
  snapshot?: OperationSnapshot;
  events: OperationEvent[];
  decisions: RecordedDecision[];
  cursorStatus: "replay" | "caught_up" | "cursor_ahead" | "snapshot_required";
  earliestSequence: number;
  resyncSnapshot?: OperationSnapshot;
  /** Sanitized terminal state used when compacted history cannot return an OperationSnapshot. */
  retainedProjection?: RetainedTerminalProjection;
};

export type RetainedTerminalProjection = {
  operationId: string;
  revision: number;
  state: Extract<OperationState, "completed" | "partial" | "failed" | "cancelled" | "expired">;
  createdAt: number;
  updatedAt: number;
  lastSequence: number;
  result?: OperationSnapshot["result"];
};

type Tombstone = {
  v: 1;
  kind: "tombstone";
  dedupeKey: string;
  operationId: string;
  requestDigest: string;
  retained: RetainedTerminalProjection;
  earliestSequence: number;
};

type StoreTransitionInput = TransitionInput & {
  decisions?: RecordedDecision[];
  resolutions?: Array<{ decisionId: string; resolution: DecisionResolution }>;
  /** Trusted current user-turn revision; recorded by the calls that receive a context. */
  turnRevision?: number;
  abortRequestedStepIds?: string[];
};

export type AppliedChange = { snapshot: OperationSnapshot; changed: boolean };

function requestAction(request: OperateRequest): string | undefined {
  return "action" in request ? request.action : undefined;
}

function validated<T>(result: Validation<T>, what: string): T {
  if (!result.ok) throw new ContractViolation(result.issues.map((entry) => ({ ...entry, path: `${what}${entry.path.slice(1)}` })));
  return result.value;
}

function assertDigest(digest: string, what: string): string {
  if (typeof digest !== "string" || !DIGEST_RE.test(digest)) {
    throw new ContractViolation([{ path: `$.${what}`, code: "bad_syntax", message: "must be sha256:<64 hex>" }]);
  }
  return digest;
}

const ISSUE_TO_ERROR: Partial<Record<IssueCode, OperationErrorCode>> = {
  invalid_revision: "revision_conflict",
  decision_mismatch: "decision_mismatch",
  decision_expired: "decision_expired",
  decision_replayed: "decision_replayed",
  validation_failure: "validation_failure",
  permission_denied: "permission_denied",
  forged_approval: "forged_approval",
};

const COMMIT_KEYS = ["v", "kind", "at", "turnRevision", "snapshot", "events", "decisions", "resolutions", "abortRequestedStepIds"];
const RESOLUTION_KINDS = ["recorded_user_decision", "additional_input"];

function commitProblem(record: unknown): string | undefined {
  if (!isRecord(record)) return "the commit record is not an object";
  for (const key of Object.keys(record)) {
    if (!COMMIT_KEYS.includes(key)) return `unexpected commit field "${key}"`;
  }
  if (record.v !== COMMIT_FORMAT_VERSION) return `unsupported commit version ${String(record.v)}`;
  if (record.kind !== "commit") return `unexpected commit kind ${String(record.kind)}`;
  if (typeof record.at !== "number" || !Number.isFinite(record.at)) return "the commit has no timestamp";
  if (typeof record.turnRevision !== "number" || !Number.isInteger(record.turnRevision) || record.turnRevision < 1) {
    return "the commit has no trusted turn revision";
  }
  const snapshot = validateOperationSnapshot(record.snapshot);
  if (!snapshot.ok) return `snapshot: ${snapshot.issues[0].path}: ${snapshot.issues[0].message}`;
  const events = record.events;
  if (!Array.isArray(events)) return "the commit has no event list";
  if (record.abortRequestedStepIds !== undefined && (!Array.isArray(record.abortRequestedStepIds) || record.abortRequestedStepIds.some((stepId) => typeof stepId !== "string"))) {
    return "abortRequestedStepIds must be a string list";
  }
  let previous: number | undefined;
  for (const event of events) {
    const checked = validateOperationEvent(event);
    if (!checked.ok) return `event: ${checked.issues[0].path}: ${checked.issues[0].message}`;
    const sequence = validateEventSequence(previous, checked.value);
    if (!sequence.ok) return `event: ${sequence.issues[0].message}`;
    previous = checked.value.sequence;
  }
  const decisions = record.decisions;
  if (decisions !== undefined) {
    if (!Array.isArray(decisions)) return "the commit decision list is not an array";
    for (const decision of decisions) {
      const checked = validateRecordedDecision(decision);
      if (!checked.ok) return `decision: ${checked.issues[0].path}: ${checked.issues[0].message}`;
    }
  }
  const resolutions = record.resolutions;
  if (resolutions !== undefined) {
    if (!Array.isArray(resolutions)) return "the commit resolution list is not an array";
    for (const entry of resolutions) {
      if (!isRecord(entry) || typeof entry.decisionId !== "string" || !entry.decisionId) return "a resolution has no decision id";
      if (!isRecord(entry.resolution) || typeof entry.resolution.kind !== "string" || !RESOLUTION_KINDS.includes(entry.resolution.kind)) {
        return `resolution ${entry.decisionId} has an unknown form`;
      }
    }
  }
  return undefined;
}

function parseCommit(text: string, file: string, line: number, operationId?: string): CommitRecord {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw new OperationLogCorruptError(file, `line ${line} is not JSON`, operationId);
  }
  const problem = commitProblem(value);
  if (problem) throw new OperationLogCorruptError(file, `line ${line}: ${problem}`, operationId);
  return value as CommitRecord;
}

export type ReadLogResult = { records: CommitRecord[]; tornTail: boolean };

function sha256(payload: Buffer): string {
  return `sha256:${createHash("sha256").update(payload).digest("hex")}`;
}

export function encodeCommitFrame(record: CommitRecord): Buffer {
  const payload = Buffer.from(JSON.stringify(record), "utf8");
  return Buffer.concat([Buffer.from(`${payload.length} ${sha256(payload)}\n`, "ascii"), payload]);
}

function readV2(buffer: Buffer, file: string, operationId?: string): ReadLogResult {
  const records: CommitRecord[] = [];
  let offset = Buffer.byteLength(FRAME_MAGIC);
  let frame = 1;
  while (offset < buffer.length) {
    const newline = buffer.indexOf(0x0a, offset);
    if (newline < 0) return { records, tornTail: true };
    const header = buffer.subarray(offset, newline).toString("ascii");
    const match = /^(\d+) (sha256:[0-9a-f]{64})$/.exec(header);
    if (!match) throw new OperationLogCorruptError(file, `frame ${frame} has an invalid header`, operationId);
    const length = Number(match[1]);
    if (!Number.isSafeInteger(length) || length < 1) throw new OperationLogCorruptError(file, `frame ${frame} has an invalid length`, operationId);
    const start = newline + 1;
    const end = start + length;
    if (end > buffer.length) return { records, tornTail: true };
    const payload = buffer.subarray(start, end);
    if (sha256(payload) !== match[2]) throw new OperationLogCorruptError(file, `frame ${frame} checksum does not match`, operationId);
    records.push(parseCommit(payload.toString("utf8"), file, frame, operationId));
    offset = end;
    frame += 1;
  }
  return { records, tornTail: false };
}

/**
 * Reads a commit log. A final segment with no trailing newline is either a complete
 * record that lost only its terminator, or a torn write; only an LF-terminated segment
 * that will not parse is treated as interior corruption.
 */
export function readCommitLog(fsLike: StoreFs, file: string, operationId?: string): ReadLogResult {
  const buffer = fsLike.readFileSync(file);
  if (buffer.length === 0) return { records: [], tornTail: false };
  if (buffer.subarray(0, Buffer.byteLength(FRAME_MAGIC)).toString("ascii") === FRAME_MAGIC) return readV2(buffer, file, operationId);
  const lastNewline = buffer.lastIndexOf(0x0a);
  const closed = lastNewline >= 0 ? buffer.subarray(0, lastNewline) : Buffer.alloc(0);
  const tail = buffer.subarray(lastNewline + 1);
  const records: CommitRecord[] = [];
  const lines = closed.length ? closed.toString("utf8").split("\n") : [];
  lines.forEach((line, index) => {
    // An LF-terminated blank line is a lost record, not formatting: skipping it would
    // hide part of the operation's history, so it is refused like any other corruption.
    if (!line.trim()) throw new OperationLogCorruptError(file, `line ${index + 1} is blank`, operationId);
    records.push(parseCommit(line, file, index + 1, operationId));
  });
  if (!tail.length) return { records, tornTail: false };
  const text = tail.toString("utf8");
  let complete = true;
  try {
    JSON.parse(text);
  } catch {
    complete = false;
  }
  if (!complete) return { records, tornTail: true };
  records.push(parseCommit(text, file, lines.length + 1, operationId));
  return { records, tornTail: false };
}

function stepOf(snapshot: OperationSnapshot, stepId: string): StepRecord {
  const step = snapshot.steps.find((entry) => entry.stepId === stepId);
  if (!step) throw new OperationStoreError("validation_failure", `no step ${stepId} in ${snapshot.operationId}`);
  return step;
}

function reject(code: OperationErrorCode, message: string): never {
  throw new OperationStoreError(code, message);
}

function assertSafeId(operationId: string): void {
  if (!SAFE_OPERATION_ID.test(operationId) || operationId.length > 128) throw new InvalidOperationIdError(operationId);
}

function operationChange(
  snapshot: OperationSnapshot,
  to: OperationState,
  trigger: OperationTransitionTrigger,
): OperationChange | undefined {
  if (snapshot.state === to) return undefined;
  if (!canTransitionOperation(snapshot.state, to, trigger)) {
    throw new OperationStoreError("invalid_transition", `"${snapshot.state}" -> "${to}" is not permitted by "${trigger}"`);
  }
  return { to, trigger };
}

/** Where an operation stands once a supplied answer has been applied. */
function afterDecision(snapshot: OperationSnapshot): OperationChange | undefined {
  if (snapshot.state === "running") return undefined;
  if (canTransitionOperation(snapshot.state, "running", "decision_supplied")) return { to: "running", trigger: "decision_supplied" };
  if (canTransitionOperation(snapshot.state, "resolving", "resolve_started")) return { to: "resolving", trigger: "resolve_started" };
  throw new OperationStoreError("invalid_transition", `cannot continue from "${snapshot.state}" after a decision`);
}

function sameJson(left: unknown, right: unknown): boolean {
  if (left === undefined || right === undefined) return left === right;
  return canonicalDigest(left as JsonValue) === canonicalDigest(right as JsonValue);
}

function eventSupportsOperationTransition(previous: OperationSnapshot, next: OperationSnapshot, events: OperationEvent[]): boolean {
  if (previous.state === next.state) return true;
  const has = (phase: OperationEvent["phase"], code?: string) => events.some((event) => event.phase === phase && (code === undefined || event.decisionCode === code));
  const terminalSettlement = (["completed", "partial", "failed", "cancelled", "expired"] as string[]).includes(next.state) && has(next.state as OperationEvent["phase"]);
  return (
    (next.state === "resolving" && (has("resolving") || has("decision_recorded", "additional_input"))) ||
    (next.state === "running" && (has("dispatched") || has("decision_recorded", "granted") || has("decision_recorded", "additional_input"))) ||
    (next.state === "awaiting_approval" && has("approval_required")) ||
    (next.state === "needs_input" && has("needs_input", "additional_input")) ||
    (next.state === "reconciling" && has("unknown_outcome", "outcome_unknown")) ||
    (next.state === "cancel_requested" && has("cancelled", "cancel_requested")) ||
    terminalSettlement ||
    (next.state === "cancelled" && has("cancelled", "cancel_requested"))
  );
}

function operationTransitionAllowed(previous: OperationSnapshot, next: OperationSnapshot): boolean {
  return OPERATION_TRANSITIONS.some((edge) => edge.from === previous.state && edge.to === next.state);
}

function eventSupportsStepTransition(before: StepRecord, after: StepRecord, events: OperationEvent[]): boolean {
  if (before.state === after.state) return true;
  const relevant = events.filter((event) => event.stepId === after.stepId);
  const has = (phase: OperationEvent["phase"], code?: string) => relevant.some((event) => event.phase === phase && (code === undefined || event.decisionCode === code));
  if (after.state === "awaiting_approval") return has("approval_required");
  if (after.state === "running") return has("dispatched") || has("decision_recorded", "granted");
  if (after.state === "succeeded") return has("succeeded") || has("reconciled", "reconciled_succeeded");
  if (after.state === "failed") return has("failed") || has("reconciled", "reconciled_failed");
  if (after.state === "outcome_unknown") return has("unknown_outcome", "outcome_unknown");
  if (after.state === "skipped") return has("decision_recorded", "denied");
  if (after.state === "cancelled") return has("cancelled") || events.some((event) => event.phase === "cancelled" || event.phase === "expired");
  return false;
}

function replayProblem(previous: OperationSnapshot, next: OperationSnapshot, record: CommitRecord, existing?: LoadedOperation): string | undefined {
  const revisionOnlyCommit = next.revision === previous.revision;
  if (!revisionOnlyCommit && next.revision !== previous.revision + 1) return "snapshot revision does not follow its predecessor";
  for (const [name, left, right] of [
    ["operationId", previous.operationId, next.operationId],
    ["actor", previous.actor, next.actor],
    ["scope", previous.scope, next.scope],
    ["request", previous.request, next.request],
    ["requestDigest", previous.requestDigest, next.requestDigest],
    ["dedupeKey", previous.dedupeKey, next.dedupeKey],
    ["budgets", previous.budgets, next.budgets],
    ["createdAt", previous.createdAt, next.createdAt],
  ] as const) {
    if (!sameJson(left, right)) return `${name} changed after acceptance`;
  }
  if (next.updatedAt < previous.updatedAt || record.at !== next.updatedAt) return "commit timestamp is inconsistent with the snapshot";
  if (revisionOnlyCommit && !sameJson({ ...previous, lastSequence: next.lastSequence }, next)) return "same-revision commit changed lifecycle facts";
  if (!revisionOnlyCommit && record.events.length === 0) return "a meaningful snapshot change has no corresponding event";
  if (next.state !== previous.state) {
    const direct = OPERATION_TRANSITIONS.some((edge) => edge.from === previous.state && edge.to === next.state);
    const settlement = (["completed", "partial", "failed", "cancelled", "expired"] as string[]).includes(next.state) && record.events.some((event) => event.phase === next.state);
    if ((!direct && !settlement) || !eventSupportsOperationTransition(previous, next, record.events)) return `illegal or unsupported operation transition ${previous.state} -> ${next.state}`;
  }
  const beforeSteps = new Map(previous.steps.map((step) => [step.stepId, step]));
  if (next.steps.length < previous.steps.length) return "the durable step set shrank";
  for (let index = 0; index < previous.steps.length; index++) {
    if (next.steps[index]?.stepId !== previous.steps[index].stepId) return "the durable step order or identity changed";
  }
  for (const step of next.steps) {
    const before = beforeSteps.get(step.stepId);
    if (!before) continue;
    if (step.capability !== before.capability || step.capabilityVersion !== before.capabilityVersion || step.effect !== before.effect || step.key !== before.key || step.targetRef !== before.targetRef || !sameJson(step.resourceKeys, before.resourceKeys) || !sameJson(step.dependencies, before.dependencies)) {
      return `step ${step.stepId} immutable identity changed`;
    }
    if (step.state !== before.state && !STEP_TRANSITIONS.some((edge) => edge.from === before.state && edge.to === step.state)) {
      return `illegal step transition ${before.state} -> ${step.state}`;
    }
    if (!eventSupportsStepTransition(before, step, record.events)) return `step ${step.stepId} transition has no matching trigger event`;
    if (step.attempts < before.attempts || step.attempts > before.attempts + 1) return `step ${step.stepId} attempts changed illegally`;
    if (step.backendOperationId !== before.backendOperationId) {
      const retryClearsAttemptIdentity = before.backendOperationId !== undefined && step.backendOperationId === undefined && before.state === "failed" && step.state === "running" && record.events.some((event) => event.stepId === step.stepId && event.phase === "dispatched");
      if (retryClearsAttemptIdentity) continue;
      const firstAssignment = before.backendOperationId === undefined && step.backendOperationId !== undefined;
      const afterDispatch = before.state === "running" && step.state === "running" && record.events.some((event) => event.stepId === step.stepId && event.phase === "progress" && event.decisionCode === "backend_operation_assigned");
      const withDispatch = before.state !== "running" && step.state === "running" && record.events.some((event) => event.stepId === step.stepId && event.phase === "dispatched");
      if (!firstAssignment || (!afterDispatch && !withDispatch)) return `step ${step.stepId} backend identity changed`;
    }
  }
  for (const pending of next.pendingDecisions) {
    if (pending.operationId !== next.operationId || !next.steps.some((step) => step.stepId === pending.stepId)) return "pending decision binding is inconsistent";
    if (!record.events.some((event) => event.stepId === pending.stepId && (event.phase === "needs_input" || event.phase === "approval_required"))) {
      const existed = previous.pendingDecisions.some((entry) => entry.decisionId === pending.decisionId);
      if (!existed) return "a new pending decision has no corresponding event";
    }
  }
  for (const outcome of next.unknownOutcomes) {
    const step = next.steps.find((entry) => entry.stepId === outcome.stepId);
    if (!step || step.state !== "outcome_unknown" || step.capability !== outcome.capability) return "unknown outcome binding is inconsistent";
  }
  for (const before of previous.unknownOutcomes) {
    const after = next.unknownOutcomes.find((entry) => entry.stepId === before.stepId);
    if (after && !sameJson(before, after)) return `unknown outcome ${before.stepId} changed after recording`;
  }
  const seenDecisionIds = new Set<string>();
  for (const decision of record.decisions ?? []) {
    const prior = existing?.decisions.get(decision.recordId);
    if (seenDecisionIds.has(decision.recordId) || (prior && prior.usedAt !== undefined)) return `duplicate decision record ${decision.recordId}`;
    if (prior && !sameJson({ ...prior, usedAt: decision.usedAt }, decision)) return `decision record ${decision.recordId} changed immutable fields`;
    seenDecisionIds.add(decision.recordId);
    const pending = previous.pendingDecisions.find((entry) => entry.decisionId === decision.decisionId);
    if (!prior && (!pending || pending.stepId !== decision.stepId || pending.revision !== decision.revision)) return "recorded decision does not bind the predecessor pending decision";
  }
  for (const resolution of record.resolutions ?? []) {
    if (!previous.pendingDecisions.some((decision) => decision.decisionId === resolution.decisionId)) return "resolution does not answer a predecessor decision";
    if (next.pendingDecisions.some((decision) => decision.decisionId === resolution.decisionId)) return "resolved decision remains pending";
    if (!record.events.some((event) => event.phase === "decision_recorded")) return "resolution has no corresponding decision event";
  }
  for (const event of record.events) {
    if (event.revision !== next.revision || event.at !== record.at) return "event revision or timestamp is inconsistent with its commit";
    if (event.stepId && !next.steps.some((step) => step.stepId === event.stepId)) return `event names unknown step ${event.stepId}`;
    if (event.stepId && event.capability !== stepOf(next, event.stepId).capability) return "event capability disagrees with its step";
  }
  if (next.state !== previous.state && !record.events.some((event) => event.phase === next.state || (next.state === "running" && (event.phase === "dispatched" || event.phase === "decision_recorded")) || (next.state === "resolving" && event.phase === "decision_recorded") || (next.state === "reconciling" && event.phase === "unknown_outcome") || (next.state === "cancel_requested" && event.phase === "cancelled" && event.decisionCode === "cancel_requested") || (next.state === "awaiting_approval" && event.phase === "approval_required") || (next.state === "needs_input" && event.phase === "needs_input"))) {
    return "operation state change has no corresponding event";
  }
  return undefined;
}

/**
 * The durable operation store. One instance owns one bench folder; every write
 * asserts the exclusive folder ownership first, and acceptance is on disk before it
 * returns an operation id.
 */
export class OperationStore {
  readonly root: string;
  readonly directory: string;
  #fs: StoreFs;
  #ownership: ExclusiveOwnership;
  #capabilityMetadata: CapabilityMetadataSource;
  #now: () => number;
  #generateId: () => string;
  #operations = new Map<string, StoredOperation>();
  #dedupe = new Map<string, { operationId: string; requestDigest: string }>();
  #tombstones = new Map<string, Tombstone>();
  #tornTails: string[] = [];
  #ignoredEntries: string[] = [];
  #repairs: Array<{ file: string; action: "terminated" | "truncated" }> = [];
  #failedAppend: Error | undefined;
  #closed = false;

  constructor(options: OperationStoreOptions) {
    this.root = options.root;
    this.directory = path.join(options.root, "operations");
    this.#fs = options.fs ?? nodeStoreFs;
    this.#ownership = options.ownership;
    this.#capabilityMetadata = options.capabilityMetadata;
    this.#now = options.now ?? (() => Date.now());
    this.#generateId = options.generateOperationId ?? (() => `op-${randomUUID()}`);
    this.#scan();
  }

  get ownerId(): string {
    return this.#ownership.ownerId;
  }

  /** Files whose final record never finished writing, detected when the store opened. */
  get tornTails(): readonly string[] {
    return this.#tornTails;
  }

  /** Entries in the operations directory that are not operation logs. */
  get ignoredEntries(): readonly string[] {
    return this.#ignoredEntries;
  }

  /** Torn tails this process has since terminated or truncated before appending. */
  get repairs(): ReadonlyArray<{ file: string; action: "terminated" | "truncated" }> {
    return this.#repairs;
  }

  close(): void {
    this.#closed = true;
  }

  /** The trusted policy the store consults before a recorded approval may dispatch. */
  approvalPolicyFor(capability: string, version: string): ApprovalRequirement {
    return this.#metadata(capability, version).approval;
  }

  /** Trusted retry policy used by recovery and dispatch. */
  retryPolicyFor(capability: string, version: string): RetryPolicy {
    return this.#metadata(capability, version).retry;
  }

  /** Recovery and dispatch call this before acting on any durable operation state. */
  assertOwnership(): void {
    this.#assertWritable();
  }

  operationIds(): string[] {
    return [...this.#operations.keys()].filter((operationId) => !this.#tombstones.has(operationId)).sort();
  }

  load(operationId: string): OperationSnapshot {
    return this.#requireSnapshot(operationId);
  }

  /** Snapshot plus replay from a cursor: only events after `afterSequence`. */
  inspect(operationId: string, afterSequence = 0): InspectResult {
    const operation = this.#require(operationId);
    const tombstone = this.#tombstones.get(operationId);
    const lastSequence = tombstone?.retained.lastSequence ?? this.#requireSnapshot(operationId).lastSequence;
    const earliestSequence = tombstone?.earliestSequence ?? operation.events[0]?.sequence ?? lastSequence;
    const cursorStatus = afterSequence > lastSequence ? "cursor_ahead" : afterSequence === lastSequence ? "caught_up" : tombstone && afterSequence < earliestSequence ? "snapshot_required" : "replay";
    return {
      ...(!tombstone ? { snapshot: this.#requireSnapshot(operationId) } : {}),
      events: cursorStatus === "replay" ? operation.events.filter((event) => event.sequence > afterSequence) : [],
      decisions: [...operation.decisions.values()],
      cursorStatus,
      earliestSequence,
      ...(tombstone ? { retainedProjection: tombstone.retained } : cursorStatus === "snapshot_required" ? { resyncSnapshot: this.#requireSnapshot(operationId) } : {}),
    };
  }

  events(operationId: string): OperationEvent[] {
    return [...this.#require(operationId).events];
  }

  pendingDecisions(operationId: string): PendingDecision[] {
    return [...this.#requireSnapshot(operationId).pendingDecisions];
  }

  recordedDecisions(operationId: string): RecordedDecision[] {
    return [...this.#require(operationId).decisions.values()];
  }

  resolutionsFor(operationId: string): Array<{ decisionId: string; resolution: DecisionResolution }> {
    const operation = this.#require(operationId);
    return [...operation.resolutions.entries()].map(([decisionId, resolution]) => ({ decisionId, resolution }));
  }

  /** Digest of the last args recorded for a step; the payload an approval binds. */
  stepPayloadDigest(operationId: string, stepId: string): string | undefined {
    return this.#require(operationId).payloadDigests.get(stepId);
  }

  /** Running steps whose backend abort must resume after restart. */
  pendingAbortStepIds(operationId: string): string[] {
    return [...this.#require(operationId).abortRequestedStepIds];
  }

  /** The compact, model-facing projection of an operation. */
  project(operationId: string): CompactOperationResult {
    return compactView(this.#requireSnapshot(operationId));
  }

  /** Records the truthful terminal state when the settled work supports one. */
  settle(operationId: string): AppliedChange {
    const now = this.#now();
    const direct = this.#apply(operationId, { now, settle: true });
    if (direct.changed) return direct;
    const snapshot = direct.snapshot;
    const target = settledState(snapshot);
    // Some settled states have no direct edge out of a waiting state: the table routes
    // them through `cancel_requested` (a cancellation) or `resolving` (continued work).
    if (target === "cancelled" && canTransitionOperation(snapshot.state, "cancel_requested", "cancel_requested")) {
      return this.#apply(operationId, {
        now,
        operation: { to: "cancel_requested", trigger: "cancel_requested" },
        settle: true,
        events: [{ phase: "cancelled", decisionCode: "cancel_requested", summary: "Cancelling the remaining work." }],
      });
    }
    if (target !== undefined && canTransitionOperation(snapshot.state, "resolving", "resolve_started")) {
      return this.#apply(operationId, {
        now,
        operation: { to: "resolving", trigger: "resolve_started" },
        settle: true,
        events: [{ phase: "resolving", summary: "Continuing after the pending question was resolved." }],
      });
    }
    return direct;
  }

  /**
   * Explicit retention boundary: whole terminal operations only. Active history is
   * never rewritten or removed, so recovery evidence for unfinished work survives.
   */
  removeTerminalOperation(operationId: string): void {
    this.#assertWritable();
    const operation = this.#require(operationId);
    const snapshot = this.#requireSnapshot(operationId);
    if (!isTerminalOperationState(snapshot.state)) {
      throw new OperationStoreError(
        "invalid_transition",
        `${operationId} is ${snapshot.state}; only a terminal operation may be removed`,
      );
    }
    const tombstone: Tombstone = {
      v: 1,
      kind: "tombstone",
      dedupeKey: snapshot.dedupeKey,
      operationId,
      requestDigest: snapshot.requestDigest,
      retained: this.#retainedTerminal(snapshot),
      earliestSequence: snapshot.lastSequence,
    };
    this.#appendTombstone(tombstone);
    try {
      this.#fs.unlinkSync(operation.file);
      this.#fsyncDirectory(this.directory);
    } catch (error) {
      this.#failedAppend = error as Error;
      throw error;
    }
    this.#operations.set(operationId, this.#retainedOperation(tombstone, path.join(this.directory, "tombstones.log")));
    this.#tombstones.set(operationId, tombstone);
  }

  /** Bounded explicit retention pass; callers own scheduling and policy. */
  pruneTerminalOperations(input: { before: number; limit: number }): string[] {
    this.#assertWritable();
    if (!Number.isFinite(input.before) || !Number.isInteger(input.limit) || input.limit < 0) {
      throw new OperationStoreError("validation_failure", "terminal pruning needs a finite before time and a non-negative integer limit");
    }
    const selected = [...this.#operations.entries()]
      .filter(([operationId, operation]) => operation.snapshot && !this.#tombstones.has(operationId) && isTerminalOperationState(operation.snapshot.state) && operation.snapshot.updatedAt < input.before)
      .sort((left, right) => left[1].snapshot!.updatedAt - right[1].snapshot!.updatedAt || left[0].localeCompare(right[0]))
      .slice(0, input.limit)
      .map(([operationId]) => operationId);
    for (const operationId of selected) this.removeTerminalOperation(operationId);
    return selected;
  }

  #retainedTerminal(snapshot: OperationSnapshot): RetainedTerminalProjection {
    if (!isTerminalOperationState(snapshot.state)) throw new OperationStoreError("invalid_transition", `${snapshot.operationId} is not terminal`);
    return {
      operationId: snapshot.operationId,
      revision: snapshot.revision,
      state: snapshot.state as RetainedTerminalProjection["state"],
      createdAt: snapshot.createdAt,
      updatedAt: snapshot.updatedAt,
      lastSequence: snapshot.lastSequence,
      ...(snapshot.result ? { result: snapshot.result } : {}),
    };
  }

  #retainedOperation(tombstone: Tombstone, file: string): RetainedOperation {
    return {
      file,
      turnRevision: 1,
      events: [],
      decisions: new Map(),
      resolutions: new Map(),
      payloadDigests: new Map(),
      abortRequestedStepIds: new Set(),
      format: "v2",
      records: [],
    };
  }

  /**
   * Durable acceptance: dedupe by the trusted tool-call tuple, reject the same key with
   * a different request, fsync the new operation log (and its directory) before the id
   * is returned.
   */
  accept(input: AcceptInput): AcceptResult {
    this.#assertWritable();
    const request = validated(validateOperateRequest(input.request), "$.request");
    const action = requestAction(request);
    if (action !== undefined) {
      throw new OperationStoreError("unsupported_request", `"${action}" is a control action, not a new operation`);
    }
    const context = validated(validateTrustedActorContext(input.context), "$.context");
    const budgets = validated(validateBudgets(input.budgets ? { ...DEFAULT_BUDGETS, ...input.budgets } : DEFAULT_BUDGETS), "$.budgets");
    const digest = requestDigest(request);
    const dedupeKey = deriveDeduplicationKey(context);
    const existing = this.#dedupe.get(dedupeKey);
    if (existing) {
      if (existing.requestDigest !== digest) throw new DuplicateRequestConflictError(dedupeKey, existing.operationId);
      const operation = this.#operations.get(existing.operationId);
      if (!operation) {
        throw new OperationLogCorruptError(this.#file(existing.operationId), "the deduplicated operation is missing", existing.operationId);
      }
      return { snapshot: operation.snapshot ?? this.#retainedReplaySnapshot(existing.operationId), replayed: true };
    }
    const now = this.#now();
    const operationId = this.#newOperationId();
    const accepted = validated(
      validateOperationSnapshot({
        contractVersion: CONTRACT_VERSION,
        operationId,
        revision: 1,
        state: "accepted",
        createdAt: now,
        updatedAt: now,
        deadlineAt: now + budgets.operationDeadlineMs,
        actor: {
          actorId: context.actorId,
          tenantId: context.tenantId,
          sessionId: context.sessionId,
          turnId: context.turnId,
        },
        scope: { ...context.scope },
        request,
        requestDigest: digest,
        dedupeKey,
        budgets,
        steps: [],
        pendingDecisions: [],
        unknownOutcomes: [],
        usage: { steps: 0, selectionRounds: 0, generationCalls: 0, attempts: 0 },
        lastSequence: 1,
      }),
      "$.operation",
    );
    const record: CommitRecord = {
      v: COMMIT_FORMAT_VERSION,
      kind: "commit",
      at: now,
      turnRevision: context.turnRevision,
      snapshot: accepted,
      events: [{ operationId, sequence: 1, at: now, phase: "accepted", revision: 1, summary: "Operation accepted." }],
    };
    this.#append(operationId, record);
    this.#fold(operationId, record, this.#file(operationId));
    return { snapshot: accepted, replayed: false };
  }

  beginResolution(operationId: string): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    if (snapshot.state === "resolving" || snapshot.state === "running") return snapshot;
    return this.#apply(operationId, {
      now: this.#now(),
      operation: operationChange(snapshot, "resolving", "resolve_started"),
      events: [{ phase: "resolving", summary: "Resolving the request against permitted capabilities." }],
    }).snapshot;
  }

  queueStep(operationId: string, input: QueueStepInput): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    if (isTerminalOperationState(snapshot.state)) {
      throw new OperationStoreError("invalid_transition", `cannot add work to a ${snapshot.state} operation`);
    }
    if (snapshot.steps.length + 1 > snapshot.budgets.maxSteps) {
      throw new OperationStoreError("budget_exceeded", `the operation allows ${snapshot.budgets.maxSteps} steps`);
    }
    if (input.key !== undefined && snapshot.steps.some((step) => step.key === input.key)) {
      throw new OperationStoreError("validation_failure", `duplicate step key "${input.key}"`);
    }
    const stepId = `step-${snapshot.steps.length + 1}`;
    const metadata = this.#metadata(input.capability);
    const step = validated(
      validateStepRecord({
        stepId,
        state: "queued",
        capability: input.capability,
        capabilityVersion: metadata.version,
        effect: metadata.effect,
        attempts: 0,
        ...(input.key !== undefined ? { key: input.key } : {}),
        ...(input.targetRef !== undefined ? { targetRef: input.targetRef } : {}),
        ...(metadata.resourceKeys?.length ? { resourceKeys: metadata.resourceKeys } : {}),
        ...(input.dependencies?.length ? { dependencies: input.dependencies } : {}),
        ...(input.queuedReason !== undefined ? { queuedReason: input.queuedReason } : {}),
      }),
      "$.step",
    );
    const event: NewEvent = {
      phase: "queued",
      stepId,
      capability: step.capability,
      summary: input.summary ?? `Queued ${step.capability} as ${stepId}.`,
      ...(input.queuedReason !== undefined ? { queueReason: input.queuedReason } : {}),
      ...(input.dependencies?.length ? { dependencies: input.dependencies } : {}),
    };
    return this.#apply(operationId, {
      now: this.#now(),
      addSteps: [step],
      usage: { ...snapshot.usage, steps: snapshot.steps.length + 1 },
      events: [event],
    }).snapshot;
  }

  /** Persists the dispatch intent (approved args digest, idempotency key) before any call. */
  startStep(operationId: string, stepId: string, input: StartStepInput): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    const step = stepOf(snapshot, stepId);
    const digest = assertDigest(input.argDigest, "argDigest");
    const approval = this.#metadata(step.capability, step.capabilityVersion).approval;
    if (step.effect !== "read" && approval !== "none") {
      throw new OperationStoreError(
        "permission_denied",
        `${step.capability} requires a recorded ${approval === "user" ? "user" : "trusted policy"} decision; dispatch it through resume after validating the current approval`,
      );
    }
    const change: StepChange = {
      stepId,
      to: "running",
      trigger: "dispatch_started",
      dispatchDigest: digest,
      patch: {
        idempotencyKey: input.idempotencyKey ?? `${operationId}/${stepId}/${step.attempts + 1}`,
        ...(input.backendOperationId !== undefined ? { backendOperationId: input.backendOperationId } : {}),
      },
    };
    return this.#apply(operationId, {
      now: this.#now(),
      steps: [change],
      operation: operationChange(snapshot, "running", "dispatch_started"),
      usage: { ...snapshot.usage, attempts: snapshot.usage.attempts + 1 },
      events: [
        {
          phase: "dispatched",
          stepId,
          capability: step.capability,
          summary: input.summary ?? `Dispatching ${step.capability}.`,
          argDigest: digest,
          retryCount: step.attempts,
        },
      ],
    }).snapshot;
  }

  /** Records the backend identity returned after dispatch; the binding is write-once. */
  recordBackendOperationId(operationId: string, stepId: string, id: string): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    const step = stepOf(snapshot, stepId);
    if (step.state !== "running") throw new OperationStoreError("invalid_transition", `step ${stepId} is ${step.state}; backend identity follows dispatch`);
    if (step.backendOperationId !== undefined) throw new OperationStoreError("invalid_transition", `step ${stepId} already has a backend operation id`);
    if (!id || id.length > 256) throw new OperationStoreError("validation_failure", "backend operation id must contain 1 to 256 characters");
    const digest = this.stepPayloadDigest(operationId, stepId);
    if (!digest) throw new OperationStoreError("validation_failure", `step ${stepId} has no recorded dispatch digest`);
    return this.#apply(operationId, {
      now: this.#now(),
      steps: [{ stepId, to: "running", trigger: "dispatch_started", dispatchDigest: digest, patch: { backendOperationId: id } }],
      events: [{ phase: "progress", stepId, capability: step.capability, decisionCode: "backend_operation_assigned", summary: `Recorded backend identity for ${step.capability}.` }],
    }).snapshot;
  }

  /** Records an observed outcome after dispatch; never a substitute for one. */
  recordStepOutcome(operationId: string, stepId: string, input: StepOutcomeInput): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    const step = stepOf(snapshot, stepId);
    const error = input.outcome === "failed" ? validated(validateOperationError(input.error), "$.error") : undefined;
    const retry = this.#metadata(step.capability, step.capabilityVersion).retry;
    const retryAvailable =
      input.outcome === "failed" && error?.retryable === true && retry.class === "idempotent" && step.attempts < retry.maxAttempts;
    const change: StepChange =
      input.outcome === "succeeded"
        ? {
            stepId,
            to: "succeeded",
            trigger: "result_observed",
            patch: { ...(input.evidenceRefs?.length ? { evidenceRefs: input.evidenceRefs } : {}) },
          }
        : {
            stepId,
            to: "failed",
            trigger: "result_failed",
            patch: { error },
          };
    return this.#apply(operationId, {
      now: this.#now(),
      steps: [change],
      settle: true,
      settleFailed: !retryAvailable,
      events: [
        {
          phase: input.outcome,
          stepId,
          capability: step.capability,
          summary: input.summary ?? `${step.capability} ${input.outcome}.`,
          retryCount: step.attempts,
          ...(input.evidenceRefs?.length ? { evidenceRefs: input.evidenceRefs } : {}),
          ...(input.elapsedMs !== undefined ? { elapsedMs: input.elapsedMs } : {}),
        },
      ],
    }).snapshot;
  }

  /**
   * Records that a dispatched write did not report an outcome. The step leaves only
   * through `reconcile_conclusive`: an unknown effect is never retried or cancelled away.
   */
  markOutcomeUnknown(operationId: string, stepId: string, input: { summary?: string } = {}): OperationSnapshot {
    const operation = this.#require(operationId);
    const snapshot = this.#requireSnapshot(operationId);
    const step = stepOf(snapshot, stepId);
    const digest = operation.payloadDigests.get(stepId);
    const now = this.#now();
    const outcome: UnknownOutcome = {
      stepId,
      capability: step.capability,
      since: now,
      ...(digest !== undefined ? { dispatchDigest: digest } : {}),
    };
    return this.#apply(operationId, {
      now,
      steps: [{ stepId, to: "outcome_unknown", trigger: "outcome_unknown" }],
      addUnknownOutcomes: [outcome],
      operation: operationChange(snapshot, "reconciling", "unknown_outcome"),
      events: [
        {
          phase: "unknown_outcome",
          stepId,
          capability: step.capability,
          summary: input.summary ?? `${step.capability} reported no outcome; it will be reconciled, never retried.`,
          decisionCode: "outcome_unknown",
          elapsedMs: Math.max(0, now - (step.startedAt ?? now)),
          ...(digest !== undefined ? { argDigest: digest } : {}),
        },
      ],
    }).snapshot;
  }

  /** Closes an unknown outcome with backend evidence instead of replaying the call. */
  reconcileStep(operationId: string, stepId: string, input: ReconcileStepInput): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    const step = stepOf(snapshot, stepId);
    if (input.conclusion === "failed" && !input.error) {
      throw new ContractViolation([{ path: "$.error", code: "missing_field", message: "a reconciled failure records an error" }]);
    }
    const patch =
      input.conclusion === "succeeded"
        ? { ...(input.evidenceRefs?.length ? { evidenceRefs: input.evidenceRefs } : {}) }
        : { error: validated(validateOperationError(input.error), "$.error") };
    return this.#apply(operationId, {
      now: this.#now(),
      steps: [{ stepId, to: input.conclusion, trigger: "reconcile_conclusive", patch }],
      removeUnknownOutcomes: [stepId],
      settle: true,
      settleFailed: true,
      events: [
        {
          phase: "reconciled",
          stepId,
          capability: step.capability,
          decisionCode: `reconciled_${input.conclusion}`,
          summary: input.summary ?? `${step.capability} was reconciled as ${input.conclusion}.`,
          ...(input.evidenceRefs?.length ? { evidenceRefs: input.evidenceRefs } : {}),
        },
      ],
    }).snapshot;
  }

  /**
   * Restarts a failed step under its declared retry class. `none` and
   * `reconcile_required` never restart, an unknown outcome is not a failure and cannot
   * reach here, and the attempt count is checked before a new intent is recorded.
   */
  retryStep(operationId: string, stepId: string, input: RetryStepInput, currentContext: TrustedActorContext): OperationSnapshot {
    const loaded = this.#require(operationId);
    const snapshot = this.#requireSnapshot(operationId);
    const context = validated(validateTrustedActorContext(currentContext), "$.context");
    if (context.actorId !== snapshot.actor.actorId || context.tenantId !== snapshot.actor.tenantId || context.sessionId !== snapshot.actor.sessionId) {
      throw new OperationStoreError("permission_denied", "only the operation's current actor, tenant, and session may retry it");
    }
    if (context.turnRevision < loaded.turnRevision) {
      throw new OperationStoreError("revision_conflict", `user turn ${context.turnRevision} was superseded by turn ${loaded.turnRevision}`);
    }
    const step = stepOf(snapshot, stepId);
    if (step.state !== "failed") {
      throw new OperationStoreError("invalid_transition", `step ${stepId} is ${step.state}; only a settled failure declares a retry`);
    }
    const now = this.#now();
    if (snapshot.deadlineAt !== undefined && now >= snapshot.deadlineAt) {
      throw new OperationStoreError("deadline_exceeded", "the operation deadline has passed");
    }
    if (!step.error?.retryable) {
      throw new OperationStoreError("invalid_transition", `step ${stepId} did not record a retryable failure`);
    }
    const retry = this.#metadata(step.capability, step.capabilityVersion).retry;
    if (input.retry.class !== retry.class || input.retry.maxAttempts !== retry.maxAttempts) {
      throw new OperationStoreError(
        "invalid_transition",
        `${step.capability} declares ${retry.class} with ${retry.maxAttempts} attempts; caller retry claims cannot change trusted policy`,
      );
    }
    const digest = assertDigest(input.argDigest, "argDigest");
    const approval = this.#metadata(step.capability, step.capabilityVersion).approval;
    if (step.effect !== "read" && approval !== "none") {
      const source = approval === "user" ? "user_ui" : "trusted_policy";
      const matching = [...loaded.decisions.values()].filter(
        (decision) =>
          decision.stepId === stepId &&
          decision.actorId === snapshot.actor.actorId &&
          decision.tenantId === snapshot.actor.tenantId &&
          decision.sessionId === snapshot.actor.sessionId &&
          decision.outcome === "granted" &&
          decision.policySource === source &&
          decision.payloadDigest === digest &&
          decision.usedAt !== undefined &&
          decision.revision < snapshot.revision,
      );
      if (!matching.some((decision) => decision.expiresAt > now && decision.revision <= snapshot.revision)) {
        if (matching.length) {
          throw new OperationStoreError("decision_expired", `the consumed ${source} approval for ${step.capability} has expired`);
        }
        throw new OperationStoreError(
          "permission_denied",
          `${step.capability} requires a consumed ${source} approval for the exact retry payload`,
        );
      }
    }
    const change: StepChange = {
      stepId,
      to: "running",
      trigger: "retry_allowed",
      dispatchDigest: digest,
      retry,
      patch: {
        idempotencyKey: input.idempotencyKey ?? `${operationId}/${stepId}/${step.attempts + 1}`,
        ...(input.backendOperationId !== undefined ? { backendOperationId: input.backendOperationId } : {}),
      },
    };
    return this.#apply(operationId, {
      now,
      turnRevision: context.turnRevision,
      steps: [change],
      operation: operationChange(snapshot, "running", "dispatch_started"),
      usage: { ...snapshot.usage, attempts: snapshot.usage.attempts + 1 },
      events: [
        {
          phase: "dispatched",
          stepId,
          capability: step.capability,
          decisionCode: "retry_allowed",
          summary: input.summary ?? `Retrying ${step.capability} under its declared ${retry.class} retry class.`,
          argDigest: digest,
          retryCount: step.attempts,
        },
      ],
    }).snapshot;
  }

  /** Cancels a running step only with evidence that no effect was applied. */
  cancelStep(operationId: string, stepId: string, input: CancelStepInput): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    const step = stepOf(snapshot, stepId);
    if (!input.evidenceRefs?.length) {
      throw new ContractViolation([{ path: "$.evidenceRefs", code: "missing_field", message: "a cancelled running step needs evidence" }]);
    }
    return this.#apply(operationId, {
      now: this.#now(),
      steps: [
        {
          stepId,
          to: "cancelled",
          trigger: "cancel_confirmed",
          cancelEvidence: input.evidenceRefs,
          patch: { evidenceRefs: input.evidenceRefs },
        },
      ],
      settle: true,
      events: [
        {
          phase: "cancelled",
          stepId,
          capability: step.capability,
          decisionCode: "cancel_confirmed",
          summary: input.summary ?? `${step.capability} was cancelled before it applied an effect.`,
          evidenceRefs: input.evidenceRefs,
        },
      ],
    }).snapshot;
  }

  /**
   * Records the pending question a step is waiting on. Re-issuing the same `decisionId`
   * with another payload digest supersedes the old binding, so a later approval of the
   * first payload is refused rather than silently reused.
   */
  requireDecision(operationId: string, stepId: string, input: RequireDecisionInput): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    const step = stepOf(snapshot, stepId);
    const digest = assertDigest(input.payloadDigest, "payloadDigest");
    const now = this.#now();
    if (snapshot.deadlineAt !== undefined && snapshot.deadlineAt <= now) {
      throw new OperationStoreError("deadline_exceeded", "the operation deadline has passed");
    }
    const ttl = input.ttlMs ?? DEFAULT_DECISION_TTL_MS;
    if (!Number.isInteger(ttl) || ttl <= 0) {
      throw new ContractViolation([{ path: "$.ttlMs", code: "out_of_range", message: "ttlMs must be a positive integer" }]);
    }
    const expiresAt = snapshot.deadlineAt !== undefined ? Math.min(now + ttl, snapshot.deadlineAt) : now + ttl;
    const pending = validated(
      validatePendingDecision({
        decisionId: input.decisionId,
        operationId,
        stepId,
        decisionClass: input.decisionClass,
        question: input.question,
        createdAt: now,
        expiresAt,
        revision: snapshot.revision + 1,
      }),
      "$.decision",
    );
    let steps: StepChange[] | undefined;
    let operation: OperationChange | undefined;
    if (input.decisionClass === "additional_input") {
      if (step.state !== "queued" && step.state !== "awaiting_approval") {
        throw new OperationStoreError("invalid_transition", `step ${stepId} is ${step.state}; it cannot wait for input`);
      }
      operation = operationChange(snapshot, "needs_input", "decision_required");
    } else {
      if (step.state === "queued") steps = [{ stepId, to: "awaiting_approval", trigger: "approval_required" }];
      else if (step.state !== "awaiting_approval") {
        throw new OperationStoreError("invalid_transition", `step ${stepId} is ${step.state}; it cannot await approval`);
      }
      operation = operationChange(snapshot, "awaiting_approval", "approval_required");
    }
    const superseded = snapshot.pendingDecisions.filter((decision) => decision.stepId === stepId).map((decision) => decision.decisionId);
    return this.#apply(operationId, {
      now,
      operation,
      steps,
      addDecisions: [pending],
      removeDecisions: superseded,
      events: [
        {
          phase: input.decisionClass === "additional_input" ? "needs_input" : "approval_required",
          stepId,
          capability: step.capability,
          summary: input.summary ?? input.question,
          decisionCode: input.decisionClass,
          argDigest: digest,
        },
      ],
    }).snapshot;
  }

  /**
   * Stores a decision recorded by the authenticated user UI or the trusted policy
   * adapter. Every binding is checked here as well as at resume: owner, tenant,
   * session, operation, step, question, revision, payload digest, and expiry.
   */
  recordDecision(record: RecordedDecision, currentContext: TrustedActorContext): OperationSnapshot {
    this.#assertWritable();
    const decision = validated(validateRecordedDecision(record), "$.decision");
    const operation = this.#require(decision.operationId);
    const snapshot = this.#requireSnapshot(decision.operationId);
    const now = this.#now();
    const current = validated(validateTrustedActorContext(currentContext), "$.context");
    if (current.actorId !== snapshot.actor.actorId || current.tenantId !== snapshot.actor.tenantId || current.sessionId !== snapshot.actor.sessionId) {
      reject("decision_mismatch", "the decision is not being recorded in the operation's current actor, tenant, and session");
    }
    if (
      snapshot.actor.actorId !== decision.actorId ||
      snapshot.actor.tenantId !== decision.tenantId ||
      snapshot.actor.sessionId !== decision.sessionId
    ) {
      reject("decision_mismatch", "the decision was recorded by another actor, tenant, or session");
    }
    if (operation.decisions.has(decision.recordId)) {
      reject("decision_replayed", `decision record ${decision.recordId} was already recorded`);
    }
    if (decision.usedAt !== undefined) reject("decision_replayed", `decision record ${decision.recordId} was already consumed`);
    const pending = snapshot.pendingDecisions.find((entry) => entry.decisionId === decision.decisionId);
    if (!pending) reject("decision_mismatch", `no pending decision ${decision.decisionId} in ${decision.operationId}`);
    if (pending.stepId !== decision.stepId) reject("decision_mismatch", "the decision names another step");
    if (pending.decisionClass !== decision.decisionClass) reject("decision_mismatch", "the decision answers another class of question");
    if (decision.revision !== snapshot.revision) {
      reject("revision_conflict", `the decision was recorded for revision ${decision.revision}; the operation is at ${snapshot.revision}`);
    }
    const digest = operation.payloadDigests.get(decision.stepId);
    if (!digest) reject("validation_failure", `step ${decision.stepId} has no recorded approved payload`);
    if (digest !== decision.payloadDigest) reject("validation_failure", "the decision does not bind the step's current approved payload");
    const bound = snapshot.deadlineAt !== undefined ? Math.min(pending.expiresAt, snapshot.deadlineAt) : pending.expiresAt;
    if (decision.expiresAt > bound) reject("permission_denied", "the decision outlives the pending question or the operation deadline");
    if (decision.recordedAt < pending.createdAt) reject("decision_mismatch", "the decision predates the pending question");
    if (decision.recordedAt > now) reject("decision_mismatch", "the decision is future dated");
    if (decision.expiresAt <= now) reject("decision_expired", "the decision has expired");
    const step = stepOf(snapshot, decision.stepId);
    return this.#apply(decision.operationId, {
      now,
      bumpRevision: false,
      decisions: [decision],
      events: [
        {
          phase: "decision_recorded",
          stepId: decision.stepId,
          capability: step.capability,
          decisionCode: decision.outcome,
          summary: `Recorded a ${decision.outcome} decision from ${decision.policySource}.`,
        },
      ],
    }).snapshot;
  }

  /**
   * Applies a cited decision. A citation that checks out is not yet permission: the
   * recorded policy source must still satisfy the step's current trusted approval
   * requirement, and a valid denial skips the step instead of dispatching it.
   */
  resume(input: ResumeInput): ResumeResult {
    this.#assertWritable();
    const request = validated(validateOperateRequest(input.request), "$.request");
    if (!("action" in request) || request.action !== "resume") {
      throw new OperationStoreError("unsupported_request", "resume requires a resume request");
    }
    const context = validated(validateTrustedActorContext(input.context), "$.context");
    const operation = this.#require(request.operationId);
    const snapshot = this.#requireSnapshot(request.operationId);
    if (snapshot.actor.actorId !== context.actorId || snapshot.actor.tenantId !== context.tenantId) {
      throw new OperationStoreError("permission_denied", "this actor or tenant does not own the operation");
    }
    if (snapshot.actor.sessionId !== context.sessionId) {
      throw new OperationStoreError("permission_denied", "only the operation's current session may resume it");
    }
    if (context.turnRevision < operation.turnRevision) {
      throw new OperationStoreError(
        "revision_conflict",
        `user turn ${context.turnRevision} was superseded by turn ${operation.turnRevision}; only the current trusted turn may resume`,
      );
    }
    const pending = snapshot.pendingDecisions.find((entry) => entry.decisionId === request.decisionId);
    if (!pending) throw new OperationStoreError("decision_mismatch", `no pending decision ${request.decisionId} in ${request.operationId}`);
    if (pending.revision !== snapshot.revision) {
      throw new OperationStoreError("revision_conflict", "the pending decision belongs to another revision");
    }
    if (request.expectedRevision !== snapshot.revision) {
      throw new OperationStoreError(
        "revision_conflict",
        `expected revision ${request.expectedRevision}; the operation is at ${snapshot.revision}`,
      );
    }
    if (!resolutionCanResolve(request.resolution, pending.decisionClass)) {
      throw new OperationStoreError(
        "decision_mismatch",
        `a ${request.resolution.kind} resolution cannot answer a ${pending.decisionClass} decision`,
      );
    }
    const step = stepOf(snapshot, pending.stepId);
    const now = this.#now();
    if (pending.expiresAt <= now || (snapshot.deadlineAt !== undefined && snapshot.deadlineAt <= now)) {
      throw new OperationStoreError("decision_expired", "the pending decision has expired");
    }
    if (request.resolution.kind === "additional_input") {
      const expectedDigest = operation.payloadDigests.get(pending.stepId);
      if (!expectedDigest || expectedDigest !== canonicalDigest(request.resolution)) {
        throw new OperationStoreError("validation_failure", "the supplied input does not match the pending decision payload");
      }
      const applied = this.#apply(snapshot.operationId, {
        now,
        turnRevision: context.turnRevision,
        operation: afterDecision(snapshot),
        removeDecisions: [pending.decisionId],
        resolutions: [{ decisionId: pending.decisionId, resolution: request.resolution }],
        settle: true,
        events: [
          {
            phase: "decision_recorded",
            stepId: pending.stepId,
            capability: step.capability,
            decisionCode: "additional_input",
            summary: "Additional input was supplied for the pending question.",
          },
        ],
      });
      return { outcome: "supply_input", snapshot: applied.snapshot, decisionId: pending.decisionId, resolution: request.resolution };
    }
    const recordId = request.resolution.recordId;
    const record = operation.decisions.get(recordId);
    if (!record) throw new OperationStoreError("decision_mismatch", `unknown decision record ${recordId}`);
    const payloadDigest = operation.payloadDigests.get(pending.stepId);
    if (!payloadDigest) throw new OperationStoreError("validation_failure", `step ${pending.stepId} has no recorded approved payload`);
    const expectation: ResumeExpectation = {
      actorId: snapshot.actor.actorId,
      tenantId: snapshot.actor.tenantId,
      sessionId: snapshot.actor.sessionId,
      operationId: snapshot.operationId,
      stepId: pending.stepId,
      decisionId: pending.decisionId,
      decisionClass: pending.decisionClass,
      payloadDigest,
      revision: snapshot.revision,
      now,
      expiryBound: snapshot.deadlineAt !== undefined ? Math.min(pending.expiresAt, snapshot.deadlineAt) : pending.expiresAt,
    };
    const verdict = checkResumeAgainstRecord(record, expectation);
    if (!verdict.ok) {
      const first = verdict.issues[0];
      throw new OperationStoreError(
        ISSUE_TO_ERROR[first.code] ?? "validation_failure",
        verdict.issues.map((issue) => issue.message).join("; "),
      );
    }
    this.#guardApprovalPolicy(step, record);
    const used: RecordedDecision = { ...record, usedAt: now };
    if (!verdict.value.dispatchAuthorized) {
      const applied = this.#apply(snapshot.operationId, {
        now,
        turnRevision: context.turnRevision,
        steps: [{ stepId: pending.stepId, to: "skipped", trigger: "approval_denied" }],
        removeDecisions: [pending.decisionId],
        decisionUse: { recordId, outcome: "denied" },
        operation: operationChange(snapshot, "resolving", "resolve_started"),
        decisions: [used],
        settle: true,
        events: [
          {
            phase: "decision_recorded",
            stepId: pending.stepId,
            capability: step.capability,
            decisionCode: "denied",
            summary: "The recorded decision denied the step; it will not run.",
          },
        ],
      });
      return { outcome: "refuse_step", snapshot: applied.snapshot, stepId: pending.stepId, decisionId: pending.decisionId, recordId };
    }
    const applied = this.#apply(snapshot.operationId, {
      now,
      turnRevision: context.turnRevision,
      steps: [
        {
          stepId: pending.stepId,
          to: "running",
          trigger: "approval_recorded",
          dispatchDigest: payloadDigest,
          patch: { idempotencyKey: `${snapshot.operationId}/${pending.stepId}/${step.attempts + 1}` },
        },
      ],
      operation: operationChange(snapshot, "running", "approval_recorded"),
      removeDecisions: [pending.decisionId],
      decisionUse: { recordId, outcome: "granted" },
      decisions: [used],
      usage: { ...snapshot.usage, attempts: snapshot.usage.attempts + 1 },
      events: [
        {
          phase: "decision_recorded",
          stepId: pending.stepId,
          capability: step.capability,
          decisionCode: "granted",
          summary: `The recorded decision granted ${step.capability}.`,
        },
        {
          phase: "dispatched",
          stepId: pending.stepId,
          capability: step.capability,
          summary: `Dispatching the approved ${step.capability}.`,
          argDigest: payloadDigest,
          retryCount: step.attempts,
        },
      ],
    });
    return {
      outcome: "dispatch",
      snapshot: applied.snapshot,
      stepId: pending.stepId,
      decisionId: pending.decisionId,
      recordId,
      dispatchDigest: payloadDigest,
    };
  }

  /** Cancels queued and awaiting steps; running steps need `cancelStep` evidence. */
  requestCancel(operationId: string, input: { reason?: string; summary?: string } = {}): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    if (isTerminalOperationState(snapshot.state)) return snapshot;
    const cancellable = snapshot.steps.filter((step) => step.state === "queued" || step.state === "awaiting_approval");
    const changes: StepChange[] = cancellable.map((step) => ({ stepId: step.stepId, to: "cancelled", trigger: "cancel_requested" }));
    const removed = snapshot.pendingDecisions
      .filter((decision) => cancellable.some((step) => step.stepId === decision.stepId))
      .map((decision) => decision.decisionId);
    const abortRequestedStepIds = snapshot.steps.filter((step) => step.state === "running").map((step) => step.stepId);
    const operation = operationChange(snapshot, "cancel_requested", "cancel_requested");
    if (!operation && !changes.length && !removed.length && !abortRequestedStepIds.length) return snapshot;
    return this.#apply(operationId, {
      now: this.#now(),
      operation,
      steps: changes,
      removeDecisions: removed,
      abortRequestedStepIds,
      settle: true,
      events: [
        {
          phase: "cancelled",
          decisionCode: "cancel_requested",
          summary: input.summary ?? `Cancellation requested${input.reason !== undefined ? `: ${input.reason}` : "."}`,
        },
      ],
    }).snapshot;
  }

  /**
   * Releases expired decisions, cancels what they blocked, and expires an operation
   * whose deadline passed only when nothing was committed and nothing is unknown.
   */
  expire(operationId: string): OperationSnapshot {
    let snapshot = this.#requireSnapshot(operationId);
    if (isTerminalOperationState(snapshot.state)) return snapshot;
    const now = this.#now();
    const expired = snapshot.pendingDecisions.filter((decision) => decision.expiresAt <= now);
    if (expired.length) {
      const changes: StepChange[] = expired
        .map((decision) => stepOf(snapshot, decision.stepId))
        .filter((step) => step.state === "queued" || step.state === "awaiting_approval")
        .map((step) => ({ stepId: step.stepId, to: "cancelled", trigger: "cancel_requested" }));
      snapshot = this.#apply(operationId, {
        now,
        steps: changes,
        removeDecisions: expired.map((decision) => decision.decisionId),
        settle: true,
        events: [
          {
            phase: "expired",
            decisionCode: "decision_expired",
            summary: "A pending decision expired without an answer.",
          },
        ],
      }).snapshot;
      if (isTerminalOperationState(snapshot.state)) return snapshot;
    }
    const deadlinePassed = snapshot.deadlineAt !== undefined && now >= snapshot.deadlineAt;
    const blocked = snapshot.steps.some(
      (step) => step.state === "running" || step.state === "awaiting_approval" || step.state === "outcome_unknown",
    );
    const clear = !blocked && !snapshot.pendingDecisions.length && !snapshot.unknownOutcomes.length;
    if (deadlinePassed && clear) {
      const changes: StepChange[] = snapshot.steps
        .filter((step) => step.state === "queued")
        .map((step) => ({ stepId: step.stepId, to: "cancelled", trigger: "cancel_requested" }));
      if (!snapshot.steps.some((step) => step.state === "succeeded") && canTransitionOperation(snapshot.state, "expired", "deadline_reached")) {
        return this.#apply(operationId, {
          now,
          operation: { to: "expired", trigger: "deadline_reached" },
          steps: changes,
          events: [
            {
              phase: "expired",
              decisionCode: "deadline_reached",
              summary: "The operation deadline passed before any step committed a change.",
            },
          ],
        }).snapshot;
      }
      if (snapshot.steps.some((step) => step.state === "succeeded")) {
        // Committed effects are never hidden behind `expired`: cancel what is left and
        // report the mixed result truthfully.
        this.#apply(operationId, {
          now,
          operation: operationChange(snapshot, "cancel_requested", "cancel_requested"),
          steps: changes,
          settle: true,
          events: [
            {
              phase: "cancelled",
              decisionCode: "cancel_requested",
              summary: "The operation deadline passed; queued work was cancelled and committed effects remain.",
            },
          ],
        });
      }
    }
    return this.settle(operationId).snapshot;
  }

  /** Selection rounds and generation calls are budgeted for the whole operation. */
  consumeBudget(operationId: string, delta: { selectionRounds?: number; generationCalls?: number }): OperationSnapshot {
    const snapshot = this.#requireSnapshot(operationId);
    const usage = { ...snapshot.usage };
    const used: string[] = [];
    if (delta.selectionRounds !== undefined) {
      if (!Number.isInteger(delta.selectionRounds) || delta.selectionRounds < 1) {
        throw new ContractViolation([{ path: "$.selectionRounds", code: "out_of_range", message: "must be a positive integer" }]);
      }
      usage.selectionRounds += delta.selectionRounds;
      if (usage.selectionRounds > snapshot.budgets.maxSelectionRounds) {
        throw new OperationStoreError("budget_exceeded", `the operation allows ${snapshot.budgets.maxSelectionRounds} selection rounds`);
      }
      used.push(`${delta.selectionRounds} selection rounds`);
    }
    if (delta.generationCalls !== undefined) {
      if (!Number.isInteger(delta.generationCalls) || delta.generationCalls < 1) {
        throw new ContractViolation([{ path: "$.generationCalls", code: "out_of_range", message: "must be a positive integer" }]);
      }
      usage.generationCalls += delta.generationCalls;
      if (usage.generationCalls > snapshot.budgets.maxGenerationCalls) {
        throw new OperationStoreError("budget_exceeded", `the operation allows ${snapshot.budgets.maxGenerationCalls} generation calls`);
      }
      used.push(`${delta.generationCalls} generation calls`);
    }
    return this.#apply(operationId, {
      now: this.#now(),
      usage,
      events: [{ phase: "progress", summary: `Used ${used.join(" and ")}.` }],
    }).snapshot;
  }

  #guardApprovalPolicy(step: StepRecord, record: RecordedDecision): void {
    const requirement = this.#metadata(step.capability, step.capabilityVersion).approval;
    if (requirement === "none") {
      throw new OperationStoreError(
        "decision_mismatch",
        `${step.capability} no longer requires approval; the recorded decision authorizes nothing`,
      );
    }
    if (requirement === "user" && record.policySource !== "user_ui") {
      throw new OperationStoreError(
        "permission_denied",
        `${step.capability} requires a user decision; a ${record.policySource} approval cannot satisfy it`,
      );
    }
    if (requirement === "policy" && record.policySource !== "trusted_policy") {
      throw new OperationStoreError(
        "permission_denied",
        `${step.capability} requires a trusted policy decision; a ${record.policySource} approval cannot satisfy it`,
      );
    }
  }

  #metadata(capability: string, expectedVersion?: string): CapabilityMetadata {
    const metadata = this.#capabilityMetadata(capability, expectedVersion ?? "");
    if (!metadata || (expectedVersion !== undefined && metadata.version !== expectedVersion)) {
      throw new OperationStoreError("unsupported_capability", `unknown capability ${capability}${expectedVersion ? `@${expectedVersion}` : ""}`);
    }
    return metadata;
  }

  #assertWritable(): void {
    if (this.#closed) throw new StoreClosedError();
    if (this.#failedAppend) {
      throw new OperationStoreError(
        "execution_failure",
        `reopen the store after a failed append before recording more work: ${this.#failedAppend.message}`,
      );
    }
    this.#ownership.assertHeld();
  }

  #file(operationId: string): string {
    assertSafeId(operationId);
    return path.join(this.directory, `${operationId}${LOG_SUFFIX}`);
  }

  #require(operationId: string): StoredOperation {
    assertSafeId(operationId);
    const operation = this.#operations.get(operationId);
    if (!operation) throw new OperationNotFoundError(operationId);
    return operation;
  }

  #requireSnapshot(operationId: string): OperationSnapshot {
    const snapshot = this.#require(operationId).snapshot;
    if (!snapshot) throw new OperationStoreError("invalid_transition", `${operationId} retains terminal identity only`);
    return snapshot;
  }

  #retainedReplaySnapshot(operationId: string): OperationSnapshot {
    const tombstone = this.#tombstones.get(operationId);
    if (!tombstone) throw new OperationNotFoundError(operationId);
    const retained = tombstone.retained;
    return {
      contractVersion: CONTRACT_VERSION,
      operationId,
      revision: retained.revision,
      state: retained.state,
      createdAt: retained.createdAt,
      updatedAt: retained.updatedAt,
      actor: { actorId: "retained", tenantId: "retained", sessionId: "retained", turnId: "retained" },
      scope: {},
      request: { instruction: "Retained terminal operation." },
      requestDigest: tombstone.requestDigest,
      dedupeKey: tombstone.dedupeKey,
      budgets: DEFAULT_BUDGETS,
      steps: [],
      pendingDecisions: [],
      unknownOutcomes: [],
      usage: { steps: 0, selectionRounds: 0, generationCalls: 0, attempts: 0 },
      lastSequence: retained.lastSequence,
      ...(retained.result ? { result: retained.result } : {}),
    };
  }

  #newOperationId(): string {
    const operationId = this.#generateId();
    if (!SAFE_OPERATION_ID.test(operationId) || operationId.length > 128) throw new InvalidOperationIdError(operationId);
    if (this.#operations.has(operationId)) {
      throw new OperationStoreError("execution_failure", `generated operation id ${operationId} already exists`);
    }
    return operationId;
  }

  #scan(): void {
    if (!this.#fs.existsSync(this.directory)) return;
    const directoryStat = this.#fs.lstatSync(this.directory);
    if (!directoryStat.isDirectory()) throw new OperationLogCorruptError(this.directory, "the operations path is not a directory");
    if (directoryStat.uid !== process.getuid?.()) throw new OperationLogCorruptError(this.directory, "the operations directory has another owner");
    if ((directoryStat.mode & 0o077) !== 0) this.#fs.chmodSync(this.directory, 0o700);
    for (const name of this.#fs.readdirSync(this.directory)) {
      if (name.endsWith(`${LOG_SUFFIX}.tmp`)) this.#fs.unlinkSync(path.join(this.directory, name));
    }
    const tombstoneFile = path.join(this.directory, "tombstones.log");
    if (this.#fs.existsSync(tombstoneFile)) this.#loadTombstones(tombstoneFile);
    for (const name of [...this.#fs.readdirSync(this.directory)].sort()) {
      if (name === "tombstones.log") continue;
      if (!name.endsWith(LOG_SUFFIX)) {
        this.#ignoredEntries.push(name);
        continue;
      }
      const operationId = name.slice(0, -LOG_SUFFIX.length);
      if (!SAFE_OPERATION_ID.test(operationId)) {
        this.#ignoredEntries.push(name);
        continue;
      }
      const file = path.join(this.directory, name);
      const stat = this.#fs.lstatSync(file);
      if (!stat.isFile()) throw new OperationLogCorruptError(file, stat.isSymbolicLink() ? "the operation log is a symbolic link" : "the operation log is not a regular file", operationId);
      if (stat.uid !== process.getuid?.()) throw new OperationLogCorruptError(file, "the operation log has another owner", operationId);
      if ((stat.mode & 0o077) !== 0) this.#fs.chmodSync(file, 0o600);
      const read = readCommitLog(this.#fs, file, operationId);
      const format = this.#fs.readFileSync(file).subarray(0, Buffer.byteLength(FRAME_MAGIC)).toString("ascii") === FRAME_MAGIC ? "v2" : "v1";
      if (read.tornTail) this.#tornTails.push(file);
      const tombstone = this.#tombstones.get(operationId);
      if (tombstone) {
        let loaded: LoadedOperation | undefined;
        this.#operations.delete(operationId);
        for (const record of read.records) this.#fold(operationId, record, file, format);
        const candidate = this.#operations.get(operationId);
        if (candidate?.snapshot) loaded = candidate;
        if (!loaded || read.tornTail || !this.#matchesTombstone(loaded.snapshot, tombstone)) {
          throw new OperationLogCorruptError(file, "an operation log conflicts with its tombstone", operationId);
        }
        this.#fs.unlinkSync(file);
        this.#fsyncDirectory(this.directory);
        this.#operations.set(operationId, this.#retainedOperation(tombstone, tombstoneFile));
        continue;
      }
      for (const record of read.records) this.#fold(operationId, record, file, format);
    }
  }

  #matchesTombstone(snapshot: OperationSnapshot, tombstone: Tombstone): boolean {
    return snapshot.operationId === tombstone.operationId &&
      snapshot.dedupeKey === tombstone.dedupeKey &&
      snapshot.requestDigest === tombstone.requestDigest &&
      isTerminalOperationState(snapshot.state) &&
      sameJson(this.#retainedTerminal(snapshot), tombstone.retained);
  }

  #appendTombstone(tombstone: Tombstone): void {
    this.#ensureDirectory();
    const file = path.join(this.directory, "tombstones.log");
    const isNew = !this.#fs.existsSync(file);
    if (!isNew) this.#repairTombstoneTail(file);
    const payload = Buffer.from(JSON.stringify(tombstone), "utf8");
    const bytes = Buffer.concat([isNew ? Buffer.from(FRAME_MAGIC, "ascii") : Buffer.alloc(0), Buffer.from(`${payload.length} ${sha256(payload)}\n`, "ascii"), payload]);
    const fd = this.#fs.openSync(file, fs.constants.O_WRONLY | fs.constants.O_APPEND | fs.constants.O_CREAT | fs.constants.O_NOFOLLOW, 0o600);
    try {
      const stat = this.#fs.fstatSync(fd);
      if (!stat.isFile() || stat.uid !== process.getuid?.()) throw new OperationLogCorruptError(file, "the tombstone index is not a regular file owned by this process");
      if (this.#fs.writeSync(fd, bytes) !== bytes.length) throw new OperationStoreError("execution_failure", `short tombstone append to ${file}`);
      this.#fs.fsyncSync(fd);
    } finally {
      this.#fs.closeSync(fd);
    }
    if (isNew) this.#fsyncDirectory(this.directory);
  }

  #loadTombstones(file: string): void {
    const stat = this.#fs.lstatSync(file);
    if (!stat.isFile()) throw new OperationLogCorruptError(file, stat.isSymbolicLink() ? "the tombstone index is a symbolic link" : "the tombstone index is not a regular file");
    if (stat.uid !== process.getuid?.()) throw new OperationLogCorruptError(file, "the tombstone index has another owner");
    if ((stat.mode & 0o077) !== 0) this.#fs.chmodSync(file, 0o600);
    const read = this.#readFileNoFollow(file, "tombstone index");
    const buffer = read.buffer;
    if (buffer.subarray(0, Buffer.byteLength(FRAME_MAGIC)).toString("ascii") !== FRAME_MAGIC) throw new OperationLogCorruptError(file, "tombstone index has no v2 header");
    let offset = Buffer.byteLength(FRAME_MAGIC);
    while (offset < buffer.length) {
      const newline = buffer.indexOf(0x0a, offset);
      if (newline < 0) {
        this.#tornTails.push(file);
        break;
      }
      const match = /^(\d+) (sha256:[0-9a-f]{64})$/.exec(buffer.subarray(offset, newline).toString("ascii"));
      if (!match) throw new OperationLogCorruptError(file, "tombstone frame header is invalid");
      const start = newline + 1;
      const end = start + Number(match[1]);
      if (end > buffer.length) {
        this.#tornTails.push(file);
        break;
      }
      const payload = buffer.subarray(start, end);
      if (sha256(payload) !== match[2]) throw new OperationLogCorruptError(file, "tombstone checksum does not match");
      let value: unknown;
      try { value = JSON.parse(payload.toString("utf8")); } catch { throw new OperationLogCorruptError(file, "tombstone payload is not JSON"); }
      if (!isRecord(value) || value.v !== 1 || value.kind !== "tombstone" || typeof value.dedupeKey !== "string" || !DIGEST_RE.test(value.dedupeKey) || typeof value.operationId !== "string" || !SAFE_OPERATION_ID.test(value.operationId) || typeof value.requestDigest !== "string" || !DIGEST_RE.test(value.requestDigest) || typeof value.earliestSequence !== "number") throw new OperationLogCorruptError(file, "tombstone schema is invalid");
      let retained: unknown = value.retained;
      if (!isRecord(retained)) {
        const legacy = validateOperationSnapshot(value.snapshot);
        if (!legacy.ok || legacy.value.operationId !== value.operationId || legacy.value.dedupeKey !== value.dedupeKey || legacy.value.requestDigest !== value.requestDigest || !isTerminalOperationState(legacy.value.state)) throw new OperationLogCorruptError(file, "tombstone retained projection is invalid");
        retained = this.#retainedTerminal(legacy.value);
      }
      if (!isRecord(retained) || retained.operationId !== value.operationId || typeof retained.revision !== "number" || !isTerminalOperationState(retained.state as OperationState) || typeof retained.createdAt !== "number" || typeof retained.updatedAt !== "number" || typeof retained.lastSequence !== "number" || retained.lastSequence !== value.earliestSequence) throw new OperationLogCorruptError(file, "tombstone retained projection is invalid");
      const tombstone = { v: 1, kind: "tombstone", dedupeKey: value.dedupeKey, operationId: value.operationId, requestDigest: value.requestDigest, retained, earliestSequence: value.earliestSequence } as Tombstone;
      this.#tombstones.set(tombstone.operationId, tombstone);
      this.#dedupe.set(tombstone.dedupeKey, { operationId: tombstone.operationId, requestDigest: tombstone.requestDigest });
      this.#operations.set(tombstone.operationId, this.#retainedOperation(tombstone, file));
      offset = end;
    }
  }

  #readFileNoFollow(file: string, label: string): { buffer: Buffer; stat: fs.Stats } {
    const fd = this.#fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW);
    try {
      const stat = this.#fs.fstatSync(fd);
      if (!stat.isFile() || stat.uid !== process.getuid?.()) throw new OperationLogCorruptError(file, `the ${label} is not a regular file owned by this process`);
      const buffer = Buffer.alloc(stat.size);
      let offset = 0;
      while (offset < buffer.length) {
        const read = this.#fs.readSync(fd, buffer, offset, buffer.length - offset, offset);
        if (read === 0) throw new OperationLogCorruptError(file, `the ${label} changed while it was read`);
        offset += read;
      }
      return { buffer, stat };
    } finally {
      this.#fs.closeSync(fd);
    }
  }

  #repairTombstoneTail(file: string): void {
    const buffer = this.#readFileNoFollow(file, "tombstone index").buffer;
    if (buffer.subarray(0, Buffer.byteLength(FRAME_MAGIC)).toString("ascii") !== FRAME_MAGIC) throw new OperationLogCorruptError(file, "tombstone index has no v2 header");
    let offset = Buffer.byteLength(FRAME_MAGIC);
    while (offset < buffer.length) {
      const newline = buffer.indexOf(0x0a, offset);
      if (newline < 0) break;
      const match = /^(\d+) (sha256:[0-9a-f]{64})$/.exec(buffer.subarray(offset, newline).toString("ascii"));
      if (!match) throw new OperationLogCorruptError(file, "tombstone frame header is invalid");
      const start = newline + 1;
      const end = start + Number(match[1]);
      if (end > buffer.length) break;
      const payload = buffer.subarray(start, end);
      if (sha256(payload) !== match[2]) throw new OperationLogCorruptError(file, "tombstone checksum does not match");
      offset = end;
    }
    if (offset === buffer.length) return;
    this.#fs.truncateSync(file, offset);
    const fd = this.#fs.openSync(file, fs.constants.O_WRONLY | fs.constants.O_NOFOLLOW);
    try {
      this.#fs.fsyncSync(fd);
    } finally {
      this.#fs.closeSync(fd);
    }
    this.#repairs.push({ file, action: "truncated" });
    this.#tornTails = this.#tornTails.filter((entry) => entry !== file);
  }

  #fold(operationId: string, record: CommitRecord, file: string, format: "v1" | "v2" = "v2"): void {
    if (record.snapshot.operationId !== operationId) {
      throw new OperationLogCorruptError(file, `the commit names ${record.snapshot.operationId}`, operationId);
    }
    if (requestDigest(record.snapshot.request) !== record.snapshot.requestDigest) {
      throw new OperationLogCorruptError(file, "the recorded request digest does not match its request", operationId);
    }
    const existing = this.#operations.get(operationId);
    if (existing) {
      if (!existing.snapshot) throw new OperationLogCorruptError(file, "an operation log conflicts with its tombstone", operationId);
      if (record.abortRequestedStepIds?.some((stepId) => existing.snapshot.steps.find((step) => step.stepId === stepId)?.state !== "running")) {
        throw new OperationLogCorruptError(file, "abort intent does not name a running predecessor step", operationId);
      }
      const problem = replayProblem(existing.snapshot, record.snapshot, record, existing);
      if (problem) throw new OperationLogCorruptError(file, problem, operationId);
    } else {
      if (record.snapshot.revision !== 1 || record.snapshot.state !== "accepted" || record.events.length !== 1 || record.events[0].phase !== "accepted") {
        throw new OperationLogCorruptError(file, "the first commit is not an accepted revision 1 snapshot", operationId);
      }
    }
    const recorded = existing ? existing.events : [];
    const lastSequence = recorded.length ? recorded[recorded.length - 1].sequence : 0;
    if (record.snapshot.lastSequence !== lastSequence + record.events.length) {
      throw new OperationLogCorruptError(
        file,
        `lastSequence ${record.snapshot.lastSequence} does not follow the ${lastSequence} recorded events`,
        operationId,
      );
    }
    let previousSequence: number | undefined = lastSequence || undefined;
    for (const event of record.events) {
      const sequence = validateEventSequence(previousSequence, event);
      if (!sequence.ok) {
        throw new OperationLogCorruptError(file, `event: ${sequence.issues[0].message}`, operationId);
      }
      previousSequence = event.sequence;
    }
    if (record.snapshot.lastSequence !== previousSequence) {
      throw new OperationLogCorruptError(
        file,
        `lastSequence ${record.snapshot.lastSequence} does not match the final event sequence ${previousSequence}`,
        operationId,
      );
    }
    if (existing && record.turnRevision < existing.turnRevision) {
      throw new OperationLogCorruptError(
        file,
        `turn revision ${record.turnRevision} goes back from the recorded ${existing.turnRevision}`,
        operationId,
      );
    }
    const claimed = this.#dedupe.get(record.snapshot.dedupeKey);
    if (claimed && claimed.operationId !== operationId) {
      throw new OperationLogCorruptError(
        file,
        `deduplication key ${record.snapshot.dedupeKey} is already held by ${claimed.operationId}`,
        operationId,
      );
    }
    const operation: LoadedOperation =
      existing ?? {
        snapshot: record.snapshot,
        file,
        turnRevision: record.turnRevision,
        events: [],
        decisions: new Map(),
        resolutions: new Map(),
        payloadDigests: new Map(),
        abortRequestedStepIds: new Set(),
        format,
        records: [],
      };
    for (const event of record.events) {
      if (event.operationId !== operationId) throw new OperationLogCorruptError(file, "an event names another operation", operationId);
      operation.events.push(event);
      if (event.stepId && event.argDigest) operation.payloadDigests.set(event.stepId, event.argDigest);
    }
    for (const decision of record.decisions ?? []) operation.decisions.set(decision.recordId, decision);
    for (const resolution of record.resolutions ?? []) operation.resolutions.set(resolution.decisionId, resolution.resolution);
    for (const stepId of record.abortRequestedStepIds ?? []) operation.abortRequestedStepIds.add(stepId);
    operation.snapshot = record.snapshot;
    operation.turnRevision = record.turnRevision;
    operation.format = format;
    operation.records.push(record);
    this.#dedupe.set(record.snapshot.dedupeKey, { operationId, requestDigest: record.snapshot.requestDigest });
    this.#operations.set(operationId, operation);
  }

  #apply(operationId: string, input: StoreTransitionInput): AppliedChange {
    this.#assertWritable();
    const operation = this.#require(operationId);
    const result = applyTransition(this.#requireSnapshot(operationId), input);
    if (!result.ok) {
      throw new OperationStoreError(result.error.code, result.error.message, {
        missing: result.error.missing,
        refs: result.error.refs,
      });
    }
    if (!result.changed) return { snapshot: result.snapshot, changed: false };
    const record: CommitRecord = {
      v: COMMIT_FORMAT_VERSION,
      kind: "commit",
      at: input.now,
      turnRevision: input.turnRevision ?? operation.turnRevision,
      snapshot: result.snapshot,
      events: result.events,
    };
    if (input.decisions?.length) record.decisions = input.decisions;
    if (input.resolutions?.length) record.resolutions = input.resolutions;
    if (input.abortRequestedStepIds?.length) record.abortRequestedStepIds = input.abortRequestedStepIds;
    if (operation.format === "v1" && operation.snapshot !== undefined) this.#rewriteV2(operation);
    this.#append(operationId, record);
    this.#fold(operationId, record, operation.file);
    return { snapshot: result.snapshot, changed: true };
  }

  #append(operationId: string, record: CommitRecord): void {
    const problem = commitProblem(record);
    if (problem) throw new OperationStoreError("validation_failure", `refusing to write an invalid commit: ${problem}`);
    const file = this.#file(operationId);
    const frame = encodeCommitFrame(record);
    try {
      const isNew = !this.#fs.existsSync(file);
      this.#ensureDirectory();
      this.#repairTail(file);
      const flags = fs.constants.O_WRONLY | fs.constants.O_APPEND | fs.constants.O_CREAT | fs.constants.O_NOFOLLOW;
      const fd = this.#fs.openSync(file, flags, 0o600);
      try {
        const stat = this.#fs.fstatSync(fd);
        if (!stat.isFile() || stat.uid !== process.getuid?.()) throw new OperationLogCorruptError(file, "the operation log is not a regular file owned by this process", operationId);
        let bytes = frame;
        if (isNew) bytes = Buffer.concat([Buffer.from(FRAME_MAGIC, "ascii"), frame]);
        const expected = bytes.length;
        const written = this.#fs.writeSync(fd, bytes);
        if (written !== expected) {
          throw new OperationStoreError("execution_failure", `short append of ${written}/${expected} bytes to ${file}`);
        }
        this.#fs.fsyncSync(fd);
      } finally {
        this.#fs.closeSync(fd);
      }
      if (isNew) {
        this.#fsyncDirectory(this.directory);
        this.#fsyncDirectory(this.root);
      }
    } catch (error) {
      // After a failed append this instance no longer knows what is durable, so it
      // refuses further writes instead of risking a duplicate operation.
      this.#failedAppend = error as Error;
      throw error;
    }
  }

  #rewriteV2(operation: LoadedOperation): void {
    const temporary = `${operation.file}.tmp`;
    const bytes = Buffer.concat([Buffer.from(FRAME_MAGIC, "ascii"), ...operation.records.map(encodeCommitFrame)]);
    try {
      if (this.#fs.existsSync(temporary)) this.#fs.unlinkSync(temporary);
      const fd = this.#fs.openSync(temporary, fs.constants.O_WRONLY | fs.constants.O_CREAT | fs.constants.O_EXCL | fs.constants.O_NOFOLLOW, 0o600);
      try {
        const written = this.#fs.writeSync(fd, bytes);
        if (written !== bytes.length) throw new OperationStoreError("execution_failure", `short rewrite of ${written}/${bytes.length} bytes to ${temporary}`);
        this.#fs.fsyncSync(fd);
      } finally {
        this.#fs.closeSync(fd);
      }
      this.#fs.renameSync(temporary, operation.file);
      this.#fsyncDirectory(this.directory);
      operation.format = "v2";
    } catch (error) {
      this.#failedAppend = error as Error;
      throw error;
    }
  }

  #ensureDirectory(): void {
    if (this.#fs.existsSync(this.directory)) return;
    this.#fs.mkdirSync(this.directory, { recursive: true, mode: 0o700 });
    this.#fsyncDirectory(this.root);
  }

  /** Creation and rename are not durable until the containing directory syncs. */
  #fsyncDirectory(directory: string): void {
    const fd = this.#fs.openSync(directory, "r");
    try {
      this.#fs.fsyncSync(fd);
    } finally {
      this.#fs.closeSync(fd);
    }
  }

  /** Terminates a complete-but-unterminated final record, or drops a torn fragment. */
  #repairTail(file: string): void {
    if (!this.#fs.existsSync(file)) return;
    const buffer = this.#fs.readFileSync(file);
    if (buffer.subarray(0, Buffer.byteLength(FRAME_MAGIC)).toString("ascii") === FRAME_MAGIC) {
      const read = readV2(buffer, file);
      if (!read.tornTail) return;
      let offset = Buffer.byteLength(FRAME_MAGIC);
      for (let index = 0; index < read.records.length; index++) {
        const newline = buffer.indexOf(0x0a, offset);
        const length = Number(buffer.subarray(offset, newline).toString("ascii").split(" ", 1)[0]);
        offset = newline + 1 + length;
      }
      this.#fs.truncateSync(file, offset);
      const fd = this.#fs.openSync(file, "r+");
      try {
        this.#fs.fsyncSync(fd);
      } finally {
        this.#fs.closeSync(fd);
      }
      this.#repairs.push({ file, action: "truncated" });
      this.#tornTails = this.#tornTails.filter((entry) => entry !== file);
      return;
    }
    if (!buffer.length || buffer[buffer.length - 1] === 0x0a) return;
    const lastNewline = buffer.lastIndexOf(0x0a);
    const tail = buffer.subarray(lastNewline + 1).toString("utf8");
    let complete = true;
    try {
      JSON.parse(tail);
    } catch {
      complete = false;
    }
    if (complete) {
      const fd = this.#fs.openSync(file, "a");
      try {
        this.#fs.writeSync(fd, "\n");
        this.#fs.fsyncSync(fd);
      } finally {
        this.#fs.closeSync(fd);
      }
      this.#repairs.push({ file, action: "terminated" });
    } else {
      this.#fs.truncateSync(file, lastNewline + 1);
      const fd = this.#fs.openSync(file, "r+");
      try {
        this.#fs.fsyncSync(fd);
      } finally {
        this.#fs.closeSync(fd);
      }
      this.#repairs.push({ file, action: "truncated" });
    }
    this.#tornTails = this.#tornTails.filter((entry) => entry !== file);
  }
}
