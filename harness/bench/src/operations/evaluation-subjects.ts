/**
 * Task 6 TypeSafe evaluation subject.
 *
 * Dependency provenance: `judgments.ts`, `typesafe.ts`, their test, and judgment
 * fixtures were imported byte-identically from reviewed untracked files in the sibling
 * O03 worktree because no clean source commit existed. No reachable local or remote ref
 * contained their blobs on 2026-09-19:
 * judgments c8ac4a208e6edf337778cd36469a02cdaacd431e, typesafe
 * 57cb6fb00edf04fc92ec13329abf86f657874701, test
 * 5fa1c7f891e8cb65fc1bb2443d7aa95a9f9a1cad.
 *
 * This subject is evaluation-only: it sends synthetic corpus state, always requests
 * shadow judgments, and returns proposals to the evaluator. It owns no dispatcher.
 */
import type { JsonValue } from "./contracts.ts";
import type {
  CandidateResource,
  EvaluationAttempt,
  EvaluationFailureCode,
  EvaluationSubject,
  EvaluationSubjectInput,
  ProviderUsage,
} from "./evaluation.ts";
import { approveProviderState, createTypeSafeJudgmentAdapter } from "./typesafe.ts";
import type { TypeSafeConfig } from "./typesafe.ts";

const QUESTION_VERSION = "operation-candidate-v1";
const QUESTION_ID = "operation_candidate";

export const EVALUATION_SUBJECT_IMPORT_PROVENANCE = {
  source: "reviewed-untracked-o03-sibling-worktree",
  cleanSourceCommit: null,
  importedBlobs: {
    judgments: "c8ac4a208e6edf337778cd36469a02cdaacd431e",
    typesafe: "57cb6fb00edf04fc92ec13329abf86f657874701",
    test: "5fa1c7f891e8cb65fc1bb2443d7aa95a9f9a1cad",
  },
  locallyModifiedForCompatibility: true,
} as const;

export const EVALUATION_SUBJECT_LIMITS = {
  candidates: 64,
  instructionChars: 2_000,
  labelChars: 200,
  noteChars: 400,
  listItems: 32,
  listItemChars: 500,
  stateBytes: 128 * 1024,
} as const;

export type TypeSafeEvaluationSubjectConfig = TypeSafeConfig;

function providerFailure(code: EvaluationFailureCode, attempts: number, usage?: ProviderUsage[]): EvaluationAttempt {
  return {
    outcome: "provider_failure",
    failure: { code },
    providerAttempts: { typesafe: attempts },
    ...(usage && usage.length ? { usage } : {}),
  };
}

function failureCodeOf(code: string | undefined): EvaluationFailureCode {
  if (code === "budget_exceeded") return "budget_exhausted";
  if (code === "aborted") return "aborted";
  if (code === "timeout" || code === "deadline_exceeded") return "timeout";
  if (code === "invalid_response" || code === "usage_unreported" || code === "response_too_large" || code === "response_body_unbounded") {
    return "invalid_response";
  }
  return "provider_error";
}

function usageOf(batch: {
  model: { model: string; version: string };
  records: Array<{ usage: { inputTokens: number; outputTokens: number } }>;
}): ProviderUsage[] {
  const usage = batch.records[0]?.usage;
  if (!usage || (usage.inputTokens === 0 && usage.outputTokens === 0)) return [];
  return [{ provider: "typesafe", inputTokens: usage.inputTokens, cachedInputTokens: 0, outputTokens: usage.outputTokens, model: batch.model.model, version: batch.model.version }];
}

function attemptsOf(batch: { usage: { calls: number } }): Record<string, number> {
  return { typesafe: batch.usage.calls };
}

function boundedInput(input: EvaluationSubjectInput): boolean {
  const limits = EVALUATION_SUBJECT_LIMITS;
  if (input.candidates.length > limits.candidates || input.authorizedIntent.instruction.length > limits.instructionChars) return false;
  if (input.candidates.some((candidate) => candidate.label.length > limits.labelChars || (candidate.note?.length ?? 0) > limits.noteChars)) return false;
  for (const values of [input.authorizedIntent.constraints, input.authorizedIntent.expectedResults, input.authorizedIntent.contextRefs]) {
    if (values && (values.length > limits.listItems || values.some((value) => value.length > limits.listItemChars))) return false;
  }
  return true;
}

function proposal(candidate: CandidateResource): EvaluationAttempt {
  let args: Record<string, JsonValue>;
  if (candidate.capability === "file.read" && candidate.path) {
    args = { path: candidate.path };
  } else if (candidate.capability === "process.inspect") {
    args = { processId: candidate.targetRef };
  } else {
    return { outcome: "unsupported", reason: "unsupported", errorCode: "unsupported_capability" };
  }
  return {
    outcome: "proposed",
    calls: [{ key: "call_1", capability: candidate.capability, capabilityVersion: "1.0.0", targetRef: candidate.targetRef, args }],
  };
}

function syntheticState(input: EvaluationSubjectInput): JsonValue {
  return {
    contract: "operation-evaluation-state/v1",
    authorizedIntent: {
      instruction: input.authorizedIntent.instruction,
      ...(input.authorizedIntent.constraints ? { constraints: [...input.authorizedIntent.constraints] } : {}),
      ...(input.authorizedIntent.expectedResults ? { expectedResults: [...input.authorizedIntent.expectedResults] } : {}),
      ...(input.authorizedIntent.contextRefs ? { contextRefs: [...input.authorizedIntent.contextRefs] } : {}),
    },
    candidates: input.candidates.map((candidate, index) => ({
      option: `c${index}`,
      label: candidate.label,
    })),
  } as JsonValue;
}

export function typeSafeEvaluationSubject(config?: TypeSafeEvaluationSubjectConfig): EvaluationSubject {
  return {
    subjectId: "typesafe-jev-1.13.0-shadow",
    availability: "available",
    kind: "injected_adapter",
    providers: ["typesafe"],
    async attempt(input, runtime) {
      if (!config) return providerFailure("missing_credentials", 0);
      if (!boundedInput(input)) return providerFailure("invalid_response", 0);

      const synthetic = syntheticState(input);
      if (Buffer.byteLength(JSON.stringify(synthetic), "utf8") > EVALUATION_SUBJECT_LIMITS.stateBytes) return providerFailure("invalid_response", 0);
      const state = approveProviderState({ state: synthetic, source: "synthetic_evaluation", maxChars: EVALUATION_SUBJECT_LIMITS.stateBytes });
      if (!state.ok) return providerFailure("provider_error", 0);

      let adapter: ReturnType<typeof createTypeSafeJudgmentAdapter>;
      try {
        adapter = createTypeSafeJudgmentAdapter({ ...config, mode: "shadow", state: state.value });
      } catch {
        return providerFailure("missing_credentials", 0);
      }
      const result = await adapter.judge(
        {
          state: state.value,
          mode: "shadow",
          ...(config.providerInputScope ? { scope: config.providerInputScope } : {}),
          questions: [
            {
              id: QUESTION_ID,
              request: {
                question: "Which listed candidate exactly satisfies the authorized intent within the supplied scope? Choose no match or ambiguous rather than guessing.",
                questionVersion: QUESTION_VERSION,
                options: input.candidates.map((candidate, index) => ({ id: `c${index}`, label: candidate.label })),
                allowAbstain: true,
              },
            },
          ],
        },
        runtime.signal,
      );
      if (!result.ok) return providerFailure("invalid_response", 0);

      const usage = usageOf(result);
      const providerAttempts = attemptsOf(result);
      const judgment = result.results[QUESTION_ID];
      if (!judgment) return providerFailure("invalid_response", result.usage.calls, usage);
      if (judgment.outcome === "provider_failure") return providerFailure(failureCodeOf(result.failureCode), result.usage.calls, usage);
      if (judgment.outcome === "no_match") {
        return { outcome: "abstain", reason: "no_match", errorCode: "no_match", providerAttempts, ...(usage.length ? { usage } : {}) };
      }
      if (judgment.outcome === "ambiguous") {
        return { outcome: "abstain", reason: "ambiguous", errorCode: "ambiguous_match", providerAttempts, ...(usage.length ? { usage } : {}) };
      }
      if (judgment.outcome !== "selected") return providerFailure("invalid_response", result.usage.calls, usage);

      const selectedIndex = /^c([0-9]+)$/.exec(judgment.optionId);
      const candidate = selectedIndex ? input.candidates[Number(selectedIndex[1])] : undefined;
      if (!candidate) return providerFailure("invalid_response", result.usage.calls, usage);
      return { ...proposal(candidate), providerAttempts, ...(usage.length ? { usage } : {}) };
    },
  };
}
