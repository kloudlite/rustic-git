import { test } from "node:test";
import assert from "node:assert/strict";
import { typeSafeEvaluationSubject } from "../src/operations/evaluation-subjects.ts";
import { runEvaluation } from "../src/operations/evaluation.ts";
import { AMBIGUOUS_OPTION, NO_MATCH_OPTION, createJudgmentBudgetAccount } from "../src/operations/judgments.ts";
import { TYPESAFE_DEFAULT_MODEL } from "../src/operations/typesafe.ts";
import { EVALUATION_SUBJECT_IMPORT_PROVENANCE, EVALUATION_SUBJECT_LIMITS } from "../src/operations/evaluation-subjects.ts";
import type { ProviderInputRequest, TypeSafeFetch, TypeSafeFetchInit, TypeSafeResponseLike } from "../src/operations/typesafe.ts";
import type { EvaluationSubjectInput } from "../src/operations/evaluation.ts";

const API_KEY = "ts_test_key_0123456789";
const input: EvaluationSubjectInput = {
  authorizedIntent: { instruction: "Read the application config", constraints: ["read only"] },
  scope: { tenantId: "tenant-a", workspaceId: "ws-a", repositoryId: "repo-a" },
  candidates: [
    {
      candidateId: "candidate_config",
      capability: "file.read",
      tenantId: "tenant-a",
      workspaceId: "ws-a",
      repositoryId: "repo-a",
      targetRef: "src.config.ts",
      path: "src/config.ts",
      label: "application config",
    },
    {
      candidateId: "candidate_process",
      capability: "process.inspect",
      tenantId: "tenant-a",
      workspaceId: "ws-a",
      targetRef: "bench-worker-3",
      label: "bench worker process",
    },
  ],
};

type FetchCall = { init: TypeSafeFetchInit; body: Record<string, unknown> };

function response(body: unknown): TypeSafeResponseLike {
  const bytes = new TextEncoder().encode(JSON.stringify(body));
  let read = false;
  return {
    status: 200,
    headers: { get: () => null },
    text: async () => new TextDecoder().decode(bytes),
    body: { getReader: () => ({ read: async () => (read ? { done: true } : ((read = true), { done: false, value: bytes })) }) },
  };
}

function answer(choice: string, model = TYPESAFE_DEFAULT_MODEL): TypeSafeResponseLike {
  const probabilities = Object.fromEntries(
    ["c0", "c1", NO_MATCH_OPTION, AMBIGUOUS_OPTION].map((id) => [id, id === choice ? 0.97 : 0.01]),
  );
  return response({
    model,
    answers: { operation_candidate: { type: "choice", choice, probabilities, confidence: 1 } },
    usage: { input_tokens: 23, output_tokens: 5 },
  });
}

function subjectWith(fetch: TypeSafeFetch, overrides: Record<string, unknown> = {}, seen: ProviderInputRequest[] = []) {
  return typeSafeEvaluationSubject({
    apiKey: API_KEY,
    fetch,
    providerInputScope: "evaluation-task-6",
    providerInputPolicy: (request) => {
      seen.push(request);
      return { authorized: true, digest: request.digest, authorizationRef: "evaluation_policy_v1" };
    },
    ...overrides,
  });
}

const runtime = (signal = new AbortController().signal) => ({ signal, now: () => 1_700_000_000_000 });

test("selected opaque candidates map to exact O01 calls without dispatch", async () => {
  const calls: FetchCall[] = [];
  const fetch: TypeSafeFetch = async (_url, init) => {
    calls.push({ init, body: JSON.parse(init.body) });
    return answer("c0");
  };
  const result = await subjectWith(fetch).attempt(input, runtime());
  assert.deepEqual(result, {
    outcome: "proposed",
    calls: [{ key: "call_1", capability: "file.read", capabilityVersion: "1.0.0", targetRef: "src.config.ts", args: { path: "src/config.ts" } }],
    usage: [{ provider: "typesafe", inputTokens: 23, cachedInputTokens: 0, outputTokens: 5, model: TYPESAFE_DEFAULT_MODEL, version: TYPESAFE_DEFAULT_MODEL }],
  });
  assert.equal(calls.length, 1);
  assert.equal(Object.keys(calls[0].body).sort().join(","), "model,questions,state");
  assert.equal(JSON.stringify(calls[0].body).includes("candidate_config"), false);
  assert.equal(JSON.stringify(calls[0].body).includes("src.config.ts"), false);
  assert.equal(JSON.stringify(calls[0].body).includes("tenant-a"), false);
  const sent = calls[0].body as { questions: Record<string, { criteria: Record<string, string> }>; state: { candidates: unknown[] } };
  assert.deepEqual(Object.keys(sent.questions.operation_candidate.criteria).sort(), [AMBIGUOUS_OPTION, "c0", "c1", NO_MATCH_OPTION].sort());
  assert.deepEqual(sent.state.candidates, [{ option: "c0", label: "application config" }, { option: "c1", label: "bench worker process" }]);

  const processResult = await subjectWith(async () => answer("c1")).attempt(input, runtime());
  assert.deepEqual(processResult.calls, [
    { key: "call_1", capability: "process.inspect", capabilityVersion: "1.0.0", targetRef: "bench-worker-3", args: { processId: "bench-worker-3" } },
  ]);
  assert.equal("dispatch" in (subjectWith(fetch) as unknown as object), false);
});

test("no_match and ambiguous remain correct abstentions", async () => {
  const noMatch = await subjectWith(async () => answer(NO_MATCH_OPTION)).attempt(input, runtime());
  assert.deepEqual(noMatch, {
    outcome: "abstain",
    reason: "no_match",
    errorCode: "no_match",
    usage: [{ provider: "typesafe", inputTokens: 23, cachedInputTokens: 0, outputTokens: 5, model: TYPESAFE_DEFAULT_MODEL, version: TYPESAFE_DEFAULT_MODEL }],
  });
  const ambiguous = await subjectWith(async () => answer(AMBIGUOUS_OPTION)).attempt(input, runtime());
  assert.equal(ambiguous.outcome, "abstain");
  assert.equal(ambiguous.reason, "ambiguous");
  assert.equal(ambiguous.errorCode, "ambiguous_match");
});

test("the evaluator sends only bounded synthetic state through evaluation policy and forces shadow advisory mode", async () => {
  const seen: ProviderInputRequest[] = [];
  const subject = subjectWith(async () => answer("c0", "jev-1.13.1"), {}, seen);
  const result = await subject.attempt(input, runtime());
  assert.equal(result.outcome, "proposed");
  assert.equal(seen.length, 1);
  assert.equal(seen[0].mode, "shadow");
  assert.equal(seen[0].scope, "evaluation-task-6");
  assert.equal(seen[0].parts.length, 1);
  assert.deepEqual(seen[0].parts[0].candidateLabels, ["application config", "bench worker process"]);
  const state = JSON.parse(seen[0].state.text) as Record<string, unknown>;
  assert.deepEqual(Object.keys(state).sort(), ["authorizedIntent", "candidates", "contract"]);
  assert.equal(JSON.stringify(state).includes("provider_failure"), false);
  assert.deepEqual(result.usage, [{ provider: "typesafe", inputTokens: 23, cachedInputTokens: 0, outputTokens: 5, model: TYPESAFE_DEFAULT_MODEL, version: "jev-1.13.1" }]);
});

test("reported TypeSafe usage is complete for evaluation aggregation and pricing", async () => {
  const selected = subjectWith(async () => answer("c0"));
  const suite = {
    baseline: { ...selected, subjectId: "usage-baseline" },
    current: { ...selected, subjectId: "usage-current" },
    proposed: { ...selected, subjectId: "usage-proposed" },
  };
  const corpus = {
    corpusVersion: "usage-v1",
    contractVersion: "v1",
    synthetic: true as const,
    capabilityEffects: { "file.read": "read" as const },
    cases: [{
      caseId: "usage-case",
      familyId: "usage-case",
      split: "tuning" as const,
      labels: ["literal_exact" as const],
      authorizedIntent: input.authorizedIntent,
      scope: input.scope,
      candidates: input.candidates,
      expectation: { kind: "calls" as const, calls: [{ key: "call_1", capability: "file.read", capabilityVersion: "1.0.0", targetRef: "src.config.ts", args: { path: "src/config.ts" } }] },
    }],
  };
  const report = await runEvaluation(corpus, {
    suite,
    splits: ["tuning"],
    pricing: { version: "test-v1", currency: "USD", providers: { typesafe: { inputTokensPerMillion: 1, cachedInputTokensPerMillion: 1, outputTokensPerMillion: 2 } } },
  });
  const proposed = report.cases.find((entry) => entry.role === "proposed");
  assert.equal(proposed?.usageKnown, true);
  const split = report.subjects.find((entry) => entry.role === "proposed")?.splits[0].score;
  assert.equal(split?.usage.providers[0].complete, true);
  assert.equal(split?.usage.providers[0].cachedInputTokens, 0);
  assert.equal(split?.cost.totalUsd, (23 + 5 * 2) / 1_000_000);
});

test("missing or denying provider input policy maps to stable provider failure without fetch", async () => {
  let calls = 0;
  const fetch: TypeSafeFetch = async () => { calls += 1; return answer("c0"); };
  const missing = typeSafeEvaluationSubject({ apiKey: API_KEY, fetch });
  assert.deepEqual(await missing.attempt(input, runtime()), { outcome: "provider_failure", failure: { code: "provider_error" } });
  const denied = typeSafeEvaluationSubject({
    apiKey: API_KEY,
    fetch,
    providerInputPolicy: () => ({ authorized: false, code: "evaluation_denied" }),
  });
  assert.deepEqual(await denied.attempt(input, runtime()), { outcome: "provider_failure", failure: { code: "provider_error" } });
  assert.equal(calls, 0);
});

test("private fault scenarios bypass every proposed subject attempt and provider fetch", async () => {
  let attempts = 0;
  let fetches = 0;
  const proposed = subjectWith(async () => { fetches += 1; return answer("c0"); });
  const counted = { ...proposed, subjectId: "typesafe-proposed", attempt: async (...args: Parameters<typeof proposed.attempt>) => { attempts += 1; return proposed.attempt(...args); } };
  const inert = (subjectId: string) => ({ ...proposed, subjectId, attempt: async () => ({ outcome: "abstain" as const, reason: "no_match" as const, errorCode: "no_match" as const }) });
  const faultCase = {
    corpusVersion: "subject-fault-v1",
    contractVersion: "v1",
    synthetic: true as const,
    capabilityEffects: { "file.read": "read" as const },
    cases: [{
      caseId: "fault-timeout",
      familyId: "fault-timeout",
      split: "tuning" as const,
      labels: ["provider_failure" as const],
      authorizedIntent: { instruction: "Read config" },
      scope: { tenantId: "tenant-a", workspaceId: "ws-a" },
      candidates: [input.candidates[0]],
      injectedFault: { provider: "typesafe" as const, scenarioId: "f900" },
      expectation: { kind: "no_call" as const, reason: "provider_failure" as const, expectedFailureCode: "timeout" as const },
    }],
  };
  const report = await runEvaluation(faultCase, {
    suite: { baseline: inert("fault-baseline"), current: inert("fault-current"), proposed: counted },
    splits: ["tuning"],
    faultScenarios: { f900: async () => ({ outcome: "provider_failure", failure: { code: "timeout" } }) },
  });
  assert.equal(attempts, 0);
  assert.equal(fetches, 0);
  assert.deepEqual(report.cases.map((entry) => entry.failure), [{ code: "timeout" }, { code: "timeout" }, { code: "timeout" }]);
});

test("private invalid-response scenarios also bypass the subject deterministically", async () => {
  let attempts = 0;
  let fetches = 0;
  const proposed = subjectWith(async () => { fetches += 1; return answer("c0"); });
  const counted = { ...proposed, subjectId: "typesafe-invalid-proposed", attempt: async (...args: Parameters<typeof proposed.attempt>) => { attempts += 1; return proposed.attempt(...args); } };
  const inert = (subjectId: string) => ({ ...proposed, subjectId, attempt: async () => ({ outcome: "abstain" as const, reason: "no_match" as const, errorCode: "no_match" as const }) });
  const corpus = {
    corpusVersion: "subject-invalid-fault-v1",
    contractVersion: "v1",
    synthetic: true as const,
    capabilityEffects: { "file.read": "read" as const },
    cases: [{
      caseId: "fault-invalid",
      familyId: "fault-invalid",
      split: "tuning" as const,
      labels: ["provider_failure" as const],
      authorizedIntent: { instruction: "Read config" },
      scope: { tenantId: "tenant-a", workspaceId: "ws-a" },
      candidates: [input.candidates[0]],
      injectedFault: { provider: "typesafe" as const, scenarioId: "f901" },
      expectation: { kind: "no_call" as const, reason: "provider_failure" as const, expectedFailureCode: "invalid_response" as const },
    }],
  };
  const report = await runEvaluation(corpus, {
    suite: { baseline: inert("invalid-baseline"), current: inert("invalid-current"), proposed: counted },
    splits: ["tuning"],
    faultScenarios: { f901: async () => ({ outcome: "provider_failure", failure: { code: "invalid_response" } }) },
  });
  assert.equal(attempts, 0);
  assert.equal(fetches, 0);
  assert.deepEqual(report.cases.map((entry) => entry.failure), [{ code: "invalid_response" }, { code: "invalid_response" }, { code: "invalid_response" }]);
});

test("malformed responses, timeout, abort, and exhausted budget use stable Task 5 failure codes", async () => {
  const malformed = await subjectWith(async () => response({ model: TYPESAFE_DEFAULT_MODEL, answers: {}, usage: { input_tokens: 1, output_tokens: 1 } })).attempt(input, runtime());
  assert.deepEqual(malformed.failure, { code: "invalid_response" });

  const timeoutFetch: TypeSafeFetch = async (_url, init) => new Promise((_, reject) => init.signal.addEventListener("abort", () => reject(new Error("secret timeout body")), { once: true }));
  const timedOut = await subjectWith(timeoutFetch, { timeoutMs: 100, maxAttempts: 1 }).attempt(input, runtime());
  assert.deepEqual(timedOut.failure, { code: "timeout" });
  assert.equal(JSON.stringify(timedOut).includes("secret timeout body"), false);

  const controller = new AbortController();
  controller.abort();
  const aborted = await subjectWith(async () => answer("c0")).attempt(input, runtime(controller.signal));
  assert.deepEqual(aborted.failure, { code: "aborted" });

  let dispatched = 0;
  const budget = createJudgmentBudgetAccount({ maxCalls: 0 });
  const exhausted = await subjectWith(async () => { dispatched += 1; return answer("c0"); }, { budget }).attempt(input, runtime());
  assert.deepEqual(exhausted.failure, { code: "budget_exhausted" });
  assert.equal(dispatched, 0);
});

test("subject bounds reject oversized evaluation input before provider policy or fetch", async () => {
  let policyCalls = 0;
  let fetches = 0;
  const subject = subjectWith(async () => { fetches += 1; return answer("c0"); }, {
    providerInputPolicy: (request: ProviderInputRequest) => { policyCalls += 1; return { authorized: true, digest: request.digest }; },
  });
  const tooMany = { ...input, candidates: Array.from({ length: EVALUATION_SUBJECT_LIMITS.candidates + 1 }, (_, index) => ({ ...input.candidates[0], candidateId: `sensitive-${index}`, label: `label ${index}` })) };
  assert.deepEqual(await subject.attempt(tooMany, runtime()), { outcome: "provider_failure", failure: { code: "invalid_response" } });
  assert.deepEqual(await subject.attempt({ ...input, authorizedIntent: { instruction: "x".repeat(EVALUATION_SUBJECT_LIMITS.instructionChars + 1) } }, runtime()), { outcome: "provider_failure", failure: { code: "invalid_response" } });
  assert.deepEqual(await subject.attempt({ ...input, candidates: [{ ...input.candidates[0], label: "x".repeat(EVALUATION_SUBJECT_LIMITS.labelChars + 1) }] }, runtime()), { outcome: "provider_failure", failure: { code: "invalid_response" } });
  assert.equal(policyCalls, 0);
  assert.equal(fetches, 0);
});

test("import provenance remains explicit after local compatibility changes", () => {
  assert.deepEqual(EVALUATION_SUBJECT_IMPORT_PROVENANCE, {
    source: "reviewed-untracked-o03-sibling-worktree",
    cleanSourceCommit: null,
    importedBlobs: {
      judgments: "c8ac4a208e6edf337778cd36469a02cdaacd431e",
      typesafe: "57cb6fb00edf04fc92ec13329abf86f657874701",
      test: "5fa1c7f891e8cb65fc1bb2443d7aa95a9f9a1cad",
    },
    locallyModifiedForCompatibility: true,
  });
});

test("missing trusted config fails closed without reading credentials or reporting secrets", async () => {
  const subject = typeSafeEvaluationSubject(undefined);
  const result = await subject.attempt(input, runtime());
  assert.deepEqual(result, { outcome: "provider_failure", failure: { code: "missing_credentials" } });
  assert.equal(JSON.stringify(result).includes(API_KEY), false);
  assert.deepEqual(subject.providers, ["typesafe"]);
  assert.equal(subject.kind, "injected_adapter");
});
