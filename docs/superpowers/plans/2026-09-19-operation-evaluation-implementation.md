# Operation Evaluation Isolation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build an isolated, no-dispatch evaluator that compares frozen baseline, current heuristic, and OpenRouter-backed TypeSafeAI JEV subjects using reviewer-only held-out oracles.

**Architecture:** Split public case inputs from reviewer-owned held-out expectations, join them only inside the scorer, and run three explicitly assigned subjects against one immutable cohort. Reuse the existing judgment and TypeSafeAI adapter contracts in shadow mode; inject all provider and dispatch boundaries so tests remain deterministic and fail closed.

**Tech Stack:** TypeScript, Node.js test runner, existing O01 operation contracts, existing O03 judgment/TypeSafeAI adapter, JSON fixtures.

---

### Task 1: Split Public Cases From Reviewer Oracles

**Files:**
- Modify: `harness/bench/src/operations/evaluation.ts`
- Modify: `harness/bench/test/operations-evaluation.test.ts`
- Modify: `harness/bench/test/fixtures/operations-evaluation/corpus.json`
- Create: `harness/bench/test/fixtures/operations-evaluation/reviewer-oracles.json`

**Step 1: Write the failing fixture-boundary tests**

Add tests asserting that every committed `held_out` case omits `expectation`, `forbidden`, `deferred`, and `providerFault`, while tuning cases retain authored expectations. Add a reviewer fixture helper used only by tests.

**Step 2: Run the focused test and verify failure**

Run in `/work/src/harness`:

```bash
node --test --test-name-pattern='public held-out fixture' bench/test/operations-evaluation.test.ts
```

Expected: FAIL because held-out cases currently include oracle-bearing fields.

**Step 3: Introduce separate public and oracle contracts**

In `evaluation.ts`, define `PublicEvaluationCase`, `EvaluationOracle`, and `EvaluationOracleBundle`. Keep tuning expectations on tuning cases, but make held-out expectations impossible in the parsed public type. Add `parseEvaluationOracleBundle` and `loadEvaluationOracleBundle` with strict unknown-field and duplicate-ID validation.

**Step 4: Move held-out answers into the reviewer fixture**

Delete oracle-bearing fields from held-out entries in `corpus.json`. Put the same expected calls/refusals, forbidden values, deferred flags, and fault declarations into `reviewer-oracles.json` keyed by `caseId`.

**Step 5: Run the focused tests**

Run:

```bash
node --test bench/test/operations-evaluation.test.ts
```

Expected: the new boundary tests pass; runner tests that still assume embedded held-out expectations fail and identify the next seam.

**Step 6: Commit**

```bash
git add harness/bench/src/operations/evaluation.ts harness/bench/test/operations-evaluation.test.ts harness/bench/test/fixtures/operations-evaluation/corpus.json harness/bench/test/fixtures/operations-evaluation/reviewer-oracles.json
git commit -m "Separate held-out evaluation oracles"
```

### Task 2: Validate And Join Reviewer Oracles

**Files:**
- Modify: `harness/bench/src/operations/evaluation.ts`
- Modify: `harness/bench/test/operations-evaluation.test.ts`

**Step 1: Write failing oracle-custody tests**

Cover missing bundle, corpus-version mismatch, missing oracle, duplicate oracle, extra unknown case, tuning-case oracle, and selecting held-out cases without an oracle. Assert failure occurs before any subject attempt.

**Step 2: Run the custody tests and verify failure**

Run:

```bash
node --test --test-name-pattern='oracle|held-out' bench/test/operations-evaluation.test.ts
```

Expected: FAIL because `runEvaluation` does not yet accept or validate reviewer oracles.

**Step 3: Implement a private scored-case join**

Add `resolveEvaluationCases(corpus, oracles, splits)` that returns scorer-only cases. Require exact selected held-out coverage, reject extras, preserve tuning expectations from the public corpus, and never attach an oracle to subject input.

**Step 4: Update `runEvaluation`**

Add `reviewerOracles?: EvaluationOracleBundle`. Require it whenever `held_out` is selected. Resolve all cases before iterating subjects so malformed custody never causes a partial provider run.

**Step 5: Run focused tests**

Run:

```bash
node --test bench/test/operations-evaluation.test.ts
```

Expected: all corpus, custody, scoring, and aggregation tests pass.

**Step 6: Commit**

```bash
git add harness/bench/src/operations/evaluation.ts harness/bench/test/operations-evaluation.test.ts
git commit -m "Validate reviewer evaluation oracles"
```

### Task 3: Enforce Baseline, Current, And Proposed Cohorts

**Files:**
- Modify: `harness/bench/src/operations/evaluation.ts`
- Modify: `harness/bench/test/operations-evaluation.test.ts`

**Step 1: Write failing suite-role tests**

Test that a suite must provide one unique `baseline`, `current`, and `proposed` subject; duplicate IDs, duplicate implementations, missing roles, and extra roles fail before attempts. Assert every report includes the same ordered case IDs for all roles.

**Step 2: Run the role tests and verify failure**

Run:

```bash
node --test --test-name-pattern='role|cohort' bench/test/operations-evaluation.test.ts
```

Expected: FAIL because subjects are currently an unconstrained array.

**Step 3: Replace the subject array with explicit roles**

Define `EvaluationSuite = { baseline; current; proposed }` and add `role` to case and subject reports. Validate distinct object identities and IDs. Keep `deterministicBaselineSubject()` frozen.

**Step 4: Add cohort fingerprints**

Compute a digest over ordered public case snapshots. Store it once in the report and on each role summary. Refuse comparison if any role lacks a score for the complete cohort.

**Step 5: Run focused tests**

Run:

```bash
node --test bench/test/operations-evaluation.test.ts
```

Expected: PASS.

**Step 6: Commit**

```bash
git add harness/bench/src/operations/evaluation.ts harness/bench/test/operations-evaluation.test.ts
git commit -m "Require distinct evaluation subjects"
```

### Task 4: Make No-Dispatch Enforceable

**Files:**
- Modify: `harness/bench/src/operations/evaluation.ts`
- Modify: `harness/bench/test/operations-evaluation.test.ts`

**Step 1: Write failing no-dispatch tests**

Assert subject input is deeply immutable, carries no executor-shaped property, and cannot mutate later subjects' snapshots. Add a malicious subject that invokes an injected test-only tripwire and assert a `dispatch_attempt` safety violation with no executed callback.

**Step 2: Run the safety tests and verify failure**

Run:

```bash
node --test --test-name-pattern='dispatch|immutable' bench/test/operations-evaluation.test.ts
```

Expected: FAIL because current inputs are ordinary mutable objects and the tripwire proves only non-use by the runner.

**Step 3: Add the evaluation runtime boundary**

Pass subjects a deep-frozen cloned input and a minimal `EvaluationRuntime` containing `signal`, `now`, and case-scoped fault access. Do not pass a dispatch callback. Instrument the test-only runtime proxy so access to forbidden executor names records `dispatch_attempt` and throws a sanitized error.

**Step 4: Record safety violations independently of proposals**

Extend `CaseScore` with `safetyViolations` and `dispatchAttempts`. A dispatch attempt forces `wholeCallCorrect` false even if the returned proposal matches the oracle.

**Step 5: Run focused tests**

Run:

```bash
node --test bench/test/operations-evaluation.test.ts
```

Expected: PASS with zero real dispatches.

**Step 6: Commit**

```bash
git add harness/bench/src/operations/evaluation.ts harness/bench/test/operations-evaluation.test.ts
git commit -m "Enforce no-dispatch evaluation runtime"
```

### Task 5: Add Deterministic Fault Injection

**Files:**
- Modify: `harness/bench/src/operations/evaluation.ts`
- Modify: `harness/bench/test/operations-evaluation.test.ts`
- Modify: `harness/bench/test/fixtures/operations-evaluation/reviewer-oracles.json`

**Step 1: Write one failing test per fault class**

Cover `timeout`, `aborted`, `invalid_response`, `provider_error`, `missing_credentials`, `budget_exhausted`, and `dispatch_attempt`. Assert stable classification, zero proposed calls, no arbitrary provider text in reports, and no credential-shaped values.

**Step 2: Run fault tests and verify failure**

Run:

```bash
node --test --test-name-pattern='fault|credential|budget' bench/test/operations-evaluation.test.ts
```

Expected: FAIL because only four coarse provider faults exist.

**Step 3: Define the fault contract**

Replace `PROVIDER_FAULT_KINDS` with the approved fault taxonomy and add a stable `EvaluationFailureCode`. Extend attempts and scores with sanitized failure metadata, separate from human notes.

**Step 4: Implement injected fault behavior**

Expose the selected public fault token only through `EvaluationRuntime`; keep the expected fault classification in the reviewer oracle. Normalize thrown errors and provider failures into stable codes.

**Step 5: Run focused tests**

Run:

```bash
node --test bench/test/operations-evaluation.test.ts
```

Expected: PASS.

**Step 6: Commit**

```bash
git add harness/bench/src/operations/evaluation.ts harness/bench/test/operations-evaluation.test.ts harness/bench/test/fixtures/operations-evaluation/reviewer-oracles.json
git commit -m "Classify evaluation faults"
```

### Task 6: Integrate The Existing Judgment Adapter Cleanly

**Files:**
- Create or integrate from a clean O03 commit: `harness/bench/src/operations/judgments.ts`
- Create or integrate from a clean O03 commit: `harness/bench/src/operations/typesafe.ts`
- Create or integrate from a clean O03 commit: `harness/bench/test/operations-typesafe.test.ts`
- Modify: `harness/bench/src/operations/evaluation.ts`
- Create: `harness/bench/src/operations/evaluation-subjects.ts`
- Create: `harness/bench/test/operations-evaluation-subjects.test.ts`

**Step 1: Establish a clean O03 source**

Do not merge the conflicted `operation-executor-o03` worktree. Locate a clean commit containing O03 or copy only the reviewed files after comparing them with O01 contracts. Record the source commit in the task notes.

**Step 2: Run O03 tests before evaluator wiring**

Run:

```bash
node --test bench/test/operations-typesafe.test.ts
```

Expected: PASS without live network access.

**Step 3: Write failing subject-adapter tests**

Test Choice mapping for selected, no-match, ambiguous, malformed response, timeout, abort, missing credential, and budget exhaustion. Assert `mode: "shadow"`, advisory actionability, pinned model provenance, complete usage handling, and no dispatch.

**Step 4: Implement `typeSafeEvaluationSubject`**

Build one bounded Choice question from the authorized intent and candidate labels. Approve only the synthetic evaluation state under an evaluation-specific provider-input policy. Convert selected candidate IDs into exact O01 calls using candidate metadata; convert all abstentions and provider failures into `EvaluationAttempt` without authorizing execution.

**Step 5: Wire trusted provider configuration**

Accept an already-resolved TypeSafe/OpenRouter configuration object from the caller. Do not read environment variables or credential files inside the evaluator. Missing config returns `missing_credentials`; secrets never enter reports.

**Step 6: Run adapter and evaluator tests**

Run:

```bash
node --test bench/test/operations-typesafe.test.ts bench/test/operations-evaluation-subjects.test.ts bench/test/operations-evaluation.test.ts
```

Expected: PASS.

**Step 7: Commit**

```bash
git add harness/bench/src/operations/judgments.ts harness/bench/src/operations/typesafe.ts harness/bench/src/operations/evaluation.ts harness/bench/src/operations/evaluation-subjects.ts harness/bench/test/operations-typesafe.test.ts harness/bench/test/operations-evaluation-subjects.test.ts
git commit -m "Evaluate TypeSafe judgments in shadow mode"
```

### Task 7: Wire The Current Heuristic Subject

**Files:**
- Modify: the production operation-selection module identified from the clean O07/O08 integration
- Modify: `harness/bench/src/operations/evaluation-subjects.ts`
- Modify: `harness/bench/test/operations-evaluation-subjects.test.ts`

**Step 1: Locate the shipped heuristic before editing**

Use graft in `/work/src` to identify the exact current target-resolution entry point and its call graph. Do not substitute the deterministic baseline if no production heuristic exists; stop and record the dependency.

**Step 2: Write a failing adapter-parity test**

For representative literal, ambiguous, missing, stale, and cross-scope inputs, assert the evaluation subject returns the same proposal/refusal as the production heuristic while using no dispatch dependency.

**Step 3: Implement the thinnest adapter**

Extract or call the pure resolution portion of the production path. Inject candidate snapshots and return proposals only. If the production function combines selection and execution, split only at the existing decision boundary and retain its behavior.

**Step 4: Run focused tests**

Run:

```bash
node --test bench/test/operations-evaluation-subjects.test.ts
```

Expected: PASS.

**Step 5: Commit**

```bash
git add harness/bench/src/operations/evaluation-subjects.ts harness/bench/test/operations-evaluation-subjects.test.ts <production-files>
git commit -m "Adapt current operation heuristic for evaluation"
```

### Task 8: Complete Metrics And Cohort Comparisons

**Files:**
- Modify: `harness/bench/src/operations/evaluation.ts`
- Modify: `harness/bench/test/operations-evaluation.test.ts`

**Step 1: Write failing metric tests**

Add tables covering zero denominators, incomplete usage, mixed provider usage, nearest-rank latency, safety violations without calls, provider attempts, parse failures, fault counts, and both baseline deltas. Assert comparison cohorts and fingerprints match.

**Step 2: Run metric tests and verify failure**

Run:

```bash
node --test --test-name-pattern='metric|comparison|usage|latency' bench/test/operations-evaluation.test.ts
```

Expected: FAIL for newly required metrics.

**Step 3: Extend report aggregation**

Add the missing counters and `baseline-current`/`baseline-proposed` comparisons. Keep unknown totals as null and keep thresholds advisory.

**Step 4: Add report redaction tests**

Serialize a report from failures containing fake keys, bearer headers, and arbitrary transport text. Assert none appears in JSON or the human summary.

**Step 5: Run focused tests**

Run:

```bash
node --test bench/test/operations-evaluation.test.ts bench/test/operations-evaluation-subjects.test.ts
```

Expected: PASS.

**Step 6: Commit**

```bash
git add harness/bench/src/operations/evaluation.ts harness/bench/test/operations-evaluation.test.ts
git commit -m "Complete operation evaluation metrics"
```

### Task 9: Add A Reviewer-Only Evaluation Entry Point

**Files:**
- Create: `harness/bench/src/operations/run-evaluation.ts`
- Create: `harness/bench/test/operations-run-evaluation.test.ts`
- Modify: `harness/package.json`
- Modify: `.gitignore`

**Step 1: Write failing CLI tests**

Test required explicit oracle path, refusal of paths inside committed public fixture directories unless in test mode, no credential output, deterministic report path, and nonzero exit on incomplete suites or custody errors.

**Step 2: Run CLI tests and verify failure**

Run:

```bash
node --test bench/test/operations-run-evaluation.test.ts
```

Expected: FAIL because no entry point exists.

**Step 3: Implement the CLI**

Require `--corpus`, `--oracles`, and `--output`. Resolve provider configuration through the existing trusted harness configuration helper. Print only run ID, case counts, output path, and sanitized failure codes. Never print configuration objects or environment variables.

**Step 4: Ignore local reviewer material**

Add a narrow ignore rule such as `.local/evaluation-oracles/` and document that CI injects the oracle file. Do not ignore the test fixture used for contract tests.

**Step 5: Run CLI and focused tests**

Run:

```bash
node --test bench/test/operations-run-evaluation.test.ts bench/test/operations-evaluation.test.ts bench/test/operations-evaluation-subjects.test.ts
```

Expected: PASS.

**Step 6: Commit**

```bash
git add harness/bench/src/operations/run-evaluation.ts harness/bench/test/operations-run-evaluation.test.ts harness/package.json .gitignore
git commit -m "Add reviewer operation evaluation runner"
```

### Task 10: Verify In The Authoritative Dev Pod

**Files:**
- Modify if needed: `docs/superpowers/plans/2026-09-18-operation-evaluation-handoff.md`

**Step 1: Create a disposable pod checkout**

From the laptop, use `deploy/dev/exec.sh` to create a disposable copy beneath `/work`, sourced from `/work/src`, without printing environment variables. Apply only the task diff.

**Step 2: Run focused Node tests**

Inside the disposable checkout's `harness` directory run:

```bash
node --test --test-concurrency=4 bench/test/operations-contracts.test.ts bench/test/operations-typesafe.test.ts bench/test/operations-evaluation.test.ts bench/test/operations-evaluation-subjects.test.ts bench/test/operations-run-evaluation.test.ts
```

Expected: all tests pass.

**Step 3: Run the strict TypeScript lane**

Use the repository's temporary strict bench TypeScript configuration or add the new files to the existing strict lane. Run `tsc --noEmit` and expect exit 0.

**Step 4: Run the full harness suite**

Run:

```bash
bun test
```

Expected: all non-Electron tests pass. If Electron/X11 is unavailable, record the exact environment-only failure rather than calling the lane green.

**Step 5: Update the handoff with evidence**

Record checkout SHA, Node/Bun versions, exact commands, test counts, exit codes, and any environment-only skip. Do not include environment dumps, provider keys, or request bodies.

**Step 6: Inspect repository hygiene**

Run `git status --short` and `git diff --check`. Confirm only intended task files are staged or modified. Do not alter unrelated `.claude` changes unless the user confirms they are disposable.

**Step 7: Commit verification notes**

```bash
git add docs/superpowers/plans/2026-09-18-operation-evaluation-handoff.md
git commit -m "Record operation evaluation verification"
```
