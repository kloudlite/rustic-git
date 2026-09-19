# O05 Audit Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close every valid O05-owned durability, replay, lifecycle, recovery, retention, and trust-boundary finding with regression coverage.

**Architecture:** Preserve the append-only operation model while introducing checksummed v2 frames, strict predecessor validation, durable dedupe tombstones, bounded compaction metadata, and hardened file access. Make capability and retry facts store-owned, and expose recovery/cursor primitives for O02/O06/O09 without implementing those later stages.

**Tech Stack:** TypeScript, Node.js synchronous filesystem APIs, `node:test`, frozen O01 operation contracts.

---

### Task 1: Trusted capability metadata

**Files:**
- Modify: `harness/bench/src/operations/store.ts`
- Test: `harness/bench/test/operations-store.test.ts`

1. Add failing tests proving callers cannot forge effect, resource keys, capability version, approval, or retry policy.
2. Run `node --test bench/test/operations-store.test.ts --test-name-pattern='trusted capability|forge'` in the dev pod and confirm failure.
3. Add a trusted capability metadata source to `OperationStoreOptions`; narrow `QueueStepInput` to capability identity, target, dependencies, queue reason, and summary.
4. Resolve and persist descriptor-owned version/effect/resources/policies inside `queueStep`; expose store-owned retry lookup to recovery.
5. Run the focused tests and commit only if requested.

### Task 2: Decision and session invariants

**Files:**
- Modify: `harness/bench/src/operations/store.ts`
- Test: `harness/bench/test/operations-store.test.ts`

1. Extend the existing failing freshness test for expired, future-dated, pre-used, wrong-session, and over-bound decisions.
2. Add failing tests for cross-session resume and expired additional input with durable actor/session/revision/payload binding.
3. Run the focused tests and confirm the failures.
4. Validate records against the trusted clock, pending decision, operation context, expiry bound, and unused state before append.
5. Reject running-step decisions; bind additional-input resolutions durably and validate them on replay.
6. Re-run focused tests.

### Task 3: Strict replay invariants

**Files:**
- Modify: `harness/bench/src/operations/store.ts`
- Modify: `harness/bench/src/operations/state.ts`
- Test: `harness/bench/test/operations-store.test.ts`

1. Split the broad replay test into focused failing cases for skipped revision, immutable identity/request/context/budgets, illegal operation transition, illegal step transition, and inconsistent event metadata.
2. Run those tests and confirm each reaches its intended guard rather than failing incidentally on sequence reuse.
3. Add predecessor validation before mutating loaded state. Reuse O01 transition predicates and compare all immutable fields canonically.
4. Validate event operation/step identity, revision, timestamp ordering, and phase against the committed transition.
5. Validate decision/resolution uniqueness and consistency during fold.
6. Re-run focused replay tests.

### Task 4: Versioned framed logs and hardened files

**Files:**
- Modify: `harness/bench/src/operations/store.ts`
- Modify: `harness/bench/test/operations-store.test.ts`
- Create: `harness/bench/test/fixtures/operations-o05/manifest.json`
- Create or modify: framed O05 corruption fixtures under `harness/bench/test/fixtures/operations-o05/`

1. Add failing tests for incomplete final v2 frame, complete bad checksum, malformed complete tail, symlink log, non-regular log, permissive mode repair/refusal, and unsupported directory fsync.
2. Add fixture-manifest completeness coverage.
3. Run focused storage tests and confirm failure.
4. Implement v2 framing with explicit length and SHA-256 digest; retain strict v1 reader detection.
5. Harden opens with no-follow flags and post-open regular-file/owner/mode checks. Create files at `0600` and the operation directory at `0700`.
6. Make directory fsync mandatory for acknowledged creation/rewrite.
7. Re-run focused tests.

### Task 5: Legacy migration and crash-safe rewrite

**Files:**
- Modify: `harness/bench/src/operations/store.ts`
- Test: `harness/bench/test/operations-store.test.ts`

1. Add failing tests that v1 replay works, first mutation rewrites to v2, and failures before temp fsync, rename, or directory fsync leave one recoverable authoritative version.
2. Run focused migration tests and confirm failure.
3. Implement same-directory temporary rewrite, file fsync, atomic rename, and directory fsync; clean stale temp files conservatively.
4. Reopen the store after every injected crash point and assert no accepted commit disappears or duplicates.
5. Re-run focused tests.

### Task 6: Durable tombstones, compaction, and cursor status

**Files:**
- Modify: `harness/bench/src/operations/store.ts`
- Test: `harness/bench/test/operations-store.test.ts`

1. Keep the existing failing tombstone test and add changed-body-after-retention, restart, tombstone corruption, compacted replay, cursor-ahead, caught-up, and snapshot-required tests.
2. Run focused retention/cursor tests and confirm failure.
3. Add a framed durable tombstone index and load it before operation logs.
4. Change terminal reclamation to persist tombstone and compact terminal snapshot/evidence before deleting obsolete history.
5. Extend `InspectResult` with explicit cursor status, earliest sequence, and resync snapshot where required.
6. Re-run focused tests.

### Task 7: Backend identity and cancellation intent

**Files:**
- Modify: `harness/bench/src/operations/store.ts`
- Modify: `harness/bench/src/operations/state.ts`
- Test: `harness/bench/test/operations-store.test.ts`

1. Add failing tests for recording backend identity after dispatch, identity immutability, and durable abort intent on cancellation.
2. Run focused tests and confirm failure.
3. Add explicit transitions for backend identity assignment and running-step abort request without changing frozen public O01 contracts.
4. Ensure replay validates both additions.
5. Re-run focused tests.

### Task 8: Recovery barriers and policy authority

**Files:**
- Modify: `harness/bench/src/operations/recovery.ts`
- Modify: `harness/bench/src/operations/store.ts`
- Test: `harness/bench/test/operations-recovery.test.ts`

1. Keep the existing waiting-state failure and add failing tests for reconcile-before-dispatch, cancellation resume-abort, expired/deadline barriers, running reads, one trusted retry source, and complete pending reconciliation.
2. Run focused recovery tests and confirm failure.
3. Remove the recovery retry callback. Query trusted policy from the store.
4. Build reconciliation/abort actions first; if any exist, suppress dispatch and retry actions. Suppress dispatch for waiting, reconciling, cancellation, and expired states.
5. Emit `resume_abort` for durable cancellation intent and include running plus unknown steps in `pendingReconciliation`.
6. Re-run focused tests.

### Task 9: Lifecycle reporting details

**Files:**
- Modify: `harness/bench/src/operations/state.ts`
- Test: `harness/bench/test/operations-store.test.ts`

1. Add failing tests that retry attempts receive a fresh `startedAt` and skipped steps appear in the non-applied settlement summary.
2. Run focused tests and confirm failure.
3. Reset attempt-local timing on retry and include skipped count in the settlement description.
4. Re-run focused tests.

### Task 10: Authoritative verification

**Files:**
- No product changes unless verification exposes a defect.

1. Run all operation tests in the dev pod from the live checkout during TDD.
2. Create a temporary dev-pod copy of the exact worktree, excluding `.git` and macOS AppleDouble files.
3. Compare SHA-256 hashes for every changed source, test, and fixture.
4. Run `node --test bench/test/operations-contracts.test.ts bench/test/operations-store.test.ts bench/test/operations-recovery.test.ts` from `harness/`.
5. Run `npm run bench:test` from `harness/`.
6. Run `npm run typecheck` from `harness/`.
7. Record exit codes and complete failure totals; distinguish environmental failures from product failures.
8. Remove the temporary copy and logs, verify their absence, and inspect final `git status`/`git diff`.
