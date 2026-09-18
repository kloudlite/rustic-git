# O10 evaluation preparation — slice handoff

Date: 2026-09-18
Task: O10 (replay corpus and evaluation runner), independent preparation slice
Base SHA: `d2944fcb695cba15e9ec2809672e279a8991bc45`
Owner: Flash (implementation), Sol (held-out review, later)
Plan: [Harness operation executor](2026-09-18-harness-operation-executor.md) · Design §12

## Status

Implemented: corpus schema/validation, split-contamination gate, injectable subject
interface, deterministic baseline, whole-call scorer, usage/cost accounting, and the
shadow-only runner with its summary. 21 authored replay cases (12 tuning, 9 held-out)
and 14 deterministic tests are in place.

Not implemented, deliberately: anything that needs O03/O07/O08/O09 (provider adapters,
recipe resolution, dispatch, UI projection), live provider runs, and threshold choice.
No live benchmark value or rollout approval is asserted anywhere in this slice.

Verification is **pending a corrected pod run**. The first pod run
(`/Volumes/kdisk/operation-executor-records/o10-20260918T131328Z`) failed in two places,
both now fixed:

1. `types.log`: `evaluation.ts(823,77)` passed `"unsupported_capability"` to an
   `IssueCode` parameter; that code is not in the frozen taxonomy. It now reports
   `"unsupported_action"` (an accepted code) for a capability outside the read pilot.
2. `contracts.log`: the corpus failed its own loading gate because
   `held-provider-timeout` reused the tuning instruction from the web-app build-settings
   case. That case's instruction and the invalid-response case's instruction were
   reworded; the split-disjointness gate and every expectation are unchanged.

O01 (`contracts.ts`, `shape.ts`) was not modified. The corrected snapshot still needs a
pod run of the same lanes; this environment cannot reach the cluster
(`kubectl get pods -A` → `dial tcp: lookup kolomi-...: no such host`). No commit was made;
the new paths sit uncommitted in the `operation-executor-o10` worktree at base
`d2944fcb`.

## Files

| Path | Role |
| --- | --- |
| `harness/bench/src/operations/evaluation.ts` | Corpus types/validation, scoring, accounting, runner. Imports `contracts.ts` read-only. |
| `harness/bench/test/operations-evaluation.test.ts` | Deterministic tests; no network, no provider key, no dispatch. |
| `harness/bench/test/fixtures/operations-evaluation/corpus.json` | The sanitized corpus and its author-written expectations. |

No other operation module, dependency manifest, renderer, taskboard, or shared bench file
was changed.

## Runner API

```ts
// A subject proposes or abstains. It must never dispatch or mutate.
type EvaluationSubject = {
  subjectId: string;
  kind: "deterministic_baseline" | "injected_adapter";
  providers: readonly string[];          // drives unknown-usage reporting
  attempt(testCase: EvaluationCase): Promise<EvaluationAttempt>;
};

runEvaluation(corpus, {
  subjects,                 // deterministic baseline and/or injected current/proposed adapters
  splits?,                  // default: tuning + held_out
  pricing?,                 // validated before use; absent => every cost stays unknown
  clock?, runId?,           // injectable for deterministic reports
}): Promise<EvaluationReport>
```

`EvaluationAttempt` carries `outcome` (`proposed`/`abstain`/`unsupported`/`provider_failure`),
optional `calls`, `reason`, `errorCode`, self-reported `latencyMs`, and per-provider
`usage` (`inputTokens`, `cachedInputTokens`, `outputTokens`). A subject that cannot report
usage omits it rather than sending zeroes.

Other exports: `parseEvaluationCorpus` / `loadEvaluationCorpus`,
`findSplitContamination`, `evaluationLabelCoverage`, `deterministicBaselineSubject`,
`scoreAttempt`, `evaluationCallSignature`, `evaluationDependenciesCorrect`,
`validatePricingTable`, `computeUsageCost`, `summarizeEvaluationReport`.

Shadow invariant: the runner has no dispatch path and no executor reference. `dispatch`
is reported as `"none"`; the optional tripwire exists so a caller can prove it is never
called. Comparing approaches therefore cannot duplicate a mutation.

## Corpus

`corpus.json` declares `capabilityEffects` (the scorer refuses to guess an effect) and 21
cases. Each case records `authorizedIntent` (sanitized instruction, constraints,
expected results, and any separately recorded authorization), the trusted `scope`,
the scoped candidate state, forbidden capabilities/targets, and an author-written
expectation (`calls` with keys/args/argsFrom/dependsOn, or `no_call` with a reason and
optional error code). Expectations are never derived from subject output.

Tuning (12): literal exact read, semantic choice, duplicate candidate names, absent
candidate, stale candidate, typo target, negation, missing referent, cross-tenant
reference, injection in a resource label, unsupported workflow, deferred edit.

Held-out (9): parallel independent reads, sequential dependency with an output binding,
semantic choice, ambiguity, literal process inspect, provider timeout, provider invalid
response, cross-tenant path traversal, deferred process destroy with a recorded grant.

Coverage is enforced for all 16 labels: `literal_exact`, `semantic_choice`,
`duplicate_candidate`, `absent_candidate`, `stale_candidate`, `typo_target`, `negation`,
`missing_referent`, `cross_tenant`, `injection_label`, `unsupported_workflow`,
`ambiguity`, `deferred_mutation`, `sequential_dependency`, `parallel_reads`,
`provider_failure`.

Editing and mutation cases are labeled `deferred_mutation` and expect `no_call`; they are
excluded from pilot rates and reported separately as refused/proposed. A case may carry
a recorded user authorization and still expect refusal, so a grant is not pilot
enablement. Tenant, workspace, process, and path names are fictional; a test scans the
fixture for credential-looking patterns.

## Metrics and accounting

Per case: whole-call correctness (action **and** arguments **and** dependency shape),
action-only and args-only correctness, outcome/abstention correctness, unnecessary
abstention, missed abstention, unsafe calls with reasons, candidate misses, stale-target
uses, latency, usage, and cost.

Per subject/split: counts plus rates, p50/p95/max latency (nearest-rank), usage, and cost.
Rules that the tests pin:

- A rate with a zero denominator is `null`, never `0` and never `NaN`.
- A provider total is `null` unless every evaluated case reported that provider
  completely; partial reports keep `observedCases`/`unknownCases` so nothing partial is
  presented as a total.
- Cost is computed only from a caller-supplied price table that passed
  `validatePricingTable`; an unpriced or incomplete provider yields `null`, never zero.
- An unknown capability or any write/destroy effect proposed in the read pilot is
  recorded as unsafe; a handle absent from the scoped candidate state is a candidate
  miss.
- Deferred cases never enter pilot rates.

The report (`reportVersion: o10-evaluation-report-v1`) is machine-readable JSON with
`synthetic: true`, `live: false`, `dispatch: "none"`, explicit `limitations`, and
`thresholds.status: "not_evaluated"`. `summarizeEvaluationReport` prints the concise
human summary.

## Tests

`bench/test/operations-evaluation.test.ts` (14 tests, all deterministic):

1. corpus validates, is synthetic, and covers every label;
2. fixture carries no credential-looking payload;
3. every case has an authored expectation; deferred and fault cases refuse;
4. baseline is repeatable, shadow-only, and makes no live claim;
5. whole-call scoring separates action, arguments, and abstention;
6. unsafe calls and candidate misses are counted, tripwire unused;
7. dependency shape is scored for parallel and sequential plans;
8. provider faults never become selections;
9. missing usage stays unknown; cost needs complete usage and validated pricing;
10. invalid pricing is rejected before any cost is reported;
11. zero denominators report `null`;
12. latency percentiles are nearest-rank;
13. deferred cases stay out of pilot rates and are reported separately;
14. split contamination (family, instruction, case id) is rejected.

## Verification to run (pod)

```bash
# inside the dev pod, Node 24, from the repository root's harness directory
node --test --test-concurrency=4 'bench/test/operations-evaluation.test.ts'
```

Also run the O01 strict type lane over the two new TypeScript files (the shared
`harness/tsconfig.main.json` does not include `bench/`), and the existing
`operations-contracts.test.ts` to confirm nothing shared regressed. Record the job IDs,
exit codes, and node version in the task record; PM2 status alone is not evidence.

## Reviewer/CI invocation

Reviewer or CI supplies the private oracle bundle at runtime. Put it under
`.local/evaluation-oracles/` (git-ignored) or outside the repository; never commit the
bundle or copy it into a source, test, or fixture directory. The trusted bootstrap module
is injected separately and exports `createEvaluationBootstrap()`, which returns the
TypeSafe configuration and runner-private fault scenarios. Neither dependency is exposed
to evaluation subjects or printed. The bootstrap is trusted executable code: reviewer/CI
must control it like a credential, keep it outside the repository, and make it owner-only
writable before passing it to the runner.

```bash
cd harness
npm run evaluation:operations -- \
  --corpus bench/test/fixtures/operations-evaluation/corpus.json \
  --oracles ../.local/evaluation-oracles/reviewer-oracles.json \
  --bootstrap /run/secrets/operation-evaluation/bootstrap.mjs \
  --output ../.local/operation-evaluation/report.json \
  --run-id "$RUN_ID"
```

`--corpus`, `--oracles`, `--bootstrap`, and `--output` are mandatory. The runner resolves
the oracle's real path and refuses committed source/fixture custody, including symlink
bypasses. CI should materialize both private inputs immediately before the run and remove
them with the job workspace or secret mount afterward.

## Remaining gates

1. Pod verification of the two files above, then a commit.
2. O03 (judgment adapter) and O07 (resolution/recipes) supply real subjects; O08 supplies
   the dispatch boundary and O09 the event projection. This slice deliberately provides
   only the injectable interface, so no test pretends the pilot exists.
3. Sol reviews held-out whole-call failures and sets task-specific thresholds. Nothing
   here chooses or implies a threshold.
4. Live TypeSafe evaluation requires configured credentials plus an explicit evaluation
   budget; neither is authorized here.
5. Read-only single-tool enablement needs a recorded rollout decision, not a score.
