# Independent review: feature/operation-executor at 8426aaa7

Range reviewed: 695f8484..8426aaa7 (72 commits, +40,696 / -1,505, 253 files).
Method: six read-only reviewers, one per area, each required to quote the lines it relied on.
The two Critical findings were re-read against the source by the coordinating reviewer and hold.
Status: all six areas complete. Three Critical claims were raised; all three were re-read against
the source by the coordinating reviewer. Nothing was executed; every finding comes from reading code.

## Verdict: NOT APPROVED

Two Critical defects in the state core, and a family of "operation never terminates" defects that
two reviewers reached independently from opposite sides (state core and scheduler). 934 green
tests did not see them because the executor tests run against an in-memory fake store
(`MemoryStore`), not the real one.

## Critical

### C1. Replay validation runs after the commit is fsynced (store.ts:2137-2138) [verified]
`#apply` calls `#append` and only then `#fold`. `#fold` runs `replayProblem`, which is stricter
than `applyTransition`, and a fold failure does not set `#failedAppend`, so the bad frame stays on
disk and later writes land on top of it.
- Path A: `recordDecision` accepts `decision.revision === snapshot.revision` (store.ts:1437), but
  fold requires `pending.revision === decision.revision` (store.ts:676). `pending.revision` is
  fixed at creation (state.ts:438). Any sibling commit between the prompt and the answer
  (`consumeBudget`, `queueStep`, `recordBackendOperationId`, another step settling) makes a legal
  approval write a frame that fold rejects. On next start `#scan` throws on that file and the
  store constructor fails, so every operation in the bench is unavailable.
- Path B: `Date.now()` stepping backwards (NTP) writes `updatedAt: now` (state.ts:445); fold
  rejects `next.updatedAt < previous.updatedAt` (store.ts:621) after the append.
Fix: run `replayProblem` and the sequence checks before `#append`; treat any fold failure as
`#failedAppend`; clamp `now = Math.max(now, snapshot.updatedAt)`.

### C2. A failed step plus a passed deadline is non-terminal forever (state.ts:296, store.ts:1692-1732) [verified]
`TERMINAL_STEP_STATES = ["succeeded","skipped","cancelled"]` (contracts.ts:681) excludes `failed`.
`expire()` computes `blocked` without looking at `failed` (store.ts:1692), requests `expired`, and
the `deadline_reached` guard then refuses because not every step is terminal. It throws
`invalid_transition` on every later `expire()`; executor.ts:184 and :197 call the same path. With
one success and one failure the operation parks in `cancel_requested` because neither `#apply` nor
the trailing `settle()` passes `settleFailed`.
Fix: pass `settleFailed: true` in both `#apply` calls in `expire()` and in the trailing `settle`;
make the guard accept `failed` as settled; add a test (failed step + queued step, advance past the
deadline, assert a terminal state) against the REAL store.

## Important, grouped by root cause

### A. A pending approval does not survive concurrency
- store.ts:1493 refuses `resume` when `pending.revision !== snapshot.revision`; nothing re-stamps
  it. Two parallel steps, one awaiting approval: the other settling makes the approval ungrantable
  until its TTL cancels it.
- executor.ts:130,142,156 computes `expectedRevision` from the snapshot, so an independent read
  running during the prompt makes `recordDecision` throw `revision_conflict`; the approved write is
  reported failed and the durable step stays `awaiting_approval`. Only single-step plans are tested.
Fix: bind the decision to `decisionId` + `payloadDigest` (already checked at store.ts:1442, 1550)
and drop the revision equality; or treat an approval-required step as operation-exclusive.

### B. Things that wait forever
- scheduler.ts:229-250: skip propagation is single-pass. Calls listed [C->B, B->A, A] with A
  failing leaves C without a result; `#finish` returns, `#pump` breaks, `submit()` never resolves.
  `validateSchedulePlan` does not require topological order. Fix: iterate skips to a fixpoint.
- executor.ts:154: `await approve(prepared.approval)` takes no signal and is not raced. An
  unanswered prompt holds a global `maxConcurrent` slot, a mutation slot and its lanes forever;
  `maxConcurrent` unanswered prompts stall every operation. Fix: race against `signal` and
  `decision.expiryBound`.
- adapters.ts:26-28, 89-127: `PlatformCall` has no signal and no timeout; no platform adapter reads
  `AdapterInput.signal`. Cancel and deadline cannot stop a stalled call. Fix: add `signal`, pass
  `AbortSignal.any([signal, AbortSignal.timeout(n)])`.
- Skips live only in scheduler memory: `ExecutorStore` has no skip method although contracts.ts:717
  defines `queued -> skipped (dependency_failed)`. After a dependency fails the durable step stays
  `queued` and `settle` cannot settle. Fix: add `skipStep` and call it before `settle`.
- `planRecovery` has no production caller; `executor.recover` handles only expiry actions, so
  `reconcile_step`, `resume_abort` and `dispatch_step` are dropped after a restart.

### C. Cancellation does not reliably stop a dispatch
- store.ts:1634-1660: `requestCancel` records `abortRequestedStepIds` but never calls
  `dispatchAuthority.invalidate`. A token issued by `resume` still authorizes the mutation after a
  recorded cancel.
- store.ts:1111, 1156, 1311: `invalidate` is the FIRST statement, above validation. A rejected
  call (success without evidence, wrong stepId) kills the live token; the step stays `running`
  with no outcome and also blocks expiry. Fix: invalidate after `#apply` returns.
- executor.ts:99-105: a throwing `requestCancel` prevents `controller.abort()`, and the
  `cancellationRecorded` flag is already set so the deadline timer cannot retry. Fix: `try/finally`.

### D. Renderer
- DecisionPrompt.tsx:27: `decisionRows()` builds new row objects on every call and `<For>` keys by
  reference, so every view change remounts every Decision card, dropping `answer`, `confirming`
  and the `pending` double-submit guard. This is the same class as the 18 Sep accordion bug and
  becomes a 500 ms reset the moment a ticking `now` is wired. Fix: `<Index>` or rows memoised by
  `decisionId`.
- App.tsx:28-37: the bridge has no `watch` and nothing passes `now`. A running operation's panel
  stays on its first snapshot until reconnect; expiry labels and the stalled note never update.
- App.tsx:34-36: nothing refreshes after a control resolves, so a granted decision still shows as
  answerable and a second click re-submits it.
- main.ts:346-371: every `operations:*` handler ignores the sender. Argument validation is sound,
  but `outcome: "granted"` is accepted from any frame with no proof of a click; Chat renders model
  HTML and only the CSP stands in between. Fix: check `senderFrame`, and for write/destroy effects
  confirm in main or require a one-shot nonce minted when the decision is delivered.
- Chat.tsx:434,442 vs Inspector.tsx:54-57: panels open with the tab id as `workspaceId`, the
  inspector looks up by workspace id or "bench", so "Background operations" is empty for anything
  opened from a session tab.
- App.tsx:376: `operationTask` is one app-level signal. Switching tabs shows session A's operation,
  with live Approve/Cancel, in session B. `afterDelete` never clears it.
- store.ts:248: `archiveSession` is a no-op; projections leak for the life of the app, and
  ToolCall.tsx:54 calls `open()` inside a `createMemo` with no release on unmount.

### E. Model-facing half
- generation.ts:1323-1327: missing `usage` is read as zero, so the token ceiling fails open and
  spend is recorded as free. typesafe.ts:909-915 already refuses this case (`usage_unreported`).
- generation.ts:1060-1068: untrusted input bytes, the instruction and labels are concatenated
  under plain `Instruction:` / `Input:` headers. Output is schema-validated and never executed, so
  the exposure is steering a proposed edit, not a grant. Fix: JSON-encode inputs as data.

## Minor (one line each)
- executor.ts:167-175: exhausted retry records `failed` twice; the real store throws
  `invalid_transition` and the provider error is replaced by a generic one. Hidden by the fake store.
- executor.ts:108 / scheduler.ts:201: `setTimeout` with a delay above 2^31-1 ms fires at once.
- adapters.ts:23,33: a 502/503/504 on a mutation is recorded failed, not unknown; reconcile never runs.
- capabilities.ts:988-989: a handler throw after the token was consumed is reported as "refused".
- contracts.ts:531: negative or fractional path indices are admitted (caught downstream).
- adapters.ts:95-127: service/package edits are GET-then-PATCH of the whole list; lanes only
  serialise within one scheduler.
- state.ts:428: a same-revision commit may carry body changes; only the post-write fold catches it.
- dispatch-authority.ts:44-52: `consume` does not check the store is still owned or open.
- generation.ts:765: request-supplied regex runs over model text with no length/nesting cap.
- typesafe.ts:1090-1092: `authorized: true` with a missing `authorizationRef` still dispatches.
- run-evaluation.ts:250-258 exit code is 0 whatever the failure totals; :75-82 oracle custody
  check admits paths inside the repo; :117-119 `--output` is unconstrained.
- operations-run-evaluation.test.ts:97-99 writes a `.mjs` into the working tree.
- operations/index.ts:23-33 re-exports fixtures, so 1168 lines of scenarios ship in the bundle.
- index.tsx:23: `KL_BOOT_TEST` skips the login gate in a packaged build; guard with `!app.isPackaged`.
- main.ts:74 changed the renderer `loadFile` path; confirm on a packaged build.
- store.ts:137: an "owner mismatch" view still carries live controls.
- Confirm.tsx: `absolute inset-0` inside a `relative` card covers the card, not the window.

## Checked and found sound
- The dispatch token: one-shot (deleted before comparison), attempt-bound, held in a WeakMap and
  never persisted, absent after restart; a `running` step goes to reconcile, never to dispatch.
- No model output reaches an authorization decision; the judgment and generation modules are not
  wired to dispatch in this range. Parsing is strict and fail-closed. No secrets in the range.
- File handling: framed length + sha256, torn tail truncated and fsynced, tmp + fsync + rename +
  directory fsync for rewrites.
- Scheduler: cycle and unknown-dependency detection, lane exclusivity with no check-then-act,
  double-settle guard, timers and listeners cleared.
- Bindings cannot smuggle an unvalidated value past approval (re-validated and digested).
- reduce.ts is idempotent by sequence; reconnect replay cannot apply an event twice.
- No TDZ-at-mount regression; nothing writes operation chatter into the transcript.

## Rust hardening (already pinned on the fleet as 05f63c02)

### R-C1. The roll lock never expires and nothing can take it over [verified in source]
`bins/slo/src/coordination.rs:53` is a bare `api.create(...)`; any error, including AlreadyExists,
fails `Ctx::new`. The ConfigMap carries no ownerReference, so Kubernetes will not collect it when
its holder dies. `deploy/roll.sh:40` does the same with `kubectl create` and `exit 3`. Every suite
and every roll share the one name.
- A holder that is OOM-killed, hits `activeDeadlineSeconds`, or a SIGKILLed `roll.sh` leaves the
  lock behind; every probe and every roll is then refused until someone deletes it by hand.
- The hourly owner holds it for up to 3300 s (`wait_for_group`), so once the schedules are resumed
  every fast run in that window fails at `Ctx::new` with `EXIT_CONFIG` before any report is filed.
- Live state checked 20 Sep: no lock present, all four CronJobs suspended. So this is not firing
  today; it becomes an outage the day the schedules are un-suspended.
Fix: on AlreadyExists read `owner_pod_uid` / `job_uid` and take over when that pod or job is gone
or terminal (the RBAC is already granted); give the fast suite its own lock name, or have it skip
and report rather than fail construction; set an ownerReference to the holder pod.

### Rust, Important
- `crates/registry/src/manifests.rs` (`put_manifest` pin loop): an early `return` on a pin error
  never unpins the layers already pinned; the post-write unpin only logs on failure; the
  publication id carries a fresh nonce so a retry never clears the old pin; and the sweep skips
  anything pinned. Result: an uncollectable blob. Reachable today because `pin` is 8 CAS attempts
  with no backoff, so concurrent pushes sharing a base layer can exhaust it. Fix: unpin on the
  early return, store `pinned_at` and let the sweep ignore pins past the grace period, add jitter.
- Same function: pins and unpins run serially (GET + conditional PUT each), about 160 sequential
  round trips for a 40-layer manifest where the old code did 16-wide concurrent HEADs. Fix:
  `buffered(STAT_CONCURRENCY)`.
- `crates/registry/src/gc.rs` `sweep_owner`: every tick lists `blobs/{owner}`, GETs every state
  record and HEADs every unpinned active blob before the empty early return (was one LIST).
  Records with `active: None, retired: []` are never deleted, so cost grows with every blob ever
  pushed.
- `gc.rs`: retired generations are deleted on the next sweep with no grace period; a GET that
  resolved the old physical key a moment earlier answers `BLOB_UNKNOWN` for a blob that exists.
- `bins/slo/src/stages/bench_ws.rs:127`: `bench.pkg.add` was a pty `kl pkg add` in the bench shell
  and is now a `/v1` PATCH with the probe's own JWT. The projected workspace token, `kl` and the
  shell path are no longer exercised under that id, so it can pass with all three broken, and the
  PATCH overwrites the package list. A probe that passes hollow.
- `crates/workspaces/src/history/outbox.rs`: full LIST of `history-outbox/` every 2 s, another
  every 30 s; unbounded with ClickHouse down.

### Rust, Minor
- `InstallError::Busy` is never constructed; `KLOUDLITE_SLO_COORDINATION` is set in yaml and read
  nowhere, and a test sets it to "0", implying an opt-out that does not exist.
- `tests/registry_gc.rs`: one assertion weakened from `head(blob_path).is_err()` to
  `blob_state::resolve(...).is_none()`, which proves the record was retired, not that bytes are gone.
- `crates/ide/src/fs/git.rs`: `b".agents"` hard-coded on the unix path; a symlink target over
  4096 bytes is silently truncated.
- Reviewer's M5 (hourly template missing `KLOUDLITE_POD_UID`) checked and NOT a defect: all four
  templates in `deploy/kloudlite.yaml` set it.

### Rust, sound
Only two things still delete a blob, and the push-vs-GC race is closed with a real CAS
(`retire_if_unpinned` checks the version then updates with it as precondition). Keep-bias holds.
Credential admission is fail-closed (503 when the check is absent) and authorizes on `spec.owner`.
IDE confinement: `confine` first, `openat` + `O_NOFOLLOW` per component, `--setenv` for env,
optional binds skip-if-absent, `/etc/kloudlite` unbound with a test. Checkout recovery never
touches an existing destination; the chown is non-recursive and does not follow symlinks.
The dev volume change is an in-place PVC expand.

## Harness wiring and dependencies

### Is the executor live?
Dormant. The scheduler, executor, recovery, generation and TypeSafe modules are imported by
nothing in the running bench. The five `/operations/*` routes answer 503 unless
`KL_OPERATION_CONTROL_MODULE` is set, and nothing in `deploy/`, `crates/` or `bins/` sets it.
The model gets no new tool. No path bypasses the proposal card today.
What IS live: the shared dispatch path. `kl_workspaces`, `kl_workspace`, `kl_pkg_*`,
`kl_intercept`, `kl_environment_service_*`, `skill` and `kl_workspace_progress` now run through
`operations/adapters.ts`. So the adapter defects above (no abort signal, no timeout, 5xx read as
definitive failure) are in the production path even though the executor is not. Also new: the
desktop now sends the person's session token to the bench process (not logged).

### Wiring, Important
- `pi/kloudlite.ts:972-978` (`resolveNamed`): the old pass-through ("not in the listing: hand it
  on as given, so /v1's own 404 is the answer") was deleted and replaced by `no_match`. The
  resolver lists `/v1/workspaces` with no `team`, and an absent team means personal. A team bench
  addressing a team workspace by id now gets "no workspace ws-abc" for start, stop, snapshot,
  clone, delete, progress, and the same for environments. Fix: on `no_match` return the id as
  given, or list with `team=KL_TEAM`. (Not confirmed whether `ws_for_owner` returns team
  workspaces for a team-scoped token; the finding rests on `list_ws`, mod.rs:425-429.)
- `package.json:48`: `fast-sha256` is a devDependency but is imported at runtime
  (bench.ts -> capabilities.ts -> contracts.ts -> canonical.ts), and the bench image installs with
  `npm ci --omit=dev`. It works today only because pi's `standardwebhooks` hoists the same
  package. A pi bump can crash every bench pod at import, and the gate (full `npm ci`) would not
  catch it. Fix: move it to `dependencies`.
- `renderer-boot.test.ts:19` now runs `npm run build` inside the concurrent suite and the gate
  runs the file a second time; it rewrites `dist/` while other tests read it. All three `t.skip`
  branches became hard asserts, so `bench:test` on a laptop with no display, or while the desktop
  app holds the single-instance lock, now fails.
- `kl_pkg_add` / `kl_pkg_rm` with no workspace: the `NAME_IT` refusal now runs after `propose`,
  so the person is shown a card for a call that cannot run. The test assertion was changed from
  the refusal sentence to "declined by the person". This contradicts the diff's own comment at
  kloudlite.ts:412.

### Wiring, Minor
- `kl_pkg_list` returns the whole workspace object instead of the package list.
- `server.ts:412`: `operationControlError` sits in the catch for every route, not only operations.
- bench.ts statically imports `pi/kloudlite.ts` and typebox at boot for a dormant feature,
  against server.ts:57's own "loaded lazily" comment.
- Electron went from 39 to 42 (three majors) with no mention in any commit subject; `main` moved
  to `dist/src/main.js`.
- `web/package.json` gained root `dependencies` (`fast-uri`, `hono`, `js-yaml`, `qs`) that nothing
  imports; `overrides` is the tool for pinning transitives.

### Wiring, sound
No regression to the 18 Sep delivery gate, per-session proposals, exchange deadlines or the
identity split tests. No `.skip` / `.todo` / `.only` added. Both test runners are in the gate, CI
gained a `harness` job the image build now needs, tests use their own temp dirs and port 0. Every
lockfile URL is registry.npmjs.org; the only install script is esbuild (dev, unchanged).

## Required before re-review
1. C1 and C2 fixed, each with a test against the real store.
2. Group A (approval vs concurrency) resolved one way or the other.
3. Every item in group B bounded or cancellable.
4. Executor tests run against the real store, not `MemoryStore`, for at least the approval,
   failure, deadline and recovery paths.
5. Adapters take an abort signal and a timeout: they are in the production tool path today.
6. `fast-sha256` moved to `dependencies`, and the team-workspace id pass-through restored,
   before the next bench image ships.
7. Roll lock takeover (R-C1) before the SLO schedules are resumed.
