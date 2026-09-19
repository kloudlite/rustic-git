# Attempt-Bound Dispatch Authorization Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Require a fresh, one-shot O05 authorization for every approval-required dispatch attempt and defer unsafe recovery reconciliation.

**Architecture:** O05 retains an identity-branded in-memory authorization registry. State transitions mint attempt-bound authorizations; O02 asks O05 to consume one against the exact dispatch before running an adapter.

**Tech Stack:** TypeScript, Node test runner, O02 capability registry, O05 operation store.

---

### Task 1: Specify authorization lifecycle

**Files:**
- Modify: `harness/bench/test/operations-store.test.ts`
- Modify: `harness/bench/test/operations-capabilities.test.ts`
- Modify: `harness/bench/test/operations-executor.test.ts`

**Steps:**
1. Add failing tests for missing, reused, stale-attempt, and mismatched authorization.
2. Add a failing retry test requiring a newly minted attempt authorization.
3. Run focused tests and confirm failures are caused by the absent lifecycle.

### Task 2: Implement attempt-bound authorization

**Files:**
- Modify: `harness/bench/src/operations/store.ts`
- Modify: `harness/bench/src/operations/executor.ts`
- Modify: `harness/bench/src/operations/capabilities.ts`

**Steps:**
1. Add the opaque authorization type and O05 issue/consume registry.
2. Return authorization from granted `resume()` and `retryStep()` transitions.
3. Replace recorded-decision proof in O02 dependencies with an O05 consumption callback and authorization.
4. Request a fresh authorization after every retry transition.
5. Run focused suites and typecheck.
6. Commit.

### Task 3: Defer unsafe recovery reconciliation

**Files:**
- Modify: `harness/bench/test/operations-executor.test.ts`
- Modify: `harness/bench/src/operations/executor.ts`

**Steps:**
1. Add a failing test proving `reconcile_step` does not invoke reconciliation from caller-supplied calls.
2. Restrict recovery execution to expiry actions.
3. Run executor and recovery suites.
4. Commit.

### Task 4: Correct diagnostics and comments

**Files:**
- Modify: `harness/bench/src/operations/scheduler.ts`
- Modify: `harness/bench/src/operations/capabilities.ts`
- Modify: `harness/bench/test/operations-scheduler.test.ts`

**Steps:**
1. Assert the exact `$.calls[...]` diagnostic.
2. Fix the path literal and registry dispatch comment.
3. Run scheduler and capability suites.
4. Commit.

### Task 5: Verify

**Steps:**
1. Run `node --test bench/test/operations-*.test.ts`.
2. Run `npm run typecheck:operations`.
3. Run `git diff --check` and inspect the cumulative diff.
4. Request independent review.
