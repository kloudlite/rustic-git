# Operation executor: review fixes, handover (20 Sep 2026)

For the executor team. This is the result of the independent review of `695f8484..8426aaa7`
(`2026-09-20-operation-executor-independent-review.md`) and the fixes for it
(`../plans/2026-09-20-operation-executor-review-fixes.md`, rulings 1-7).

## Where it is

- Branch `fix/opexec-review`, head `a024b87b`, 38 non-merge commits on top of `8426aaa7`.
- A local `feature/operation-executor` was fast-forwarded to the same commit. It is NOT pushed,
  and the pod's `/work/src` was never touched. Pulling it in is your step.
- Gate on `a024b87b`, run in `harness/`: `npm run typecheck` clean, `npm run test:components`
  30/30, `node --test --test-concurrency=4 'bench/test/*.test.ts'` 1004/1004, none skipped.
  `npm run test:operations` covers only 5 of the 17 operation test files; use the glob.
- The Rust commits (`crates/registry`, `bins/slo`, `deploy/roll.sh`, the bench package probe)
  also live on `fix/hardening-review` off `desktop-login`. They are not on the fleet yet.

## What changed, by behaviour

Store and executor
- A commit replay would refuse is refused BEFORE it reaches the log, as a refused call
  (`validation_failure`), never as a corrupt log. One bad write can no longer make the store
  constructor throw for every operation. A clock that steps backwards is clamped.
- An approval binds to the pending decision's own revision, not the operation's current one,
  so a sibling step settling between the prompt and the answer no longer strands it. The payload
  digest is what protects a changed payload.
- An approval wait is bounded: it races the step's abort and the store's own decision expiry, so
  an unanswered card no longer holds a concurrency slot forever.
- Cancel revokes the dispatch token of every running step. A refused `recordStepOutcome`,
  `markOutcomeUnknown` or `cancelStep` no longer kills a live token.
- An operation whose step failed settles. A failure is never relabelled `expired`; `expired`
  means nothing ran and nothing failed.
- A step skipped because its dependency failed is recorded durably (`skipStep`), and the scheduler
  propagates skips to a fixpoint, so a chain `C -> B -> A` with A failing resolves.
- `recover()` returns `{ handled, deferred }`. Recovery execution is still not built; what it
  defers is now reported instead of dropped.

Adapters and wiring
- Every platform call takes an `AbortSignal` and a 30 s bound. On a mutation, 502/503/504 and a
  thrown transport are `unknown_outcome`; 500 is a definitive failure. Reads never go unknown.
- A team bench lists its team's workspaces (`KL_TEAM`). An id that is not in the listing is passed
  through, so `/v1`'s own 404 is the answer.
- The operation error envelope is used on `/operations/*` only.
- `kl_pkg_add` / `kl_pkg_rm` with no workspace refuse before a card is drawn.
- The capability runtime loads on first use, keeping `pi/kloudlite.ts` out of the bench's static
  import graph. `bench.capabilityRuntime` is a Promise now.

Model
- A schema `pattern` is capped at 256 characters and every test runs under a 50 ms `node:vm`
  bound. Instruction, constraints, facts and inputs travel as one JSON block of data.
- A response with no usable token usage is `usage_unreported`, never zero cost.
- The evaluation CLI exits 10 when any attempt failed, refuses an oracle or an output path
  anywhere inside the repository, and never returns 1 deliberately.

UI and desktop
- `operations:*` IPC is accepted from the window's main frame only. `KL_BOOT_TEST` is ignored in
  a packaged build.
- A decision card stays mounted while its operation changes (keyed by `decisionId`); an operation
  shows only in the session that owns it; a dependency skip is shown as a skip.
- Test scenarios are out of the shipped bundle; the hash library the bench imports ships.

## Checked and NOT defects

- Timer overflow (scheduler #9): the budget schema caps a deadline at 24 h, far below the
  2^31-1 ms at which Node clamps a timer. A test pins the ceiling; no re-arming helper.
- 4096-byte symlink buffer: equals `PATH_MAX`, so a longer target cannot exist. Comment only.
- `loadFile` path: `__dirname/../renderer` resolves to the built `index.html`. No change.
- Hourly template missing `KLOUDLITE_POD_UID`: all four CronJob templates set it.

## Left open on purpose

- Recovery execution (deferred actions are reported, not run).
- `environment.service.put/rm` and `workspace.packages.add/rm` are GET-then-PATCH of a whole
  list; two racing edits can lose one. Marked `ponytail:`; the fix is a server-side add/remove
  verb or an `If-Match` on the PATCH.
- The UI learns of operation changes through a bounded 2 s poll; there is no operation event on
  the bench stream yet.
- Latent, harmless today: a 502/503/504 on a mutation is `unknown_outcome` with `retryable: true`.
  Every `idempotent` capability is a read, so the executor's retry loop never sees it. If a
  mutation is ever made `idempotent`, that loop would record `failed` and retry without
  reconciling; make the flag `false` first.
- `cancellation` in `execute()` says a failed `requestCancel` is retried by "a later abort"; the
  abort listener is `once`, so only the deadline timer retries it. Settle and expire still
  reconcile the durable state.

## Two questions for you

1. Electron 39 to 42: yours to schedule, or ours?
2. `reviewer-oracles.json`: the evaluation CLI now refuses any oracle path inside the repository.
   Where should the held-out file live?
