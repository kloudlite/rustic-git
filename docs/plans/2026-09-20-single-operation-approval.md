# Single Operation Approval Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make the executor/store the sole approval owner and narrow same-revision timestamp replay acceptance to decision evidence.

**Architecture:** Capability preparation validates and canonicalizes an approval request without side effects. The executor durably processes one decision, then dispatches with proof that the registry validates without invoking approval again.

**Tech Stack:** TypeScript, Node test runner, O02 capability registry, O05 operation store.

---

### Task 1: Specify the single approval boundary

**Files:**
- Modify: `harness/bench/test/operations-executor.test.ts`
- Modify: `harness/bench/test/operations-capabilities.test.ts`

**Step 1:** Change the executor registry fixture to expose approval preparation and assert the order `decision -> decision-recorded -> resume -> dispatch` with one approval callback.

**Step 2:** Add denial and forged-proof tests proving neither reaches the adapter.

**Step 3:** Run the focused tests and verify they fail because approval is still requested inside `dispatch`.

### Task 2: Move approval ownership to the executor

**Files:**
- Modify: `harness/bench/src/operations/capabilities.ts`
- Modify: `harness/bench/src/operations/executor.ts`

**Step 1:** Add a side-effect-free registry preparation method that returns validated args and the canonical approval request/expectation.

**Step 2:** Remove the approval callback from registry dispatch dependencies and accept recorded proof for approval-required calls.

**Step 3:** Have the executor obtain, persist, and consume the single decision before dispatch.

**Step 4:** Run executor and capability tests and verify they pass.

**Step 5:** Commit the approval change.

### Task 3: Restrict same-revision replay timestamps

**Files:**
- Modify: `harness/bench/test/operations-store.test.ts`
- Modify: `harness/bench/src/operations/store.ts`

**Step 1:** Add a failing replay test for a non-decision same-revision commit whose commit/event timestamps exceed snapshot `updatedAt`.

**Step 2:** Require same-revision commits to contain only decision-recorded evidence and retain exact event/commit timestamp equality.

**Step 3:** Run store tests and verify delayed decisions still pass while forged timestamp drift fails.

**Step 4:** Commit the replay validation change.

### Task 4: Verify the operation subsystem

**Files:**
- Verify: `harness/bench/test/operations-*.test.ts`

**Step 1:** Run executor, capability, scheduler, recovery, and store tests.

**Step 2:** Run `npm run typecheck:operations`.

**Step 3:** Run `git diff --check` and inspect the final diff.

**Step 4:** Commit any necessary test-only corrections, then request independent review.
