# Operation Executor O06/O07 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the operation scheduler/executor and bounded candidate-resolution recipes required before O08 single-tool integration.

**Architecture:** O06 consumes accepted capability and durable-state interfaces to run validated DAGs under dependency and conflict constraints. O07 resolves trusted scoped facts into bounded read-only recipes, using deterministic matching first and TypeSafe judgments only over validated candidate sets. The slices develop in isolated worktrees and integrate only after independent review.

**Tech Stack:** TypeScript, Node test runner, existing operation contracts/capability registry/store, TypeSafe judgment adapter, Electron renderer build.

---

### Task 1: O06 Scheduler Contract Tests

**Files:**
- Create: `harness/bench/test/operations-scheduler.test.ts`
- Create: `harness/bench/src/operations/scheduler.ts`

1. Write failing tests for cycle rejection, missing dependencies, invalid typed output bindings, independent read overlap, dependency barriers, global/per-operation concurrency, FIFO fairness, and declared read/write conflict keys.
2. Add cases that serialize collection writes and unknown command footprints while allowing disjoint reads.
3. Run the focused test in the dev pod and confirm failures describe missing scheduler behavior.
4. Implement the minimal scheduler queue, dependency readiness, conflict-lane acquisition, deadline, and abort behavior using O01 contracts.
5. Re-run focused tests and strict operation typecheck in the dev pod.
6. Commit the scheduler increment.

### Task 2: O06 Execution And Recovery

**Files:**
- Create: `harness/bench/test/operations-executor.test.ts`
- Create: `harness/bench/src/operations/executor.ts`
- Modify only if required by an accepted interface gap: O06-owned files above

1. Write failing tests proving dispatch goes through O02 adapters with trusted actor/context, intent persists before dispatch, unknown mutation outcomes reconcile before retry, and cancellation preserves committed evidence.
2. Add aggregation cases for completed, failed, partial, cancelled, skipped dependent work, and unresolved reconciling outcomes without generic rollback.
3. Run focused tests in the dev pod and confirm they fail before implementation.
4. Implement execution orchestration over the scheduler, capability dispatcher, and O05 persistence interfaces.
5. Run O06 focused tests, the operation suites affected by O02/O05, and strict operation typecheck.
6. Commit and request an independent final-source review; do not merge.

### Task 3: O07 Resolution Contract Tests

**Files:**
- Create: `harness/bench/test/operations-resolve.test.ts`
- Create: `harness/bench/src/operations/resolve.ts`

1. Write failing tests for explicit IDs, exact literals, duplicate names, absent/stale candidates, typos, negation, missing referents, scope filtering, and cross-tenant rejection.
2. Cover `known`, `unspecified`, `explicitly_clear`, `ambiguous`, and `unsupported` values with per-field provenance.
3. Prove deterministic matches avoid model calls and semantic selection receives only current validated candidates plus `no_match`/`ambiguous` outcomes.
4. Run focused tests in the dev pod and confirm the expected failures.
5. Implement minimal deterministic resolution and bounded O03 judgment fallback without inventing resources or silently substituting stale identities.
6. Run focused tests and strict operation typecheck, then commit.

### Task 4: O07 Bounded Read Recipes

**Files:**
- Create: `harness/bench/test/operations-recipes.test.ts`
- Create: `harness/bench/src/operations/recipes.ts`
- Modify only if required by an accepted interface gap: O07-owned files above

1. Write failing tests for workspace lookup/progress, bench-owned process metadata, and tool/skill discovery recipes.
2. Prove dependent choices are staged after newly fetched facts, expansion obeys step/round/candidate limits, unsupported workflows return to the main agent, and no recipe invokes workspace logs or an unrestricted agent ask.
3. Run focused tests in the dev pod and confirm failures precede implementation.
4. Implement the bounded recipe registry and expansion against O07 resolution outputs; expose execution-ready DAGs compatible with O06 without editing O06 files.
5. Run all O07 focused tests, O03 judgment tests, capability-contract tests, and strict operation typecheck.
6. Commit and request an independent final-source review; do not merge.

### Task 5: Integrate And Gate The Wave

**Files:**
- Modify only for reviewed integration conflicts: O06/O07-owned operation modules and tests
- Update after successful verification: `docs/architecture-view/tasks.json`

1. Review both branch diffs and focused dev-pod evidence.
2. Merge O06, resolve conflicts minimally, and run its focused tests plus typecheck.
3. Merge O07, resolve conflicts minimally, and run both focused suites plus typecheck.
4. Run the complete non-renderer Node suite with explicit test files, excluding only the Electron boot test and helper scripts.
5. Run renderer build and Electron boot under Xvfb separately.
6. Mark O06/O07 verified only if every required gate passes; otherwise record the exact blocker. Start O08 from the resulting verified SHA.
