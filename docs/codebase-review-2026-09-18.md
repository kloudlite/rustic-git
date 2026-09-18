# Kloudlite codebase review — 18 September 2026

Reviewed branch: `desktop-login`, commit `695f8484d1c60bef7470a0d7243910b7cb9c4777`, in `/Volumes/kdisk/rustic-git-wt/desktop-login`.

[Architecture diagrams](architecture-2026-09-18.md) were prepared before this report. The diagrams describe the code, not a deployment verification. The fleet remained on `858b99ee` during this review.

## Assessment

The main service boundaries are useful and worth preserving: repository ownership is separate from workspace reconciliation; models run in the bench while workspace tools run remotely; Kubernetes holds desired/observed workspace state; Redis is a cache and notification channel. A rewrite or a new service split would not address the most urgent problems.

The priority is to make those boundaries hold on every path. Direct file reads and aggregate diffs enforce different rules; file tools and exec expose different trees; the two API routers accept the same bench credential under different revocation rules. Recovery and release checks also miss states they claim to cover.

This was a review across the major subsystems, using Astra for architecture/data integrity and Luna for API/security and desktop/web/runtime inspection. It was selective source inspection, not a line-by-line audit or a penetration test. The tracked source inventory contains approximately 237,000 lines across Rust, TypeScript, Python and shell files, including tests. No absence-of-defect claim is implied for unexamined paths.

## Validation and limits

| Check | Result |
|---|---|
| Harness TypeScript typecheck | Passed |
| Harness bench suite | 512 passed, 0 failed, 1 skipped |
| Skipped harness check | Renderer boot: Electron did not expose the expected window; this is not a renderer pass |
| Rust workspace/all-target clippy, locked, with agent stall-dump feature | Passed in the dev pod |
| First Rust nextest run | 874 passed, 1 failed, 19 skipped; fail-fast left 1,638 selected tests unrun |
| Complete nextest run, same source, no-fail-fast | 2,513 passed, 19 skipped |
| Initial Rust failure | `crates/ide/tests/trees.rs:288` compared file enumeration order; passed on rerun, so retained as a flake |
| Exchange recovery-window reproduction | An open persisted exchange disappeared from `recent(500)` after 500 newer rows, including after reopening the log |
| Rollout job-selector reproduction | Selector sees `kloudlite-slo-hourly-*` but misses active `hourly-manual-*` and the helper's `hourly-HHMMSS` names |

The API revocation, sandbox, symlink, GC and crash-window findings below are source-confirmed paths, not live exploit demonstrations. No owner credentials were read for this review. No test workspaces, snapshots, production data or live sandbox files were modified. The source branch was pushed/pulled into the dev pod before the user requested review; no images were built or deployed in this resumed session. Only these review/architecture documents were added to the laptop checkout.

## Findings

P1 means fix before the next release that relies on the affected boundary. P2 means a concrete reliability or verification defect to address in the next hardening pass. These are prioritization labels, not claims that every trigger has occurred on the fleet.

### R01 · P1 · Aggregate diff can read outside the workspace through symlinks

**Evidence:** [filesystem diff handler](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/ide/src/fs/mod.rs:301), [worktree byte reader](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/ide/src/fs/git.rs:80), [aggregate diff enumeration](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/ide/src/fs/git.rs:379).

A named file goes through `confine`, but `/fs/diff` without `path` discovers changed paths through Git status and reads them with `std::fs::read(root.join(rel))`. An untracked symlink to a readable file outside the tree is followed, and the target's bytes can be returned as added lines. The IDE server performs this read outside the command sandbox, so a working bubblewrap wrapper does not protect it.

**Suggestion:** use one confined worktree-read operation for discovered and explicitly requested paths. Represent a symlink as its link text, consistent with Git's file model; do not follow it for a diff. Apply the same review to numstat and other derived file views. **Regression:** create an external sentinel and a symlink inside a temporary repository; aggregate diff must never include the sentinel bytes. Include agent-tree paths and normal tracked/untracked files. **Confidence:** high from source.

### R02 · P1 · Main-session exec exposes subagent trees

**Evidence:** [sandbox bind construction](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/ide/src/sandbox.rs:88).

The main sandbox binds the entire workspace root read/write, including the nested `.agents` directory, without masking it. A main-session command can read or change `.agents/<agent>/...` even though the file tools refuse that path. This bypass exists while sandboxing is active; it is separate from the explicitly permitted unwrapped fallback.

**Suggestion:** mask agent subvolumes from the main sandbox, or bind an explicit view containing only the main tree. Preserve the intended ability for each agent to use its own tree. **Regression:** execute real read and write attempts against another tree inside bubblewrap, rather than only checking the generated argument list or file-tool refusal. **Confidence:** high from source.

### R03 · P1 · Directory routes bypass bench credential revocation checks

**Evidence:** [directory bench conversion](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/api/src/lib.rs:456), [workspace API bench checks](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/workspaces/src/api/mod.rs:630), [token lifetime](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/core/src/jwt.rs:279).

On admitted repository/team routes, the directory router verifies the bench JWT and finds its user, then substitutes a normal person session. It does not check the parent CLI login or whether the bench still admits tools. The workspace router checks both. A still-unexpired bench token can therefore retain admitted repository authority after its parent is revoked or its bench is stopped/deleted, for the remaining portion of its 15-minute lifetime. Existing team authorization still applies; this is not a claim of arbitrary access to other users.

**Suggestion:** share the bench liveness/revocation decision across both routers before translating identity. Keep the route allow-list and normal owner authorization. **Regression:** issue a bench credential, revoke its parent or invalidate its bench, and exercise both routers' read/write routes; both should refuse under the documented revocation-cache bound. **Confidence:** high from source; not exercised against a live credential.

### R04 · P1 · Registry GC can delete a layer while a manifest successfully publishes

**Evidence:** [GC's second scan and deletion](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/registry/src/gc.rs:340), [manifest presence check](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/registry/src/manifests.rs:237), [manifest publication](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/registry/src/manifests.rs:272).

The possible interleaving is: GC finishes its second reference scan; a push's HEAD check sees an old unreferenced blob; GC deletes it; the push publishes a manifest referencing that blob. Reusing an old blob does not gain the new-upload grace period. The push can succeed and the subsequent pull lack a layer. This is existing acknowledged `ponytail` debt, not a newly introduced regression; comments claiming the second scan closes the window are stronger than the mechanism.

**Suggestion:** coordinate deletion with publication at the owner/blob scope, using a protocol that remains correct across server and worker processes. A further scan alone is insufficient. **Regression:** use barriers in the object-store test double to force this exact interleaving and prove either the push refuses or the layer survives. **Confidence:** high from source.

### R05 · P1 · Partially unreadable volumes are stamped with incomplete usage

**Evidence:** [usage aggregation](/Volumes/kdisk/rustic-git-wt/desktop-login/bins/agent/src/usage.rs:84), [quota consumption of the stamp](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/workspaces/src/quota.rs:45).

The new zero-as-unknown parser is appropriate, but `volume_usage` drops unknown child measurements and aggregates the remaining ones. A valid small snapshot plus an unreadable large live worktree becomes a small, freshly timestamped total. Directory and entry errors are also skipped. Keeping the previous reading works only when all children fail. This can undercount quota usage for volumes above the 1 GiB floor.

**Suggestion:** distinguish an absent expected directory from a failed listing, and treat any unreadable expected child as an incomplete aggregate. Preserve the last complete reading and expose its age/unknown state. **Regression:** mix one valid child with one zero/error child, plus directory/entry failures; none should publish a fresh partial total. **Confidence:** high from source.

### R06 · P2 · Wrapped exec ignores a requested nested working directory

**Evidence:** [exec command construction](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/ide/src/tools/exec.rs:81), [sandbox chdir argument](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/ide/src/sandbox.rs:111).

The caller's confined `cwd` is assigned to the wrapper process, but bubblewrap receives an explicit `--chdir` to the tree root. Thus `cwd: "web"` executes at the root when sandboxing is active, while the unwrapped path uses the requested directory. Builds or relative writes can target the wrong project.

**Suggestion:** pass the confined requested directory into the sandbox argument builder. **Regression:** run `pwd` and a relative sentinel write with a nested `cwd`, for both wrapped and unwrapped paths. **Confidence:** high from source.

### R07 · P2 · Fresh-worktree ownership does not converge after an interrupted checkout

**Evidence:** [checkout existence check and create/chown sequence](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/workspaces/src/engine/snapshot.rs:63), [controller treatment of an existing worktree](/Volumes/kdisk/rustic-git-wt/desktop-login/bins/agent/src/controller/worktree.rs:150).

Creating a fresh subvolume and assigning tenant ownership are separate operations. A crash or chown failure between them leaves the destination root-owned. Retry returns `WORKTREE_EXISTS` before repair, and the controller treats that result as success. A fresh bench can remain unable to create `.bench`. This is a retry gap, separate from the explicitly pending migration of old root-owned benches.

**Suggestion:** make completion of a known fresh checkout idempotent, or publish a prepared staging subvolume only after ownership is correct. Do not recursively change ownership of arbitrary existing restored data. **Regression:** interrupt after create/before chown, reconcile again, then verify uid 1000 can create bench state. **Confidence:** high from source.

### R08 · P2 · Recovery and deadlines ignore active asks outside the newest 500 exchanges

**Evidence:** [restart recovery](/Volumes/kdisk/rustic-git-wt/desktop-login/harness/bench/src/bench.ts:263), [deadline sweep](/Volumes/kdisk/rustic-git-wt/desktop-login/harness/bench/src/bench.ts:796), [bounded history query](/Volumes/kdisk/rustic-git-wt/desktop-login/harness/bench/src/exchanges.ts:55).

Both lifecycle paths use `recent(500)`. A queued/running ask older than 500 subsequent exchanges remains persisted but is not recovered or expired. A temporary-log reproduction confirmed `persistedOpen=true` and `visibleToResumeAndSweep=false`, including after reopening the log.

**Suggestion:** query all nonterminal exchanges for lifecycle work; use a history window only for presentation. **Regression:** keep one old ask open, append more than 500 completed exchanges, restart and advance the deadline; the old ask must settle or resume. **Confidence:** reproduced with the actual ExchangeLog implementation.

### R09 · P2 · Renderer queue state disagrees with runtime lifecycle state

**Evidence:** [queue and pending-ask projections](/Volumes/kdisk/rustic-git-wt/desktop-login/harness/src/renderer/live.ts:75).

`asksOf` excludes only `done` and `failed`, leaving terminal `blocked` and `cancelled` asks in the pending view. `queueOf` maps these states to `pending`, and recognizes `working` while the runtime uses `running`. A completed cancellation or timeout can continue looking unfinished, and active work can look merely queued.

**Suggestion:** share an explicit exchange-state type and terminal/active predicates across runtime and renderer. Retain terminal rows in history with their actual outcome. **Regression:** table-test every runtime state in queue, pending cards, and footer projections. **Confidence:** high from source.

### R10 · P2 · Desktop REST requests can wait indefinitely

**Evidence:** [BenchClient HTTP request](/Volumes/kdisk/rustic-git-wt/desktop-login/harness/src/bench-client.ts:105).

Requests have neither a deadline nor an abort signal, and response completion relies on `end`. A connection that remains open without completing can leave bootstrap, approvals, files or settings pending indefinitely. WebSocket connection state does not establish that each HTTP request will finish.

**Suggestion:** use a total request deadline, abort/destroy on expiry, handle aborted/error responses, and settle each promise once. Give genuinely long routes an explicit larger budget. **Regression:** serve headers without ending the body, stall before headers, and abort mid-response; every request must reject within its bound with a usable error. **Confidence:** high from source.

### R11 · P2 · Kubernetes history transitions are lost on a ClickHouse write failure

**Evidence:** [watch state update](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/workspaces/src/history/watch.rs:818), [failed insert handling](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/workspaces/src/history/watch.rs:851).

The watcher advances/removes its previous object before writing the derived rows. Failed inserts only produce a warning. There is no retry queue; the same unchanged object no longer produces the transition, and deletion cannot be recovered from a later list. A temporary history outage can permanently remove the lifecycle evidence needed to diagnose an incident.

**Suggestion:** preserve failed event batches for retry, using their existing deterministic IDs. Define a bounded persistent outbox or an explicit retention/backpressure policy; a relist cannot reconstruct every intermediate transition. **Regression:** fail an insert, recover ClickHouse, and verify a phase change and deletion each appear once. **Confidence:** high from source.

### R12 · P2 · Publishing the bench image is not gated on its own checks

**Evidence:** [CI test job](/Volumes/kdisk/rustic-git-wt/desktop-login/.github/workflows/image.yml:20), [bench image publication](/Volumes/kdisk/rustic-git-wt/desktop-login/.github/workflows/image.yml:265), [dev ship gate](/Volumes/kdisk/rustic-git-wt/desktop-login/deploy/dev/pod/ship.sh:38).

The workflows and dev ship script gate Rust and web, then publish the bench image. Neither path runs the harness typecheck or bench suite. The manual checks run during this review therefore do not establish an enforced release invariant. The benchmark suite is currently the only place many lifecycle regressions are checked.

**Suggestion:** add a bench/harness gate using the deployed Node major, locked dependencies, typecheck, and bench tests. Require it before publishing the bench image in both paths. Tie any `--no-gate` bypass to recorded checks for the exact source commit. **Regression:** a deliberately failing bench test must prevent bench image publication. **Confidence:** high from workflow/script inspection.

### R13 · P2 · Renderer boot verification can silently inspect nothing

**Evidence:** [boot test](/Volumes/kdisk/rustic-git-wt/desktop-login/harness/bench/test/renderer-boot.test.ts:20), [single-instance lock](/Volumes/kdisk/rustic-git-wt/desktop-login/harness/src/main.ts:19).

The test uses a fixed debug port, shares the app's profile/instance lock, and skips when no page appears. `KL_BOOT_TEST` is passed but is not used by main to isolate the test. An already-running desktop can cause the child to exit and the check to skip. It also tests whichever `dist` build is present, with no guarantee that the build matches source. This review's run skipped with “electron did not open a window”.

**Suggestion:** build the exact source, use an isolated temporary app profile and assigned debug port, capture child exit/output, and require the test in a display-capable release job. Skip only explicitly unsupported environments. Use a deterministic authenticated/mock bench state to exercise the actual app view, not just login. **Confidence:** source path plus observed skip; the skip's exact cause was not independently proven.

### R14 · P2 · Rollout safety misses manual probe jobs and admin readiness

**Evidence:** [probe wait selector](/Volumes/kdisk/rustic-git-wt/desktop-login/deploy/roll.sh:22), [rollout wait list](/Volumes/kdisk/rustic-git-wt/desktop-login/deploy/roll.sh:51), [manual job naming](/Volumes/kdisk/rustic-git-wt/desktop-login/deploy/dev/run-job.sh:34).

The wait recognizes names containing `slo-`, but manual jobs use `hourly-manual-*` or `<suite>-HHMMSS`. It can report no active probe while a manual run is still measuring the fleet. After applying the manifest, the script also waits for server, user API, worker and web, but not `kloudlite-admin`, even though admin is updated by the same apply.

**Suggestion:** identify probe jobs using stable labels/template provenance; coordinate probe start and rollout with a shared lock rather than a one-time observation. Wait for every changed workload, including admin, before declaring success. **Regression:** active scheduled and manual job fixtures both block rollout; an unready admin prevents success. **Confidence:** selector reproduced; readiness omission confirmed in source.

### R15 · P2 · The bench package probe exercises a retired capability and hides command failure

**Evidence:** [probe command](/Volumes/kdisk/rustic-git-wt/desktop-login/bins/slo/src/stages/bench_ws.rs:116), [CLI credential requirements](/Volumes/kdisk/rustic-git-wt/desktop-login/bins/kl/src/api.rs:13), [shell environment and mounts](/Volumes/kdisk/rustic-git-wt/desktop-login/crates/workspaces/src/k8s/workspace.rs:314), [PTY result handling](/Volumes/kdisk/rustic-git-wt/desktop-login/bins/slo/src/stages/bench.rs:963).

`bench.pkg.add` runs `kl pkg add` in the home-only bench shell. That shell intentionally lacks the platform environment and workspace token the CLI requires. The ttyd helper returns status zero on clean transport completion, so the failed command becomes a later 30-second spec-poll timeout. The earlier ttyd framing fix is already an ancestor of deployed `858b99ee`; shipping it again does not solve this mismatch.

**Suggestion:** retire this invalid bench-shell SLO or replace it with the supported bench-to-workspace package journey, keeping authenticated workspace package coverage. Do not add credentials to the shell just to satisfy a stale probe. Where command status is needed, use an explicit result mechanism rather than interpreting a closed terminal as exit zero. **Regression:** verify supported mutation and deliberate shell refusal separately. **Confidence:** high from code/design; no new live package mutation performed.

### R16 · P2 · Probe cleanup can undo an unrelated node decommission

**Evidence:** [unscoped cleanup](/Volumes/kdisk/rustic-git-wt/desktop-login/deploy/dev/run-job.sh:75).

After any helper-driven run, the script removes the decommission label/annotation from every labeled node. It does not check that the current run set them or preserve an earlier operator decision. A legitimate maintenance/decommission operation overlapping a probe can be undone by cleanup.

**Suggestion:** record the run's node mutations and original values, attach a run owner marker, and revert only still-owned changes using a conditional update. **Regression:** pre-mark one node as operator-owned and one as probe-owned; cleanup may restore only the latter. **Confidence:** high from source; not executed against live nodes.

## Architectural suggestions

1. **Centralize security decisions, not whole services.** A shared bench-credential admission contract and a shared tree-access layer would address R01–R03 without coupling the directory and workspace implementations into one large handler.
2. **Make lifecycle state explicit end to end.** Define typed exchange states, terminal predicates, durable active-work queries and restart rules once. Have the renderer display the runtime's state rather than translate unrelated string vocabularies. This targets R08–R10 and the recurring stale-queue incidents.
3. **Treat uncertainty as a value.** Storage usage, cache readiness and telemetry delivery need complete/unknown/stale distinctions. Dropping failed children or inserts silently converts partial knowledge into a confident answer. Apply this principle to R05 and R11.
4. **Make a release an auditable artifact.** Record commit, test results, image digests and required manifest/schema changes together. The build can remain in the dev pod; the gate and rollout should consume one release record and coordinate with probes. This targets R12–R15 and the prior wrong-build verification incidents.
5. **Refactor the largest production modules around ownership.** `harness/bench/src/bench.ts` is about 2,055 lines; `harness/pi/kloudlite.ts` about 1,370; renderer `Chat.tsx` about 1,363. Extract exchange lifecycle, delivery/approval handling, tool registration and transcript presentation behind stable interfaces. Preserve behavior with current tests before restructuring; avoid a repo-wide mechanical rewrite.
6. **Refresh architecture/runbooks alongside capability changes.** `harness/README.md` and `deploy/BACKUPS.md` still describe retired runtime/storage behavior in places, including object-store workspace snapshot assumptions. Mark historical sections explicitly and derive inventories from current manifests where practical. The new architecture diagrams provide a current starting point, but should be checked as part of future boundary changes.
7. **Prove recovery on isolated resources.** Add controlled crash-point tests for checkout/publish, queued asks and telemetry retries; keep an isolated btrfs/cluster integration job for tests that ordinary CI skips. Test restore procedures for the actual current storage model instead of inferring recoverability from backup configuration.

## Suggested order

| Order | Work | Evidence required before completion |
|---|---|---|
| 1 | R01–R03: filesystem and credential boundaries | External-symlink diff refusal, main-exec agent-tree refusal, revoked-token denial on both routers |
| 2 | R04–R07: storage integrity and execution correctness | Deterministic GC interleaving, partial-usage failure, interrupted checkout recovery, nested-cwd execution |
| 3 | R08–R11: lifecycle and diagnostic reliability | More than 500 exchanges across restart, all renderer states, stalled HTTP deadlines, retried history inserts |
| 4 | R12–R16: release/probe guarantees | Enforced harness gate, non-skipped renderer smoke test, manual-probe rollout guard, supported package journey, scoped cleanup |

Keep shell feature work parked as requested. Before any deployment, review the fixes and rerun the relevant checks on the carrying commit. The green unit-suite rerun does not close the source findings above.

## Coverage

| Area | Inspection performed |
|---|---|
| Git/registry/storage | Representative repository ownership/election, Git protection and merge-worker paths; manifest publication and blob GC |
| Workspaces | Checkout/snapshot ownership, usage/quota path, controllers and pod construction, tree tool execution and filesystem views |
| APIs/security | Directory identity conversion, ownership/team checks, workspace tool/bench admission, gateway ticket/token boundaries, CLI credential requirements |
| Desktop/runtime | Exchanges, restart/deadline handling, queue projections, HTTP client, skills/identity integration, renderer boot checks |
| Web | Representative server-only clients, auth bearer extraction, API/admin separation, server actions and owner filtering; no additional substantiated issue from that selective pass |
| Operations | CI and dev ship gates, image pinning, rollout coordination, SLO helper/cleanup, Kubernetes history delivery, backup/runbook consistency |

Not completed: exhaustive endpoint fuzzing, adversarial concurrency testing, load/capacity benchmarking, live exploit tests, restore drills, or every UI interaction. Existing cloud backup settings and external dependency advisories were not independently audited.


Implementation progress and subsequent source-review findings are tracked in [review-hardening-2026-09-18.md](review-hardening-2026-09-18.md). The findings and test counts above remain the original baseline review.
