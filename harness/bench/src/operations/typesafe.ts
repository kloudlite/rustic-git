/**
 * O03 — bounded TypeSafe HTTP adapter for Choice / Score / Noul judgments.
 *
 * `POST https://api.typesafe.ai/v1/systemone` evaluates one trusted `state` against a
 * map of typed questions and returns one answer per question id. This adapter owns the
 * provider boundary: it pins the model (`jev-1.13.0`), keeps the bearer key in trusted
 * configuration only, refuses unapproved state, bounds request and response size, reads
 * the body through a cap, retries `429`/`529` with bounded backoff inside the deadline
 * while honoring `retry-after`, never retries `401`/`422`, and honours caller aborts.
 *
 * Batching is deliberate. Independent questions about the same state go out in one
 * request (`judge`), so N questions cost one call rather than N; the frozen
 * single-question `JudgmentAdapter` methods are implemented on top of the same path.
 *
 * Authorization of outbound input is a trusted-policy decision, not a shape check.
 * `approveProviderState` only records what trusted code classified and digests it; the
 * marker, digest, and secret-pattern scan are bookkeeping and defense in depth, and a
 * caller can hand-craft all of them. Every dispatch therefore requires a
 * `providerInputPolicy` that explicitly authorizes the exact request body digest and
 * scope. O07/O08 own that policy: they must implement tenant/scope input rules and pass
 * the evaluator in trusted configuration. This adapter enforces the decision — absence,
 * denial, a thrown evaluator, or a digest that does not match the bytes about to be
 * sent all fail closed before `fetch` — but it cannot verify that the policy itself is
 * correct, and nothing here is a claim that real tenant policy has been implemented.
 */
import { ContractViolation, canonicalDigest, stableStringify, validateJsonValue } from "./contracts.ts";
import type {
  ChoiceJudgmentRequest,
  ChoiceJudgmentResult,
  JudgmentAdapter,
  JudgmentModel,
  NoulJudgmentRequest,
  NoulJudgmentResult,
  ScoreJudgmentRequest,
  ScoreJudgmentResult,
  Validation,
  ValidationIssue,
} from "./contracts.ts";
import { field, hasOwnKey, isPlainRecord } from "./shape.ts";
import type { JsonValue } from "./shape.ts";
import {
  DEFAULT_NOUL_POLICY,
  JUDGMENT_LIMITS,
  QUESTION_ID_RE,
  judgmentFailureResults,
  isPinnedModel,
  parseTypeSafeResponse,
  resolveJudgmentAnswers,
  toWireQuestion,
  validateJudgmentQuestions,
  validateNoulDecisionPolicy,
} from "./judgments.ts";
import type {
  JudgmentBudgetAccount,
  JudgmentFailure,
  JudgmentKind,
  JudgmentQuestion,
  JudgmentQuestionEntry,
  JudgmentRecorder,
  JudgmentRecord,
  JudgmentRecordContext,
  JudgmentRecordMode,
  JudgmentResult,
  JudgmentUsage,
  NoulDecisionPolicy,
} from "./judgments.ts";

export const TYPESAFE_ENDPOINT = "https://api.typesafe.ai/v1/systemone";
/** Initial pin from the design review; aliases move, so a calibrated policy pins a version. */
export const TYPESAFE_DEFAULT_MODEL = "jev-1.13.0";
export const TYPESAFE_DEFAULTS = {
  timeoutMs: 30_000,
  deadlineMs: 60_000,
  maxAttempts: 3,
  backoffMs: 250,
  maxBackoffMs: 4_000,
  maxResponseBytes: JUDGMENT_LIMITS.responseBytes,
  maxStateChars: 120_000,
} as const;
export const TYPESAFE_ATTEMPT_STATUSES: readonly number[] = [429, 529];
/** Error names safe to report: a transport can otherwise put arbitrary text in `name`. */
export const SAFE_TRANSPORT_ERROR_NAMES: readonly string[] = [
  "Error",
  "TypeError",
  "RangeError",
  "SyntaxError",
  "URIError",
  "EvalError",
  "ReferenceError",
  "AggregateError",
  "AbortError",
  "TimeoutError",
  "FetchError",
  "NetworkError",
];
const API_KEY_RE = /^[\x21-\x7e]{8,512}$/;
const SOURCE_RE = /^[A-Za-z0-9][A-Za-z0-9._:-]*$/;
const SINGLE_QUESTION_IDS: Record<string, string> = {
  choice: "single_choice",
  score: "single_score",
  noul: "single_noul",
};

const fail = <T>(path: string, code: ValidationIssue["code"], message: string): Validation<T> => ({
  ok: false,
  issues: [{ path, code, message }],
});

const truncate = (text: string, max: number): string => (text.length <= max ? text : `${text.slice(0, max - 3)}...`);

/** A transport exception's `name` is reported only when it is one we recognize. */
function safeErrorName(error: unknown): string {
  return error instanceof Error && SAFE_TRANSPORT_ERROR_NAMES.includes(error.name) ? error.name : "unknown";
}

// ---------------------------------------------------------------------------
// Credential handling: keys live in config, never in state, bodies, or diagnostics
// ---------------------------------------------------------------------------

/**
 * Deterministic shapes that must never reach a provider or a log line. Deterministic
 * checks are defense in depth, not proof; the trusted policy decides eligibility first.
 */
export const SECRET_SHAPED_PATTERNS: ReadonlyArray<{ name: string; re: RegExp }> = [
  { name: "authorization_header", re: /\bauthorization\s*[:=]\s*\S+/i },
  { name: "bearer_token", re: /\bbearer\s+[A-Za-z0-9._~+/=-]{16,}/i },
  {
    name: "credential_assignment",
    re: /\b(?:api[_-]?key|access[_-]?token|auth[_-]?token|secret|password|passwd|client[_-]?secret)\b\s*[:=]\s*["']?[A-Za-z0-9._~+/=-]{12,}/i,
  },
  { name: "private_key_block", re: /-----BEGIN [A-Z ]*PRIVATE KEY-----/ },
  { name: "provider_key", re: /\b(?:sk|rk)-[A-Za-z0-9_-]{16,}\b/ },
];

/** Names of the credential shapes present in `text`. */
export function scanForSecretShapes(text: string): string[] {
  return SECRET_SHAPED_PATTERNS.filter((pattern) => pattern.re.test(text)).map((pattern) => pattern.name);
}

/**
 * Best-effort masking for caller-facing diagnostics. Defense in depth, not a control:
 * provider bodies and transport exception messages are never reported at all, so this
 * helper must not be relied on to sanitize untrusted text.
 */
export function redactDiagnostic(text: string, secrets: readonly string[] = []): string {
  let out = text;
  for (const secret of secrets) {
    if (secret.length >= 8) out = out.split(secret).join("[redacted]");
  }
  for (const pattern of SECRET_SHAPED_PATTERNS) out = out.replace(new RegExp(pattern.re.source, "gi"), "[redacted]");
  return out;
}

// ---------------------------------------------------------------------------
// Approved provider input
// ---------------------------------------------------------------------------

/** Text, or structured JSON such as records or a chat log (the provider's `state`). */
export type TypeSafeState = string | JsonValue;

/**
 * State that a trusted component — never a model and never raw request context — has
 * classified as provider-eligible. The digest travels with the bytes so the adapter can
 * refuse input that changed after approval.
 */
export type ApprovedProviderState = {
  approval: "trusted_policy";
  classification: "provider_eligible";
  source: string;
  digest: string;
  byteLength: number;
  state: TypeSafeState;
};

export type ApproveProviderStateInput = {
  state: TypeSafeState;
  source: string;
  approval?: "trusted_policy";
  maxChars?: number;
};

function stateText(state: TypeSafeState): string {
  return typeof state === "string" ? state : stableStringify(state);
}

function describeState(state: TypeSafeState, path: string, maxChars: number): ValidationIssue | undefined {
  if (typeof state !== "string") {
    const json = validateJsonValue(state, path);
    if (!json.ok) return json.issues[0];
  }
  const text = stateText(state);
  if (state === "") return { path, code: "empty_string", message: "provider state must not be empty" };
  if (text.length > maxChars) {
    return { path, code: "string_too_long", message: `provider state is ${text.length} characters; the limit is ${maxChars}` };
  }
  const shaped = scanForSecretShapes(text);
  if (shaped.length) {
    return {
      path,
      code: "forbidden_key",
      message: `provider state carries credential-shaped text (${shaped.join(", ")}); classify eligible input before approving it`,
    };
  }
  return undefined;
}

/**
 * Bookkeeping for trusted policy (O07/O08), never authorization: it records what
 * trusted code classified as provider-eligible and binds that to a digest. A caller can
 * forge the marker and recompute the digest, so dispatch additionally requires the
 * `providerInputPolicy` decision on the exact body.
 */
export function approveProviderState(input: unknown, defaults: { maxChars?: number } = {}): Validation<ApprovedProviderState> {
  const maxChars = defaults.maxChars ?? TYPESAFE_DEFAULTS.maxStateChars;
  if (!isPlainRecord(input)) return fail("$", "not_object", "approveProviderState takes { state, source }");
  const issues: ValidationIssue[] = [];
  for (const key of Object.keys(input)) {
    if (key !== "state" && key !== "source" && key !== "approval" && key !== "maxChars") {
      issues.push({ path: field("$", key), code: "unknown_field", message: `unknown field "${key}"` });
    }
  }
  if (input.approval !== undefined && input.approval !== "trusted_policy") {
    issues.push({ path: field("$", "approval"), code: "bad_syntax", message: 'approval must be "trusted_policy"' });
  }
  const source = input.source;
  if (typeof source !== "string" || source.length === 0 || source.length > 128 || !SOURCE_RE.test(source)) {
    issues.push({
      path: field("$", "source"),
      code: "bad_syntax",
      message: "source must name the trusted component that classified this state",
    });
  }
  if (input.maxChars !== undefined && typeof input.maxChars !== "number") {
    issues.push({ path: field("$", "maxChars"), code: "wrong_type", message: "maxChars must be a number" });
  }
  const limit = typeof input.maxChars === "number" ? input.maxChars : maxChars;
  const stateIssue = describeState(input.state as TypeSafeState, field("$", "state"), limit);
  if (stateIssue) issues.push(stateIssue);
  if (issues.length) return { ok: false, issues };
  const state = input.state as TypeSafeState;
  return {
    ok: true,
    value: {
      approval: "trusted_policy",
      classification: "provider_eligible",
      source: source as string,
      digest: canonicalDigest(state as JsonValue),
      byteLength: Buffer.byteLength(stateText(state), "utf8"),
      state,
    },
  };
}

export function validateApprovedProviderState(value: unknown, path = "$"): Validation<ApprovedProviderState> {
  const approvalHint = "provider state must come from trusted policy (approveProviderState); raw context is not approved input";
  if (!isPlainRecord(value)) return fail(path, "permission_denied", approvalHint);
  for (const key of ["approval", "classification", "source", "digest", "byteLength", "state"]) {
    if (!hasOwnKey(value, key)) return fail(path, "permission_denied", approvalHint);
  }
  const issues: ValidationIssue[] = [];
  if (value.approval !== "trusted_policy") {
    issues.push({ path: field(path, "approval"), code: "permission_denied", message: 'approval must be "trusted_policy"' });
  }
  if (value.classification !== "provider_eligible") {
    issues.push({
      path: field(path, "classification"),
      code: "permission_denied",
      message: 'classification must be "provider_eligible"',
    });
  }
  if (typeof value.source !== "string" || value.source.length === 0 || value.source.length > 128) {
    issues.push({ path: field(path, "source"), code: "bad_syntax", message: "source must be a short trusted-origin label" });
  }
  if (typeof value.digest !== "string" || !/^sha256:[0-9a-f]{64}$/.test(value.digest)) {
    issues.push({ path: field(path, "digest"), code: "bad_syntax", message: "digest must be sha256:<64 hex>" });
  }
  if (typeof value.byteLength !== "number" || !Number.isInteger(value.byteLength) || value.byteLength < 0) {
    issues.push({ path: field(path, "byteLength"), code: "wrong_type", message: "byteLength must be a non-negative integer" });
  }
  const stateIssue = describeState(value.state as TypeSafeState, field(path, "state"), TYPESAFE_DEFAULTS.maxStateChars);
  if (stateIssue) issues.push(stateIssue);
  if (issues.length) return { ok: false, issues };
  const state = value.state as TypeSafeState;
  if (canonicalDigest(state as JsonValue) !== value.digest) {
    return fail(field(path, "digest"), "permission_denied", "state digest does not match its content; the state changed after approval");
  }
  if (Buffer.byteLength(stateText(state), "utf8") !== value.byteLength) {
    return fail(field(path, "byteLength"), "bad_syntax", "byteLength does not match the approved state");
  }
  return { ok: true, value: value as unknown as ApprovedProviderState };
}

// ---------------------------------------------------------------------------
// Transport (injectable; the default is fetch with redirects disabled)
// ---------------------------------------------------------------------------

export type TypeSafeBodyReader = { read(): Promise<{ done: boolean; value?: Uint8Array }>; cancel?(): Promise<void> };
export type TypeSafeBodyStream = { getReader(): TypeSafeBodyReader };
export type TypeSafeResponseLike = {
  status: number;
  headers: { get(name: string): string | null };
  body?: TypeSafeBodyStream | null;
  text(): Promise<string>;
};
export type TypeSafeFetchInit = {
  method: "POST";
  headers: Record<string, string>;
  body: string;
  signal: AbortSignal;
  /** A redirect could forward the bearer key to another origin, so none are followed. */
  redirect: "error";
};
export type TypeSafeFetch = (url: string, init: TypeSafeFetchInit) => Promise<TypeSafeResponseLike>;
export type TypeSafeSleep = (ms: number, signal?: AbortSignal) => Promise<void>;

// ---------------------------------------------------------------------------
// Trusted provider-input policy (the only authorization to send input)
// ---------------------------------------------------------------------------

export type ProviderInputPart = {
  id: string;
  version: string;
  kind: JudgmentKind;
  /** Exact instruction text sent for this question, including any context string. */
  instructions: string;
  /** Candidate labels or rubric levels; empty for Noul. */
  candidateLabels: string[];
};

/**
 * Everything a trusted evaluator needs to authorize one exact outbound request. `body`
 * and `digest` are authoritative: the digest is computed by the adapter over the exact
 * bytes it would send, and a decision only counts when it returns that same digest.
 */
export type ProviderInputRequest = {
  endpoint: string;
  model: string;
  mode: JudgmentRecordMode;
  body: string;
  digest: string;
  /**
   * Opaque trusted scope label for this operation, forwarded from configuration or the
   * bundle. The policy decides what it means; the adapter never treats it as authority.
   */
  scope?: string;
  state: { source: string; digest: string; text: string };
  parts: ProviderInputPart[];
};

export type ProviderInputDecision =
  | { authorized: true; digest: string; authorizationRef?: string }
  | { authorized: false; code: string };

/**
 * O07/O08 supply this evaluator from trusted configuration. It decides whether the
 * exact state, question text, and candidate labels may leave for this provider. Absence
 * of a policy is a denial, not an implicit approval.
 */
export type ProviderInputPolicy = (request: ProviderInputRequest) => ProviderInputDecision | Promise<ProviderInputDecision>;

/**
 * The normalized, per-question view the trusted policy evaluates: one part per question,
 * carrying the exact instruction text sent (context merged in, via the same wire shape
 * the body uses) and the candidate labels or rubric levels that may leave. Reserved
 * abstention options are not candidates and are therefore not listed here; they still
 * travel in the body and are covered by its digest.
 */
function providerInputParts(questions: JudgmentQuestion[]): ProviderInputPart[] {
  return questions.map((question) => {
    const request = question.request;
    const candidateLabels =
      question.kind === "choice"
        ? (request as { options: Array<{ label: string }> }).options.map((option) => option.label)
        : question.kind === "score"
          ? [...(request as { rubric: string[] }).rubric]
          : [];
    return {
      id: question.id,
      version: request.questionVersion,
      kind: question.kind,
      instructions: toWireQuestion(question).instructions,
      candidateLabels,
    };
  });
}

async function defaultFetch(url: string, init: TypeSafeFetchInit): Promise<TypeSafeResponseLike> {
  const response = await fetch(url, {
    method: init.method,
    headers: init.headers,
    body: init.body,
    signal: init.signal,
    redirect: init.redirect,
  });
  return response as unknown as TypeSafeResponseLike;
}

function defaultSleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(new Error("aborted"));
      return;
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = (): void => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
      reject(new Error("aborted"));
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/** `retry-after` is seconds or an HTTP date; anything else carries no hint. */
export function parseRetryAfterMs(header: string | null | undefined, nowMs: number): number | undefined {
  if (header === null || header === undefined) return undefined;
  const trimmed = header.trim();
  if (trimmed.length === 0) return undefined;
  if (/^\d+(\.\d+)?$/.test(trimmed)) return Math.round(Number(trimmed) * 1000);
  const date = Date.parse(trimmed);
  return Number.isFinite(date) ? Math.max(0, date - nowMs) : undefined;
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

export type TypeSafeConfig = {
  /** Trusted configuration only. The key is never part of the state, a body, or a log line. */
  apiKey: string;
  /** Default approved state for the frozen single-question methods; a bundle may override it. */
  state?: ApprovedProviderState;
  model?: string;
  /** Per HTTP attempt. A bundle timeout narrows it further, never raises it. */
  timeoutMs?: number;
  /** Per `judge` call, retries included. */
  deadlineMs?: number;
  /** Adapter default; a bundle may narrow live to shadow or override it explicitly. */
  mode?: JudgmentRecordMode;
  /**
   * Trusted provider-input authorization. Required for every dispatch; O07/O08 wire it.
   * Without it the adapter fails closed before `fetch`.
   */
  providerInputPolicy?: ProviderInputPolicy;
  /**
   * Default trusted scope label handed to the policy request. O07/O08 set the
   * authenticated tenant/workspace/session scope; a bundle may narrow it per operation.
   */
  providerInputScope?: string;
  /**
   * Opt-in to reading a response with `text()` when the transport exposes no stream.
   * Only a trusted transport may enable it; the default refuses an unbounded read.
   */
  allowTextBodyFallback?: boolean;
  maxAttempts?: number;
  backoffMs?: number;
  maxBackoffMs?: number;
  maxResponseBytes?: number;
  maxStateChars?: number;
  budget?: JudgmentBudgetAccount;
  recorder?: JudgmentRecorder;
  noul?: { threshold?: number; undecidedBand?: number };
  fetch?: TypeSafeFetch;
  now?: () => number;
  sleep?: TypeSafeSleep;
};

export type NormalizedTypeSafeConfig = Required<
  Omit<TypeSafeConfig, "budget" | "recorder" | "noul" | "maxStateChars" | "state" | "providerInputPolicy" | "providerInputScope">
> & {
  maxStateChars: number;
  state?: ApprovedProviderState;
  budget?: JudgmentBudgetAccount;
  recorder?: JudgmentRecorder;
  providerInputPolicy?: ProviderInputPolicy;
  /**
   * Normalized to "" when no trusted scope was configured; an empty label is never
   * forwarded to the policy, so an absent scope stays absent rather than becoming one.
   */
  providerInputScope: string;
  noul: NoulDecisionPolicy;
};

function boundedInt(path: string, value: unknown, min: number, max: number): ValidationIssue | undefined {
  if (typeof value !== "number" || !Number.isInteger(value)) {
    return { path, code: "wrong_type", message: "must be an integer" };
  }
  if (value < min || value > max) return { path, code: "out_of_range", message: `must be ${min}..${max}` };
  return undefined;
}

export function validateTypeSafeConfig(value: unknown): Validation<NormalizedTypeSafeConfig> {
  if (!isPlainRecord(value)) return fail("$", "not_object", "adapter config must be an object");
  const issues: ValidationIssue[] = [];
  const known = new Set([
    "apiKey",
    "state",
    "model",
    "timeoutMs",
    "deadlineMs",
    "mode",
    "providerInputPolicy",
    "providerInputScope",
    "allowTextBodyFallback",
    "maxAttempts",
    "backoffMs",
    "maxBackoffMs",
    "maxResponseBytes",
    "maxStateChars",
    "budget",
    "recorder",
    "noul",
    "fetch",
    "now",
    "sleep",
  ]);
  for (const key of Object.keys(value)) {
    if (!known.has(key)) issues.push({ path: field("$", key), code: "unknown_field", message: `unknown field "${key}"` });
  }
  if (typeof value.apiKey !== "string" || !API_KEY_RE.test(value.apiKey)) {
    issues.push({
      path: field("$", "apiKey"),
      code: "bad_syntax",
      message: "apiKey must be trusted configuration: 8..512 printable, whitespace-free characters",
    });
  }
  const state = value.state === undefined ? { ok: true as const, value: undefined } : validateApprovedProviderState(value.state, field("$", "state"));
  if (value.state !== undefined && !state.ok) issues.push(...state.issues);
  const model = value.model === undefined ? TYPESAFE_DEFAULT_MODEL : value.model;
  if (typeof model !== "string" || model.length === 0 || model.length > 128) {
    issues.push({ path: field("$", "model"), code: "bad_syntax", message: "model must be a non-empty name" });
  }
  const mode = value.mode === undefined ? "live" : value.mode;
  if (mode !== "live" && mode !== "shadow") {
    issues.push({ path: field("$", "mode"), code: "bad_syntax", message: 'mode must be "live" or "shadow"' });
  }
  if (value.providerInputPolicy !== undefined && typeof value.providerInputPolicy !== "function") {
    issues.push({ path: field("$", "providerInputPolicy"), code: "wrong_type", message: "providerInputPolicy must be a function" });
  }
  if (
    value.providerInputScope !== undefined &&
    (typeof value.providerInputScope !== "string" ||
      value.providerInputScope.length === 0 ||
      value.providerInputScope.length > 128 ||
      !QUESTION_ID_RE.test(value.providerInputScope))
  ) {
    issues.push({
      path: field("$", "providerInputScope"),
      code: "bad_syntax",
      message: "providerInputScope must be an opaque id-shaped scope label",
    });
  }
  if (value.allowTextBodyFallback !== undefined && typeof value.allowTextBodyFallback !== "boolean") {
    issues.push({ path: field("$", "allowTextBodyFallback"), code: "wrong_type", message: "allowTextBodyFallback must be a boolean" });
  }
  const bounds: Array<[string, unknown, number, number]> = [
    ["timeoutMs", value.timeoutMs, 100, 10 * 60 * 1000],
    ["deadlineMs", value.deadlineMs, 100, 30 * 60 * 1000],
    ["maxAttempts", value.maxAttempts, 1, JUDGMENT_LIMITS.maxAttempts],
    ["backoffMs", value.backoffMs, 0, 60_000],
    ["maxBackoffMs", value.maxBackoffMs, 0, 60_000],
    ["maxResponseBytes", value.maxResponseBytes, 1_024, 4 * 1024 * 1024],
    ["maxStateChars", value.maxStateChars, 1, 4_000_000],
  ];
  for (const [key, raw, min, max] of bounds) {
    if (raw === undefined) continue;
    const issue = boundedInt(field("$", key), raw, min, max);
    if (issue) issues.push(issue);
  }
  for (const key of ["fetch", "now", "sleep"] as const) {
    if (value[key] !== undefined && typeof value[key] !== "function") {
      issues.push({ path: field("$", key), code: "wrong_type", message: `${key} must be a function` });
    }
  }
  if (value.budget !== undefined) {
    const budget = value.budget;
    if (
      !isPlainRecord(budget) ||
      typeof budget.claimCall !== "function" ||
      typeof budget.recordUsage !== "function" ||
      typeof budget.snapshot !== "function"
    ) {
      issues.push({ path: field("$", "budget"), code: "wrong_type", message: "budget must implement claimCall/recordUsage/snapshot" });
    }
  }
  if (value.recorder !== undefined) {
    const recorder = value.recorder;
    if (!isPlainRecord(recorder) || typeof recorder.record !== "function") {
      issues.push({ path: field("$", "recorder"), code: "wrong_type", message: "recorder must implement record(record)" });
    }
  }
  const noul = validateNoulDecisionPolicy(value.noul === undefined ? DEFAULT_NOUL_POLICY : value.noul, field("$", "noul"));
  if (!noul.ok) issues.push(...noul.issues);
  if (issues.length) return { ok: false, issues };
  const noulPolicy = noul.ok ? noul.value : DEFAULT_NOUL_POLICY;
  const pick = <K extends keyof typeof TYPESAFE_DEFAULTS>(key: K, raw: unknown): number =>
    raw === undefined ? TYPESAFE_DEFAULTS[key] : (raw as number);
  return {
    ok: true,
    value: {
      apiKey: value.apiKey as string,
      state: state.ok ? state.value : undefined,
      model: model as string,
      mode: mode as JudgmentRecordMode,
      providerInputPolicy: value.providerInputPolicy as ProviderInputPolicy | undefined,
      providerInputScope: typeof value.providerInputScope === "string" ? value.providerInputScope : "",
      allowTextBodyFallback: value.allowTextBodyFallback === true,
      timeoutMs: pick("timeoutMs", value.timeoutMs),
      deadlineMs: pick("deadlineMs", value.deadlineMs),
      maxAttempts: pick("maxAttempts", value.maxAttempts),
      backoffMs: pick("backoffMs", value.backoffMs),
      maxBackoffMs: pick("maxBackoffMs", value.maxBackoffMs),
      maxResponseBytes: pick("maxResponseBytes", value.maxResponseBytes),
      maxStateChars: pick("maxStateChars", value.maxStateChars),
      budget: value.budget as JudgmentBudgetAccount | undefined,
      recorder: value.recorder as JudgmentRecorder | undefined,
      noul: noulPolicy,
      fetch: (value.fetch as TypeSafeFetch | undefined) ?? defaultFetch,
      now: (value.now as (() => number) | undefined) ?? (() => Date.now()),
      sleep: (value.sleep as TypeSafeSleep | undefined) ?? defaultSleep,
    },
  };
}

export function parseTypeSafeConfig(value: unknown): NormalizedTypeSafeConfig {
  const result = validateTypeSafeConfig(value);
  if (!result.ok) throw new ContractViolation(result.issues);
  return result.value;
}

// ---------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------

export type TypeSafeBundle = {
  /** Required unless the adapter was configured with a trusted default state. */
  state?: ApprovedProviderState;
  questions: JudgmentQuestionEntry[];
  /** Narrows the per-attempt timeout for this bundle; never raises the configured one. */
  timeoutMs?: number;
  /** Shadow runs still record advisory provenance, but their outcomes never authorize work. */
  mode?: JudgmentRecordMode;
  /** Trusted scope label for this operation; narrows the configured default when present. */
  scope?: string;
  recorder?: JudgmentRecorder;
  budget?: JudgmentBudgetAccount;
};

export type TypeSafeUsageReport = JudgmentUsage & { calls: number };

/** Per-call model provenance: `version` is only claimed when this response carried it. */
export type TypeSafeModelReport = JudgmentModel & {
  observed: boolean;
  /** The configured model is an exact version id rather than a moving alias. */
  pinned: boolean;
};

export type JudgmentBatchFields = {
  /** Exactly one entry per requested question id, whatever happened upstream. */
  results: Record<string, JudgmentResult>;
  records: JudgmentRecord[];
  issues: ValidationIssue[];
  model: TypeSafeModelReport;
  usage: TypeSafeUsageReport;
  /** Machine-readable terminal provider-boundary failure, when the batch failed uniformly. */
  failureCode?: string;
};

/** A live batch: its decisions may inform dispatch once policy allows the action. */
export type LiveJudgmentBatch = JudgmentBatchFields & { ok: true; mode: "live"; actionability: "live" };

/** A shadow batch: advisory only, and never a source of dispatchable decisions. */
export type AdvisoryJudgmentBatch = JudgmentBatchFields & { ok: true; mode: "shadow"; actionability: "advisory" };

export type TypeSafeBundleResult = LiveJudgmentBatch | AdvisoryJudgmentBatch | { ok: false; issues: ValidationIssue[] };

export function judgmentBatchIsLive(result: TypeSafeBundleResult): result is LiveJudgmentBatch {
  return result.ok && result.actionability === "live";
}

/**
 * The actionability gate: anything that wants to dispatch on a judgment must pass
 * through this, so an advisory (shadow) outcome can never be used as a live decision.
 * The gate answers "live or advisory" for the batch; it certifies no single answer, so
 * callers still check each entry's `outcome` and `judgmentIsActionable(record)`.
 */
export function requireLiveJudgmentBatch(
  result: TypeSafeBundleResult,
): { ok: true; batch: LiveJudgmentBatch } | { ok: false; reason: "rejected" | "advisory"; message: string } {
  if (!result.ok) return { ok: false, reason: "rejected", message: "the judgment bundle was rejected before dispatch" };
  if (result.actionability !== "live") {
    return { ok: false, reason: "advisory", message: `a ${result.mode} judgment is advisory and cannot drive dispatch` };
  }
  return { ok: true, batch: result };
}

export interface TypeSafeJudgmentAdapter extends JudgmentAdapter {
  /** The configured model and pin. The version that answered is reported per call. */
  readonly model: JudgmentModel;
  readonly mode: JudgmentRecordMode;
  judge(bundle: TypeSafeBundle, signal?: AbortSignal): Promise<TypeSafeBundleResult>;
  usage(): TypeSafeUsageReport;
  /** The last version this adapter observed answering; `undefined` until there is one. */
  observedModelVersion(): string | undefined;
}

type ValidatedBundle = {
  state: ApprovedProviderState;
  questions: JudgmentQuestion[];
  timeoutMs?: number;
  /** Absent means "use the adapter's configured mode". */
  mode?: JudgmentRecordMode;
  scope?: string;
  recorder?: JudgmentRecorder;
  budget?: JudgmentBudgetAccount;
};

export function validateTypeSafeBundle(value: unknown, path = "$"): Validation<ValidatedBundle> {
  if (!isPlainRecord(value)) return fail(path, "not_object", "a judgment bundle must be an object");
  const issues: ValidationIssue[] = [];
  for (const key of Object.keys(value)) {
    if (
      key !== "state" &&
      key !== "questions" &&
      key !== "timeoutMs" &&
      key !== "mode" &&
      key !== "scope" &&
      key !== "recorder" &&
      key !== "budget"
    ) {
      issues.push({ path: field(path, key), code: "unknown_field", message: `unknown field "${key}"` });
    }
  }
  const state = validateApprovedProviderState(value.state, field(path, "state"));
  if (!state.ok) issues.push(...state.issues);
  const questions = validateJudgmentQuestions(value.questions, field(path, "questions"));
  if (!questions.ok) issues.push(...questions.issues);
  if (value.timeoutMs !== undefined) {
    const issue = boundedInt(field(path, "timeoutMs"), value.timeoutMs, 100, 10 * 60 * 1000);
    if (issue) issues.push(issue);
  }
  if (value.mode !== undefined && value.mode !== "live" && value.mode !== "shadow") {
    issues.push({ path: field(path, "mode"), code: "bad_syntax", message: 'mode must be "live" or "shadow"' });
  }
  if (
    value.scope !== undefined &&
    (typeof value.scope !== "string" || value.scope.length === 0 || value.scope.length > 128 || !QUESTION_ID_RE.test(value.scope))
  ) {
    issues.push({ path: field(path, "scope"), code: "bad_syntax", message: "scope must be an opaque id-shaped scope label" });
  }
  if (value.recorder !== undefined && (!isPlainRecord(value.recorder) || typeof value.recorder.record !== "function")) {
    issues.push({ path: field(path, "recorder"), code: "wrong_type", message: "recorder must implement record(record)" });
  }
  if (
    value.budget !== undefined &&
    (!isPlainRecord(value.budget) ||
      typeof value.budget.claimCall !== "function" ||
      typeof value.budget.recordUsage !== "function" ||
      typeof value.budget.snapshot !== "function")
  ) {
    issues.push({ path: field(path, "budget"), code: "wrong_type", message: "budget must implement claimCall/recordUsage/snapshot" });
  }
  if (issues.length || !state.ok || !questions.ok) return { ok: false, issues };
  return {
    ok: true,
    value: {
      state: state.value,
      questions: questions.value,
      timeoutMs: value.timeoutMs as number | undefined,
      mode: value.mode as JudgmentRecordMode | undefined,
      scope: value.scope as string | undefined,
      recorder: value.recorder as JudgmentRecorder | undefined,
      budget: value.budget as JudgmentBudgetAccount | undefined,
    },
  };
}

type AttemptOutcome =
  | { kind: "answers"; model: string; answers: Record<string, unknown>; usage: JudgmentUsage }
  | { kind: "retry"; code: string; message: string; retryAfterMs?: number }
  | { kind: "fail"; code: string; message: string };

export function createTypeSafeJudgmentAdapter(config: TypeSafeConfig): TypeSafeJudgmentAdapter {
  const cfg = parseTypeSafeConfig(config);
  const stats: TypeSafeUsageReport = { calls: 0, inputTokens: 0, outputTokens: 0 };
  let lastObservedModel: string | undefined;

  const bodyText = (bundle: ValidatedBundle): string =>
    JSON.stringify({
      state: bundle.state.state,
      model: cfg.model,
      questions: Object.fromEntries(bundle.questions.map((question) => [question.id, toWireQuestion(question)])),
    });

  type BoundedRead = { ok: true; text: string } | { ok: false; code: string; message: string };

  async function readBounded(response: TypeSafeResponseLike, maxBytes: number): Promise<BoundedRead> {
    const declared = Number(response.headers.get("content-length") ?? "");
    if (Number.isFinite(declared) && declared > maxBytes) {
      return { ok: false, code: "response_too_large", message: `provider declared ${declared} bytes; the limit is ${maxBytes}` };
    }
    const body = response.body;
    if (body && typeof body.getReader === "function") {
      const reader = body.getReader();
      const chunks: Uint8Array[] = [];
      let total = 0;
      for (;;) {
        let step: { done: boolean; value?: Uint8Array };
        try {
          step = await reader.read();
        } catch (error) {
          return { ok: false, code: "invalid_response", message: `provider body read failed (${safeErrorName(error)})` };
        }
        if (step.done) break;
        if (!step.value) continue;
        total += step.value.byteLength;
        if (total > maxBytes) {
          try {
            await reader.cancel?.();
          } catch {
            /* the over-large body is already refused; a failed cancel changes nothing */
          }
          return { ok: false, code: "response_too_large", message: `provider response exceeds ${maxBytes} bytes` };
        }
        chunks.push(step.value);
      }
      const merged = new Uint8Array(total);
      let offset = 0;
      for (const chunk of chunks) {
        merged.set(chunk, offset);
        offset += chunk.byteLength;
      }
      return { ok: true, text: new TextDecoder().decode(merged) };
    }
    if (!cfg.allowTextBodyFallback) {
      return {
        ok: false,
        code: "response_body_unbounded",
        message: "provider response exposes no byte stream and reading it as text would be unbounded",
      };
    }
    const text = await response.text();
    const bytes = new TextEncoder().encode(text).byteLength;
    if (bytes > maxBytes) {
      return { ok: false, code: "response_too_large", message: `provider response is ${bytes} bytes; the limit is ${maxBytes}` };
    }
    return { ok: true, text };
  }

  async function attemptOnce(
    body: string,
    attemptTimeoutMs: number,
    authorizedDigest: string,
    signal?: AbortSignal,
  ): Promise<AttemptOutcome> {
    // Invariant: the bytes sent are exactly the bytes trusted policy authorized.
    if (canonicalDigest(body) !== authorizedDigest) {
      return {
        kind: "fail",
        code: "provider_input_tampered",
        message: "the outbound request no longer matches the digest trusted policy authorized",
      };
    }
    const shaped = scanForSecretShapes(body);
    if (body.includes(cfg.apiKey) || shaped.length) {
      return {
        kind: "fail",
        code: "credential_in_input",
        message: `refusing to dispatch: the provider request carries credential-shaped text (${body.includes(cfg.apiKey) ? "configured_key" : shaped.join(", ")})`,
      };
    }
    if (Buffer.byteLength(body, "utf8") > JUDGMENT_LIMITS.requestBytes) {
      return { kind: "fail", code: "payload_too_large", message: `provider request exceeds ${JUDGMENT_LIMITS.requestBytes} bytes` };
    }
    const controller = new AbortController();
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      controller.abort();
    }, attemptTimeoutMs);
    const onAbort = (): void => controller.abort();
    signal?.addEventListener("abort", onAbort, { once: true });
    try {
      const response = await cfg.fetch(TYPESAFE_ENDPOINT, {
        method: "POST",
        headers: {
          authorization: `Bearer ${cfg.apiKey}`,
          "content-type": "application/json",
          accept: "application/json",
        },
        body,
        signal: controller.signal,
        redirect: "error",
      });
      if (response.status >= 200 && response.status <= 299) {
        const read = await readBounded(response, cfg.maxResponseBytes);
        if (!read.ok) return { kind: "fail", code: read.code, message: read.message };
        let parsed: unknown;
        try {
          parsed = JSON.parse(read.text);
        } catch {
          return { kind: "fail", code: "invalid_response", message: "provider response body is not JSON" };
        }
        const responseBody = parseTypeSafeResponse(parsed);
        if (!responseBody.ok) {
          const usageIssue = responseBody.issues.find((issue) => issue.path === "$.usage" || issue.path.startsWith("$.usage."));
          if (usageIssue) {
            return {
              kind: "fail",
              code: "usage_unreported",
              message: "the provider did not report usable token usage, so the answers cannot be accounted for",
            };
          }
          return {
            kind: "fail",
            code: "invalid_response",
            message: `provider response is malformed: ${responseBody.issues.map((issue) => `${issue.code}@${issue.path}`).join("; ")}`,
          };
        }
        return {
          kind: "answers",
          model: responseBody.value.model,
          answers: responseBody.value.answers,
          usage: responseBody.value.usage,
        };
      }
      if (TYPESAFE_ATTEMPT_STATUSES.includes(response.status)) {
        const hint = parseRetryAfterMs(response.headers.get("retry-after"), cfg.now());
        return {
          kind: "retry",
          code: `http_${response.status}`,
          message: `provider returned ${response.status}${hint === undefined ? "" : `; retry-after ${hint} ms`}`,
          retryAfterMs: hint,
        };
      }
      if (response.status === 401) {
        return {
          kind: "fail",
          code: "http_401",
          message: "provider rejected the configured key (401); check trusted adapter configuration",
        };
      }
      if (response.status === 422) {
        return {
          kind: "fail",
          code: "http_422",
          // A 422 body can quote the request back, so no part of it is reported.
          message: "provider rejected the request as invalid (422)",
        };
      }
      return { kind: "fail", code: `http_${response.status}`, message: `provider returned HTTP ${response.status}` };
    } catch (error) {
      if (signal?.aborted) return { kind: "fail", code: "aborted", message: "the caller aborted the judgment" };
      if (timedOut) return { kind: "retry", code: "timeout", message: `provider call exceeded ${attemptTimeoutMs} ms` };
      // A transport exception message is arbitrary text, so only a known name is reported.
      return { kind: "retry", code: "network_error", message: `provider call failed (${safeErrorName(error)})` };
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
    }
  }

  async function judge(bundleInput: TypeSafeBundle, signal?: AbortSignal): Promise<TypeSafeBundleResult> {
    const withState =
      isPlainRecord(bundleInput) && !hasOwnKey(bundleInput, "state") && cfg.state ? { ...bundleInput, state: cfg.state } : bundleInput;
    const validated = validateTypeSafeBundle(withState);
    if (!validated.ok) return { ok: false, issues: validated.issues };
    const bundle = validated.value;
    const mode = bundle.mode ?? cfg.mode;
    const scope = bundle.scope ?? cfg.providerInputScope;
    const budget = bundle.budget ?? cfg.budget;
    const recorder = bundle.recorder ?? cfg.recorder;
    const body = bodyText(bundle);
    const digest = canonicalDigest(body);
    const modelReport = (observed: boolean, version?: string): TypeSafeModelReport => ({
      provider: "typesafe",
      model: cfg.model,
      version: observed && version ? version : cfg.model,
      observed: observed && version !== undefined,
      pinned: isPinnedModel(cfg.model),
    });
    const batchResult = (fields: JudgmentBatchFields): TypeSafeBundleResult => {
      if (mode === "live") {
        const live: LiveJudgmentBatch = { ...fields, ok: true, mode: "live", actionability: "live" };
        return live;
      }
      const advisory: AdvisoryJudgmentBatch = { ...fields, ok: true, mode: "shadow", actionability: "advisory" };
      return advisory;
    };
    const failure = (
      reason: JudgmentFailure,
      attempts: number,
      observed: boolean,
      version?: string,
      authorizationRef?: string,
      usage: JudgmentUsage = { inputTokens: 0, outputTokens: 0 },
    ): TypeSafeBundleResult => {
      const recordContext: JudgmentRecordContext = {
        state: { digest: bundle.state.digest, source: bundle.state.source },
        model: { requested: cfg.model, resolved: version ?? cfg.model },
        modelObserved: observed,
        usage,
        attempts,
        batchQuestions: bundle.questions.length,
        mode,
        decidedAt: cfg.now(),
      };
      recordContext.requestDigest = digest;
      if (authorizationRef) recordContext.authorizationRef = authorizationRef;
      const resolved = judgmentFailureResults(bundle.questions, reason, recordContext);
      record(resolved.records, recorder);
      return batchResult({
        results: resolved.results,
        records: resolved.records,
        issues: [],
        model: modelReport(observed, version),
        usage: { ...stats },
        failureCode: reason.code,
      });
    };

    // A calibrated policy pins an exact version. An alias can still be shadowed, but it
    // can never produce an actionable live decision.
    if (mode === "live" && !isPinnedModel(cfg.model)) {
      return failure(
        {
          code: "uncalibrated_model",
          message: "a live judgment requires an exact pinned model version; a moving alias is not calibrated",
          retryable: false,
        },
        0,
        false,
      );
    }

    // Trusted provider-input policy authorizes the exact body before anything leaves.
    let authorizationRef: string | undefined;
    const policy = cfg.providerInputPolicy;
    let denial: JudgmentFailure | undefined;
    if (!policy) {
      denial = {
        code: "provider_input_unattested",
        message: "no trusted provider-input policy is configured, so this judgment input is not authorized to leave",
        retryable: false,
      };
    } else {
      let decision: unknown;
      try {
        decision = await policy({
          endpoint: TYPESAFE_ENDPOINT,
          model: cfg.model,
          mode,
          body,
          digest,
          ...(scope ? { scope } : {}),
          state: { source: bundle.state.source, digest: bundle.state.digest, text: stateText(bundle.state.state) },
          parts: providerInputParts(bundle.questions),
        });
      } catch {
        denial = {
          code: "provider_input_denied",
          message: "the trusted provider-input policy failed while evaluating this request (policy_error)",
          retryable: false,
        };
      }
      if (!denial) {
        if (!isPlainRecord(decision)) {
          denial = {
            code: "provider_input_unattested",
            message: "the trusted provider-input policy returned no usable decision",
            retryable: false,
          };
        } else if (decision.authorized !== true) {
          const code =
            typeof decision.code === "string" && decision.code.length <= 64 && QUESTION_ID_RE.test(decision.code) ? decision.code : "denied";
          denial = {
            code: "provider_input_denied",
            message: `the trusted provider-input policy denied this request (${code})`,
            retryable: false,
          };
        } else if (decision.digest !== digest) {
          denial = {
            code: "provider_input_tampered",
            message: "the trusted provider-input policy authorized a digest that does not match the outbound body",
            retryable: false,
          };
        } else if (typeof decision.authorizationRef === "string" && decision.authorizationRef.length <= 128 && QUESTION_ID_RE.test(decision.authorizationRef)) {
          authorizationRef = decision.authorizationRef;
        }
      }
    }
    if (denial) return failure(denial, 0, false);

    const questionTimeouts = bundle.questions
      .map((question) => question.request.timeoutMs)
      .filter((value): value is number => typeof value === "number");
    const perAttemptTimeout = Math.max(
      1,
      Math.min(cfg.timeoutMs, bundle.timeoutMs ?? Number.MAX_SAFE_INTEGER, ...(questionTimeouts.length ? questionTimeouts : [Number.MAX_SAFE_INTEGER])),
    );
    const deadlineAt = cfg.now() + cfg.deadlineMs;
    const batchQuestions = bundle.questions.length;

    let attempts = 0;
    let last: JudgmentFailure = { code: "provider_failure", message: "no provider attempt was made", retryable: false };
    for (;;) {
      if (signal?.aborted) {
        return failure({ code: "aborted", message: "the caller aborted before dispatch", retryable: false }, attempts, false, undefined, authorizationRef);
      }
      if (cfg.now() >= deadlineAt) {
        last = { code: "deadline_exceeded", message: `the ${cfg.deadlineMs} ms provider deadline is exhausted`, retryable: true };
        break;
      }
      if (attempts >= cfg.maxAttempts) break;
      if (budget && !budget.claimCall()) {
        last = { code: "budget_exceeded", message: "the judgment budget is exhausted; no further provider call was made", retryable: false };
        break;
      }
      attempts += 1;
      stats.calls += 1;
      // An attempt never runs past the deadline, and never asks for more time than remains.
      const attemptTimeoutMs = Math.max(1, Math.min(perAttemptTimeout, deadlineAt - cfg.now()));
      const outcome = await attemptOnce(body, attemptTimeoutMs, digest, signal);
      if (outcome.kind === "answers") {
        const observed = outcome.model;
        lastObservedModel = observed;
        stats.inputTokens += outcome.usage.inputTokens;
        stats.outputTokens += outcome.usage.outputTokens;
        budget?.recordUsage(outcome.usage);
        if (mode === "live" && observed !== cfg.model) {
          // Calibration is per version: an actionable decision requires the exact pin.
          return failure(
            {
              code: "model_version_changed",
              message: `the provider answered with a version other than the configured pin (${cfg.model}), so no decision is actionable`,
              retryable: false,
            },
            attempts,
            true,
            observed,
            authorizationRef,
            outcome.usage,
          );
        }
        const recordContext: JudgmentRecordContext = {
          state: { digest: bundle.state.digest, source: bundle.state.source },
          model: { requested: cfg.model, resolved: observed },
          modelObserved: true,
          usage: outcome.usage,
          attempts,
          batchQuestions,
          mode,
          decidedAt: cfg.now(),
        };
        recordContext.requestDigest = digest;
        if (authorizationRef) recordContext.authorizationRef = authorizationRef;
        const resolved = resolveJudgmentAnswers({
          questions: bundle.questions,
          response: { model: observed, answers: outcome.answers, usage: outcome.usage },
          model: { requested: cfg.model, resolved: observed },
          context: recordContext,
          noul: cfg.noul,
        });
        record(resolved.records, recorder);
        return batchResult({
          results: resolved.results,
          records: resolved.records,
          issues: resolved.issues,
          model: modelReport(true, observed),
          usage: { ...stats },
        });
      }
      last = { code: outcome.code, message: outcome.message, retryable: outcome.kind === "retry" };
      if (outcome.kind === "fail") break;
      const backoff = Math.min(cfg.maxBackoffMs, cfg.backoffMs * 2 ** (attempts - 1));
      const wait = Math.max(outcome.retryAfterMs ?? 0, backoff);
      if (attempts >= cfg.maxAttempts) break;
      const remaining = deadlineAt - cfg.now();
      if (wait >= remaining) {
        last = {
          code: last.code,
          message: `${last.message}; retry abandoned with ${Math.max(0, Math.round(remaining))} ms left in the deadline`,
          retryable: true,
        };
        break;
      }
      try {
        await cfg.sleep(wait, signal);
      } catch {
        if (signal?.aborted) {
          return failure(
            { code: "aborted", message: "the caller aborted during provider backoff", retryable: false },
            attempts,
            false,
            undefined,
            authorizationRef,
          );
        }
        last = { code: "backoff_failed", message: `provider backoff wait failed after ${wait} ms`, retryable: true };
        break;
      }
    }
    return failure({ ...last, message: `${last.code}: ${last.message}` }, attempts, false, undefined, authorizationRef);
  }

  const single = async (
    id: string,
    request: ChoiceJudgmentRequest | ScoreJudgmentRequest | NoulJudgmentRequest,
    signal?: AbortSignal,
  ): Promise<JudgmentResult> => {
    if (cfg.mode !== "live") {
      // The frozen single-question results have no actionability channel, so a shadow
      // adapter refuses to hand back a decision here instead of leaking an advisory one.
      return {
        outcome: "provider_failure",
        retryable: false,
        message: "advisory_shadow: shadow judgments are not actionable; call judge() and gate the result with requireLiveJudgmentBatch()",
      };
    }
    if (!cfg.state) {
      return {
        outcome: "provider_failure",
        retryable: false,
        message:
          "invalid_request: single-question calls need an approved provider state; configure config.state or call judge() with a bundle state",
      };
    }
    const result = await judge({ questions: [{ id, request }] }, signal);
    if (!result.ok) {
      return {
        outcome: "provider_failure",
        retryable: false,
        message: truncate(`invalid_request: ${result.issues.map((issue) => `${issue.code}@${issue.path}`).join("; ")}`, JUDGMENT_LIMITS.messageChars),
      };
    }
    return result.results[id];
  };

  return {
    get model() {
      return { provider: "typesafe" as const, model: cfg.model, version: cfg.model };
    },
    get mode() {
      return cfg.mode;
    },
    judge,
    usage: () => ({ ...stats }),
    observedModelVersion: () => lastObservedModel,
    async choice(request, signal): Promise<ChoiceJudgmentResult> {
      return (await single(SINGLE_QUESTION_IDS.choice, request, signal)) as ChoiceJudgmentResult;
    },
    async score(request, signal): Promise<ScoreJudgmentResult> {
      return (await single(SINGLE_QUESTION_IDS.score, request, signal)) as ScoreJudgmentResult;
    },
    async noul(request, signal): Promise<NoulJudgmentResult> {
      return (await single(SINGLE_QUESTION_IDS.noul, request, signal)) as NoulJudgmentResult;
    },
  };
}

function record(records: readonly JudgmentRecord[], recorder?: JudgmentRecorder): void {
  if (!recorder) return;
  for (const entry of records) recorder.record(entry);
}
