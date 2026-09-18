import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { canonicalDigest, validateRecordedDecision } from "../src/operations/contracts.ts";
import {
  DEFAULT_JUDGMENT_BUDGET,
  NO_MATCH_OPTION,
  createJudgmentBudgetAccount,
  createShadowRecorder,
  isPinnedModel,
  judgmentIsActionable,
  parseJudgmentQuestion,
  questionDigest,
  validateJudgmentBudget,
  validateJudgmentQuestion,
  validateJudgmentQuestions,
  validateJudgmentRecord,
  validateNoulDecisionPolicy,
} from "../src/operations/judgments.ts";
import type { JudgmentQuestionEntry, JudgmentRecord, JudgmentResult } from "../src/operations/judgments.ts";
import {
  TYPESAFE_DEFAULT_MODEL,
  TYPESAFE_ENDPOINT,
  approveProviderState,
  createTypeSafeJudgmentAdapter,
  judgmentBatchIsLive,
  parseRetryAfterMs,
  redactDiagnostic,
  requireLiveJudgmentBatch,
  scanForSecretShapes,
  validateApprovedProviderState,
  validateTypeSafeBundle,
  validateTypeSafeConfig,
} from "../src/operations/typesafe.ts";
import type {
  ApprovedProviderState,
  ProviderInputPolicy,
  ProviderInputRequest,
  TypeSafeBundleResult,
  TypeSafeConfig,
  TypeSafeFetch,
  TypeSafeFetchInit,
  TypeSafeResponseLike,
} from "../src/operations/typesafe.ts";

const FIXTURES = path.join(path.dirname(fileURLToPath(import.meta.url)), "fixtures", "operations-judgments");
const API_KEY = "ts_test_key_0123456789";
const DIGEST_LIKE = `sha256:${"ab".repeat(32)}`;
const read = <T>(relative: string): T => JSON.parse(fs.readFileSync(path.join(FIXTURES, relative), "utf8")) as T;

const NOUL_REQUEST = { question: "Does this convey urgency?", questionVersion: "v1" };
const CHOICE_REQUEST = {
  question: "Which team should handle this?",
  questionVersion: "v1",
  options: [
    { id: "billing", label: "Payments, invoicing, refunds" },
    { id: "technical", label: "Bugs, outages, integrations" },
  ],
};
const SCORE_REQUEST = {
  question: "How frustrated is the customer?",
  questionVersion: "v2",
  rubric: ["Calm", "Frustrated", "Very angry"],
};
const ABSTAIN_KEYS = [NO_MATCH_OPTION, "cannot_tell_between_options"];

function approvedState(overrides: { source?: string; state?: unknown } = {}): ApprovedProviderState {
  const result = approveProviderState({ state: overrides.state ?? { session: "s-1" }, source: overrides.source ?? "session_context" });
  assert.equal(result.ok, true, JSON.stringify(result));
  return result.ok ? result.value : (undefined as unknown as ApprovedProviderState);
}

// ---------------------------------------------------------------------------
// A deterministic transport mock: no network, no account, no clock
// ---------------------------------------------------------------------------

type TransportSpec = {
  status?: number;
  headers?: Record<string, string>;
  body?: unknown;
  rawBody?: string;
  errorName?: string;
  errorMessage?: string;
  stream?: boolean;
  noStream?: boolean;
  holdUntilAbort?: boolean;
};

type TransportCall = { url: string; init: TypeSafeFetchInit; body: Record<string, unknown> };

function mockTransport(specs: TransportSpec[]): { fetch: TypeSafeFetch; calls: TransportCall[] } {
  const queue = [...specs];
  const calls: TransportCall[] = [];
  const fetcher: TypeSafeFetch = async (url, init) => {
    const spec: TransportSpec = queue.shift() ?? { status: 599, body: { unexpected: "call" } };
    calls.push({ url, init, body: JSON.parse(init.body) as Record<string, unknown> });
    if (spec.holdUntilAbort) {
      return new Promise<TypeSafeResponseLike>((_, reject) => {
        init.signal.addEventListener("abort", () => reject(new Error("mock transport aborted")), { once: true });
      });
    }
    if (spec.errorName) {
      const error = new Error(spec.errorMessage ?? "mock transport failure");
      error.name = spec.errorName;
      throw error;
    }
    const headers = new Map(Object.entries(spec.headers ?? {}).map(([key, value]) => [key.toLowerCase(), value] as const));
    const text = spec.rawBody ?? JSON.stringify(spec.body ?? {});
    const bytes = new TextEncoder().encode(text);
    const response: TypeSafeResponseLike = {
      status: spec.status ?? 200,
      headers: { get: (name) => headers.get(name.toLowerCase()) ?? null },
      text: async () => text,
    };
    if (!spec.noStream) {
      const midpoint = Math.ceil(bytes.length / 2);
      const chunks = spec.stream ? [bytes.slice(0, midpoint), bytes.slice(midpoint)] : [bytes];
      let index = 0;
      response.body = {
        getReader: () => ({
          read: async () => (index < chunks.length ? { done: false as const, value: chunks[index++] } : { done: true as const }),
          cancel: async () => {
            index = chunks.length;
          },
        }),
      };
    }
    return response;
  };
  return { fetch: fetcher, calls };
}

const providerBody = (answers: Record<string, unknown>, model: string = TYPESAFE_DEFAULT_MODEL): Record<string, unknown> => ({
  model,
  answers,
  usage: { input_tokens: 120, output_tokens: 30 },
});

/** Test stand-in for the O07/O08 trusted policy: authorizes exactly what it was shown. */
const authorizeExactly: ProviderInputPolicy = (request) => ({ authorized: true, digest: request.digest });

function adapterWith(
  transport: { fetch: TypeSafeFetch },
  overrides: Partial<Omit<TypeSafeConfig, "apiKey" | "state" | "fetch">> = {},
  state: ApprovedProviderState = approvedState(),
): ReturnType<typeof createTypeSafeJudgmentAdapter> {
  return createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state,
    fetch: transport.fetch,
    providerInputPolicy: authorizeExactly,
    ...overrides,
  });
}

const single = (id: string, request: unknown): JudgmentQuestionEntry => ({ id, request: request as JudgmentQuestionEntry["request"] });

function issueCodes(result: { ok: true } | { ok: false; issues: Array<{ code: string }> }): string[] {
  return result.ok ? [] : [...new Set(result.issues.map((issue) => issue.code))].sort();
}

function resultOf(result: TypeSafeBundleResult, id: string): JudgmentResult {
  assert.equal(result.ok, true, JSON.stringify(result.ok ? {} : result.issues));
  return (result as { results: Record<string, JudgmentResult> }).results[id];
}

function failureOf(result: TypeSafeBundleResult, id: string): { retryable: boolean; message: string } {
  const value = resultOf(result, id);
  assert.equal(value.outcome, "provider_failure", `expected provider_failure, got ${JSON.stringify(value)}`);
  return value as { retryable: boolean; message: string };
}

// ---------------------------------------------------------------------------
// Fixture corpus
// ---------------------------------------------------------------------------

type Manifest = { contractVersion: string; files: Array<{ path: string; kind: string }> };
type CaseFile = {
  name: string;
  config?: Record<string, unknown>;
  /** Which trusted-policy behaviour this case exercises; defaults to authorizing exactly. */
  policy?: "authorize" | "deny" | "tamper" | "absent";
  state: { source: string; state: unknown };
  stateInBundle?: boolean;
  questions: Array<{ id: string; request: unknown }>;
  transports: TransportSpec[];
  expect: {
    ok: boolean;
    issues?: string[];
    issueCodes?: string[];
    actionability?: "live" | "advisory";
    results?: Record<string, Record<string, unknown>>;
    calls?: number;
    sleeps?: number[];
    usage?: Record<string, number>;
    modelVersion?: string;
    records?: Array<Record<string, unknown>>;
    request?: { model?: string; questionIds?: string[]; criteria?: Record<string, string[]> };
  };
};
type StateFile = { input: Record<string, unknown>; expect: { ok: boolean; issues?: string[] } };

const policies: Record<string, ProviderInputPolicy | undefined> = {
  authorize: authorizeExactly,
  deny: () => ({ authorized: false, code: "tenant_policy_denied" }),
  tamper: () => ({ authorized: true, digest: DIGEST_LIKE }),
  absent: undefined,
};

function matchExpected(actual: Record<string, unknown>, expected: Record<string, unknown>, label: string): void {
  for (const [key, value] of Object.entries(expected)) {
    if (key === "messageIncludes") {
      const message = actual.message;
      assert.equal(typeof message, "string", `${label}.message`);
      for (const fragment of value as string[]) {
        assert.ok((message as string).includes(fragment), `${label}.message should include ${JSON.stringify(fragment)}: ${message}`);
      }
      continue;
    }
    if (key === "messageExcludes") {
      const message = String(actual.message ?? "");
      for (const fragment of value as string[]) {
        assert.equal(message.includes(fragment), false, `${label}.message must not include ${JSON.stringify(fragment)}: ${message}`);
      }
      continue;
    }
    assert.deepEqual(actual[key], value, `${label}.${key}`);
  }
}

test("every fixture case resolves exactly as recorded and none is unlisted", async () => {
  const manifest = read<Manifest>("manifest.json");
  assert.equal(manifest.contractVersion, "judgment/v1");
  for (const entry of manifest.files) {
    const fixture = read<CaseFile | StateFile>(entry.path);
    if (entry.kind === "approved_state_input") {
      const state = fixture as StateFile;
      const approved = approveProviderState(state.input);
      assert.equal(approved.ok, state.expect.ok, `${entry.path} approval`);
      assert.deepEqual(issueCodes(approved), state.expect.issues ?? [], `${entry.path} issues`);
      continue;
    }
    const caseFile = fixture as CaseFile;
    const transport = mockTransport(caseFile.transports);
    const approved = approveProviderState({ state: caseFile.state.state, source: caseFile.state.source });
    assert.equal(approved.ok, true, `${entry.path} state`);
    const state = approved.ok ? approved.value : approvedState();
    const sleeps: number[] = [];
    const adapter = createTypeSafeJudgmentAdapter({
      apiKey: API_KEY,
      ...(caseFile.stateInBundle ? {} : { state }),
      fetch: transport.fetch,
      providerInputPolicy: policies[caseFile.policy ?? "authorize"],
      sleep: async (ms: number) => {
        sleeps.push(ms);
      },
      ...(caseFile.config as Partial<Omit<TypeSafeConfig, "apiKey" | "state" | "fetch" | "sleep">> | undefined),
    });
    const result = await adapter.judge({
      questions: caseFile.questions as JudgmentQuestionEntry[],
      ...(caseFile.stateInBundle ? { state } : {}),
    });

    if (!caseFile.expect.ok) {
      assert.equal(result.ok, false, `${entry.path} should be rejected`);
      assert.deepEqual(issueCodes(result), caseFile.expect.issues ?? [], `${entry.path} issues`);
      assert.equal(transport.calls.length, 0, `${entry.path} must not dispatch`);
      continue;
    }
    assert.equal(result.ok, true, `${entry.path} should resolve: ${JSON.stringify(issueCodes(result))}`);
    if (!result.ok) continue;
    const expectedActionability = caseFile.expect.actionability ?? "live";
    assert.equal(result.actionability, expectedActionability, `${entry.path} actionability`);
    assert.equal(result.mode, expectedActionability === "live" ? "live" : "shadow", `${entry.path} mode`);
    assert.equal(judgmentBatchIsLive(result), expectedActionability === "live", `${entry.path} live gate`);
    assert.equal(
      requireLiveJudgmentBatch(result).ok,
      expectedActionability === "live",
      `${entry.path} requireLiveJudgmentBatch`,
    );
    if (caseFile.expect.issueCodes) {
      assert.deepEqual([...new Set(result.issues.map((issue) => issue.code))].sort(), caseFile.expect.issueCodes, `${entry.path} issue codes`);
    }
    assert.deepEqual(Object.keys(result.results).sort(), caseFile.questions.map((question) => question.id).sort(), `${entry.path} ids`);
    for (const [id, expected] of Object.entries(caseFile.expect.results ?? {})) {
      matchExpected(result.results[id] as unknown as Record<string, unknown>, expected, `${entry.path} results.${id}`);
    }
    if (caseFile.expect.calls !== undefined) assert.equal(transport.calls.length, caseFile.expect.calls, `${entry.path} calls`);
    if (caseFile.expect.sleeps) assert.deepEqual(sleeps, caseFile.expect.sleeps, `${entry.path} sleeps`);
    for (const [key, value] of Object.entries(caseFile.expect.usage ?? {})) {
      assert.equal((result.usage as unknown as Record<string, number>)[key], value, `${entry.path} usage.${key}`);
    }
    if (caseFile.expect.modelVersion) assert.equal(result.model.version, caseFile.expect.modelVersion, `${entry.path} model`);
    for (const [index, expected] of (caseFile.expect.records ?? []).entries()) {
      const record = result.records[index] as unknown as Record<string, unknown>;
      assert.ok(record, `${entry.path} records[${index}]`);
      matchExpected(record, expected, `${entry.path} records[${index}]`);
    }
    if (caseFile.expect.request) {
      const call = transport.calls[0];
      assert.ok(call, `${entry.path} should send one request`);
      assert.equal(call.url, TYPESAFE_ENDPOINT);
      assert.equal(call.init.headers.authorization, `Bearer ${API_KEY}`);
      if (caseFile.expect.request.model) assert.equal(call.body.model, caseFile.expect.request.model, `${entry.path} model pin`);
      const questions = call.body.questions as Record<string, { criteria?: Record<string, string> }>;
      if (caseFile.expect.request.questionIds) {
        assert.deepEqual(Object.keys(questions).sort(), [...caseFile.expect.request.questionIds].sort(), `${entry.path} request ids`);
      }
      for (const [id, keys] of Object.entries(caseFile.expect.request.criteria ?? {})) {
        assert.deepEqual(Object.keys(questions[id].criteria ?? {}).sort(), [...keys].sort(), `${entry.path} criteria ${id}`);
      }
      assert.equal(JSON.stringify(call.body).includes(API_KEY), false, `${entry.path} key never enters the body`);
    }
  }
  const listed = new Set(manifest.files.map((entry) => entry.path));
  const onDisk = (fs.readdirSync(FIXTURES, { recursive: true }) as string[])
    .filter((name) => name.endsWith(".json") && name !== "manifest.json")
    .map((name) => name.split(path.sep).join("/"));
  for (const name of onDisk) assert.ok(listed.has(name), `fixture ${name} is missing from the manifest`);
  assert.equal(listed.size, manifest.files.length);
});

// ---------------------------------------------------------------------------
// Trusted input and credentials
// ---------------------------------------------------------------------------

test("provider state is approved by trusted policy and refuses to drift", () => {
  const approved = approveProviderState({ state: "Help! My payouts are failing.", source: "session_context" });
  assert.equal(approved.ok, true);
  if (!approved.ok) return;
  assert.equal(approved.value.approval, "trusted_policy");
  assert.equal(approved.value.classification, "provider_eligible");
  assert.equal(approved.value.digest.startsWith("sha256:"), true);
  assert.equal(approved.value.byteLength, "Help! My payouts are failing.".length);
  assert.equal(validateApprovedProviderState(approved.value).ok, true);
  assert.deepEqual(issueCodes(validateApprovedProviderState({ ...approved.value, digest: DIGEST_LIKE })), ["permission_denied"]);
  assert.deepEqual(issueCodes(validateApprovedProviderState({ sessionId: "s-1", text: "raw context" })), ["permission_denied"]);
  assert.deepEqual(issueCodes(validateApprovedProviderState("Help!")), ["permission_denied"]);
  assert.deepEqual(issueCodes(approveProviderState({ state: "x", source: "has space" })), ["bad_syntax"]);
  assert.deepEqual(issueCodes(approveProviderState({ state: "x", source: "session_context", extra: 1 })), ["unknown_field"]);
  assert.deepEqual(
    issueCodes(approveProviderState({ state: "x".repeat(600), source: "session_context", maxChars: 512 })),
    ["string_too_long"],
  );
});

test("a bundle refuses raw state, and credential-shaped text never reaches the provider", async () => {
  const raw = validateTypeSafeBundle({ state: { session: "s-1" }, questions: [single("q", NOUL_REQUEST)] });
  assert.deepEqual(issueCodes(raw), ["permission_denied"]);
  assert.deepEqual(issueCodes(validateTypeSafeBundle({ questions: [single("q", NOUL_REQUEST)] })), ["permission_denied"]);

  const secret = approveProviderState({
    state: "deploy log\nauthorization: Bearer sk-live-abcdefghijklmnop",
    source: "session_context",
  });
  assert.deepEqual(issueCodes(secret), ["forbidden_key"]);
  assert.ok(scanForSecretShapes("api_key = 0123456789abcdef").includes("credential_assignment"));
  assert.ok(scanForSecretShapes("-----BEGIN RSA PRIVATE KEY-----").includes("private_key_block"));
  assert.deepEqual(scanForSecretShapes("ordinary harness state"), []);
  assert.equal(redactDiagnostic(`failed with ${API_KEY}`, [API_KEY]).includes(API_KEY), false);
  assert.equal(redactDiagnostic("authorization: Bearer sk-live-abcdefghijklmnop").includes("sk-live"), false);

  const transport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) }]);
  const result = await adapterWith(transport).judge({ questions: [single("q", { question: `use ${API_KEY}`, questionVersion: "v1" })] });
  assert.equal(transport.calls.length, 0);
  const failure = failureOf(result, "q");
  assert.equal(failure.message.includes("credential_in_input"), true);
  assert.equal(failure.message.includes(API_KEY), false);
});

// ---------------------------------------------------------------------------
// Batching, request shape, and the frozen per-question interface
// ---------------------------------------------------------------------------

test("one request carries the whole batch and answers stay keyed to question ids", async () => {
  const transport = mockTransport([
    {
      status: 200,
      body: providerBody({
        urgent: { type: "noul", noul: 0.92 },
        dept: {
          type: "choice",
          choice: "technical",
          probabilities: {
            billing: 0.08,
            technical: 0.85,
            sales: 0.07,
            [NO_MATCH_OPTION]: 0,
            [ABSTAIN_KEYS[1]]: 0,
          },
          confidence: 0.82,
        },
        frustration: {
          type: "score",
          score: 1.6,
          legend: { "0": "Calm", "1": "Frustrated", "2": "Very angry" },
          probabilities: { "0": 0.05, "1": 0.3, "2": 0.65 },
          confidence: 0.78,
        },
      }),
    },
  ]);
  const adapter = adapterWith(transport);
  const result = await adapter.judge({
    questions: [
      single("urgent", NOUL_REQUEST),
      single("dept", { ...CHOICE_REQUEST, options: [...CHOICE_REQUEST.options, { id: "sales", label: "Pricing, upgrades" }] }),
      single("frustration", SCORE_REQUEST),
    ],
  });
  assert.equal(transport.calls.length, 1, "a batch is a single provider call");
  assert.deepEqual(resultOf(result, "urgent"), { outcome: "yes", modelVersion: TYPESAFE_DEFAULT_MODEL });
  assert.deepEqual(resultOf(result, "dept"), { outcome: "selected", optionId: "technical", confidence: 0.82 });
  assert.deepEqual(resultOf(result, "frustration"), {
    outcome: "scored",
    scores: [{ criterion: SCORE_REQUEST.question, score: 1.6 }],
    modelVersion: TYPESAFE_DEFAULT_MODEL,
  });
  if (!result.ok) return;
  assert.deepEqual(Object.keys(result.results).sort(), ["dept", "frustration", "urgent"]);
  assert.deepEqual(result.model, {
    provider: "typesafe",
    model: TYPESAFE_DEFAULT_MODEL,
    version: TYPESAFE_DEFAULT_MODEL,
    observed: true,
    pinned: true,
  });
  assert.deepEqual(result.usage, { calls: 1, inputTokens: 120, outputTokens: 30 });
  const sentQuestions = transport.calls[0].body.questions as Record<string, unknown>;
  assert.deepEqual(Object.keys(sentQuestions).sort(), ["dept", "frustration", "urgent"]);
  assert.equal(transport.calls[0].body.model, TYPESAFE_DEFAULT_MODEL);
  assert.equal(transport.calls[0].init.redirect, "error", "a redirect could forward the key to another origin");
  assert.deepEqual(result.records.map((record) => record.decision).sort(), ["scored", "selected", "yes"]);
  assert.deepEqual(result.records.map((record) => record.mode), ["live", "live", "live"]);
  assert.equal(result.records.every((record) => record.advisory && record.batchQuestions === 3), true);
});

test("the frozen per-question methods use the same batched path", async () => {
  const transport = mockTransport([
    {
      status: 200,
      body: providerBody({
        single_choice: {
          type: "choice",
          choice: "billing",
          probabilities: { billing: 0.9, technical: 0.1, [NO_MATCH_OPTION]: 0, [ABSTAIN_KEYS[1]]: 0 },
          confidence: 0.9,
        },
      }),
    },
    { status: 200, body: providerBody({ single_noul: { type: "noul", noul: 0.1 } }) },
  ]);
  const adapter = adapterWith(transport);
  assert.deepEqual(await adapter.choice(CHOICE_REQUEST), { outcome: "selected", optionId: "billing", confidence: 0.9 });
  assert.equal(transport.calls.length, 1);
  assert.equal(adapter.model.version, TYPESAFE_DEFAULT_MODEL);
  assert.deepEqual(await adapter.noul(NOUL_REQUEST), { outcome: "no", modelVersion: TYPESAFE_DEFAULT_MODEL });
  assert.equal(adapter.model.version, TYPESAFE_DEFAULT_MODEL, "the configured pin is reported on the adapter");
  assert.equal(adapter.observedModelVersion(), TYPESAFE_DEFAULT_MODEL, "the observed version is reported separately");
  assert.equal(adapter.usage().calls, 2);
  assert.equal(adapter.mode, "live");

  const stateless = createTypeSafeJudgmentAdapter({ apiKey: API_KEY, fetch: transport.fetch, providerInputPolicy: authorizeExactly });
  const refused = await stateless.choice(CHOICE_REQUEST);
  assert.equal(refused.outcome, "provider_failure");
  assert.ok(refused.outcome === "provider_failure" && refused.message.includes("invalid_request"));
  assert.equal(transport.calls.length, 2, "a stateless single call never reaches the provider");

  const shadow = createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state: approvedState(),
    fetch: transport.fetch,
    mode: "shadow",
    providerInputPolicy: authorizeExactly,
  });
  const advisory = await shadow.choice(CHOICE_REQUEST);
  assert.equal(shadow.mode, "shadow");
  assert.equal(advisory.outcome, "provider_failure", "a shadow adapter cannot deliver a dispatchable choice");
  assert.ok(advisory.outcome === "provider_failure" && advisory.message.includes("advisory_shadow"));
  assert.equal(transport.calls.length, 2, "the shadow refusal dispatches nothing");
});

// ---------------------------------------------------------------------------
// Answers: ranges, candidate keys, probability shapes, ids, and types
// ---------------------------------------------------------------------------

test("choice no_match and ties are first-class decisions", async () => {
  const noMatchTransport = mockTransport([
    {
      status: 200,
      body: providerBody({
        dept: {
          type: "choice",
          choice: NO_MATCH_OPTION,
          probabilities: { billing: 0.1, technical: 0.1, [NO_MATCH_OPTION]: 0.8, [ABSTAIN_KEYS[1]]: 0 },
          confidence: 0.8,
        },
      }),
    },
  ]);
  assert.deepEqual(resultOf(await adapterWith(noMatchTransport).judge({ questions: [single("dept", CHOICE_REQUEST)] }), "dept"), {
    outcome: "no_match",
  });

  const tieTransport = mockTransport([
    {
      status: 200,
      body: providerBody({
        dept: {
          type: "choice",
          choice: "billing",
          probabilities: { billing: 0.5, technical: 0.5, [NO_MATCH_OPTION]: 0, [ABSTAIN_KEYS[1]]: 0 },
          confidence: 0.5,
        },
      }),
    },
  ]);
  assert.deepEqual(resultOf(await adapterWith(tieTransport).judge({ questions: [single("dept", CHOICE_REQUEST)] }), "dept"), {
    outcome: "ambiguous",
  });
});

test("invalid answers name the reason and never become a decision", async () => {
  const transport = mockTransport([
    { status: 200, body: providerBody({ dept: { type: "noul", noul: 0.9 } }) },
    {
      status: 200,
      body: providerBody({
        dept: {
          type: "choice",
          choice: "sale",
          probabilities: { billing: 0.5, technical: 0.5, [NO_MATCH_OPTION]: 0, [ABSTAIN_KEYS[1]]: 0 },
          confidence: 0.5,
        },
      }),
    },
    {
      status: 200,
      body: providerBody({
        dept: {
          type: "choice",
          choice: "billing",
          probabilities: { billing: 0.4, technical: 0.5, [NO_MATCH_OPTION]: 0.1, [ABSTAIN_KEYS[1]]: 0 },
          confidence: 0.5,
        },
      }),
    },
    {
      status: 200,
      body: providerBody({
        dept: {
          type: "choice",
          choice: "billing",
          probabilities: { billing: 0.95, technical: 0.05, [NO_MATCH_OPTION]: 0.05, [ABSTAIN_KEYS[1]]: 0 },
          confidence: 0.9,
        },
      }),
    },
  ]);
  const adapter = adapterWith(transport);
  const ask = (): Promise<TypeSafeBundleResult> => adapter.judge({ questions: [single("dept", CHOICE_REQUEST)] });
  assert.equal(failureOf(await ask(), "dept").message.includes("wrong_type"), true);
  assert.equal(failureOf(await ask(), "dept").message.includes("unknown_field"), true);
  assert.equal(failureOf(await ask(), "dept").message.includes("bad_syntax"), true);
  const shifted = failureOf(await ask(), "dept");
  assert.equal(shifted.message.includes("out_of_range"), true, "probabilities that do not sum to 1 are refused: " + shifted.message);
  assert.equal(transport.calls.length, 4);
});

test("missing and unknown answer ids are reported beside the answers that resolved", async () => {
  const transport = mockTransport([
    { status: 200, body: providerBody({ b: { type: "noul", noul: 0.9 }, extra: { type: "noul", noul: 0.1 } }) },
  ]);
  const result = await adapterWith(transport).judge({ questions: [single("a", NOUL_REQUEST), single("b", NOUL_REQUEST)] });
  assert.equal(resultOf(result, "b").outcome, "yes");
  assert.equal(failureOf(result, "a").message.includes("missing_answer"), true);
  if (!result.ok) return;
  assert.deepEqual([...new Set(result.issues.map((issue) => issue.code))].sort(), ["missing_field", "unknown_field"]);
  assert.deepEqual(result.issues.map((issue) => issue.path).sort(), ["$.answers.a", "$.answers.extra"]);
});

test("noul has no separate confidence, and the undecided band is explicit", async () => {
  const transport = mockTransport([
    { status: 200, body: providerBody({ q: { type: "noul", noul: 0.5 } }) },
    { status: 200, body: providerBody({ q: { type: "noul", noul: 0.6 } }) },
  ]);
  const strict = adapterWith(transport);
  const first = await strict.judge({ questions: [single("q", NOUL_REQUEST)] });
  assert.deepEqual(resultOf(first, "q"), { outcome: "yes", modelVersion: TYPESAFE_DEFAULT_MODEL });
  assert.equal(first.ok && first.records[0].confidence, null, "a noul judgment records no invented confidence");
  assert.equal(first.ok && first.records[0].decision, "yes");

  const banded = adapterWith(transport, { noul: { threshold: 0.5, undecidedBand: 0.4 } });
  assert.deepEqual(resultOf(await banded.judge({ questions: [single("q", NOUL_REQUEST)] }), "q"), { outcome: "unknown" });
  assert.deepEqual(issueCodes(validateNoulDecisionPolicy({ threshold: 0.5, undecidedBand: 2 })), ["out_of_range"]);
  assert.deepEqual(issueCodes(validateNoulDecisionPolicy({ threshold: 0.5, other: 1 })), ["unknown_field"]);
});

// ---------------------------------------------------------------------------
// Retry, timeout, abort, and response bounds
// ---------------------------------------------------------------------------

test("429 and 529 retry with bounded backoff inside the deadline", async () => {
  let clock = 0;
  const sleeps: number[] = [];
  const transport = mockTransport([
    { status: 429, headers: { "retry-after": "2" } },
    { status: 529 },
    { status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) },
  ]);
  const adapter = createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state: approvedState(),
    fetch: transport.fetch,
    now: () => clock,
    sleep: async (ms) => {
      sleeps.push(ms);
      clock += ms;
    },
    providerInputPolicy: authorizeExactly,
    backoffMs: 100,
    deadlineMs: 10_000,
    maxAttempts: 3,
  });
  const result = await adapter.judge({ questions: [single("q", NOUL_REQUEST)] });
  assert.equal(resultOf(result, "q").outcome, "yes");
  assert.deepEqual(sleeps, [2_000, 200], "the retry hint wins, then exponential backoff grows");
  assert.equal(transport.calls.length, 3);
  assert.equal(result.ok && result.usage.calls, 3);

  const impatientTransport = mockTransport([{ status: 429, headers: { "retry-after": "2" } }]);
  const impatient = createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state: approvedState(),
    fetch: impatientTransport.fetch,
    now: () => clock,
    sleep: async (ms) => {
      sleeps.push(ms);
    },
    providerInputPolicy: authorizeExactly,
    deadlineMs: 1_000,
  });
  const abandoned = failureOf(await impatient.judge({ questions: [single("q", NOUL_REQUEST)] }), "q");
  assert.equal(impatientTransport.calls.length, 1, "a retry hint beyond the deadline is not waited out");
  assert.equal(abandoned.retryable, true);
  assert.equal(abandoned.message.includes("retry abandoned"), true);
});

test("401 and 422 are never retried, and 429 retries stop at the attempt ceiling", async () => {
  const unauthorizedTransport = mockTransport([{ status: 401, rawBody: '{"error":"invalid key"}' }]);
  const unauthorized = failureOf(await adapterWith(unauthorizedTransport).judge({ questions: [single("q", NOUL_REQUEST)] }), "q");
  assert.equal(unauthorizedTransport.calls.length, 1);
  assert.equal(unauthorized.retryable, false);
  assert.equal(unauthorized.message.includes("http_401"), true);

  const invalidTransport = mockTransport([{ status: 422, rawBody: '{"detail":"questions.q is malformed"}' }]);
  const invalid = failureOf(await adapterWith(invalidTransport).judge({ questions: [single("q", NOUL_REQUEST)] }), "q");
  assert.equal(invalidTransport.calls.length, 1);
  assert.equal(invalid.retryable, false);
  assert.equal(invalid.message.includes("http_422"), true);
  assert.equal(invalid.message.includes("questions.q is malformed"), false, "the provider body is never echoed");

  const overloadedTransport = mockTransport([{ status: 529 }, { status: 529 }]);
  const overloaded = failureOf(
    await adapterWith(overloadedTransport, { backoffMs: 0, maxAttempts: 2 }).judge({ questions: [single("q", NOUL_REQUEST)] }),
    "q",
  );
  assert.equal(overloadedTransport.calls.length, 2);
  assert.equal(overloaded.retryable, true);

  const networkTransport = mockTransport([{ errorName: "TypeError" }, { status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) }]);
  const recovered = await adapterWith(networkTransport, { backoffMs: 0 }).judge({ questions: [single("q", NOUL_REQUEST)] });
  assert.equal(resultOf(recovered, "q").outcome, "yes");
  assert.equal(networkTransport.calls.length, 2);
});

test("per-attempt timeouts and caller aborts produce distinct outcomes", async () => {
  const stalledTransport = mockTransport([{ holdUntilAbort: true }, { holdUntilAbort: true }]);
  const stalled = failureOf(
    await adapterWith(stalledTransport, { timeoutMs: 120, maxAttempts: 2, backoffMs: 0 }).judge({ questions: [single("q", NOUL_REQUEST)] }),
    "q",
  );
  assert.equal(stalledTransport.calls.length, 2);
  assert.equal(stalled.retryable, true);
  assert.equal(stalled.message.includes("timeout"), true);

  const abortedTransport = mockTransport([{ holdUntilAbort: true }]);
  const adapter = adapterWith(abortedTransport, { timeoutMs: 5_000 });
  const controller = new AbortController();
  const pending = adapter.judge({ questions: [single("q", NOUL_REQUEST)] }, controller.signal);
  setTimeout(() => controller.abort(), 10);
  const aborted = failureOf(await pending, "q");
  assert.equal(abortedTransport.calls.length, 1);
  assert.equal(aborted.retryable, false);
  assert.equal(aborted.message.includes("aborted"), true);

  const skippedTransport = mockTransport([]);
  const preAborted = new AbortController();
  preAborted.abort();
  const skipped = failureOf(await adapterWith(skippedTransport).judge({ questions: [single("q", NOUL_REQUEST)] }, preAborted.signal), "q");
  assert.equal(skippedTransport.calls.length, 0);
  assert.equal(skipped.message.includes("aborted before dispatch"), true);
});

test("responses are bounded as text, as a stream, and by declaration", async () => {
  const body = JSON.stringify(providerBody({ q: { type: "noul", noul: 0.9 } }));
  const asText = failureOf(
    await adapterWith(mockTransport([{ status: 200, rawBody: body + "x".repeat(4_000) }]), { maxResponseBytes: 1_024 }).judge({
      questions: [single("q", NOUL_REQUEST)],
    }),
    "q",
  );
  assert.equal(asText.message.includes("response_too_large"), true);

  const streamed = await adapterWith(mockTransport([{ status: 200, rawBody: body, stream: true }])).judge({
    questions: [single("q", NOUL_REQUEST)],
  });
  assert.deepEqual(resultOf(streamed, "q"), { outcome: "yes", modelVersion: TYPESAFE_DEFAULT_MODEL });

  const streamBound = failureOf(
    await adapterWith(mockTransport([{ status: 200, rawBody: body + "x".repeat(4_000), stream: true }]), { maxResponseBytes: 1_024 }).judge({
      questions: [single("q", NOUL_REQUEST)],
    }),
    "q",
  );
  assert.equal(streamBound.message.includes("exceeds"), true);

  const declared = failureOf(
    await adapterWith(mockTransport([{ status: 200, headers: { "content-length": "999999" }, rawBody: body }]), { maxResponseBytes: 1_024 }).judge({
      questions: [single("q", NOUL_REQUEST)],
    }),
    "q",
  );
  assert.equal(declared.message.includes("declared"), true);

  const malformed = failureOf(await adapterWith(mockTransport([{ status: 200, rawBody: "not json" }])).judge({ questions: [single("q", NOUL_REQUEST)] }), "q");
  assert.equal(malformed.message.includes("invalid_response"), true);
  const shapeless = failureOf(
    await adapterWith(mockTransport([{ status: 200, body: { model: TYPESAFE_DEFAULT_MODEL, usage: { input_tokens: 1, output_tokens: 1 } } }])).judge({
      questions: [single("q", NOUL_REQUEST)],
    }),
    "q",
  );
  assert.equal(shapeless.message.includes("invalid_response"), true);
});

test("retry hints are parsed from seconds, dates, and junk", () => {
  const now = Date.parse("2026-09-18T00:00:00Z");
  assert.equal(parseRetryAfterMs("2", 0), 2_000);
  assert.equal(parseRetryAfterMs(" 0.5 ", 0), 500);
  assert.equal(parseRetryAfterMs(new Date(now + 3_000).toUTCString(), now), 3_000);
  assert.equal(parseRetryAfterMs("soon", 0), undefined);
  assert.equal(parseRetryAfterMs(null, 0), undefined);
});

// ---------------------------------------------------------------------------
// Budgets, provenance, and shadow recording
// ---------------------------------------------------------------------------

test("one budget account charges calls and tokens across the operation", async () => {
  const budget = createJudgmentBudgetAccount({ maxCalls: 1, maxInputTokens: 10_000 });
  const transport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) }]);
  const adapter = createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state: approvedState(),
    fetch: transport.fetch,
    budget,
    providerInputPolicy: authorizeExactly,
  });
  assert.equal(resultOf(await adapter.judge({ questions: [single("q", NOUL_REQUEST)] }), "q").outcome, "yes");
  const exhausted = failureOf(await adapter.judge({ questions: [single("q", NOUL_REQUEST)] }), "q");
  assert.equal(transport.calls.length, 1, "an exhausted call budget stops before dispatch");
  assert.equal(exhausted.message.includes("budget_exceeded"), true);
  assert.deepEqual(budget.snapshot(), {
    limits: { maxCalls: 1, maxInputTokens: 10_000, maxOutputTokens: DEFAULT_JUDGMENT_BUDGET.maxOutputTokens },
    calls: 1,
    inputTokens: 120,
    outputTokens: 30,
    exhausted: true,
  });

  const tokenBudget = createJudgmentBudgetAccount({ maxCalls: 4, maxInputTokens: 100 });
  const tokenTransport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) }]);
  const tokenAdapter = createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state: approvedState(),
    fetch: tokenTransport.fetch,
    budget: tokenBudget,
    providerInputPolicy: authorizeExactly,
  });
  await tokenAdapter.judge({ questions: [single("q", NOUL_REQUEST)] });
  const tokenLimited = failureOf(await tokenAdapter.judge({ questions: [single("q", NOUL_REQUEST)] }), "q");
  assert.equal(tokenTransport.calls.length, 1, "observed tokens exhaust the account for later calls");
  assert.equal(tokenLimited.message.includes("budget_exceeded"), true);
  assert.deepEqual(issueCodes(validateJudgmentBudget({ maxCalls: DEFAULT_JUDGMENT_BUDGET.maxCalls + 1 })), ["out_of_range"]);
  assert.deepEqual(issueCodes(validateJudgmentBudget({ maxCalls: 2, extra: 1 })), ["unknown_field"]);
});

test("every judgment records provenance with its question and model version", async () => {
  const transport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.1 } }) }]);
  const state = approvedState({ source: "user_turn_context" });
  const adapter = adapterWith(transport, {}, state);
  const result = await adapter.judge({ questions: [single("q", { question: NOUL_REQUEST.question, questionVersion: "v7" })] });
  assert.equal(result.ok, true);
  if (!result.ok) return;
  const record = result.records[0];
  assert.equal(record.contract, "judgment/v1");
  assert.equal(record.questionVersion, "v7");
  assert.equal(record.questionDigest.startsWith("sha256:"), true);
  assert.equal(record.stateDigest, state.digest);
  assert.equal(record.stateSource, "user_turn_context");
  assert.deepEqual(record.model, { provider: "typesafe", model: TYPESAFE_DEFAULT_MODEL, version: TYPESAFE_DEFAULT_MODEL });
  assert.equal(record.modelObserved, true);
  assert.equal(record.versionChanged, false);
  assert.equal(record.requestedPinned, true);
  assert.equal(record.batchQuestions, 1);
  assert.equal(record.attempts, 1);
  assert.equal(record.requestDigest?.startsWith("sha256:"), true, "the exact outbound body is recorded by digest");
  assert.equal(record.advisory, true);
  assert.equal(validateJudgmentRecord(record).ok, true);
  assert.equal(validateRecordedDecision(record).ok, false, "a judgment is never an approval record");
  assert.deepEqual(result.model, {
    provider: "typesafe",
    model: TYPESAFE_DEFAULT_MODEL,
    version: TYPESAFE_DEFAULT_MODEL,
    observed: true,
    pinned: true,
  });
  assert.equal(result.mode, "live");
  assert.equal(result.actionability, "live");
  assert.equal(adapter.observedModelVersion(), TYPESAFE_DEFAULT_MODEL);

  assert.equal(isPinnedModel("jev-1.13.0"), true);
  assert.equal(isPinnedModel("jev-latest"), false);
  assert.equal(isPinnedModel("jev-preview"), false);
  const versionA = parseJudgmentQuestion({ id: "q", request: { question: "x?", questionVersion: "v1" } });
  const versionB = parseJudgmentQuestion({ id: "q", request: { question: "x?", questionVersion: "v2" } });
  const withContext = parseJudgmentQuestion({ id: "q", request: { question: "x?", questionVersion: "v1", context: "extra" } });
  assert.notEqual(questionDigest(versionA), questionDigest(versionB), "changed instructions need a new version");
  assert.notEqual(questionDigest(versionA), questionDigest(withContext), "context is part of the recorded question");
});

test("a shadow recorder keeps advisory decisions without dispatching anything", () => {
  const state = approvedState();
  const recorder = createShadowRecorder({ now: () => 1_700_000_000_000 });
  const question = parseJudgmentQuestion({ id: "q", request: NOUL_REQUEST });
  const record = recorder.recordDecision({
    question,
    state: { digest: state.digest, source: state.source },
    model: { requested: TYPESAFE_DEFAULT_MODEL, resolved: TYPESAFE_DEFAULT_MODEL },
    decision: "yes",
    confidence: null,
  });
  assert.equal(record.mode, "shadow");
  assert.equal(record.advisory, true);
  assert.equal(record.decidedAt, 1_700_000_000_000);
  assert.equal(judgmentIsActionable(record), false, "a shadow decision can never drive dispatch");
  assert.equal(validateJudgmentRecord(record).ok, true);
  assert.equal(recorder.records().length, 1);
  assert.equal(recorder.mode, "shadow");

  const live: JudgmentRecord = { ...record, mode: "live" };
  assert.equal(judgmentIsActionable(live), true);
  const sink = createShadowRecorder();
  sink.record(live);
  assert.equal(sink.records()[0].mode, "shadow", "a shadow recorder writes shadow records only");
  assert.deepEqual(issueCodes(validateJudgmentRecord({ ...record, advisory: false })), ["forged_approval"]);
  assert.deepEqual(issueCodes(validateJudgmentRecord({ ...record, confidence: 4 })), ["out_of_range"]);
  assert.deepEqual(issueCodes(validateJudgmentRecord({ ...record, mode: "authoritative" })), ["bad_syntax"]);
});

test("shadow mode resolves answers as advisory outcomes that cannot be delivered live", async () => {
  const transport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }, "jev-1.13.1") }]);
  const result = await adapterWith(transport).judge({ questions: [single("q", NOUL_REQUEST)], mode: "shadow" });
  assert.equal(resultOf(result, "q").outcome, "yes");
  assert.equal(result.ok, true);
  if (!result.ok) return;
  assert.equal(result.mode, "shadow");
  assert.equal(result.actionability, "advisory");
  assert.deepEqual(result.model, {
    provider: "typesafe",
    model: TYPESAFE_DEFAULT_MODEL,
    version: "jev-1.13.1",
    observed: true,
    pinned: true,
  });
  assert.equal(result.records[0].mode, "shadow");
  assert.equal(result.records[0].versionChanged, true, "an observed version change is recorded in shadow provenance");
  assert.equal(judgmentIsActionable(result.records[0]), false);
  assert.equal(judgmentBatchIsLive(result), false, "an advisory batch is never live");
  const gate = requireLiveJudgmentBatch(result);
  assert.equal(gate.ok, false);
  assert.equal(gate.ok === false && gate.reason, "advisory");
  assert.equal(gate.ok === false && gate.message.includes("advisory"), true);
});

// ---------------------------------------------------------------------------
// Trusted provider-input authorization (the only permission to send input)
// ---------------------------------------------------------------------------

test("trusted policy sees the exact body, digest, state, and question labels", async () => {
  const seen: ProviderInputRequest[] = [];
  const transport = mockTransport([
    {
      status: 200,
      body: providerBody({
        dept: {
          type: "choice",
          choice: "billing",
          probabilities: { billing: 0.7, technical: 0.2, [NO_MATCH_OPTION]: 0.05, [ABSTAIN_KEYS[1]]: 0.05 },
          confidence: 0.7,
        },
      }),
    },
  ]);
  const adapter = adapterWith(transport, {
    providerInputPolicy: (request) => {
      seen.push(request);
      return { authorized: true, digest: request.digest, authorizationRef: "policy_ref_7" };
    },
  });
  const result = await adapter.judge({ questions: [single("dept", CHOICE_REQUEST)], timeoutMs: 5_000, scope: "tenant_acme" });
  assert.equal(seen.length, 1, "policy runs once per distinct outbound body");
  const request = seen[0];
  assert.equal(request.endpoint, TYPESAFE_ENDPOINT);
  assert.equal(request.model, TYPESAFE_DEFAULT_MODEL);
  assert.equal(request.mode, "live");
  assert.equal(request.scope, "tenant_acme", "the trusted scope label is forwarded, not inferred");
  assert.deepEqual(request.state, {
    source: "session_context",
    digest: approvedState().digest,
    text: JSON.stringify({ session: "s-1" }),
  });
  assert.deepEqual(
    request.parts.map((part) => ({ id: part.id, version: part.version, kind: part.kind, labels: part.candidateLabels })),
    [{ id: "dept", version: "v1", kind: "choice", labels: ["Payments, invoicing, refunds", "Bugs, outages, integrations"] }],
  );
  assert.equal(request.parts[0].instructions.includes(CHOICE_REQUEST.question), true);
  assert.equal(request.digest, canonicalDigest(request.body), "the digest binds the exact bytes");
  const sent = JSON.parse(request.body) as { model: string; state: unknown; questions: Record<string, { criteria: Record<string, string> }> };
  assert.equal(sent.model, TYPESAFE_DEFAULT_MODEL);
  assert.deepEqual(Object.keys(sent.questions.dept.criteria).sort(), [
    "billing",
    "cannot_tell_between_options",
    "none_of_the_listed_options",
    "technical",
  ]);
  assert.deepEqual(resultOf(result, "dept"), { outcome: "selected", optionId: "billing", confidence: 0.7 });
  assert.equal(result.ok, true);
  if (!result.ok) return;
  assert.equal(result.records[0].authorizationRef, "policy_ref_7");
  assert.equal(result.records[0].requestDigest, request.digest);
  assert.equal(transport.calls.length, 1);
});

test("absent, denying, thrown, malformed, or mismatched policy fails closed before fetch", async () => {
  const cases: Array<{ name: string; policy: ProviderInputPolicy | undefined; expect: string[]; absent: string[] }> = [
    { name: "absent", policy: undefined, expect: ["provider_input_unattested"], absent: [] },
    { name: "denied", policy: () => ({ authorized: false, code: "tenant_policy_denied" }), expect: ["provider_input_denied", "tenant_policy_denied"], absent: [] },
    {
      name: "thrown",
      policy: () => {
        throw new Error(`leaked ${API_KEY} inside the exception`);
      },
      expect: ["provider_input_denied", "policy_error"],
      absent: [API_KEY, "inside the exception"],
    },
    { name: "malformed", policy: () => ({ authorized: true }) as never, expect: ["provider_input_tampered"], absent: [] },
    { name: "mismatched digest", policy: () => ({ authorized: true, digest: DIGEST_LIKE }), expect: ["provider_input_tampered"], absent: [] },
  ];
  for (const entry of cases) {
    const transport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) }]);
    const adapter = adapterWith(transport, { providerInputPolicy: entry.policy });
    const result = await adapter.judge({ questions: [single("q", NOUL_REQUEST)] });
    const failure = failureOf(result, "q");
    for (const fragment of entry.expect) {
      assert.ok(failure.message.includes(fragment), `${entry.name}: expected ${fragment} in ${failure.message}`);
    }
    for (const fragment of entry.absent) {
      assert.equal(failure.message.includes(fragment), false, `${entry.name}: message must not leak ${fragment}`);
    }
    assert.equal(failure.retryable, false, `${entry.name}: an unauthorized request is not retryable`);
    assert.equal(transport.calls.length, 0, `${entry.name}: nothing leaves the process`);
    assert.equal(result.ok && result.records[0].decision, "provider_failure");
    assert.equal(result.ok && typeof result.failureCode, "string", `${entry.name}: structured failure code`);
    assert.equal(result.ok && judgmentIsActionable(result.records[0]), false);
  }
});

test("the trusted scope label reaches the policy and can be denied", async () => {
  const transport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) }]);
  const seen: Array<string | undefined> = [];
  const adapter = adapterWith(transport, {
    providerInputScope: "tenant_configured",
    providerInputPolicy: (request) => {
      seen.push(request.scope);
      return request.scope === "tenant_configured"
        ? { authorized: true, digest: request.digest }
        : { authorized: false, code: "scope_denied" };
    },
  });
  assert.equal(resultOf(await adapter.judge({ questions: [single("q", NOUL_REQUEST)] }), "q").outcome, "yes");
  assert.equal(transport.calls.length, 1);
  const denied = failureOf(await adapter.judge({ questions: [single("q", NOUL_REQUEST)], scope: "other_tenant" }), "q");
  assert.equal(denied.message.includes("scope_denied"), true);
  assert.equal(transport.calls.length, 1, "a scope denial dispatches nothing");
  assert.deepEqual(seen, ["tenant_configured", "other_tenant"]);
});

test("an unpatterned secret is refused by policy and never reaches results or records", async () => {
  const secret = "correct-horse-battery staple";
  const transport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) }]);
  const state = approveProviderState({ state: `deploy note: ${secret}`, source: "session_context" });
  assert.equal(state.ok, true, "the shape/regex scan alone does not catch this text");
  const adapter = createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state: state.ok ? state.value : approvedState(),
    fetch: transport.fetch,
    providerInputPolicy: (request) => (request.state.text.includes(secret) ? { authorized: false, code: "sensitive_input" } : { authorized: true, digest: request.digest }),
  });
  const result = await adapter.judge({ questions: [single("q", NOUL_REQUEST)] });
  const failure = failureOf(result, "q");
  assert.equal(failure.message.includes("sensitive_input"), true);
  assert.equal(transport.calls.length, 0);
  assert.equal(JSON.stringify(result).includes(secret), false, "the secret appears in no result or record");
  assert.equal(JSON.stringify(result.issues ?? []).includes(secret), false);
});

test("provider-echoed text never reaches diagnostics or records", async () => {
  const secret = "payout reconciliation key note";
  const invalidTransport = mockTransport([{ status: 422, rawBody: JSON.stringify({ detail: `rejected because of ${secret}` }) }]);
  const invalid = failureOf(await adapterWith(invalidTransport).judge({ questions: [single("q", NOUL_REQUEST)] }), "q");
  assert.equal(invalid.message.includes("422"), true);
  assert.equal(invalid.message.includes(secret), false, "a 422 body is never echoed");

  const throwingTransport = mockTransport([{ errorName: secret, errorMessage: `transport failed with ${secret}` }]);
  const thrown = failureOf(await adapterWith(throwingTransport, { maxAttempts: 1 }).judge({ questions: [single("q", NOUL_REQUEST)] }), "q");
  assert.equal(thrown.message.includes("network_error"), true);
  assert.equal(thrown.message.includes(secret), false, "an exception message is never echoed");
  assert.equal(thrown.message.includes("(unknown)"), true, "an unrecognized error name is masked");
  assert.equal(JSON.stringify(thrown).includes(secret), false);

  const unknownKeyTransport = mockTransport([
    { status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 }, [`key ${secret}`]: { type: "noul", noul: 0.1 } }) },
  ]);
  const unknownKey = await adapterWith(unknownKeyTransport).judge({ questions: [single("q", NOUL_REQUEST)] });
  assert.equal(resultOf(unknownKey, "q").outcome, "yes");
  assert.equal(unknownKey.ok, true);
  if (!unknownKey.ok) return;
  assert.deepEqual(unknownKey.issues.map((issue) => issue.path), ["$.answers.<unrecognized>"]);
  assert.equal(JSON.stringify(unknownKey).includes(secret), false, "a provider-supplied key is not echoed");
});

// ---------------------------------------------------------------------------
// Deadlines, calibration, per-call model provenance, usage, and body bounds
// ---------------------------------------------------------------------------

test("no request starts past the deadline, and an attempt never outlives it", async () => {
  const stepping = mockTransport([]);
  let tick = 0;
  const expired = await createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state: approvedState(),
    fetch: stepping.fetch,
    providerInputPolicy: authorizeExactly,
    now: () => (tick += 100_000),
    deadlineMs: 1_000,
  }).judge({ questions: [single("q", NOUL_REQUEST)] });
  const expiredFailure = failureOf(expired, "q");
  assert.equal(expiredFailure.message.includes("deadline_exceeded"), true);
  assert.equal(expiredFailure.retryable, true);
  assert.equal(stepping.calls.length, 0, "a request is never started after the deadline");

  let clock = 0;
  const sleeps: number[] = [];
  const retrying = mockTransport([{ status: 429, headers: { "retry-after": "0.05" } }, { holdUntilAbort: true }]);
  const capped = await createTypeSafeJudgmentAdapter({
    apiKey: API_KEY,
    state: approvedState(),
    fetch: retrying.fetch,
    providerInputPolicy: authorizeExactly,
    now: () => clock,
    sleep: async (ms) => {
      sleeps.push(ms);
      clock += ms;
    },
    backoffMs: 0,
    maxAttempts: 2,
    deadlineMs: 250,
  }).judge({ questions: [single("q", NOUL_REQUEST)] });
  const cappedFailure = failureOf(capped, "q");
  assert.deepEqual(sleeps, [50]);
  assert.equal(retrying.calls.length, 2);
  assert.equal(cappedFailure.message.includes("exceeded 200 ms"), true, "the second attempt is capped by the time left: " + cappedFailure.message);
  assert.equal(cappedFailure.retryable, true);
});

test("live judgments require an exact pin, and a version change is never actionable", async () => {
  const aliasTransport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }, "jev-1.13.0") }]);
  const alias = await adapterWith(aliasTransport, { model: "jev-latest" }).judge({ questions: [single("q", NOUL_REQUEST)] });
  const aliasFailure = failureOf(alias, "q");
  assert.equal(aliasFailure.message.includes("uncalibrated_model"), true);
  assert.equal(aliasTransport.calls.length, 0, "an alias is never dispatched live");
  assert.equal(alias.ok && alias.model.pinned, false);

  const shadowAliasTransport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }, "jev-1.13.0") }]);
  const shadowAlias = await adapterWith(shadowAliasTransport, { model: "jev-latest", mode: "shadow" }).judge({
    questions: [single("q", NOUL_REQUEST)],
  });
  assert.equal(resultOf(shadowAlias, "q").outcome, "yes");
  assert.equal(shadowAlias.ok && shadowAlias.actionability, "advisory");
  assert.equal(shadowAlias.ok && shadowAlias.model.pinned, false);
  assert.equal(shadowAliasTransport.calls.length, 1);

  const changedTransport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }, "jev-1.13.1") }]);
  const changed = await adapterWith(changedTransport).judge({ questions: [single("q", NOUL_REQUEST)] });
  const changedFailure = failureOf(changed, "q");
  assert.equal(changedFailure.message.includes("model_version_changed"), true);
  assert.equal(changedFailure.message.includes("jev-1.13.1"), false, "the resolved id stays out of diagnostics");
  assert.equal(changedFailure.retryable, false);
  assert.equal(changedTransport.calls.length, 1);
  assert.equal(changed.ok && changed.model.version, "jev-1.13.1", "the observed version is still reported truthfully");
  assert.equal(changed.ok && changed.model.observed, true);
  assert.equal(changed.ok && changed.records[0].versionChanged, true);
  assert.equal(changed.ok && changed.records[0].decision, "provider_failure");
});

test("a failed call never inherits a version an earlier call observed", async () => {
  const transport = mockTransport([
    { status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }, "jev-1.13.1") },
    { status: 500, rawBody: "{}" },
  ]);
  const adapter = adapterWith(transport);
  const first = await adapter.judge({ questions: [single("q", NOUL_REQUEST)] });
  assert.equal(first.ok && first.model.version, "jev-1.13.1");
  assert.equal(adapter.observedModelVersion(), "jev-1.13.1");

  const second = await adapter.judge({ questions: [single("q", NOUL_REQUEST)] });
  assert.equal(second.ok, true);
  if (!second.ok) return;
  assert.deepEqual(second.model, {
    provider: "typesafe",
    model: TYPESAFE_DEFAULT_MODEL,
    version: TYPESAFE_DEFAULT_MODEL,
    observed: false,
    pinned: true,
  }, "a failed call reports the configured pin and says it observed nothing");
  assert.equal(second.records[0].modelObserved, false);
  assert.equal(second.records[0].model.version, TYPESAFE_DEFAULT_MODEL);
  assert.equal(second.records[0].versionChanged, false);
  assert.equal(adapter.observedModelVersion(), "jev-1.13.1", "the adapter still reports the last version it observed");
});

test("unreported usage fails closed instead of returning an unaccounted decision", async () => {
  const missingTransport = mockTransport([
    { status: 200, body: { model: TYPESAFE_DEFAULT_MODEL, answers: { q: { type: "noul", noul: 0.9 } } } },
  ]);
  const budget = createJudgmentBudgetAccount({ maxCalls: 4, maxInputTokens: 10_000 });
  const missing = await adapterWith(missingTransport, { budget }).judge({ questions: [single("q", NOUL_REQUEST)] });
  const missingFailure = failureOf(missing, "q");
  assert.equal(missingFailure.message.includes("usage_unreported"), true);
  assert.equal(missingFailure.retryable, false);
  assert.equal(missing.ok && judgmentIsActionable(missing.records[0]), false);
  assert.equal(budget.snapshot().calls, 1, "the call is still charged");
  assert.equal(budget.snapshot().inputTokens, 0, "no tokens are invented");

  const malformedTransport = mockTransport([
    {
      status: 200,
      body: {
        model: TYPESAFE_DEFAULT_MODEL,
        answers: { q: { type: "noul", noul: 0.9 } },
        usage: { input_tokens: "many", output_tokens: -1 },
      },
    },
  ]);
  const malformed = await adapterWith(malformedTransport).judge({ questions: [single("q", NOUL_REQUEST)] });
  assert.equal(failureOf(malformed, "q").message.includes("usage_unreported"), true);
});

test("an unbounded text fallback is refused unless the transport is explicitly trusted", async () => {
  const body = JSON.stringify(providerBody({ q: { type: "noul", noul: 0.9 } }));
  const unbounded = failureOf(
    await adapterWith(mockTransport([{ status: 200, rawBody: body, noStream: true }])).judge({ questions: [single("q", NOUL_REQUEST)] }),
    "q",
  );
  assert.equal(unbounded.message.includes("response_body_unbounded"), true);

  const trusted = await adapterWith(mockTransport([{ status: 200, rawBody: body, noStream: true }]), { allowTextBodyFallback: true }).judge({
    questions: [single("q", NOUL_REQUEST)],
  });
  assert.deepEqual(resultOf(trusted, "q"), { outcome: "yes", modelVersion: TYPESAFE_DEFAULT_MODEL });

  const trustedOversize = failureOf(
    await adapterWith(mockTransport([{ status: 200, rawBody: body + "x".repeat(4_000), noStream: true }]), {
      allowTextBodyFallback: true,
      maxResponseBytes: 1_024,
    }).judge({ questions: [single("q", NOUL_REQUEST)] }),
    "q",
  );
  assert.equal(trustedOversize.message.includes("response_too_large"), true);

  const streamOnlyTransport = mockTransport([{ status: 200, rawBody: body }]);
  const streamed = await adapterWith(streamOnlyTransport).judge({ questions: [single("q", NOUL_REQUEST)] });
  assert.deepEqual(resultOf(streamed, "q"), { outcome: "yes", modelVersion: TYPESAFE_DEFAULT_MODEL });
});

test("concurrent calls reserve budget atomically", async () => {
  const budget = createJudgmentBudgetAccount({ maxCalls: 1, maxInputTokens: 10_000 });
  const transport = mockTransport([{ status: 200, body: providerBody({ q: { type: "noul", noul: 0.9 } }) }]);
  const adapter = adapterWith(transport, { budget });
  const [first, second] = await Promise.all([
    adapter.judge({ questions: [single("q", NOUL_REQUEST)] }),
    adapter.judge({ questions: [single("q", NOUL_REQUEST)] }),
  ]);
  assert.equal(transport.calls.length, 1, "exactly one concurrent call is dispatched");
  const outcomes = [first, second].map((result) => (result.ok ? result.results.q.outcome : "rejected")).sort();
  assert.deepEqual(outcomes, ["provider_failure", "yes"]);
  const failed = [first, second].find((result) => result.ok && result.results.q.outcome === "provider_failure");
  assert.ok(failed && failed.ok && failed.results.q.outcome === "provider_failure");
  if (failed && failed.ok && failed.results.q.outcome === "provider_failure") {
    assert.equal(failed.results.q.message.includes("budget_exceeded"), true);
  }
});

// ---------------------------------------------------------------------------
// Question validation and adapter configuration
// ---------------------------------------------------------------------------

test("questions are validated before anything is dispatched", async () => {
  const valid = validateJudgmentQuestion({ id: "dept", request: CHOICE_REQUEST });
  assert.equal(valid.ok, true);
  assert.equal(valid.ok && valid.value.kind, "choice");
  const cases: Array<[unknown, string[]]> = [
    [{ id: "dept", request: { ...CHOICE_REQUEST, options: [{ id: NO_MATCH_OPTION, label: "x" }] } }, ["reserved_key"]],
    [
      { id: "dept", request: { ...CHOICE_REQUEST, options: [{ id: "billing", label: "a" }, { id: "billing", label: "b" }] } },
      ["duplicate_key"],
    ],
    [{ id: "dept", request: { ...CHOICE_REQUEST, rubric: ["a", "b"] } }, ["unknown_field"]],
    [{ id: "q", request: { question: "x?", questionVersion: "v1", rubric: ["only one"] } }, ["too_few_items"]],
    [{ id: "q", request: { question: "x?" } }, ["missing_field"]],
    [{ id: "q", request: { ...SCORE_REQUEST, allowAbstain: true } }, ["unknown_field"]],
    [{ id: "q", request: { ...NOUL_REQUEST, timeoutMs: 10 } }, ["out_of_range"]],
    [{ id: "q", request: { ...NOUL_REQUEST, context: "x".repeat(1_100) } }, ["string_too_long"]],
  ];
  for (const [value, codes] of cases) {
    const result = validateJudgmentQuestion(value);
    assert.equal(result.ok, false, JSON.stringify(value));
    assert.deepEqual(issueCodes(result), codes, JSON.stringify(value));
  }
  assert.deepEqual(issueCodes(validateJudgmentQuestions([single("q", NOUL_REQUEST), single("q", NOUL_REQUEST)])), ["duplicate_key"]);
  const overfull = validateJudgmentQuestions(Array.from({ length: 33 }, (_, index) => single(`q${index}`, NOUL_REQUEST)));
  assert.deepEqual(issueCodes(overfull), ["too_many_items"]);

  const transport = mockTransport([]);
  const rejected = await adapterWith(transport).judge({ questions: [single("q", { question: "x?" })] });
  assert.equal(rejected.ok, false);
  assert.equal(transport.calls.length, 0, "an invalid question never reaches the provider");
});

test("adapter configuration is validated and the key stays in configuration", () => {
  const parsed = validateTypeSafeConfig({ apiKey: API_KEY });
  assert.equal(parsed.ok, true);
  assert.equal(parsed.ok && parsed.value.model, TYPESAFE_DEFAULT_MODEL);
  assert.equal(parsed.ok && parsed.value.maxAttempts, 3);
  assert.equal(parsed.ok && parsed.value.timeoutMs, 30_000);
  assert.equal(parsed.ok && parsed.value.mode, "live");
  assert.equal(parsed.ok && parsed.value.allowTextBodyFallback, false);
  assert.equal(parsed.ok && parsed.value.providerInputPolicy, undefined, "policy absence is valid config but never a dispatch");
  const cases: Array<[unknown, string[]]> = [
    [{ apiKey: "short" }, ["bad_syntax"]],
    [{ apiKey: "has whitespace here" }, ["bad_syntax"]],
    [{ apiKey: API_KEY, extra: true }, ["unknown_field"]],
    [{ apiKey: API_KEY, maxAttempts: 0 }, ["out_of_range"]],
    [{ apiKey: API_KEY, maxAttempts: 99 }, ["out_of_range"]],
    [{ apiKey: API_KEY, backoffMs: -1 }, ["out_of_range"]],
    [{ apiKey: API_KEY, deadlineMs: 10 }, ["out_of_range"]],
    [{ apiKey: API_KEY, mode: "both" }, ["bad_syntax"]],
    [{ apiKey: API_KEY, providerInputPolicy: "nope" }, ["wrong_type"]],
    [{ apiKey: API_KEY, providerInputScope: "tenant with space" }, ["bad_syntax"]],
    [{ apiKey: API_KEY, allowTextBodyFallback: "yes" }, ["wrong_type"]],
    [{ apiKey: API_KEY, noul: { threshold: 2 } }, ["out_of_range"]],
    [{ apiKey: API_KEY, budget: { claimCall: () => true } }, ["wrong_type"]],
    [{ apiKey: API_KEY, recorder: "nope" }, ["wrong_type"]],
  ];
  for (const [value, codes] of cases) {
    assert.deepEqual(issueCodes(validateTypeSafeConfig(value)), codes, JSON.stringify(value));
  }
  assert.throws(() => createTypeSafeJudgmentAdapter({ apiKey: "bad" }));
});
