# Opaque Dispatch Authority Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make approval-required dispatch authorizations unforgeable, attempt-bound, and one-shot while restoring scheduler regression coverage.

**Architecture:** A shared in-memory authority owns opaque authorization tokens and their bound claims. O05 issues tokens after durable authorization; O02 burns and validates them immediately before adapter invocation. Capability version remains the approval-policy revision boundary.

**Tech Stack:** TypeScript, Node test runner, O02 capability registry, O05 operation store.

---

### Task 1: Specify Unforgeable Consumption

**Files:**
- Create: `harness/bench/src/operations/dispatch-authority.ts`
- Modify: `harness/bench/test/operations-store.test.ts`
- Modify: `harness/bench/test/operations-capabilities.test.ts`

**Steps:**
1. Add failing tests for fabricated tokens, foreign-authority tokens, and burn-on-mismatch behavior.
2. Add failing tests for state and attempt invalidation.
3. Run the focused tests and confirm they fail because authorization is still callback-based.
4. Commit the tests only if repository convention permits red commits; otherwise retain them for Task 2.

### Task 2: Implement Shared Opaque Authority

**Files:**
- Create: `harness/bench/src/operations/dispatch-authority.ts`
- Modify: `harness/bench/src/operations/store.ts`
- Modify: `harness/bench/src/operations/capabilities.ts`
- Modify: `harness/bench/src/operations/executor.ts`

**Steps:**
1. Define an opaque token whose valid identities are held only in the authority's private registry.
2. Implement `issue(binding)` and burn-before-check `consume(token, claims)`.
3. Have O05 issue tokens from granted `resume()` and validated `retryStep()`.
4. Replace public authorization callbacks with token dependencies.
5. Inject the same authority into O05 and O02 through executor construction.
6. Run capability, store, and executor suites plus operation typecheck.
7. Commit.

### Task 3: Add Real End-to-End Retry Coverage

**Files:**
- Modify: `harness/bench/test/operations-executor.test.ts`

**Steps:**
1. Add a real `OperationStore` plus real `CapabilityRegistry` approval-required idempotent mutation fixture.
2. Assert the initial token cannot authorize the retry and the retry token succeeds once.
3. Assert state changes invalidate outstanding tokens.
4. Run the executor suite and typecheck.
5. Commit.

### Task 4: Restore Scheduler Coverage

**Files:**
- Modify: `harness/bench/test/operations-scheduler.test.ts`

**Steps:**
1. Restore typed open-object output binding tests removed by the prior commit.
2. Assert the exact missing-output path `$.calls[1].argsFrom.id` and message.
3. Run the scheduler suite.
4. Commit.

### Task 5: Verify and Review

**Steps:**
1. Run `node --test bench/test/operations-*.test.ts`.
2. Run `npm run typecheck:operations`.
3. Run `git diff --check` and inspect the cumulative diff.
4. Request independent review against this design.
5. Fix Critical and Important findings before completion.
