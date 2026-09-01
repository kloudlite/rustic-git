# Sync Points Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replicate live worktrees continuously — bounded data loss on node death, re-hosting from the latest state, a real flush on stop — without growing the snapshot set.

**Architecture:** A sync point is a `Snapshot` CR with `spec.transient: true`, cut by a generation-gated agent beat and replicated by the unchanged pull beat. Retention keeps exactly one transient per worktree. Stop cuts a final transient and waits for a replica to hold it. A node re-hosting a worktree checks out the latest transient before falling back to the last commit. Plus one cleanup task deleting leftovers found by the survey.

**Tech Stack:** Rust (kube-rs controllers), btrfs `send -p`, k3s.

**Spec:** `docs/superpowers/specs/2026-09-02-sync-points-design.md`

## Global Constraints

- A transient NEVER enters a commit's `parent` chain and NEVER advances `status.head`. `push` semantics are byte-for-byte unchanged.
- Retention: at most one `Ready` transient per worktree; the previous is deleted only AFTER the new one is `Ready` (keep-biased, as `retain` and `pull_volume` already are).
- Defaults, verbatim: `WS_SYNC_SECS=60`, `WS_STOP_FLUSH_TIMEOUT_SECS=600`.
- Naming: beat transients `sync-{worktree}-{8 hex}`; stop transients keep the existing fixed names `stop-{env}` / `stop-{ws}`. Generation annotation key: `rustic-git.io/synced-generation`.
- Every task: `cargo test --workspace --locked` green and `cargo clippy --workspace -- -D warnings` clean before its commit. btrfs-gated engine tests count only from a build-0 run (`0.00s` = skipped).
- Record the test-function count of every crate touched before and after; name every disappeared test.
- Comments explain WHY; deliberate shortcuts carry `// ponytail: <ceiling and upgrade path>`. Commit subjects imperative sentence case, no attribution trailers of any kind.
- CRD schema changes regenerate `deploy/k3s/crds.yaml` via `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml`.

---

### Task 1: `transient` on the Snapshot CR, and `Engine::generation` restored

**Files:**
- Modify: `crates/workspaces/src/crd.rs` (`SnapshotSpec`), `deploy/k3s/crds.yaml` (regenerated)
- Modify: `crates/workspaces/src/engine/ops.rs` (restore `generation`, `generation_of`, `parse_generation`)
- Test: `crates/workspaces/src/engine/ops.rs` unit test; `crates/workspaces/tests/crd_yaml.rs` drift check

**Interfaces:**
- Produces: `SnapshotSpec.transient: bool` (`#[serde(default)]`, doc: a sync point — never in a parent chain, never a head, retained one-per-worktree); `Engine::generation(&self, volume: &str, ws: &str) -> Result<u64, EngErr>` reading the WORKTREE (`pool.worktree(volume, ws)`), NOT the old `pool.live(id)`; `pub fn parse_generation(subvolume_show: &str) -> Option<u64>`.

- [ ] **Step 1: failing test** — in ops.rs tests:
```rust
#[test]
fn parse_generation_reads_the_generation_line() {
    let show = "vol/ws-1/live/ws-1\n\tName: ws-1\n\tGeneration: 10197\n\tGen at creation: 4\n";
    assert_eq!(parse_generation(show), Some(10197));
    assert_eq!(parse_generation("no such line"), None);
}
```
- [ ] **Step 2: run → FAIL** (`parse_generation` undefined).
- [ ] **Step 3: implement** by restoring from `git show 9e405f6c^1:crates/workspaces/src/engine/ops.rs` — `parse_generation` verbatim, `generation_of` verbatim, and a NEW `generation(volume, ws)` that calls `generation_of(&self.pool.worktree(volume, ws))`. Doc comment: why the worktree path (commit model: `live` is a directory of worktrees).
- [ ] **Step 4: add the field** to `SnapshotSpec` after `pinned`:
```rust
/// A sync point, not a commit: cut by the agent's sync beat (or a stop) from a live worktree so a
/// replica holds its latest state. Never a `parent` of anything, never a worktree's `head`, and
/// retained ONE per worktree — see `snapshot::retain`. `push` never sets this.
#[serde(default)]
pub transient: bool,
```
Fix every `SnapshotSpec { .. }` literal (`api.rs::create_commit`, `controller.rs::stop_push`, test fixtures) with `transient: false`. Regenerate crds.yaml.
- [ ] **Step 5: gates; commit** `Add the transient flag to Snapshot and restore Engine::generation`.

### Task 2: The sync beat

**Files:**
- Create: `bins/agent/src/sync.rs`
- Modify: `bins/agent/src/lib.rs` (`mod sync;`), `bins/agent/src/controller.rs` (`spawn_sync(ctx.clone())` beside `spawn_pull` at ~:261; add `pub fn sync_interval()` next to `home_push_interval`'s old spot)
- Test: `bins/agent/src/sync.rs` unit tests (pure decision fn), `bins/agent/tests/reconcile.rs` (one mock-client beat test)

**Interfaces:**
- Consumes: `Engine::generation(volume, ws)` (T1), `crd::SnapshotSpec.transient` (T1), `peer::interesting_volumes`-style listing of Workspaces/Environments with `status.nodeName == me`.
- Produces: `pub async fn sync_beat(ctx: &Arc<Ctx>)`; `pub const SYNCED_GENERATION: &str = "rustic-git.io/synced-generation"`; pure `pub fn due(current: u64, recorded: Option<u64>) -> bool` (`recorded.is_none_or(|g| current > g)`); `pub fn sync_name(worktree: &str) -> String` = `format!("sync-{worktree}-{}", crd::short_hex())` (make `short_hex` `pub` if it is not).

- [ ] **Step 1: failing tests** (in sync.rs):
```rust
#[test]
fn due_only_when_the_generation_moved() {
    assert!(due(5, None));
    assert!(due(6, Some(5)));
    assert!(!due(5, Some(5)));
    assert!(!due(4, Some(5)));
}
```
- [ ] **Step 2: run → FAIL.**
- [ ] **Step 3: implement `sync_beat`.** Per pass:
  1. List Workspaces and Environments; keep those with `status.nodeName == ctx.node`, `status.volume_ref = Some(v)`, phase not `Stopped`, and (workspaces) `status.pod_ref.is_some()`.
  2. For each `(volume, worktree)`: list `Snapshot`s for the volume (`spec.volume={volume}` field selector); find the current transient for this worktree = the `Ready` one with `spec.transient && spec.worktree == worktree` and the highest `SYNCED_GENERATION` (parse the annotation). If ANY `Working` transient exists for this worktree, skip — one cut in flight at a time (same F1 rule `create_commit` applies).
  3. `let gen = spawn_blocking(engine.generation(&volume, &worktree))`; on error warn and continue.
  4. If `!due(gen, recorded)` continue.
  5. Create `Snapshot` named `sync_name(&worktree)`: `transient: true`, `parent` = current transient's name or `""`, `message: None`, `pinned: false`, annotation `SYNCED_GENERATION = gen`, labels `commit_labels(owner, volume)`, ownerReference to the parent object (copy the shape `stop_push` uses). 409 = fine.
  Every per-object failure is `warn!` + `continue`; the beat never aborts.
- [ ] **Step 4: `spawn_sync`** — copy `spawn_pull` verbatim with `sync_interval()` (`WS_SYNC_SECS`, default 60) and `crate::sync::sync_beat`. Call it right after `spawn_pull(ctx.clone())`.
- [ ] **Step 5: reconcile.rs test** `the_sync_beat_cuts_a_transient_only_when_the_worktree_generation_moved`: mock routes — one placed running Workspace, an empty Snapshot list, then a POST to `/apis/rustic-git.io/v1alpha1/snapshots` recorded; run `sync_beat` with an engine whose pool is a tmpdir. Because `generation` shells out to btrfs, gate the engine call the way `ensure_homecache` is gated (`Engine.has_btrfs`): when `!has_btrfs`, `generation` returns `Err`, so this test asserts the beat WARNS AND CREATES NOTHING on a generation error (keep-biased), and a second test with a fake `generation` seam is not required — the pure `due` test covers the decision. Say this explicitly in the test's doc comment.
- [ ] **Step 6: gates; commit** `Cut a sync point whenever a live worktree's generation moves`.

### Task 3: Cutting a transient — no head, one-per-worktree retention

**Files:**
- Modify: `bins/agent/src/snapshot.rs` (`reconcile_commit` ~:100-110, `retain` ~:199, `worktree_heads`)
- Test: `bins/agent/src/snapshot.rs` `commit_tests`

**Interfaces:**
- Consumes: `SnapshotSpec.transient` (T1).
- Produces: `retain` behaviour: for a transient cut, delete every OTHER `Ready` transient of the same worktree; for a commit cut, transients are ignored (never counted in the chain — they are not in it anyway — and never deleted by the commit path).

- [ ] **Step 1: failing tests** (`commit_tests`, using the file's existing `snapshot(...)` fixture extended with a `transient` arg):
```rust
#[tokio::test]
async fn a_transient_cut_does_not_advance_head() { /* Ready transient for ws-1; assert NO PUT/PATCH to the workspace status route */ }
#[tokio::test]
async fn a_new_ready_transient_deletes_the_previous_one_for_its_worktree_only() {
    // Ready transients: sync-ws-1-a (old), sync-ws-1-b (new, just cut), sync-ws-2-c (other worktree).
    // retain(ctx, "vol-1", "sync-ws-1-b") → exactly one DELETE, of sync-ws-1-a.
}
#[tokio::test]
async fn a_working_previous_transient_is_never_deleted() { /* previous is Working → no DELETE */ }
```
- [ ] **Step 2: run → FAIL.**
- [ ] **Step 3: implement.** In `reconcile_commit`, after `patch_status(... Ready)`: `if !s.spec.transient { advance_head(...).await?; }` then `retain(...)` always. In `retain`: if the cut snapshot is transient, the pass is: list `Ready` snapshots for the volume, delete those with `spec.transient && spec.worktree == cut.spec.worktree && name != cut`; return (no chain walk). If it is a commit, the existing chain walk runs, with `by_name` filtered to `!spec.transient` so a transient can never be mistaken for a chain member.
- [ ] **Step 4: gates; commit** `Keep one sync point per worktree and never make it a head`.

### Task 4: Re-host from the latest sync point

**Files:**
- Modify: `bins/agent/src/controller.rs` (`effective_head` ~:1952 for workspaces; the environment checkout at ~:2438)
- Modify: `bins/agent/src/snapshot.rs` (new `pub(crate) async fn latest_transient(ctx, volume, worktree) -> Result<Option<String>, ReconcileErr>`)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Produces: `latest_transient` — the `Ready` transient of `(volume, worktree)` with the highest `SYNCED_GENERATION` annotation, else `None`.

- [ ] **Step 1: failing test** `a_workspace_starting_on_a_new_node_checks_out_its_latest_sync_point_over_its_head`: placed workspace with `status.head = "vol-1-aaaaaaaa"` and no worktree on this pool; Snapshot list route returns that commit plus a Ready transient `sync-ws-1-bbbbbbbb` (annotation gen 9); assert the engine's checkout was asked for `sync-ws-1-bbbbbbbb`. Use the pool tmpdir: `checkout` fails without btrfs, so assert on the ERROR MESSAGE naming the source snapshot path (`{pool}/vol/vol-1/snap/sync-ws-1-bbbbbbbb`) — the file already has tests that assert on engine errors this way; follow them.
- [ ] **Step 2: run → FAIL.**
- [ ] **Step 3: implement.** Workspace: `let effective_head = latest_transient(ctx, &id, &w.name_any()).await?.or(prev.head.clone()).or_else(|| clone_commit.map(str::to_string));` — ONLY when this node has no worktree yet (`!engine.pool.worktree(&id, &ws).exists()`); an existing worktree is never swapped by this. Environment: same at ~:2438. Doc comment: the data-loss window is now one `WS_SYNC_SECS`.
- [ ] **Step 4: gates; commit** `Re-host a worktree from its latest sync point`.

### Task 5: Flush on stop, gated on a replica

**Files:**
- Modify: `bins/agent/src/controller.rs` (`stop_push` ~:2746, `stop_workspace` ~:1683, `stop_environment` ~:2321)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `SnapshotSpec.transient`, `VolumeReplica.status.{phase,lastSyncAt}`.
- Produces: `stop_push` creates a TRANSIENT (`transient: true`, `parent` = the worktree's latest transient or `""`, `worktree` = the PARENT's name — today it wrongly passes `volume` for both, which only worked because an environment's worktree is named after its volume); `StopPush::Landed` now means Ready AND replicated; new `pub fn flush_timeout() -> Duration` (`WS_STOP_FLUSH_TIMEOUT_SECS`, default 600); `stop_workspace` calls `stop_push` with name `stop-{ws}` (restoring the gate the shared-home work removed, but as a sync point).

- [ ] **Step 1: failing tests**:
  - `a_stop_waits_for_a_replica_to_hold_the_flush`: Ready `stop-env-1` transient, replicas list shows only THIS node Synced → `StopPush::Waiting`, no StatefulSet delete.
  - `a_stop_proceeds_once_another_replica_is_synced_after_the_cut`: another node's replica `Synced` with `lastSyncAt` later than the transient's Ready transition → `Landed`, deletes proceed.
  - `a_stop_tears_down_after_the_flush_timeout_with_a_condition`: transient Ready for longer than `flush_timeout()` (set the env var to `0` in the test) with no replica → teardown proceeds and the written status carries condition reason `FlushUnreplicated`.
  - `a_workspace_stop_cuts_a_sync_point_before_deleting_the_pod`: order of calls: snapshot create … pod DELETE only after Landed.
- [ ] **Step 2: run → FAIL.**
- [ ] **Step 3: implement.** In `stop_push`'s `Some(Ready)` arm: list `VolumeReplica`s for the volume; `replicated = any(r.spec.node != ctx.node && phase == "Synced" && last_sync_at >= ready_at)` where `ready_at` is the transient's `Ready` condition time — add `ready_at: Option<String>` to `SnapshotStatus` (T1 already regenerated the CRD; regenerate again here) written by `reconcile_commit` at the Ready patch. If not replicated and `now - ready_at < flush_timeout()` → `Waiting`; past the timeout → `Landed` plus return a flag the caller turns into `crd::condition("Ready", true, "FlushUnreplicated", "stopped without a replica holding the final sync point", gen)`. Restore the workspace gate in `stop_workspace` (the commit that removed it is `40a8a79`'s ancestor in the rewritten history — search `git log -S'stop-home-'` — but do NOT restore the home logic, only the `stop_push` call/wait shape with name `stop-{ws}` and `w.name_any()` as the worktree).
- [ ] **Step 4: gates; commit** `Flush a stopping worktree to a replica before tearing it down`.

### Task 6: Pull beat and placement — transients are just snapshots (verification task)

**Files:**
- Modify: `bins/agent/src/peer.rs` only if a test proves a gap
- Test: `bins/agent/src/peer.rs` `reconcile_tests`

- [ ] **Step 1: test** `a_ready_transient_is_pulled_and_counts_toward_synced`: Snapshot list with one Ready commit and one Ready transient; local has the commit only; assert the pull GETs `…/commit/vol-1/sync-ws-1-x?parent=` and the replica status is `Syncing` until it lands, `Synced` after. Should pass WITHOUT code changes — if it does, that is the deliverable.
- [ ] **Step 2: test** `a_deleted_transient_is_dropped_from_every_replica`: CR list lacks `sync-ws-1-a` which is local → `drop_commit` called. Should also pass unchanged.
- [ ] **Step 3: gates; commit** `Prove the pull beat replicates sync points unchanged` (tests only).

### Task 7: Config, docs, e2e

**Files:**
- Modify: `deploy/k3s/agent-daemonset.yaml` (env `WS_SYNC_SECS: "60"` with a WHY comment; `WS_STOP_FLUSH_TIMEOUT_SECS: "600"`)
- Modify: `CLAUDE.md` ("Four verbs" paragraph gains sync points; the sentence claiming stop pushes a full commit changes), `deploy/k3s/README.md` (a "Sync points" paragraph: what to expect in `kubectl get snapshots`, the one-per-worktree rule, the two env vars)
- Modify: `tests/ws_e2e.sh` — one assertion: write a file in a running workspace, wait `> WS_SYNC_SECS`, assert a `Ready` transient `sync-{ws}-*` exists and that the previous one (after a second write + wait) is gone.

- [ ] **Step 1–3: edit, `bash -n tests/ws_e2e.sh`, gates; commit** `Document and configure sync points`.

### Task 8: Cleanup — leftovers with no reader

**Files:** as listed per item. Batch as ONE dispatch (same-shape deletions), ONE review.

Known from the survey (verify each with a grep before deleting; if a caller exists, keep it and say so):
- `crates/workspaces/src/model.rs:18` `Region.agent_token`, `api.rs:167` `/v1/regions/{id}/rotate-token` + `rotate_region_token`, `random_token` if caller-free, `api.rs:303/321-349` token minting on region create/re-register. The consumer (`bins/server/src/vol_agent.rs`) is already deleted. Update `api_*` tests that assert `agent_token`.
- `deploy/k3s/rotate-agent-token.sh` (rotates that token) and its mentions in `deploy/RECOVERY.md`, `deploy/BACKUPS.md`, `deploy/k3s/README.md`.
- `tests/ws_e2e.sh`: `RUSTIC_GIT_VOL_AGENT_TOKENS`, `VOL_AGENT_TOKEN`, the `/vol-agent` comments; `deploy/k3s/env.example.sh:30` and `deploy/k3s/env.sh:29` `WS_REGISTRY_URL` lines.
- Stale prose: `CLAUDE.md:131-132` (`vol/{owner}/{id}` in `vol_agent.rs`), `:180-182` (`/v1` writes a `SnapshotRequest`; history reads `done` SnapshotRequests), `:198` (`WS_REGISTRY_URL`); `README.md:42,88,110,138,198,207` (SnapshotRequest as a CRD, `/vol-agent`, `repo/vol/{owner}/{id}`). Rewrite to the commit model + sync points.
- `deploy/k3s/agent-daemonset.yaml`: `RUST_BACKTRACE` and `XDG_CACHE_HOME` are set on the agent container and read by nothing there — delete; `NIX_REMOTE` stays only if the nix-daemon sidecar reads it (check `nix-conf.yaml`/the sidecar spec).
- Reads with no writer — add each to the daemonset with its default and a one-line WHY, so config is discoverable: `WS_NODE_DEAD_SECS=600`, `WS_PEER_SEND_TIMEOUT_SECS`, `WS_SNAPSHOT_KEEP=10`; `WS_RUNTIME_CLASS` is deliberately unset (comment exists) — leave; `WS_PEER_SECRET` and `WS_REGION` come from the Secret — leave.
- `crates/workspaces/src/k8s.rs` `HOME_LOCAL_DIRS`: sole use is the runbook's rsync exclude list; move the list INTO `deploy/k3s/README.md` verbatim and delete the constant + its doc.
From the whole-repo audit (verified; rulings inline):
- `delete` the rest of the `agent_token` surface the first bullet names: `WS_AGENT_HEADER`, `list_regions`' `clear()` loop, and its test scaffolding — `crates/workspaces/tests/api_user.rs:445-489` (rotate round-trip), `agent_token` fields at `api_teams.rs:65`, `api_user.rs:200,1080`, `meta_store.rs:15,27,31`. ~120 test lines; each named in the report.
- `delete` `pub const MERGE_LEASE` and its doc — zero references. [`crates/app/src/lib.rs:595`]
- `delete` `SnapshotStatus.size_bytes` — constructed `None` at all four sites, never read. [`crates/workspaces/src/crd.rs:256`, `api.rs:1657`, `controller.rs:1809,2788`, `crd.rs:976`]. Regenerate crds.yaml.
- `delete` `OwnerBindingSpec.home_quota_gb` + `DEFAULT_HOME_QUOTA_GB` + `default_home_quota_gb()` — read by nothing; a structural CRD prunes the stored field on read, so no deser risk. [`crd.rs:653`, `claim.rs:306`]. Regenerate crds.yaml.
- `delete` dependency `object_store` from `bins/agent/Cargo.toml:34` and `zstd` from `crates/workspaces/Cargo.toml:42` — zero references in either crate's src/tests (verified).
- `stdlib` `crates/core::hex` is `hex::encode` verbatim; call `hex::encode` at the ~15 sites, delete fn + doc + test. [`crates/core/src/err.rs:38`]
- `delete` `gpg::emails_of` (alias of `verified_emails`, one caller) — make `verified_emails` pub. [`crates/api/src/gpg.rs:120`, `credentials.rs:335`]
- `delete` `browse_api::pulls::now_ms` (3-line wrapper around `ownership::now_ms() as i64`) — cast at call sites. [`bins/server/src/browse_api/pulls.rs:37`]
- `delete` `nix_volume` and `agent_secret_binding` — one-expression wrappers with one call site each; inline. [`crates/workspaces/src/k8s.rs:626`]
- `shrink` `registry::{routing_key,pool_coords}` and `workspaces::registry::{routing_key,pool_coords}` differ only in `"img"` vs `"vol"`; one pair taking `kind: &str` in `crates/storage`. [`crates/workspaces/src/registry.rs:45`]
- `shrink` `env_namespace`'s 5-line match to one expression. [`crd.rs:773`]
- `shrink` the three one-implementation traits `MembershipCheck`/`CliTokenCheck`/`AuthorizedKeys` (all implemented only by `bins/api`'s `Dir`, held as three `Option<Arc<dyn>>`) into one `trait Directory` and one `ApiState` field; one stub per test file instead of three. [`crates/workspaces/src/api.rs:46`]. Ruling: last in the batch — largest blast radius, do it after everything else is green.
- Ruling — REJECTED: "delete `VolumeReplicaStatus.last_sync_at`". True that nothing reads it today; Task 5's flush gate reads it. Keep.
- Ruling — REJECTED: "yagni `runtimeclass.yaml` + `install-gvisor.sh`, `WS_RUNTIME_CLASS` unset". The daemonset (`agent-daemonset.yaml:145-152`) documents that unset is deliberate: tenant pods pick gvisor from the node label, and the env var is the operator override. Keep all three.
- Ruling — the three unset agent knobs (`WS_NODE_DEAD_SECS`, `WS_PEER_SEND_TIMEOUT_SECS`, `WS_SNAPSHOT_KEEP`): the audit says inline the defaults and delete the readers; this plan sets them in the daemonset instead. Two of them are tuned during Task 9's node-death test, so they are operator knobs, not constants. Cost if wrong: three env lines.
- Verified NOT dead, no action: `RUSTIC_GIT_METRICS_ADDR` (read via `metrics::init`), the web/nginx/build env vars, all test helpers in `tests/common` and per-crate `tests/*.rs`.

Audit's own estimate for its items: −410 lines, −2 deps.

- [ ] **Step 1:** record test counts per touched crate. **Step 2:** delete/rewrite. **Step 3:** gates, `bash -n` on touched scripts, counts reconciled with every disappeared test named. **Step 4: commit** `Delete the object-store era's leftovers`.

### Task 9: Rollout and live verification (controller-run)

- [ ] pin → roll agents → `kubectl get snapshots` shows one `sync-…` per running worktree within 2×`WS_SYNC_SECS`; write in a pod, confirm a NEW transient replaces the old within the interval and the replica row on the other node goes `Syncing→Synced`.
- [ ] Stop a workspace: pod deletion happens only after the other node's replica is `Synced` past the flush.
- [ ] Re-host: with `env-0` carrying the `session` role, delete the workspace's pod AND unclaim it (`status.nodeName=""`), confirm whichever node claims checks out `sync-…` and the last edit is present.
- [ ] Node death: stop kubelet on one node > `WS_NODE_DEAD_SECS`; the survivor claims; the edit from the last sync interval is there; anything after it is the documented loss window.

## Self-review

- Spec coverage: beat ✓T2, cut/no-head/retention ✓T3, replication ✓T6, flush+gate+timeout ✓T5, re-host ✓T4, config/costs ✓T7, cleanup ✓T8, `Engine::generation` restore ✓T1.
- Type consistency: `SnapshotSpec.transient` (T1) used by T2/T3/T5; `latest_transient(ctx, volume, worktree)` (T4) reused by T5 for the stop parent; `SYNCED_GENERATION` (T2) read by T3/T4; `ready_at` added in T5 (regenerate crds.yaml there).
- Placeholder scan: T8's audit line is intentionally open until the audit lands; everything else names files, symbols and tests.
