# O06 Final Review Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the final scheduler and executor review findings without persisting recovery payloads or permitting restart-time dispatch.

**Architecture:** Recovery becomes conservative and executes only reconciliation and expiry actions. Live execution owns retry limits and a composed cancellation signal, while scheduler validation gains typed open-output support and cancellation outcomes remain adapter-authoritative.

**Tech Stack:** TypeScript, Node test runner, O02 capability registry, O05 operation store, O06 scheduler/executor.

---

### Task 1: Make Recovery Non-Dispatching

**Files:**
- Modify: `harness/bench/src/operations/executor.ts`
- Test: `harness/bench/test/operations-executor.test.ts`

**Step 1: Write failing tests**

Replace the mixed recovery test with assertions that `reconcile_step`, `expire_decision`, and `expire_operation` still write, while `dispatch_step`, `retry_candidate`, and `resume_abort` perform zero registry dispatches and do not call `startStep`, `retryStep`, or `cancelStep`.

**Step 2: Verify RED**

Run: `cd /work/o06-final/harness && node --test bench/test/operations-executor.test.ts`

Expected: failures showing queued and retry recovery dispatch through the registry.

**Step 3: Implement the minimum behavior**

In `OperationExecutor.recover`, continue immediately for `await_decision`, `dispatch_step`, `retry_candidate`, and `resume_abort`. Keep ownership checks, reconciliation, and expiry behavior. Move call/descriptor lookup below the non-dispatch guards so deferred actions need no trusted call reconstruction.

**Step 4: Verify GREEN**

Run the focused executor test again. Expected: all executor tests pass.

**Step 5: Commit**

Commit message: `Defer unsafe operation recovery dispatch`

### Task 2: Settle Retry Exhaustion

**Files:**
- Modify: `harness/bench/src/operations/executor.ts`
- Test: `harness/bench/test/operations-executor.test.ts`

**Step 1: Write the failing test**

Add an idempotent capability test whose first and second attempts both return retryable failures with `maxAttempts: 2`. Assert exactly two dispatches, one authorized retry, a failed durable step, and a failed executor result rather than a thrown retry authorization error.

**Step 2: Verify RED**

Run the executor test. Expected: rejection from `retryStep` after the second failed attempt.

**Step 3: Implement the minimum behavior**

After each retryable failure, record it. Read the durable step and retry only while `attempts < descriptor.retry.maxAttempts`; otherwise leave the final failure recorded and return it through `#record` without recording it twice.

**Step 4: Verify GREEN**

Run the executor test. Expected: all pass.

**Step 5: Commit**

Commit message: `Settle exhausted operation retries`

### Task 3: Persist Deadline Cancellation Intent

**Files:**
- Modify: `harness/bench/src/operations/executor.ts`
- Test: `harness/bench/test/operations-executor.test.ts`

**Step 1: Write the failing test**

Add a running mutation with a near durable deadline. Hold dispatch until its signal aborts, return an inconclusive provider failure, and assert `requestCancel` occurs before unknown-outcome recording. Retain the already-expired deadline test for `expire()` plus scheduler rejection.

**Step 2: Verify RED**

Run the executor test. Expected: deadline aborts scheduler work without `cancel-requested` in the store log.

**Step 3: Implement the minimum behavior**

Create one executor `AbortController`. Forward caller abort into a helper that records cancellation intent once and aborts the controller. Set a deadline timer that invokes the same helper. Pass the controller signal to the scheduler and clear the timer/listeners in `finally`. Keep scheduler `deadlineAt` for queue enforcement and persist `expire()` when the deadline rejection escapes.

**Step 4: Verify GREEN**

Run the executor test. Expected: all pass and listener/timer cleanup remains covered.

**Step 5: Commit**

Commit message: `Persist operation deadline cancellation`

### Task 4: Preserve Read Failures During Abort Races

**Files:**
- Modify: `harness/bench/src/operations/executor.ts`
- Test: `harness/bench/test/operations-executor.test.ts`

**Step 1: Write the failing test**

Add a read whose adapter returns `provider_failure` after the signal aborts. Assert the durable step is failed with the provider error and no `cancelStep` call or synthetic evidence occurs.

**Step 2: Verify RED**

Run the executor test. Expected: current code records cancellation.

**Step 3: Implement the minimum behavior**

In `#record`, enter cancellation handling only for `error.code === "cancelled"`. Mutation failures observed after signal abort remain unknown/reconciling because their effect may have happened; read failures remain their authoritative adapter failure. Continue requiring explicit refs before cancelling a mutation.

**Step 4: Verify GREEN**

Run the executor test. Expected: all pass.

**Step 5: Commit**

Commit message: `Preserve authoritative read failures`

### Task 5: Accept Typed Open-Object Outputs

**Files:**
- Modify: `harness/bench/src/operations/scheduler.ts`
- Test: `harness/bench/test/operations-scheduler.test.ts`

**Step 1: Write failing tests**

Add a source output schema with `type: "object"` and schema-valued `additionalProperties`. Assert a compatible named output binding validates and an incompatible target type fails. Also assert `additionalProperties: true` remains invalid because it supplies no type.

**Step 2: Verify RED**

Run: `cd /work/o06-final/harness && node --test bench/test/operations-scheduler.test.ts`

Expected: compatible open-output binding is rejected as undeclared.

**Step 3: Implement the minimum behavior**

Resolve the binding's initial output schema from an explicit property first, then schema-valued `additionalProperties`. Feed that schema into `selectedSchema` and existing assignability checks. Do not treat boolean `true` as a typed schema.

**Step 4: Verify GREEN**

Run the scheduler test. Expected: all pass.

**Step 5: Commit**

Commit message: `Validate typed open operation outputs`

### Task 6: Final Verification and Review

**Files:**
- Verify all modified operation files and tests.

**Step 1: Run affected suites**

Run:

```sh
cd /work/o06-final/harness
node --test bench/test/operations-executor.test.ts bench/test/operations-scheduler.test.ts bench/test/operations-store.test.ts
npm run typecheck:operations
```

Expected: zero failures and zero type errors.

**Step 2: Run repository checks**

Run: `git -C /work/o06-final diff --check && git -C /work/o06-final status --short`

Expected: no whitespace errors and only intended files modified.

**Step 3: Run the broad harness suite**

Run: `cd /work/o06-final/harness && npm run bench:test`

Expected: all non-Electron tests pass. If `renderer-boot.test.ts` fails solely because the pod has no X server or `$DISPLAY`, report that environmental limitation exactly.

**Step 4: Commit remaining verification-only changes if any**

Do not commit generated output. Use an imperative sentence-case subject.

**Step 5: Request independent review**

Review the range from `7c67ef18720bff2f7f88a0dcd9364e161b596e39` to the new head, with findings ordered by severity. Do not push or merge.
