# Operation Executor Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to
> implement this plan task-by-task. One implementer per lane, one task at a time, a review after
> every task.

**Goal:** Close every finding of the 20 Sep independent review of `feature/operation-executor`
(8426aaa7) and of the Rust hardening release on the fleet (05f63c02), each held by a test that
failed before the fix.

**Architecture:** Five lanes with disjoint file ownership, each in its own git worktree, run in
parallel; tasks inside a lane are serial because they share files. The four harness lanes branch
from `fix/opexec-review` (8426aaa7 plus this plan). The Rust lane branches from `desktop-login`
(0bc31e9d): the Rust is byte-identical on both branches and is what the fleet runs, so its fixes
must be shippable without the executor. Lanes merge back into `fix/opexec-review`, where the full
gate runs once.

**Tech stack:** TypeScript on `node:test` (two component files on vitest), Solid, Electron 42;
Rust workspace.

**Spec:** `docs/superpowers/specs/2026-09-20-operation-executor-independent-review.md`. Finding
ids below (C1, sched #4, R-C1, wiring F1 ...) are that document's. Behavioural authority:
`docs/superpowers/specs/2026-09-18-harness-operation-executor-design.md` and
`harness/bench/src/operations/CONTRACTS.md`.

**Why this plan carries no pre-written code:** the fixes land inside about 14,000 lines written by
another team. Code written here without reading every caller would be guesswork presented as
instruction. Each task instead names the exact lines, the behaviour required, the failing test to
write first, and the fix approach. The implementer reads the code and writes it; the reviewer
holds the result against this text.

## Global constraints

- Test first. Write the test, run it, see it fail for the stated reason, then fix. The report
  quotes the failing line and the passing line.
- New store and executor tests use the REAL `OperationStore` on a `mkdtemp` directory.
  `MemoryStore` is not used in any new test. 934 green tests missed both Criticals because of it.
- Nothing waits forever: every `await` added or touched has a bound or a cancel path.
- No new dependencies.
- Stay inside the lane's files (table below). If a fix needs another lane's file, stop and report.
- Never `git stash`. Stage by path. Commit only your own files. One commit per task. Subject in
  imperative sentence case; no attribution lines and no tool names (the commit-msg hook rejects
  them).
- Never launch the desktop app (`npm run start`, `electron .`): the owner's running instance holds
  the single-instance lock. Do not run `bench/test/renderer-boot.test.ts` locally.
- Never read or print a credential. Never touch the dev pod, the fleet, or another worktree.
- Harness gate per task: `npm run typecheck`, then `node --test` on the test files touched and
  their neighbours. Lane-end gate: `node --test --test-concurrency=4 bench/test/operations-*.test.ts`
  (`npm run test:operations` runs only 5 of the 17 operation test files; the store, executor,
  scheduler, recovery, capabilities and dispatch-authority suites are outside it). UI lane also
  runs `npm run test:components`. WIRING lane also runs `bench-tools.test.ts` and
  `proposals.test.ts`.
- Rust gate per task: `cargo clippy -p <crate> --all-targets -- -D warnings` and
  `cargo test -p <crate>`, with
  `CARGO_TARGET_DIR=/Volumes/kdisk/rustic-git/target CARGO_NET_GIT_FETCH_WITH_CLI=true`.
  Lane-end: `cargo test --workspace`. Check `df -h /Volumes/kdisk` first. Never delete a build
  directory.
- Comments say why, at the density of the surrounding file. When a contract changes, change
  `CONTRACTS.md` in the same commit.

## Lanes

| Lane | Worktree (under /Volumes/kdisk/rustic-git-wt) | Branch | Owns |
|---|---|---|---|
| CORE | opexec-core | fix/opexec-core | `harness/bench/src/operations/{store,state,contracts,dispatch-authority,recovery,executor,scheduler,capabilities}.ts`, `CONTRACTS.md`, their tests |
| WIRING | opexec-wiring | fix/opexec-wiring | `operations/adapters.ts`, `harness/pi/*`, `harness/bench/src/{bench,server}.ts`, `harness/package*.json`, `web/package.json`, `web/bun.lock`, `deploy/dev/pod/harness-gate.sh`, `bench-tools` / `proposals` / `renderer-boot` tests |
| MODEL | opexec-model | fix/opexec-model | `operations/{generation,judgments,typesafe,run-evaluation,evaluation,evaluation-subjects,shape}.ts`, their tests and fixtures |
| UI | opexec-ui | fix/opexec-ui | `harness/src/**`, the two vitest files, `renderer-queue.test.ts` |
| RUST | hardening-fix | fix/hardening-review | `crates/**`, `bins/**`, `tests/**`, `deploy/**` except `deploy/dev/pod/harness-gate.sh` |

## Rulings made while planning

Each is a decision the review left open. Recorded with what it costs if wrong.

1. **An approval binds to the pending decision's own revision, not to the snapshot's.** The
   pending decision's revision is fixed when the question is raised (state.ts:438) and replay
   already demands exactly that binding (store.ts:676). Making the pre-write checks agree with
   replay removes the corrupting mismatch at its root. What protects against approving a changed
   payload is the payload digest (store.ts:1442, 1550), which a sibling step cannot alter. Rejected
   alternative: freezing all commits while a decision is pending, which leaves the mismatch in
   place for any future committer. Cost if wrong: an approval stays grantable while unrelated
   steps progress, which is the intended behaviour for parallel plans.
2. **`expired` means nothing ran and nothing failed.** With a failed step and a passed deadline the
   operation settles through `settledState(snapshot, true)`, so the terminal state reports the
   failure. Cost if wrong: a label, never a hang.
3. **The fast suite reads the lock and never holds it; only a roll stops it.** (Corrected 20 Sep
   after R-1's first pass. The first version of this ruling had a fast run skip under any live
   holder.) On 16 Sep the owner had the fast suite stop yielding to the hourly (8cd61d51), because
   32 % of fast runs in six hours were standing aside; the shared lock silently reversed that. The
   lock exists to keep probes and a ROLL apart, not probes apart from each other. So: a fast run
   only GETs the lock; a live holder of kind `roll` makes it yield with a report naming the roll;
   a probe holder is ignored. `roll.sh` takes the lock BEFORE it waits for running probe jobs, so
   no probe can start in the gap, and it waits for a live probe holder instead of exiting. Every
   takeover is a preconditioned delete followed by the ordinary create, which needs only the
   `get`, `create` and `delete` the probe's Role already has. Cost if wrong: a fast sample taken
   beside an hourly, which the owner measured and accepted on 16 Sep.
4. **`bench.pkg.add` keeps the API mechanism and is made truthful; the shell variant stays parked.**
   The owner parked all shell work on 18 Sep, and the original pty path depends on it. The task
   stops the probe overwriting the package list, verifies the package is present in the pod after
   the PATCH, and corrects the catalogue and `deploy/slo.md` wording to what is measured. Restoring
   the shell path is an owner decision, listed at the end.
5. **No native confirmation dialog is added for approvals.** The existing proposal cards carry the
   same exposure and are the owner's chosen design. The task adds the sender-frame check and a
   test that pins the CSP (no inline or eval script). Residual risk is listed at the end.

6. **Recovery execution is not rebuilt here.** The executor team removed it deliberately as unsafe
   and guards that with tests. C-6 only makes `recover` report what it deferred. The open item goes
   to the owner and the executor team: until reconcile and abort execution exist, an operation
   that was mid-dispatch at a crash stays non-terminal after restart. Cost if wrong: none now; the
   gap is visible instead of silent.

7. **The in-memory test store is frozen, not rewritten.** Its 18 tests check call order and the
   absence of calls, which is what a recording fake is for. Store-dependent behaviour is tested on
   the real store by the tasks that fix it, and a ratchet stops the fake from spreading. Cost if
   wrong: an interaction test could still pass against a store call the real store would refuse;
   the real-store test beside it would fail.

---

## CORE lane

### C-1 Validate before the write (review C1, state M1)

**Files:** `store.ts` (`#apply` near 2095-2140, `#fold` 2026-2075), `state.ts` 428-445, store and
state tests.

**Required behaviour:** a commit that replay would refuse is refused BEFORE any byte is written,
with `OperationStoreError("validation_failure")`, and the store stays usable. If the post-append
fold ever throws, the store records it as `#failedAppend` so no later write lands on top. A clock
that steps backwards never produces an unloadable log.

**Tests first (real store):**
1. Reproduce path A as it exists today: two steps, step A `requireDecision`, then any sibling
   commit (`queueStep`), then `recordDecision` with `revision` equal to the current snapshot
   revision. Assert: it throws a store error, the log file's size is unchanged, a further valid
   commit succeeds, and a NEW `OperationStore` on the same directory constructs and loads the
   operation. Today this fails at the last assertion. (C-2 will change the expectation of the
   first assertion; keep the rest.)
2. Inject a clock that returns `t`, then `t - 5000`. The second commit succeeds,
   `snapshot.updatedAt >= previous.updatedAt`, and a new store on the directory loads it.
3. `state.ts`: `applyTransition` with `bumpRevision: false` and any body change returns a failure.

**Fix approach:** split the read-only validation half of `#fold` into a function both callers use.
Call it in `#apply` before `#append`. Wrap the post-append `#fold`: on throw set `#failedAppend`
and rethrow. Clamp `now` to `Math.max(input.now, previous.updatedAt)` for both the snapshot and
the record. In `state.ts` refuse a same-revision commit that changes the body.

**Commit:** `Refuse an invalid commit before it reaches the log`

### C-2 An approval survives concurrent progress (core I3, sched #3, ruling 1)

**Files:** `store.ts` 1437-1440, 1491-1500, `CONTRACTS.md`, store and executor tests.

**Required behaviour:** `recordDecision` requires `decision.revision === pending.revision`.
`resume` drops the `pending.revision !== snapshot.revision` refusal and compares
`request.expectedRevision` with `pending.revision`. Replay (store.ts:676) is unchanged and now
agrees with both. Payload digest, decision id, expiry, actor and turn checks are untouched.

**Verify by reading, and report:** in `executor.ts` there is no `await` between the `load()` at
line 130 and `requireDecision` at line 149, so the predicted `expectedRevision` is exact and
`waiting.revision` equals the pending decision's revision. If that is not true, stop and report.

**Tests first (real store, real executor):**
1. A plan with one approval-required write and one independent read. `approve` resolves only after
   the read has settled. The write dispatches and succeeds; a new store on the directory loads a
   terminal operation.
2. A decision whose `revision` differs from `pending.revision` is refused with
   `revision_conflict` and nothing is written.
3. A changed payload digest is still refused. A second record for the same `decisionId` is still
   refused.

**Commit:** `Bind an approval to the decision it answers`

### C-3 Every operation reaches a terminal state (review C2, sched #1, #4, #7, #8, ruling 2)

**Files:** `state.ts` 296, `store.ts` `expire` 1666-1735 and a new `skipStep`, `executor.ts`
(`ExecutorStore`, retry loop 166-176, settle at 184), `scheduler.ts` `#ready` 229-252, tests.

**Required behaviour:**
- The `deadline_reached` guard accepts `failed` as settled; `expire()` passes
  `settleFailed: true` to both `#apply` calls and to the trailing `settle`.
- The store gains `skipStep(operationId, stepId)` using the existing
  `queued -> skipped (dependency_failed)` edge (contracts.ts:717). The executor persists every
  scheduler-skipped key through it before `settle`.
- `#ready` propagates skips to a fixpoint before `#finish`, so plan order does not matter.
- An exhausted retry returns the failed result directly instead of recording `failed` twice; the
  provider's own error reaches the result.
- The retry loop stops when `signal.aborted`.

**Tests first (real store):**
1. Failed step plus queued dependent, clock past the deadline: `expire()` returns a terminal
   snapshot and a second `expire()` does not throw.
2. One success plus one failure, past the deadline: terminal, and not `expired`.
3. Calls listed `[C dependsOn B, B dependsOn A, A]` with A failing: `submit()` resolves, B and C
   are `skipped` in the durable snapshot, the operation is terminal.
4. A retryable failure with attempts exhausted: the recorded error is the provider's.

**Commit:** `Settle an operation whose step failed`

### C-4 Cancellation revokes; validation comes before revocation (core I1, I2, M2, sched #5, #11)

**Files:** `store.ts` 1111, 1156, 1311, `requestCancel` 1634-1660, `dispatch-authority.ts`,
`capabilities.ts` 982-989, `executor.ts` 99-105, tests.

**Required behaviour:**
- `requestCancel` and `expire` invalidate the dispatch token of every step they abort or cancel.
- In `recordStepOutcome`, `cancelStep` and the third site, `invalidate` runs after `#apply`
  returns, so a refused call leaves the live token intact.
- The executor's cancellation runs `requestCancel` in `try` and `controller.abort()` in `finally`,
  and sets `cancellationRecorded` only on success.
- `capabilities.ts` checks store ownership beside `consume`, and reports a handler throw after
  consumption as an unknown outcome with its own reason, not as "refused".

**Tests first (real store):**
1. `resume` issues a token, `requestCancel` follows, `consume` returns false, the handler never ran.
2. A refused `recordStepOutcome` (success without evidence) leaves the token consumable once.
3. A store whose `requestCancel` throws: the controller is still aborted and no queued step
   dispatches afterwards.
4. A handler that throws after `consume`: the result is `unknown_outcome`.

**Commit:** `Revoke dispatch authority when an operation is cancelled`

### C-5 Bounded waits in the executor (sched #2, #9, #12)

**Files:** `executor.ts` 108, 154, `scheduler.ts` 201, `contracts.ts` 531, tests.

**Required behaviour:** `approve(...)` is raced against the step's abort signal and against
`decision.expiryBound`; losing the race cancels the step and releases its slot and lanes. Timer
delays are clamped to 2^31-1 ms and re-armed. `selectOutputPath` refuses a negative or fractional
index.

**Tests first:** an `approve` that never resolves plus an abort: `execute()` returns and the
scheduler has no running entry. The same with the expiry bound and an injected clock. A deadline
30 days out does not fire at once.

**Commit:** `Bound the wait for an approval`

### C-6 Recovery never drops an action silently (core persistence gap, ruling 6)

**Files:** `executor.ts` `recover`, `recovery.ts` (read), executor tests.

**Scope, corrected 20 Sep.** The first version of this task asked `recover` to carry out
`reconcile_step`, `resume_abort` and `dispatch_step`. Reading the history showed the executor team
REMOVED recovery dispatch on purpose ("Remove unreachable recovery dispatch", after their own review
called it unsafe), and two existing tests assert that `recover` reconciles, retries and dispatches
nothing. Rebuilding that here would override a deliberate safety decision and is feature work on
their roadmap, not a review fix. What remains a defect is the silence: `recover` returns `void` and
ignores five of the seven action kinds, so a caller cannot tell that nothing happened.

**Required behaviour:** `recover` returns `{ handled: RecoveryAction[]; deferred: RecoveryAction[] }`.
`expire_decision` and `expire_operation` are handled as today. `await_decision`, `reconcile_step`,
`resume_abort`, `dispatch_step` and `retry_candidate` are returned in `deferred` with NO side
effect. The switch is exhaustive (a `never` check), so a new action kind fails to compile rather
than vanishing. `CONTRACTS.md` states plainly that recovery execution is not built: after a restart
an operation with a running or unknown step stays non-terminal until it is.

**Tests first:** every action kind lands in exactly one of the two lists; the two existing
"recover does nothing unsafe" tests still pass unchanged; an action for another operation still
throws.

**Commit:** `Report what recovery deferred instead of dropping it`

### C-7 The recording fake is frozen; store behaviour is tested on the real store (ruling 7)

**Files:** `harness/bench/test/operations-executor.test.ts`.

**Scope, corrected 20 Sep.** The first version asked for every executor test to move to the real
`OperationStore`. Reading the suite showed the 18 tests on the in-memory fake are INTERACTION
tests: they assert the order of store calls and the absence of calls, and they seed step states
directly, which the real store forbids. Rewriting them would be large and would risk weakening
them. The review's complaint was narrower: no store-dependent behaviour (approval, failure,
deadline, recovery) was tested against the real store. Tasks C-1 to C-6 add those tests.

**Required behaviour:** the fake is renamed `RecordingStore` with a header comment stating what it
is for (call order and absence of calls) and what it must never be used for (anything whose
correctness depends on the store accepting or refusing a transition). A ratchet test reads the test
file and fails if the number of `new RecordingStore(` occurrences exceeds today's count. One
real-store recovery test is added: a log left with a `running` step, a new store, `planRecovery`,
then `recover` — the action is reported as deferred and the durable state is unchanged. The test
file's header lists, for each of the four paths, the real-store test that covers it.

**Commit:** `Freeze the recording store and cover recovery on the real one`

---

## WIRING lane

### W-1 Platform adapters can be stopped, and find team workspaces (sched #6, #10, #13, wiring F1)

**Files:** `operations/adapters.ts`, `harness/pi/kloudlite.ts` (`call`, `resolveNamed` 972-978),
adapter and `bench-tools` tests.

**Required behaviour:** `PlatformCall` takes an `AbortSignal`; every platform adapter passes
`AbortSignal.any([input.signal, AbortSignal.timeout(n)])`. For a mutation, 502, 503 and 504 map to
`unknown_outcome`; other statuses keep today's mapping. The resolver lists with
`team=KL_TEAM` when it is set, and an id not in the listing is passed through as given so the
platform's own 404 is the answer. The GET-then-PATCH list edits get a `ponytail:` comment naming
the lost-update ceiling and the upgrade path.

**Tests first:** an adapter call whose platform never answers settles on abort and on timeout. A
504 on `workspace.create` yields `unknown_outcome`. With `KL_TEAM=acme`, `kl_workspace_start
{id: "ws-abc"}` for an id absent from the listing reaches the PATCH.

**Commit:** `Let a platform call be aborted, and pass an unlisted id through`

### W-2 A call that cannot run never asks the person (wiring F4, F5, F8)

**Files:** `harness/pi/kloudlite.ts` 1003-1014, 945, `harness/pi/catalog.ts`,
`bench-tools.test.ts` 1061-1066, `proposals.test.ts`.

**Required behaviour:** `kl_pkg_add` and `kl_pkg_rm` with no workspace answer the "name the
workspace" sentence before any proposal is raised; the original assertion is restored. Both get an
`ask:` line naming the packages and the workspace. `kl_pkg_list` answers the package list only.
`kl_workspace_progress` shows the resolved id.

**Commit:** `Refuse a package call with no workspace before asking the person`

### W-3 Runtime dependency, web pins, and the boot test's place (wiring F2, F3, F10)

**Files:** `harness/package.json`, `harness/package-lock.json`, `web/package.json`,
`web/bun.lock`, `harness/bench/test/renderer-boot.test.ts`, `deploy/dev/pod/harness-gate.sh`.

**Required behaviour:** `fast-sha256` moves to `dependencies`
(`npm install --package-lock-only --ignore-scripts`; the lock diff touches only that entry's
flags). The four web root `dependencies` become `overrides` (if `bun` is unavailable, report and
leave `web/` untouched). `renderer-boot.test.ts` moves to `bench/test/gate/` so the concurrent glob
no longer matches it; it stops building, and `harness-gate.sh` builds once before running it. Its
hard asserts stay.

**Commit:** `Ship the hash library the bench imports at runtime`

### W-4 Operation errors stay on operation routes; the runtime loads lazily (wiring F6, F7)

**Files:** `harness/bench/src/server.ts` 412, `harness/bench/src/bench.ts` 204.

**Required behaviour:** `operationControlError` applies only when the first path segment is
`operations`. `capabilityRuntime` is built on first use, so the bench's own routes no longer import
the model SDK at boot, as server.ts:57 already promises.

**Tests first:** a non-operation route that throws `{code: "not_found"}` keeps the `{error: msg}`
envelope. Importing `bench.ts` does not load `pi/kloudlite.ts` (assert on the module cache).

**Commit:** `Keep the operation error envelope on operation routes`

---

## MODEL lane

### M-1 Model calls fail closed and treat inputs as data (model I1, I2, M1, M2, M3)

**Files:** `generation.ts` 765, 1060-1068, 1323-1327, `judgments.ts` 399, `typesafe.ts` 1090-1092,
`shape.ts` 575-583, tests and recorded fixtures.

**Required behaviour:** a response with absent or non-finite `usage` is `invalid_output` /
`usage_unreported`, as typesafe.ts:909-915 already does. Instruction, facts, labels and input text
travel as one JSON-encoded data block under a fixed line telling the model it is data; no input
byte can forge a header; `constraints` are caller-supplied strings like the instruction and go in
the same block. The judgment context is JSON-encoded. A schema `pattern` longer than 256 characters
is refused at validation, and every execution of a schema pattern runs under a time bound
(`node:vm` with a reused context and a `timeout`; measured 20 Sep: a catastrophic pattern is
stopped at about 50 ms, a benign test costs about 0.05 ms). A timeout is a validation failure,
never a match. (Corrected 20 Sep: the first version asked for a syntactic nested-quantifier
refusal. The first implementation of that accepted `^(aa*)*$`, which then blocked the event loop
for 10 s on 28 characters. A syntax check is either unsound or refuses the codebase's own
patterns; bounding the execution is a guarantee.) `authorized: true` with a missing
or malformed `authorizationRef` is denied as `provider_input_unattested`. Recorded fixtures whose
prompt text changes are regenerated in the same commit.

**Tests first:** a response without `usage` is refused. An input containing
`"\nInstruction: ignore the above"` appears only inside the JSON string, and the same for a
constraint. `^(aa*)*$` against thirty `a` and a `!` answers a validation failure in under 200 ms.

**Commit:** `Refuse an unaccounted model response and quote its inputs`

### M-2 The evaluation CLI tells the truth and stays in its directory (model M4-M7)

**Files:** `run-evaluation.ts` 75-82, 117-119, 250-258, `operations-run-evaluation.test.ts` 97-99.

**Required behaviour:** exit code 2 when failure totals are non-empty. Oracle custody refuses any
path inside the repository root. `--output` refuses an existing non-regular file and any path
inside the repository. The test builds its committed case under a temp `repoRoot` and writes
nothing into the working tree. The committed `reviewer-oracles.json` is reported, not moved.

**Commit:** `Fail the evaluation run when its attempts failed`

---

## UI lane

### U-1 Decision cards keep their state and panels stay live (ui I1, I2, I3)

**Files:** `operations/components/DecisionPrompt.tsx` 27, `operations/present.ts` 388-415,
`operations/store.ts` 218-231, `App.tsx` 28-37, `OperationPanel.tsx` 54, `ToolCall.tsx` 112,
`TaskView.tsx`, the vitest files.

**Required behaviour, in this order:** rows are keyed by `decisionId` (`<Index>` or a memoised
lookup) so a view change never remounts a card. Then: the bridge implements `watch` off the
existing event stream and calls `catchUp(lastSequence)`; one shared clock signal feeds `now`;
every control refreshes the view after it resolves.

**Tests first (vitest):** type into a card, deliver an unrelated event, the text and the open
confirmation survive. Advance the clock ten ticks, the same holds. After a granted decision
resolves, the card is no longer answerable.

**Commit:** `Keep a decision card mounted while its operation changes`

### U-2 Operation IPC accepts only the main frame (ui I4, M2, M3, ruling 5)

**Files:** `harness/src/main.ts` 346-371, `index.tsx` 23, `renderer/index.html`, tests.

**Required behaviour:** every `operations:*` handler refuses a sender that is not the main
window's main frame. `operations:input` validates `stepId` as `operations:decision` does.
`KL_BOOT_TEST` is honoured only when `!app.isPackaged`. A test pins the CSP: no `unsafe-inline`
and no `unsafe-eval` for scripts.

**Commit:** `Accept operation controls only from the main frame`

### U-3 An operation's view belongs to its session (ui I5, I6, I7, M5)

**Files:** `App.tsx` 376, `Chat.tsx` 434-442, `inspector/Inspector.tsx` 54-57,
`operations/store.ts` 137, 241-248, `ToolCall.tsx` 54.

**Required behaviour:** the open operation is stored with its session and rendered only in that
session's tab; `afterDelete` and archive clear it. `archiveSession` disposes the session's
projections. `ToolCall` opens its projection from `onMount` and releases it in `onCleanup`. Chat
and the inspector use the same owner keys. An owner-mismatch view carries no live controls.

**Tests first:** open an operation in session A, select session B: B shows none. Archive A: its
projections are gone. The inspector lists an operation opened from a session tab.

**Commit:** `Show an operation only in the session that owns it`

### U-4 Fixtures leave the bundle; the dialog covers the window (ui M1, M4, M6)

**Files:** `operations/index.ts` 23-33, `ui/Confirm.tsx`, `main.ts` 74 (read only).

**Required behaviour:** the barrel no longer re-exports `fixtures/scenarios.ts`; tests import it
directly. `Confirm` renders through a `Portal` or `fixed`. For `loadFile`: read
`tsconfig.main.json` and the vite `outDir` and state in the report whether `__dirname/../renderer`
resolves to the built `index.html`; change nothing if it does.

**Commit:** `Keep test scenarios out of the shipped bundle`

---

## RUST lane

### R-1 A dead holder never blocks the fleet (review R-C1, Rust M1b, ruling 3)

**Files:** `bins/slo/src/coordination.rs`, `ctx.rs` 212-232, `main.rs` 116-121, `deploy/roll.sh`
40-60, `deploy/kloudlite.yaml` (the dead `KLOUDLITE_SLO_COORDINATION` env),
`bins/slo/tests/out_of_process.rs`.

**Required behaviour:** the lock ConfigMap carries an ownerReference to its holder pod when there
is one (`blockOwnerDeletion` false), and records `kind` (`roll` or `probe`) and `suite`. On
AlreadyExists, `acquire` reads `owner_pod_uid` / `job_uid` and, when that pod or job is gone or
terminal, takes over by a delete preconditioned on the lock's uid and resourceVersion followed by
the ordinary create; never a PUT (the Role has no `update`, and needs none). The FAST suite never
acquires: it GETs the lock, yields with a report naming the holder only when a LIVE holder of kind
`roll` has it, and otherwise runs (ruling 3). `roll.sh` takes the lock first and only then waits
for running probe jobs; a live probe holder makes it wait inside the existing two-hour bound, a
live roll holder makes it `exit 3` naming the holder, a dead holder is taken over the same way.
The unread env and the test that sets it to "0" are removed. No RBAC change.

**Tests first:** against the fake API server in `out_of_process.rs`, passing under the default
parallel test runner: a lock whose owner pod is absent is taken over by delete-then-create and the
stub saw no PUT; a lock whose owner is live is respected; a fast run under a live HOURLY lock runs
normally and creates no lock; a fast run under a live ROLL lock exits 0 having reported a yield
naming the roll.

**Commit:** `Take over a roll lock whose holder is gone`

### R-2 A failed push leaves no pin behind (Rust I1, I2)

**Files:** `crates/registry/src/manifests.rs` (`put_manifest`), `blob_state.rs`, registry tests.

**Required behaviour:** any early return after the first pin unpins what was pinned. Each pin
records `pinned_at`; the sweep ignores a pin older than the grace period. CAS retries back off with
jitter. Pins and unpins run through `buffered(STAT_CONCURRENCY)`. The load-bearing rules in
`CLAUDE.md` hold: only two things delete a blob; manifest bytes verbatim.

**Tests first:** a manifest PUT whose third pin fails leaves zero pins. Eight concurrent pushes
sharing a base layer all succeed. A pin older than the grace period does not protect its blob.

**Commit:** `Release blob pins when a manifest push fails`

### R-3 The sweep is cheap when idle and patient with readers (Rust I3, I4, M1a, M2)

**Files:** `crates/registry/src/gc.rs`, `blob_state.rs`, `tests/registry_gc.rs`.

**Required behaviour:** age is judged from the record's `installed_at`, with no HEAD per blob. A
record with no active generation and no retired ones is deleted with its read version as the
precondition. A retired generation is deleted only after the grace period, by `retired_at`.
`InstallError::Busy` and its three match arms go. The weakened assertion is restored: the test
captures the active key before the sweep and asserts `head(key).is_err()` as well.

**Commit:** `Give a retired blob generation a grace period`

### R-4 Probes and the outbox say what they do (Rust I5, I6, M3, M4, ruling 4)

**Files:** `bins/slo/src/stages/bench_ws.rs` 127, `crates/workspaces/src/slo/catalogue.rs`,
`deploy/slo.md`, `crates/workspaces/src/history/outbox.rs`, `crates/ide/src/fs/git.rs`.

**Required behaviour:** `bench.pkg.add` reads the current package list and appends, never
overwrites; after the PATCH it verifies in the pod that the package is present; its catalogue
entry and `deploy/slo.md` describe the API path (the test that holds the two equal must pass). The
outbox drainer stops listing at `DRAIN_BATCH` and backs off on error. `git.rs` uses `TREES_DIR` on
both paths and reads a symlink target of any length.

**Commit:** `Describe the bench package probe as what it measures`

---

## Integration

1. Merge `fix/hardening-review`, then the four harness lanes, into `fix/opexec-review`, one at a
   time, never chained. Conflicts are not expected: file ownership is disjoint.
2. Full gate in the dev pod in its own checkout (`/work/opexec-fix`, never `/work/src`, which
   belongs to the executor team): `deploy/dev/pod/harness-gate.sh`, and
   `cargo clippy --workspace --all-targets -- -D warnings` with `cargo test --workspace`.
3. One whole-branch review of `8426aaa7..fix/opexec-review`.
4. Hand over. Merging into `feature/operation-executor`, and shipping `fix/hardening-review` to
   the fleet, are the owner's calls.

## Owner decisions this plan does not take

- Building recovery execution (reconcile, abort, re-dispatch after a restart). It was removed as
  unsafe; until it exists a crash mid-dispatch leaves an operation non-terminal (ruling 6).
- Restoring the shell path of `bench.pkg.add` (shells were parked on 18 Sep).
- Whether a granted write or destroy should also be confirmed by a native dialog (ruling 5).
- Where the held-out `reviewer-oracles.json` should live; it is committed in the tree today.
- Whether the Electron 39 to 42 jump, which no commit subject mentions, was intended.
