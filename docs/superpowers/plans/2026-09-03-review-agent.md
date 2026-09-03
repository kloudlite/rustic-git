# Agent Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land every finding in `docs/superpowers/reviews/2026-09-03-details/agent.md` — 3 Critical, 6 Important, 7 Minor, 9 Cleanup — plus two audit cuts (one shared `cas` helper for the four hand-built Test+Replace patches, and deletion of the agent-side `VolumeSource::RestoreOf` arms), without changing what the agent decides in any case the review calls correct.

**Architecture:** `bins/agent` is a controller, not a worker: it watches its own node's objects and converges them. Every fix here keeps the two rules the crate is built on — a sweep is keep-biased (a fresh read before any delete, a partial view acts on nothing), and a running working copy never moves. Three Criticals are missing keep-bias guards on delete paths; the Importants are cost and bounding (blocking work off the reactor, a floor on a peer-driven beat, a server-side send bound, a receive ceiling, a correct ordering key, a node-scoped watch); the rest is mechanical.

**Tech Stack:** Rust, `kube` 0.99-era (`Api`, `Controller`, `watcher::Config`), `json_patch`, `axum` for the peer listener, `tokio` (`spawn_blocking` for all btrfs work), `tempfile` + `rustic_git_workspaces::kube_test` (`mock_client`/`Recorder`/`Route`) for tests.

**Spec:** `docs/superpowers/specs/2026-09-03-stop-interrupt-decommission-design.md` and `docs/superpowers/specs/2026-09-03-durable-snapshots-design.md` (the vocabulary note — read **snapshot** for **commit** — and simplifications 2, 6, 9, 11 are cited by individual tasks). Review: `docs/superpowers/reviews/2026-09-03-details/agent.md`; summary `docs/superpowers/reviews/2026-09-03-codebase-review.md`.

## Global Constraints

- **No tool attribution anywhere.** Commit subjects are imperative sentence case; no `Co-Authored-By`, no `Generated with`, no model name in code, comments or messages.
- **Comments say WHY, never what.** Match the density of `bins/server/src/router/route.rs`. Do not add a comment that restates the line under it.
- **Keep every `// ponytail:` marker** you edit near. A marker whose ceiling has moved gets its text corrected (Task 22); a marker whose code is deleted goes with it and the plan says so explicitly.
- **A running working copy never moves.** No task may make a `Running` worktree's volume releasable, unpinnable, or deletable.
- **Every sweep is keep-biased:** a fresh read immediately before a delete, an unreadable or partial view deletes nothing, a list error aborts the whole pass.
- **Gate for every task** (run unpiped, exactly this, both commands, before the commit step):
  ```
  cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings
  ```
  `--test-threads=1` because several tests in this crate set process env (`WS_DEFAULT_IMAGE`, `WS_NODE_DEAD_SECS`). Expect `exit=0`. Clippy `--all-targets` has pre-existing lints in test targets: the bar is **no new warning in a file you touched**.
- **Line numbers in this plan are from master `f87fddb1`.** They drift as you land tasks; the symbol names are the real address, so if a line number misses, `grep -n` the function name.

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `bins/agent/src/peer.rs` | Shrinks from 3444 lines to the module root: `PeerState`, the router, the two handlers (`commit`, `wake`), auth, and the shared small helpers. Tasks 1–7 land here first; Task 18 splits it. |
| `bins/agent/src/peer/pull.rs` (new, Task 18) | `pull_beat`, `pull_beat_with`, `interesting_volumes`, `pull_volume`, `pull_one`, `nearest_held_ancestor`, `retired`, `write_replica_status`. |
| `bins/agent/src/peer/sweeps.rs` (new, Task 18) | `volume_decision`, `sweep_volumes`, `sweep_dead_nodes`, `reap_dead_replicas`, `mark_parent`, `mark_parent_of`, `unplace_parent`, `retire_pass`, `should_retire`, `orphan_voldirs`, `orphan_snaps`, `sweep_orphan_snapshots`, `sweep_orphan_snap_bytes`, `collect_unreferenced_volumes`. |
| `bins/agent/src/peer/wake.rs` (new, Task 18) | `wake_peers`, `Next`, `after_pass`, `retry_delay`, `RETRY_SOON`, `MIN_WAKE_GAP`. |
| `bins/agent/src/peer/placement.rs` (new, Task 18) | `pool_nodes`, `placeable_nodes`, `live_nodes`, `standby_count`, `preferred_node`, `up_to_date`, `up_to_date_nodes`, `newest_transient`, `node_is_dead`, `decommissioning`, `unplaceable`, `node_dead_secs`. |
| `bins/agent/src/controller/volume.rs` | Gains `cas` (Task 17), loses the four hand-built patches and the two `RestoreOf` arms (Task 16). |
| `bins/agent/src/controller/worktree.rs` (new, Task 19) | `worktree_gate` — the ~120-line start/head/checkout sequence shared by `apply_workspace` and `apply_environment`, plus the eight-line start-spread block. |
| `bins/agent/src/controller/workspace.rs` | Loses the duplicated block to Task 19; `cleanup_parent` gains the `seeded_from_cuts` guard (Task 2); `kept_conditions` gains two types (Task 10). |
| `bins/agent/src/controller/environment.rs` | Loses the duplicated block to Task 19; `drain_services` halves its poll rate (Task 12). |
| `bins/agent/src/snapshot.rs` | `seeded_from_cuts` becomes `pub(crate)` (Task 2); `reconcile_commit` gains the Volume-store pre-filter (Task 9). |
| `bins/agent/src/sync.rs` | `sync_one`'s ordering key becomes `crd::transient_generation_of` (Task 8). |
| `bins/agent/src/janitor.rs` | `btrfs_delete` stops unwrapping (Task 13). |
| `crates/workspaces/src/kube_test.rs` | Gains `agent_test_ctx` is **not** placed here — see Task 23; it gains nothing. |
| `bins/agent/src/testsupport.rs` (new, Task 23) | The one `NoopNix` + `test_ctx` the five in-crate test modules share. |
| `crates/workspaces/src/crd.rs` | Loses `compatible_nodes` from both status structs (Task 20) and `VolumeSource::RestoreOf` is removed **by the workspaces-api plan, not this one** (Task 16 depends on that). |
| `deploy/k3s/agent-rbac.yaml` | Loses the `apps/deployments` rule and three stale table rows (Task 21). |

---

## Task 1: C1 — a fresh GET before `retired()` deletes bytes

**Files:**
- Modify: `bins/agent/src/peer.rs:552-570` (`retired`'s doc + body) and `:676-690` (the caller loop inside `pull_volume`)
- Test: `bins/agent/src/peer.rs` `mod reconcile_tests` (in-file)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `retired` keeps its signature `fn retired(have: &HashSet<String>, existing: &HashSet<String>, any_pull_failed: bool) -> Vec<String>` — it stays a pure decision. The guard is added at the **call site**, which is where the API client lives. Task 18 moves both into `peer/pull.rs` unchanged.

**Why:** `pull_volume` lists the volume's `Snapshot` CRs first, reads `local_commits` after, and at the end deletes every local name absent from that listing. The `Snapshot` reconciler runs concurrently in the same process: a push whose CR is created after the list and whose btrfs cut lands before the delete is on disk and absent from `existing`, so a `Ready` user push loses its bytes while its record survives — the next `checkout` fails `NO_SUCH_RECORD` and `permanent_reason` makes that terminal. `sweep_orphan_snap_bytes` closes exactly this race with one fresh `get_opt` per candidate; `retired()` deletes on the same evidence with no such guard, and the window is a whole `btrfs receive` of tens of GiB.

- [ ] **Step 1: Write the failing test**

Add to `mod reconcile_tests` in `bins/agent/src/peer.rs`, beside `pull_volume_keeps_everything_on_a_snapshot_list_error`:

```rust
    /// C1: a snapshot whose CR appeared AFTER this pass's listing (a push racing the pull) is on
    /// disk and absent from `existing` — the fresh GET is what stops the pull beat deleting a
    /// Ready push's bytes. The `snap/` directory standing in for a subvolume is enough: the
    /// assertion is that the GET happened and the directory survived.
    #[tokio::test]
    async fn a_push_that_landed_during_the_pass_is_never_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let snap_dir = tmp.path().join("vol").join("vol-1").join("snap");
        std::fs::create_dir_all(snap_dir.join("push-late")).unwrap();
        let routes = vec![
            // The pass's own listing: empty, taken before the push's CR existed.
            get(SNAPSHOTS, list_of("Snapshot", vec![])),
            // The fresh per-candidate GET: the CR is there now.
            get(format!("{SNAPSHOTS}/push-late"), ready_snapshot("push-late", "vol-1", "")),
            not_found(format!("{VOLREPLICAS}/{}", crd::replica_name("vol-1", "node-b"))),
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let http = peer_http_client().unwrap();

        pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1", &[]).await;

        assert!(
            rec.calls().contains(&format!("GET {SNAPSHOTS}/push-late")),
            "the retire loop must GET each candidate fresh before deleting it: {:?}",
            rec.calls()
        );
        assert!(snap_dir.join("push-late").exists(), "a snapshot whose CR exists must keep its bytes");
    }

    /// The other half: a name whose CR really is gone still gets dropped, so the guard did not
    /// turn the retire into a no-op.
    #[tokio::test]
    async fn a_snapshot_with_no_cr_at_all_is_still_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let snap_dir = tmp.path().join("vol").join("vol-1").join("snap");
        std::fs::create_dir_all(snap_dir.join("gone")).unwrap();
        let routes = vec![
            get(SNAPSHOTS, list_of("Snapshot", vec![])),
            not_found(format!("{SNAPSHOTS}/gone")),
            not_found(format!("{VOLREPLICAS}/{}", crd::replica_name("vol-1", "node-b"))),
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let http = peer_http_client().unwrap();

        pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1", &[]).await;

        assert!(rec.calls().contains(&format!("GET {SNAPSHOTS}/gone")));
        assert!(!snap_dir.join("gone").exists(), "a name with no CR is still reclaimed");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-agent-bin a_push_that_landed_during_the_pass -- --test-threads=1; echo exit=$?`
Expected: FAIL — no `GET /apis/rustic-git.io/v1alpha1/snapshots/push-late` in `rec.calls()`, and the directory is gone.

- [ ] **Step 3: Add the guard at the call site**

In `bins/agent/src/peer.rs`, replace the retire loop inside `pull_volume` (the block introduced by the comment "Drop any local commit whose CR is gone entirely"):

```rust
    // Drop any local commit whose CR is gone entirely (not merely `Working` — `existing` holds
    // every phase). `drop_commit` is Ok-on-absent, so every node that ever held a copy converges
    // on the same disk state without a second round trip to confirm it.
    // Gated on `any_pull_failed` — see `retired`.
    for name in retired(&have, &existing, any_pull_failed) {
        // `existing` was listed before `local_commits`, and the Snapshot reconciler cuts in this
        // same process: a push whose CR was created inside that window is on disk and absent from
        // the listing, and deleting it loses a Ready push nothing can recover. One fresh GET per
        // candidate, exactly as `sweep_orphan_snap_bytes` does; a present record OR a failed GET
        // keeps the bytes. Candidates are rare, so this is a GET per reclaim, not per pass.
        if !matches!(snap_api.get_opt(&name).await, Ok(None)) {
            continue;
        }
        // btrfs delete takes a blocking flock and shells out — never on the reactor thread.
        let (engine, vol, cname) = (ctx.engine.clone(), volume.to_string(), name.clone());
        match tokio::task::spawn_blocking(move || engine.drop_commit(&vol, &cname)).await {
            Ok(Ok(())) => {
                have.remove(&name);
            }
            Ok(Err(e)) => tracing::warn!(%volume, snapshot = %name, error = %e, "pull: dropping a retired commit failed; left for the next pass"),
            Err(e) => tracing::warn!(%volume, snapshot = %name, error = %e, "pull: the retire drop task panicked"),
        }
    }
```

- [ ] **Step 4: Correct `retired`'s doc**

The function is still the decision; only the evidence changed. Append to its doc comment, above the existing `ponytail:` marker (**keep that marker verbatim** — the all-or-nothing ceiling it names has not moved):

```rust
/// The names this returns are CANDIDATES, not verdicts: the caller re-GETs each one before
/// deleting anything, because this list is computed from a Snapshot listing taken before
/// `local_commits` and a push cut in that window is on disk and absent from it.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning in `peer.rs`.

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/peer.rs
git commit -m "Re-read a snapshot record before the pull beat drops its bytes"
```

**RBAC / admission:** none. `get` on `snapshots` is already granted (the byte sweep makes the same call); no spec write, so the ValidatingAdmissionPolicy is untouched.

---

## Task 2: C3 — `cleanup_parent` never deletes a cut a `SeededFrom` volume still names

**Files:**
- Modify: `bins/agent/src/snapshot.rs:232` (`seeded_from_cuts` visibility)
- Modify: `bins/agent/src/controller/workspace.rs:596-638` (`cleanup_parent`, deletion loop at `:615-617`)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `pub(crate) async fn seeded_from_cuts(ctx: &Arc<Ctx>, volume: &str) -> Result<std::collections::HashSet<String>, ReconcileErr>` — unchanged body, `async fn` → `pub(crate) async fn`. `cleanup_parent`'s signature is unchanged: `async fn cleanup_parent(id: &str, uid: &str, volume: Option<String>, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>`.

**Why:** `retain` consults `seeded_from_cuts` and refuses to prune a transient that a not-yet-materialized `SeededFrom` Volume names — the spec's one recovery path for an interrupted parent (node A dies, someone clones the interrupted workspace, then deletes the source). `cleanup_parent` deletes **every** non-snapshot record of the worktree with no such check, so the ordinary delete destroys the exact cut the rescue clone is seeding from and the clone settles `Permanent/NoSuchSnapshot`.

- [ ] **Step 1: Write the failing test**

Add to `bins/agent/tests/reconcile.rs` (follow the file's existing `mock_client` + `Recorder` idiom; `SNAPSHOTS`/`VOLUMES` path consts are already defined there — reuse them, and if a const is missing add it beside its siblings):

```rust
/// C3: deleting an interrupted workspace must not delete the sync point a rescue clone is
/// seeding from. `retain` has this rule; the delete path did not, so an ordinary delete
/// destroyed the documented recovery for an interrupted parent.
#[tokio::test]
async fn deleting_a_parent_keeps_a_sync_point_a_seeded_clone_still_names() {
    let tmp = tempfile::tempdir().unwrap();
    let seeded_vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-rescue", "uid": "v-rescue"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-b", "region": "r1", "quotaGb": 5,
                 "replicas": 2, "source": {"seededFrom": {"volume": "vol-1", "snapshot": "sync-a"}}},
        // Not Ready: an already-materialized clone has copied the bytes and released the pin.
        "status": {"phase": "creating", "subvolumePresent": false}
    });
    let routes = vec![
        get(SNAPSHOTS, list_of("Snapshot", vec![
            transient_snapshot("sync-a", "vol-1", "ws-1"),
            transient_snapshot("sync-b", "vol-1", "ws-1"),
        ])),
        get(VOLUMES, list_of("Volume", vec![seeded_vol])),
        Route { method: "DELETE", path: format!("{SNAPSHOTS}/sync-b"), status: 200, body: serde_json::json!({"status": "Success"}) },
    ];
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

    cleanup_parent_for_test(&ctx, "ws-1", "ws-uid", "vol-1").await.expect("cleanup succeeds");

    assert!(
        !rec.calls().contains(&format!("DELETE {SNAPSHOTS}/sync-a")),
        "a cut a SeededFrom volume names must survive its parent's delete: {:?}",
        rec.calls()
    );
    assert!(
        rec.calls().contains(&format!("DELETE {SNAPSHOTS}/sync-b")),
        "every other sync point of the worktree still goes: {:?}",
        rec.calls()
    );
}

/// A failed Volume listing must delete NOTHING: a half-seen set is exactly the case that drops
/// the cut somebody is waiting on, and the finalizer retries the whole cleanup on an Err.
#[tokio::test]
async fn a_failed_seeded_listing_deletes_no_sync_points() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        get(SNAPSHOTS, list_of("Snapshot", vec![transient_snapshot("sync-a", "vol-1", "ws-1")])),
        Route { method: "GET", path: VOLUMES.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) },
    ];
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

    let out = cleanup_parent_for_test(&ctx, "ws-1", "ws-uid", "vol-1").await;

    assert!(out.is_err(), "a partial view must requeue the finalizer, not proceed");
    assert!(
        rec.calls().iter().all(|c| !c.starts_with("DELETE ")),
        "nothing is deleted on a partial view: {:?}",
        rec.calls()
    );
}
```

`transient_snapshot` (add it beside the file's other fixture builders if absent):

```rust
fn transient_snapshot(name: &str, volume: &str, worktree: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name, "uid": format!("uid-{name}")},
        "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": "", "transient": true},
        "status": {"phase": "ready"},
    })
}
```

`cleanup_parent` is private to `controller::workspace`. Expose it to the integration suite the way the crate already exposes its other internals — add, in `bins/agent/src/controller/workspace.rs` directly above `cleanup_parent`:

```rust
/// The integration suite drives the delete path directly: the finalizer combinator around it is
/// `kube`'s, not ours, and mocking a finalizer round trip tests that crate rather than this rule.
#[doc(hidden)]
pub async fn cleanup_parent_for_test(ctx: &Arc<Ctx>, id: &str, uid: &str, volume: &str) -> Result<Action, ReconcileErr> {
    cleanup_parent(id, uid, Some(volume.to_string()), ctx).await
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-agent-bin --test reconcile deleting_a_parent_keeps_a_sync_point -- --test-threads=1; echo exit=$?`
Expected: FAIL — `DELETE .../snapshots/sync-a` is in `rec.calls()` (and, before the helper exists, a compile error naming `cleanup_parent_for_test`).

- [ ] **Step 3: Make `seeded_from_cuts` reachable**

In `bins/agent/src/snapshot.rs:232`:

```rust
pub(crate) async fn seeded_from_cuts(ctx: &Arc<Ctx>, volume: &str) -> Result<std::collections::HashSet<String>, ReconcileErr> {
```

Body unchanged. Its doc comment already states the rule; append one line:

```rust
/// Read by BOTH reclaimers of a sync point — `retain` and the delete path's `cleanup_parent`.
/// Two copies of this predicate is how one of them deletes what the other is protecting.
```

- [ ] **Step 4: Consult it in `cleanup_parent`**

In `bins/agent/src/controller/workspace.rs`, between the `snaps.list(...)` that fills `items` and the deletion loop:

```rust
    // The same predicate `retain` applies, for the same reason: an interrupted parent's rescue
    // clone (`VolumeSource::SeededFrom`) names one of these cuts by id and has not copied the
    // bytes yet. Deleting it settles that clone `Permanent/NoSuchSnapshot` — the documented
    // recovery path destroyed by an ordinary delete. An Err, never an empty set: the finalizer
    // retries, and a half-seen listing is exactly the case that deletes what is still needed.
    let seeded = crate::snapshot::seeded_from_cuts(ctx, &volume).await?;
    for s in items.iter().filter(|s| !s.is_snapshot() && s.spec.worktree == id && !seeded.contains(&s.name_any())) {
        delete_ignoring_404(&snaps, &s.name_any()).await?;
    }
```

(replacing the existing `for s in items.iter().filter(|s| !s.is_snapshot() && s.spec.worktree == id)` loop).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning.

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/snapshot.rs bins/agent/src/controller/workspace.rs bins/agent/tests/reconcile.rs
git commit -m "Keep sync points a seeded clone still names when its parent is deleted"
```

**RBAC / admission:** none. The added call is `list volumes`, already granted (`retain` makes the same call from the same ServiceAccount). No spec write.

---

## Task 3: C2 — the sweep un-places parents left behind by a crash mid-release

**Files:**
- Modify: `bins/agent/src/peer.rs:975-1088` (`sweep_volumes`, the skip guard at `:985-987`)
- Modify: `bins/agent/src/controller/stop.rs:190-192` (the comment that is only true for the spread path)
- Test: `bins/agent/src/peer.rs` `mod reconcile_tests`

**Interfaces:**
- Consumes: nothing from Tasks 1–2.
- Produces: `sweep_volumes` keeps its signature `pub(crate) async fn sweep_volumes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, owners: &HashSet<String>, reason: &'static str, mark_running: bool)`. Task 18 moves it to `peer/sweeps.rs` unchanged.

**Why:** the release arm clears `spec.nodeName` first (correct — a failed CAS with parents already cleared leaves them claimable on a node that does not own the volume), then un-places each parent. A crash between the two leaves an empty pin and parents still carrying `status.nodeName = <dead node>`, and **nothing recovers**: the next beat skips the volume (`owner.is_empty() → continue`), no live node's parent watch matches (`status.nodeName={me}`), the unplaced claim watch (`status.nodeName=`) does not match either because the field is not empty, and `resolve_volume`'s mismatch self-heal only runs on the node named — the dead one. The workspace can never start again without `kubectl patch --subresource=status`.

- [ ] **Step 1: Write the failing test**

Add to `mod reconcile_tests` in `bins/agent/src/peer.rs`:

```rust
    /// C2: the crash window. The volume's pin is already empty (the release CAS landed) and its
    /// parents still name the dead node (the un-place did not). No watch anywhere matches that
    /// state, so the sweep is the only thing that can free it — it must not skip the volume for
    /// having no owner.
    #[tokio::test]
    async fn an_empty_pinned_volume_with_placed_parents_is_still_unplaced() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-1", "uid": "ws-uid"},
            "spec": {"owner": "alice", "name": "ws-1", "desiredState": "Stopped"},
            "status": {"phase": "stopped", "nodeName": "node-dead", "volumeRef": "vol-1"},
        });
        let routes = vec![
            get(format!("{WORKSPACES}/ws-1"), ws.clone()),
            Route { method: "PUT", path: format!("{WORKSPACES}/ws-1/status"), status: 200, body: ws },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        // The volume as the crash left it: empty pin, and a parent still placed on the dead node.
        let vol = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "vol-1", "uid": "v1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "", "region": "r1",
                     "quotaGb": 5, "replicas": 2},
        });
        let mut parent = parent_at("Workspace", "ws-1", "vol-1", crd::Phase::Stopped, true);
        parent.node_name = "node-dead".into();
        parent.pod_ref = None;
        let beat = crate::listing::Beat {
            volumes: vec![serde_json::from_value(vol).unwrap()],
            replicas: vec![],
            parents: vec![parent.clone()],
            all_parents: vec![parent],
        };
        let dead: HashSet<String> = ["node-dead".to_string()].into_iter().collect();

        sweep_volumes(&ctx, &beat, &dead, "NodeDead", true).await;

        let sent = rec.sent("PUT", &format!("{WORKSPACES}/ws-1/status"));
        assert_eq!(sent.len(), 1, "the stranded parent must be un-placed exactly once: {:?}", rec.calls());
        assert_eq!(sent[0]["status"]["nodeName"], "", "un-place clears the parent's pin: {}", sent[0]);
        assert!(
            rec.calls().iter().all(|c| !c.starts_with("PATCH ")),
            "the pin is already clear; nothing re-patches the volume spec: {:?}",
            rec.calls()
        );
    }

    /// The guard's other side: an empty-pinned volume whose parents are ALSO unplaced is nobody's
    /// business, and the sweep must not walk it every beat forever.
    #[tokio::test]
    async fn an_empty_pinned_volume_with_no_placed_parents_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![]);
        let vol = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "vol-1", "uid": "v1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "", "region": "r1", "quotaGb": 5, "replicas": 2},
        });
        let mut parent = parent_at("Workspace", "ws-1", "vol-1", crd::Phase::Stopped, true);
        parent.node_name = String::new();
        parent.pod_ref = None;
        let beat = crate::listing::Beat {
            volumes: vec![serde_json::from_value(vol).unwrap()],
            replicas: vec![],
            parents: vec![parent.clone()],
            all_parents: vec![parent],
        };
        let dead: HashSet<String> = ["node-dead".to_string()].into_iter().collect();

        sweep_volumes(&ctx, &beat, &dead, "NodeDead", true).await;

        assert!(rec.calls().is_empty(), "an already-converged volume costs no writes: {:?}", rec.calls());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-agent-bin an_empty_pinned_volume -- --test-threads=1; echo exit=$?`
Expected: FAIL — `an_empty_pinned_volume_with_placed_parents_is_still_unplaced` sees zero PUTs, because the `owner.is_empty()` guard skipped the volume.

- [ ] **Step 3: Make an unowned volume a sweepable case**

In `bins/agent/src/peer.rs`, inside `sweep_volumes`, replace the skip guard:

```rust
    for vol in beat.volumes.iter().cloned() {
        let owner = vol.spec.node_name.clone();
        let name = vol.name_any();
        // `all_parents`, not `parents`: this volume is owned by another node, so this node's own
        // scoped list would show none of its parents and every arm would read as "nothing on it".
        let parents: Vec<&crate::listing::Parent> = beat.all_parents.iter().filter(|p| p.volume == name).collect();
        // An EMPTY pin with parents still placed is the crash window between the release CAS and
        // the un-place: no watch matches such a parent (`status.nodeName` is neither this node nor
        // empty) and `resolve_volume`'s self-heal only runs on the node it names — the dead one.
        // The sweep is the only thing that can see it, so it finishes the release rather than
        // skipping the volume for having no owner. Nothing is re-patched: the pin is already clear.
        let stranded = owner.is_empty() && parents.iter().any(|p| !p.node_name.is_empty());
        if !stranded && (owner.is_empty() || !owners.contains(&owner)) {
            continue;
        }
```

then, further down, make the release arm skip its CAS when the pin is already clear. Replace `let mut cur = vol; if release {` with:

```rust
        let mut cur = vol;
        // A stranded volume is already released — its verdict is the un-place below, and there is
        // no pin left to compare-and-set.
        if release && !stranded {
```

and, at the verdict `match`, let a stranded volume fall straight through to the parent loop with `release: true`:

```rust
        let (why, reason, release) = if stranded {
            (
                format!("volume {name} has no owner; finishing an interrupted release"),
                reason,
                true,
            )
        } else {
            match volume_decision(&name, &owner, &parents, reason) {
                VolumeVerdict::Mark { .. } if !mark_running => continue,
                VolumeVerdict::Mark { why } => (why, reason, false),
                VolumeVerdict::Release { why, reason } => (why, reason, true),
            }
        };
```

The parent loop and the status write below are unchanged: a stranded volume takes the same `("Degraded", true)`/`("Placed", false)` choice its caller already makes, and `mark_parent_of`'s own idle check keeps a converged parent from being rewritten every beat.

- [ ] **Step 4: Correct the comment the fix falsifies**

In `bins/agent/src/controller/stop.rs:190-192`, the claim "a cleared pin with placed parents self-heals through the mismatch branch" is true for the spread path (the named node is the live owner) and false for the dead-node sweep, which applied it too. Replace with:

```rust
    // The two-step move, deliberately kept over an owner-writes-the-target handoff: a handoff
    // would need the admission policy to allow ANY `nodeName` change, and this reuses the CAS the
    // takeover path already proved. Pin first, parents second — the reverse leaves parents
    // claimable on a node that does not own the volume. A crash BETWEEN them leaves a cleared pin
    // with placed parents: here that self-heals through `resolve_volume`'s mismatch branch,
    // because the node named is this live owner. It does not when a DEAD node's sweep crashes
    // there, which is why `sweep_volumes` treats an empty pin with placed parents as its own case.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`. Both new tests pass and every existing `sweep_volumes` test still passes — in particular the drain tests, which pass `mark_running: false` and must be unaffected (a drained node's volumes have a non-empty pin).

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/peer.rs bins/agent/src/controller/stop.rs
git commit -m "Finish an interrupted volume release from the sweep"
```

**RBAC / admission:** none new. The un-place is a `status` PUT on `workspaces`/`environments`, already granted; the ValidatingAdmissionPolicy (`deploy/k3s/agent-admission.yaml`) fences **spec** writes and this path now makes strictly fewer of them (no CAS on an already-empty pin).

---

## Task 4: I1 — btrfs and directory walks off the reactor in `retire_pass`

**Files:**
- Modify: `bins/agent/src/peer.rs` — `retire_pass` (`janitor::cleanup_local` for orphan voldirs, `janitor::drop_stale_worktrees`, `cleanup_local` for a retired copy), `interesting_volumes`' `voldir(&id).exists()`, `retire_pass`'s own `voldir(&id).exists()`
- Test: `bins/agent/src/peer.rs` `mod reconcile_tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `interesting_volumes` becomes `async`: `async fn interesting_volumes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, live: &[String]) -> Vec<String>`. Its one caller is `pull_beat_with`, which is already async — change `for id in interesting_volumes(ctx, &beat, &live) {` to `for id in interesting_volumes(ctx, &beat, &live).await {`.

**Why:** `janitor::cleanup_local` walks the tree with `std::fs::read_dir` and shells out to `std::process::Command::new("btrfs").…status()` per subvolume. A volume with many snapshots is seconds to minutes of a reactor thread, and `controller/mod.rs`'s own module doc states the rule ("Long btrfs work runs on `spawn_blocking` … a `LocalSet`/single-reactor-thread design would let one workspace's lock wait freeze every other in-flight operation"). `sweep_orphan_snap_bytes`, two functions up, already obeys it. Every reconcile and every peer `btrfs send` on the node stalls behind a retire.

- [ ] **Step 1: Write the failing test**

There is no way to assert "did not block the reactor" from a unit test without a clock. Assert the *shape* instead — the thing that regresses is a direct call, and the thing that proves the fix is that the work still happens when driven from a `current_thread` runtime that would deadlock on a blocking call holding its only thread. Add to `mod reconcile_tests`:

```rust
    /// I1: the retire's btrfs work must not run on the reactor. Driven on a single-threaded
    /// runtime with a concurrent task that must make progress WHILE the retire runs: with
    /// `cleanup_local` called inline the sleeper cannot be polled until the walk finishes, and
    /// with it on `spawn_blocking` it can. The orphan voldir is a real directory tree, so the
    /// walk is real work either way.
    #[test]
    fn the_retire_pass_does_not_walk_the_pool_on_the_reactor() {
        let tmp = tempfile::tempdir().unwrap();
        // 200 nested directories: enough walking that an inline call is unambiguously ordered
        // before the sleeper, without making the test slow.
        for i in 0..200 {
            std::fs::create_dir_all(tmp.path().join("vol").join("orphan").join("snap").join(format!("c{i}"))).unwrap();
        }
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (ctx, _rec) = test_ctx(tmp.path(), "node-a", vec![get(SNAPSHOTS, list_of("Snapshot", vec![]))]);
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let f = flag.clone();
            let ticker = tokio::spawn(async move {
                tokio::task::yield_now().await;
                f.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            retire_pass(&ctx, &beat_of(vec![], vec![], vec![]), &[]).await;
            ticker.await.unwrap();
            assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
            assert!(!tmp.path().join("vol").join("orphan").exists(), "the orphan voldir is still reclaimed");
        });
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-agent-bin the_retire_pass_does_not_walk -- --test-threads=1; echo exit=$?`
Expected: FAIL — the assertion on the reclaimed directory passes, but the test hangs or the `spawn_blocking` call sites do not exist yet; on a `current_thread` runtime the inline walk starves `ticker`. (If it happens to pass by scheduling luck, it still fails at Step 5's clippy or review — the code change below is the deliverable and the test is its regression net.)

- [ ] **Step 3: Move each of the three call sites onto `spawn_blocking`**

In `bins/agent/src/peer.rs`, `retire_pass`, the orphan voldir loop:

```rust
    for id in orphan_voldirs(&ctx.engine.pool.root.join("vol"), &known) {
        tracing::info!(volume = %id, "pull: retire: no Volume CR; dropping the orphaned local copy");
        // A voldir walk plus one `btrfs subvolume delete` per subvolume under it: seconds to
        // minutes of a thread, and this beat shares its reactor with every reconcile and every
        // peer send on the node. Same rule `sweep_orphan_snap_bytes` follows two functions up.
        let (engine, vol) = (ctx.engine.clone(), id.clone());
        if let Err(e) = tokio::task::spawn_blocking(move || janitor::cleanup_local(&engine, &vol)).await {
            tracing::warn!(volume = %id, error = %e, "pull: retire: the orphan-voldir cleanup task panicked");
        }
    }
```

the stale-worktree drop inside the `!should_retire` arm:

```rust
                if let Ok(Some(fresh)) = Api::<crd::Volume>::all(ctx.client.clone()).get_opt(&id).await {
                    if fresh.spec.node_name != ctx.node {
                        let (engine, vol, owner, me) =
                            (ctx.engine.clone(), id.clone(), v.spec.node_name.clone(), ctx.node.clone());
                        match tokio::task::spawn_blocking(move || janitor::drop_stale_worktrees(&engine, &vol, &owner, &me)).await {
                            Ok(dropped) if dropped > 0 => {
                                tracing::info!(volume = %id, dropped, "pull: dropped stale live worktree(s) left by a takeover")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(volume = %id, error = %e, "pull: the stale-worktree drop task panicked"),
                        }
                    }
                }
```

and the retired-copy drop at the end of the loop:

```rust
        let (engine, vol) = (ctx.engine.clone(), id.clone());
        if let Err(e) = tokio::task::spawn_blocking(move || janitor::cleanup_local(&engine, &vol)).await {
            tracing::warn!(volume = %id, error = %e, "pull: retire: the copy-drop task panicked");
            continue;
        }
        tracing::info!(volume = %id, "pull: retire: slot moved elsewhere, copy dropped");
```

- [ ] **Step 4: Move the two `voldir(...).exists()` probes**

`Path::exists` is a `stat`, not a walk, but both run once per volume per beat on a node holding many volumes and the same thread is the reactor's. Batch each function's probes into one `spawn_blocking`, which costs one hop per beat instead of one syscall per volume on the reactor.

In `interesting_volumes` (now `async`), before the loop:

```rust
    // One hop off the reactor for every probe this pass needs, rather than a `stat` per volume on
    // the reactor thread: the answer is a set, and the pool does not change under this beat.
    let ids: Vec<String> = beat.volumes.iter().map(|v| v.name_any()).collect();
    let engine = ctx.engine.clone();
    let held: HashSet<String> = tokio::task::spawn_blocking(move || {
        ids.into_iter().filter(|id| engine.pool.voldir(id).exists()).collect::<HashSet<String>>()
    })
    .await
    .unwrap_or_default();
```

then `let hold_a_copy = held.contains(&id);`. Do the same in `retire_pass` for its own `!ctx.engine.pool.voldir(&id).exists()` guard, reusing one `held` set for both the `should_retire` loop and the earlier `sweep_orphan_snap_bytes` call is **not** in scope — `sweep_orphan_snap_bytes` keeps its own probe, which is already inside its own per-volume error handling.

Update the one call site in `pull_beat_with`: `for id in interesting_volumes(ctx, &beat, &live).await {`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/peer.rs
git commit -m "Run the retire pass's btrfs and pool walks off the reactor"
```

**RBAC / admission:** none — no API surface changes.

---

## Task 5: I2 — a floor between wake-driven pull passes

**Files:**
- Modify: `bins/agent/src/peer.rs:326-362` (`Next`, `retry_delay`, `after_pass`)
- Modify: `bins/agent/src/controller/run.rs:355-386` (`spawn_pull`)
- Test: `bins/agent/src/peer.rs` `mod reconcile_tests` (`after_pass` is already tested without a clock)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub(crate) const MIN_WAKE_GAP: Duration = Duration::from_secs(5);` and a new signature
  `pub(crate) fn after_pass(wake: &tokio::sync::Notify, missed: bool, misses: &mut u32, since_last_start: Duration) -> Next`.
  `Next` is unchanged (`RunAgain` / `RetrySoon(Duration)` / `Wait`). `spawn_pull` is the only caller.

**Why:** `wake` fires `notify_one` with no rate limit; `after_pass` returns `RunAgain` whenever a permit is pending and `spawn_pull` then runs the next pass with no sleep at all. A peer POSTing `/peer/v1/wake` in a loop pins this node in a back-to-back pull beat — ~6 cluster-wide LISTs plus a field-selected `Snapshot` LIST and a `local_commits` walk per interesting volume, forever. The secret is a shared fleet-wide symmetric token, so "authenticated" is a weak boundary. Coalescing already exists (`notify_one` leaves at most one permit however many wakes arrive, and `snapshot::wake_worthy` coalesces on the sending side); what is missing is a floor.

- [ ] **Step 1: Write the failing test**

Add beside the existing `after_pass` tests in `mod reconcile_tests`:

```rust
    /// I2: a wake still wins, but never sooner than `MIN_WAKE_GAP` after the last pass STARTED —
    /// a peer looping POSTs on `/peer/v1/wake` must not pin this node in a back-to-back beat.
    #[test]
    fn a_wake_arriving_inside_the_floor_waits_out_the_remainder() {
        let wake = tokio::sync::Notify::new();
        wake.notify_one();
        let mut misses = 0;
        let next = after_pass(&wake, false, &mut misses, Duration::from_secs(1));
        assert_eq!(next, Next::RetrySoon(MIN_WAKE_GAP - Duration::from_secs(1)));
    }

    #[test]
    fn a_wake_after_the_floor_runs_again_at_once() {
        let wake = tokio::sync::Notify::new();
        wake.notify_one();
        let mut misses = 0;
        assert_eq!(after_pass(&wake, false, &mut misses, MIN_WAKE_GAP), Next::RunAgain);
    }

    /// The floor never delays a RETRY that is already longer than it: a missed pass's own backoff
    /// still governs, and a wake inside the floor does not shorten it.
    #[test]
    fn the_floor_never_shortens_a_missed_passes_backoff() {
        let wake = tokio::sync::Notify::new();
        wake.notify_one();
        let mut misses = 3;
        let next = after_pass(&wake, true, &mut misses, Duration::from_secs(0));
        assert_eq!(next, Next::RetrySoon(retry_delay(4)));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-agent-bin a_wake_arriving_inside_the_floor -- --test-threads=1; echo exit=$?`
Expected: FAIL to compile — `after_pass` takes three arguments and `MIN_WAKE_GAP` does not exist.

- [ ] **Step 3: Add the floor**

In `bins/agent/src/peer.rs`:

```rust
/// The minimum gap between the STARTS of two wake-driven passes. `/peer/v1/wake` is
/// unauthenticated beyond a fleet-wide symmetric secret, and a peer POSTing it in a loop otherwise
/// pins this node in a back-to-back beat — six cluster-wide LISTs plus a Snapshot LIST and a
/// directory walk per interesting volume, forever. Five seconds keeps a stop or a clone
/// effectively immediate while capping one compromised or buggy agent's reach into the API server.
pub(crate) const MIN_WAKE_GAP: Duration = Duration::from_secs(5);

/// `misses` counts CONSECUTIVE passes that missed something, and is reset by any clean pass — a
/// volume that starts fetching again returns the node to its ordinary beat immediately.
///
/// `since_last_start` is measured from when the pass that just ended BEGAN, not from when it
/// ended: a slow pass has already paid the floor, and measuring from the end would let a long
/// receive earn an extra idle 5 s it does not need.
pub(crate) fn after_pass(wake: &tokio::sync::Notify, missed: bool, misses: &mut u32, since_last_start: Duration) -> Next {
    use futures::FutureExt;
    *misses = if missed { misses.saturating_add(1) } else { 0 };
    let woken = wake.notified().now_or_never().is_some();
    let backoff = missed.then(|| retry_delay(*misses));
    match (woken, backoff) {
        // A wake inside the floor keeps its permit's effect — the pass still happens — but only
        // after the remainder. `RetrySoon` is right and `Wait` is not: the wake must not be lost.
        (true, None) if since_last_start < MIN_WAKE_GAP => Next::RetrySoon(MIN_WAKE_GAP - since_last_start),
        (true, None) => Next::RunAgain,
        // A missed pass's own backoff is always at least `RETRY_SOON` (30 s), which is longer than
        // the floor: the floor can never shorten it, and a wake during it is taken by the select.
        (_, Some(d)) => Next::RetrySoon(d),
        (false, None) => Next::Wait,
    }
}
```

- [ ] **Step 4: Pass the elapsed time from `spawn_pull`**

In `bins/agent/src/controller/run.rs`, `spawn_pull`:

```rust
            let started = tokio::time::Instant::now();
            let missed = crate::peer::pull_beat(&ctx).await;
            // Wakes that arrived DURING the pass decide whether to go straight round again — but
            // never sooner than `MIN_WAKE_GAP` after this pass began, so a peer looping on
            // `/peer/v1/wake` cannot drive this node's beat continuously.
            next = crate::peer::after_pass(&wake, missed, &mut misses, started.elapsed());
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/peer.rs bins/agent/src/controller/run.rs
git commit -m "Put a floor between wake-driven pull passes"
```

**RBAC / admission:** none. This narrows what an authenticated peer can cause; the NetworkPolicy and the shared `WS_PEER_SECRET` are unchanged.

---

## Task 6: I3 — bound the served send, and stop the lock outliving it

**Files:**
- Modify: `bins/agent/src/peer.rs:132-176` (`commit` handler), `:182-223` (`KillOnDrop`), `:265-268` (`send_timeout`)
- Modify: `deploy/k3s/agent.yaml` — add `WS_PEER_SERVE_TIMEOUT_SECS` to the DaemonSet env with the default written out
- Test: `bins/agent/tests/peer.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `fn serve_timeout() -> Duration` — `WS_PEER_SERVE_TIMEOUT_SECS`, default 900. `send_timeout()` is unchanged (client side, default 3600).

**Why:** the per-volume `AsyncMutex` is held for the life of the response body (moved into `KillOnDrop`). A puller that opens the connection and stops reading holds it until its **own** `send_timeout` fires, and the server has no timeout of its own — the doc at the `send_timeout` definition says so explicitly. Every other node's pull of that volume queues behind it: replication of one volume stops fleet-wide for up to an hour on a single wedged connection, `Replicated` never goes true, and nothing on that volume can start elsewhere.

- [ ] **Step 1: Write the failing test**

`bins/agent/tests/peer.rs` already drives the router with a fake `btrfs send` shell script. Add:

```rust
/// I3: the server bounds its own send. A puller that stops reading must not hold the volume's
/// send lock until its own hour-long client timeout — the next puller of the same volume waits
/// behind it, fleet-wide. With a one-second serve timeout, the second request must be served.
#[tokio::test]
async fn a_stalled_puller_does_not_hold_the_volume_send_lock() {
    std::env::set_var("WS_PEER_SERVE_TIMEOUT_SECS", "1");
    let (app, tmp) = router_with_fake_btrfs("slow"); // a script that writes one byte, then sleeps 60
    std::fs::create_dir_all(tmp.path().join("vol").join("v1").join("snap").join("c1")).unwrap();

    let first = tokio::spawn({
        let app = app.clone();
        async move { send_request(&app, "/peer/v1/commit/v1/c1").await }
    });
    // Long enough for the first request to have taken the lock, short enough to be inside the
    // fake script's 60 s sleep: the point is that the SECOND request is not blocked behind it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        send_request(&app, "/peer/v1/commit/v1/c1"),
    )
    .await;

    assert!(second.is_ok(), "the second pull must not wait out the first puller's stall");
    let _ = first.await;
    std::env::remove_var("WS_PEER_SERVE_TIMEOUT_SECS");
}
```

Add the `slow` variant to the file's existing fake-btrfs script builder (it already writes one per test shape): a script whose `send` arm is `printf x; sleep 60`. `router_with_fake_btrfs` and `send_request` are the file's existing helpers — reuse them verbatim; if the file names them differently, use its names rather than introducing new ones.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-agent-bin --test peer a_stalled_puller -- --test-threads=1; echo exit=$?`
Expected: FAIL — the 5 s timeout on the second request elapses, because the first holds the lock for the whole 60 s sleep.

- [ ] **Step 3: Bound the served stream**

In `bins/agent/src/peer.rs`, beside `send_timeout`:

```rust
/// `WS_PEER_SERVE_TIMEOUT_SECS`, default 900. Deliberately SHORTER than the client's
/// `send_timeout` (3600) and a separate knob: the client's bound protects the puller, this one
/// protects the SOURCE. The per-volume send lock is held for the life of this body, so a puller
/// that opens the connection and stops reading otherwise blocks every other node's pull of that
/// volume for the client's full hour — one wedged connection stopping a volume's replication
/// fleet-wide. A legitimate send that needs longer than 15 minutes of TOTAL wall clock raises
/// this; the puller retries from the next source either way.
fn serve_timeout() -> Duration {
    Duration::from_secs(std::env::var("WS_PEER_SERVE_TIMEOUT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(900))
}
```

and in the `commit` handler, wrap the streamed body:

```rust
    let killer = KillOnDrop { stdout, child: Some(child), stderr_task, volume: volume_id, name: commit_name, _guard: guard };
    // Dropping the stream on timeout drops `KillOnDrop`, which kills and reaps the child AND
    // releases the send lock — the whole point. The puller sees a truncated body after its 200,
    // which is the case it already handles (`pull_one`'s failed-receive path deletes the partial
    // and tries the next source).
    let bounded = tokio_stream::StreamExt::timeout(
        tokio_util::io::ReaderStream::new(killer),
        serve_timeout(),
    );
    let body = Body::from_stream(futures::TryStreamExt::map_err(
        futures::StreamExt::map(bounded, |r| r.map_err(|_| std::io::Error::other("peer: serve timeout"))?),
        std::io::Error::other,
    ));
    (StatusCode::OK, body).into_response()
```

If `tokio_stream` is not already a dependency of the agent binary, do **not** add it — use the plain form instead, which needs nothing new:

```rust
    let stream = tokio_util::io::ReaderStream::new(killer);
    let deadline = tokio::time::Instant::now() + serve_timeout();
    let body = Body::from_stream(futures::stream::unfold(Box::pin(stream), move |mut s| async move {
        match tokio::time::timeout_at(deadline, futures::StreamExt::next(&mut s)).await {
            Ok(Some(chunk)) => Some((chunk, s)),
            Ok(None) => None,
            // The deadline is on the WHOLE body, not per chunk: a puller trickling one byte a
            // minute is the same wedge as one reading nothing.
            Err(_) => Some((Err(std::io::Error::other("peer: serve timeout")), s)),
        }
    }));
    (StatusCode::OK, body).into_response()
```

Check `bins/agent/Cargo.toml` first and take whichever form needs no new dependency.

- [ ] **Step 4: Write the env down in the deploy**

In `deploy/k3s/agent.yaml`, in the DaemonSet's container env beside `WS_PEER_SEND_TIMEOUT_SECS` (add it if that one is also implicit — an env the operator must know about belongs in the manifest, not only in a doc comment):

```yaml
            # The SOURCE-side bound on one streamed `btrfs send`. Shorter than the puller's
            # WS_PEER_SEND_TIMEOUT_SECS on purpose: the send lock is per volume, so a stalled
            # puller otherwise blocks that volume's replication fleet-wide for the client's hour.
            - name: WS_PEER_SERVE_TIMEOUT_SECS
              value: "900"
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/peer.rs bins/agent/tests/peer.rs deploy/k3s/agent.yaml
git commit -m "Bound a served btrfs send so a stalled puller cannot hold a volume"
```

**RBAC / admission:** none. Deploy touchpoint only: `deploy/k3s/agent.yaml` gains one env var; no image repin is implied by this task alone (the roll order in `CLAUDE.md` still applies when this ships — pin the image before the yaml).

---

## Task 7: I4 — a size ceiling on `btrfs receive`

**Files:**
- Modify: `bins/agent/src/peer.rs:720-774` (`pull_one`), `:572-...` (`pull_volume`, to compute and pass the ceiling), `:132-152` (`commit` handler, the 413)
- Test: `bins/agent/src/peer.rs` `mod reconcile_tests` and `bins/agent/tests/peer.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `fn receive_ceiling(quota_gb: u32) -> u64` — `quota_gb` GiB × `WS_PEER_RECEIVE_SLACK` (default 3), with a floor of 1 GiB so a quota-less or tiny volume still has headroom for its metadata.
  - `pull_one` gains a `max_bytes: u64` parameter, after `parent`.
  - `CommitQuery` gains `max: Option<u64>`; the `commit` handler answers `413 PAYLOAD_TOO_LARGE` when the puller's declared ceiling is below what a full send of that volume could need.

**Why:** the received subvolume lands under `snap/` on the pulling node with no quota (quotas are per `live` worktree and per volume in `volume_work`, never per received snapshot) and no byte cap. A peer answering with an arbitrarily long body fills the pool, and pool exhaustion takes down every workspace on the node, not just the one volume. A truncated receive already deletes the partial, so the failure mode is the existing one.

- [ ] **Step 1: Write the failing tests**

In `mod reconcile_tests` (the pure half):

```rust
    /// I4: the ceiling is the volume's own quota times slack, never unbounded, and never below a
    /// floor a snapshot's metadata needs even on a tiny or quota-less volume.
    #[test]
    fn the_receive_ceiling_follows_the_volumes_quota() {
        assert_eq!(receive_ceiling(10), 10 * 3 * 1024 * 1024 * 1024);
        assert_eq!(receive_ceiling(0), 1024 * 1024 * 1024, "a quota-less volume still gets the floor");
    }
```

In `bins/agent/tests/peer.rs` (the wire half):

```rust
/// I4: a puller declares the ceiling it will accept, and a source that cannot fit a full send
/// under it says so BEFORE streaming — a 413 is a fetchable answer, a truncated body after a 200
/// is a wasted transfer.
#[tokio::test]
async fn a_ceiling_below_the_volumes_quota_is_refused_with_413() {
    let (app, tmp) = router_with_fake_btrfs("ok");
    std::fs::create_dir_all(tmp.path().join("vol").join("v1").join("snap").join("c1")).unwrap();

    let resp = send_request(&app, "/peer/v1/commit/v1/c1?max=1").await;

    assert_eq!(resp.status(), 413, "a ceiling that cannot fit the volume is refused up front");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-agent-bin the_receive_ceiling_follows -- --test-threads=1; echo exit=$?`
Expected: FAIL to compile — `receive_ceiling` does not exist.

- [ ] **Step 3: Add the ceiling and cap the copy**

In `bins/agent/src/peer.rs`:

```rust
/// The most bytes a single receive of a volume's snapshot may write. Derived from the volume's own
/// `spec.quotaGb`, because that IS the answer to "how big can this volume's data be" — a separate
/// env would be a second, drifting copy of it. Times a slack factor for btrfs metadata,
/// reflink-broken copies and a snapshot cut just before a large delete; floored at 1 GiB so a
/// quota-less volume (`quotaGb: 0`) still receives rather than failing at zero.
///
/// ponytail: one ceiling per receive, not per volume total — N concurrent receives of one volume
/// can still exceed it N times. The pool-level guard is the quota `volume_work` already sets;
/// this is the bound on a single stream from a peer we do not otherwise trust to be finite.
fn receive_ceiling(quota_gb: u32) -> u64 {
    let slack: u64 = std::env::var("WS_PEER_RECEIVE_SLACK").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    (u64::from(quota_gb) * slack * 1024 * 1024 * 1024).max(1024 * 1024 * 1024)
}
```

In `pull_one`, add the parameter and cap the copy:

```rust
async fn pull_one(
    engine: &Engine,
    btrfs_bin: &str,
    http: &reqwest::Client,
    addr: &str,
    secret: &str,
    volume: &str,
    name: &str,
    parent: Option<&str>,
    max_bytes: u64,
) -> Result<(), String> {
    let mut url = format!("http://{addr}/peer/v1/commit/{volume}/{name}?max={max_bytes}");
    if let Some(p) = parent {
        url = format!("{url}&parent={p}");
    }
```

and, around the copy:

```rust
    // `take(max_bytes + 1)`: the extra byte is how a stream that WOULD exceed the ceiling is told
    // apart from one that exactly fills it. A peer answering with an unbounded body otherwise
    // fills the pool, and a full pool takes down every workspace on this node, not one volume.
    let mut reader = StreamReader::new(resp.bytes_stream().map_err(std::io::Error::other)).take(max_bytes + 1);
    let copy_result = tokio::io::copy(&mut reader, &mut stdin).await;
    let _ = stdin.shutdown().await;
    drop(stdin);
    let ok = match copy_result {
        Ok(n) if n > max_bytes => {
            tracing::warn!(%volume, %name, max_bytes, "pull: the source exceeded this volume's receive ceiling");
            let _ = child.wait().await;
            false
        }
        Ok(_) => matches!(child.wait().await, Ok(s) if s.success()),
        Err(_) => {
            let _ = child.wait().await;
            false
        }
    };
```

(`AsyncReadExt::take` — add `use tokio::io::AsyncReadExt;` if the module does not already import it. The existing `if !ok { … delete the partial … }` block below is unchanged and is what makes an over-ceiling receive leave nothing behind.)

In `pull_volume`, compute the ceiling once per volume from the beat's own copy of the Volume and pass it to both `pull_one` calls:

```rust
    // The volume's own quota is the ceiling's source. A volume missing from the beat's listing is
    // one this node holds a copy of without a CR; the floor applies.
    let quota_gb = beat.volumes.iter().find(|v| v.name_any() == volume).map(|v| v.spec.quota_gb).unwrap_or(0);
    let max_bytes = receive_ceiling(quota_gb);
```

In the `commit` handler, honour the declared ceiling before spawning anything:

```rust
#[derive(serde::Deserialize)]
struct CommitQuery {
    parent: Option<String>,
    max: Option<u64>,
}
```

and, after the `valid_segment` checks and before the send lock:

```rust
    // The puller declares what it will accept; a source that cannot fit a full send under it says
    // so BEFORE streaming. A truncated body after a 200 costs both sides the whole transfer, and
    // the puller cannot tell it from a crashed `btrfs send`. One Volume GET, on a path that is
    // about to spawn a root `btrfs send` and stream tens of GiB — not a cost worth avoiding.
    if let Some(max) = q.max {
        let quota = Api::<crd::Volume>::all(state.client.clone())
            .get_opt(&volume)
            .await
            .ok()
            .flatten()
            .map(|v| v.spec.quota_gb)
            .unwrap_or(0);
        if max < receive_ceiling(quota) {
            return (StatusCode::PAYLOAD_TOO_LARGE, Body::empty()).into_response();
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`. Existing `pull_one` tests need the new argument — pass `receive_ceiling(0)` in each.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/peer.rs bins/agent/tests/peer.rs
git commit -m "Cap a peer receive at the volume's own quota"
```

**RBAC / admission:** none new. The handler's `get` on `volumes` is already granted (the pull beat lists them from the same ServiceAccount). No spec write.

---

## Task 8: I5 — `sync_one` compares the same key the rest of the system uses

**Files:**
- Modify: `bins/agent/src/sync.rs:100-124`
- Test: `bins/agent/src/sync.rs` `mod` tests (the file already has them)

**Interfaces:**
- Consumes: nothing.
- Produces: nothing new — `crd::transient_generation_of(&crd::Snapshot) -> u64` already exists and is already used by `peer.rs` and `newest_transient_of`.

**Why:** `recorded` starts `None`, and `Some(_) >= None` is true and `None >= None` is true, so the first `Ready` transient always wins regardless of its annotation, and a later one with **no** annotation (a documented keep-biased path when `record_post_cut_generation` fails) only loses if `recorded` is already `Some`. Iteration order of a list response therefore decides both `parent` (the `btrfs send -p` base) and `recorded_state` (the definition-change comparison) — a redundant full send instead of a delta, and a spurious definition-change cut every beat, which is exactly the "cut once per interval forever" failure the annotation exists to prevent.

- [ ] **Step 1: Write the failing test**

Add to `bins/agent/src/sync.rs`'s test module:

```rust
    /// I5: the newest recorded sync point is the one with the highest generation, whatever order
    /// the list came back in — a missing annotation is generation 0, never a winner. Before this,
    /// `Some(_) >= None` made the first Ready transient win and the send parent a coin flip.
    #[tokio::test]
    async fn the_newest_sync_point_wins_regardless_of_list_order() {
        let tmp = tempfile::tempdir().unwrap();
        // Deliberately newest-first: the buggy comparison keeps whichever came first.
        let routes = vec![get(
            SNAPSHOTS,
            list_of("Snapshot", vec![
                transient_with_generation("sync-new", "vol-1", "ws-1", Some(7)),
                transient_with_generation("sync-none", "vol-1", "ws-1", None),
            ]),
        )];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        sync_one(&ctx, &live_worktree("ws-1", "vol-1")).await;

        // The cut this beat creates names its send parent; with no btrfs the generation read fails
        // and nothing is created, so assert on the parent the DECISION picked instead.
        assert_eq!(newest_recorded_for_test(&ctx, "vol-1", "ws-1").await, ("sync-new".to_string(), 7));
        let _ = rec;
    }

    /// The reverse order must give the same answer.
    #[tokio::test]
    async fn the_newest_sync_point_wins_when_it_is_listed_last() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![get(
            SNAPSHOTS,
            list_of("Snapshot", vec![
                transient_with_generation("sync-none", "vol-1", "ws-1", None),
                transient_with_generation("sync-new", "vol-1", "ws-1", Some(7)),
            ]),
        )];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);
        assert_eq!(newest_recorded_for_test(&ctx, "vol-1", "ws-1").await, ("sync-new".to_string(), 7));
    }
```

The scan is currently inline in `sync_one`. Lift it into a pure function so it is assertable at all — this is the deliverable, not a test-only affordance:

```rust
/// The newest recorded sync point of `worktree`: its name, its recorded generation, and the state
/// it froze. Pure, and keyed by `crd::transient_generation_of` — the SAME ordering key
/// `newest_transient_of` and the pull beat's replica branches use. Three call sites computing
/// "which cut is newest" three ways is three answers that can disagree about a send parent.
///
/// A record with no generation annotation is generation 0, never a winner: the annotation write
/// is a documented keep-biased path that can fail, and a failed write must not promote its record.
pub(crate) fn newest_recorded(snapshots: &[crd::Snapshot], worktree: &str) -> Option<(String, u64, Option<crd::SnapshotState>)> {
    snapshots
        .iter()
        .filter(|s| s.spec.transient && s.spec.worktree == worktree)
        .filter(|s| s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready))
        .max_by(|a, b| {
            (crd::transient_generation_of(a), a.name_any()).cmp(&(crd::transient_generation_of(b), b.name_any()))
        })
        .map(|s| (s.name_any(), crd::transient_generation_of(s), s.spec.state.clone()))
}
```

Then the tests assert on `newest_recorded` directly rather than through a `_for_test` shim — drop `newest_recorded_for_test` from the test bodies above and assert:

```rust
        let list: Vec<crd::Snapshot> = vec![/* the two fixtures, deserialized */];
        let (name, gen, _) = newest_recorded(&list, "ws-1").expect("a Ready transient exists");
        assert_eq!((name.as_str(), gen), ("sync-new", 7));
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-agent-bin the_newest_sync_point_wins -- --test-threads=1; echo exit=$?`
Expected: FAIL to compile — `newest_recorded` does not exist.

- [ ] **Step 3: Use it in `sync_one`**

Replace the inline scan in `bins/agent/src/sync.rs` (the `let mut recorded: Option<u64>` block through the end of its `for` loop) with:

```rust
    // One cut in flight at a time — the same rule `create_snapshot` applies, and the reason this
    // beat can run on a tick without piling snapshots onto a slow btrfs.
    if list.items.iter().any(|s| {
        s.spec.transient
            && s.spec.worktree == live.name
            && s.status.as_ref().map(|st| st.phase) == Some(crd::Phase::Working)
    }) {
        tracing::debug!(worktree = %live.name, "sync: a transient is Working, skipping this pass");
        return;
    }
    let (parent, recorded, recorded_state) = match newest_recorded(&list.items, &live.name) {
        Some((name, gen, state)) => (name, Some(gen), state),
        None => (String::new(), None, None),
    };
```

Everything below (`recorded`, `recorded_state`, `parent`) reads the same as before. `recorded` stays `Option<u64>` for the "nothing recorded at all" case the comparison below already distinguishes — the bug was comparing two `Option`s against each other, not holding one.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/sync.rs
git commit -m "Order sync points by the generation key the rest of the agent uses"
```

**RBAC / admission:** none.

---

## Task 9: I6 — the Snapshot reconciler answers "not mine" without an API call

**Files:**
- Modify: `bins/agent/src/snapshot.rs:52-72` (`reconcile_commit`'s opening arms)
- Modify: `bins/agent/src/controller/run.rs:248-255` (the `Snapshot` controller — comment only; see below)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: no signature changes. `reconcile_commit` reads `ctx.volumes` (the shared, node-scoped Volume store already built in `run.rs:85-107`) before `worktree_node`.

**Why:** the `Snapshot` watch has no field or label selector, and the first thing `reconcile_commit` does for a `Working` snapshot is a `Workspace` GET plus (on a miss) an `Environment` GET, purely to discover it belongs to another node. With `N` nodes and the sync beat cutting one transient per live worktree per `WS_SYNC_SECS`, that is `N ×` the cluster's cut rate in wasted GETs, plus a `requeue(TICK)` every 15 s for any snapshot whose worktree cannot be resolved. It is the largest avoidable per-object API cost in the agent and it grows with cluster size × worktree count.

**Why the store and not a selector:** a `Snapshot` carries no `spec.nodeName`, so there is nothing for a field selector to select on, and a label would be a second copy of `Volume.spec.nodeName` that some path forgets to stamp — exactly the failure `heal_labels` exists to paper over elsewhere. The Volume store is already shared and already scoped to this node (`watcher::Config::fields("spec.nodeName={me}")`), so "is this volume mine" is an in-memory read.

- [ ] **Step 1: Write the failing test**

Add to `bins/agent/tests/reconcile.rs`:

```rust
/// I6: a Snapshot on another node's volume costs NO API calls. Every node watches every
/// Snapshot, so the ~(N-1)/N that are not ours were each paying a Workspace GET (and an
/// Environment GET on the miss) just to discover that.
#[tokio::test]
async fn a_snapshot_on_another_nodes_volume_makes_no_api_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![]);
    // The node-scoped Volume store holds only this node's volumes; `vol-elsewhere` is absent.
    seed_volume_store(&ctx, vec![volume_json("vol-mine", "node-a")]);

    let s = Arc::new(snapshot_cr("push-1", "vol-elsewhere", "ws-1"));
    let action = rustic_git_agent::snapshot::reconcile_commit(s, ctx.clone()).await.expect("no error");

    assert!(rec.calls().is_empty(), "a foreign volume's snapshot must cost nothing: {:?}", rec.calls());
    assert_eq!(action, kube::runtime::controller::Action::await_change());
}

/// The volume IS ours but the worktree cannot be resolved yet (a push racing `volumeRef`
/// visibility): still a requeue, still through `worktree_node`. The pre-filter must not turn a
/// racing push into a silently hung one.
#[tokio::test]
async fn a_snapshot_on_my_volume_still_resolves_its_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        not_found(format!("{WORKSPACES}/ws-1")),
        not_found(format!("{ENVIRONMENTS}/ws-1")),
    ];
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
    seed_volume_store(&ctx, vec![volume_json("vol-mine", "node-a")]);

    let s = Arc::new(snapshot_cr("push-1", "vol-mine", "ws-1"));
    let action = rustic_git_agent::snapshot::reconcile_commit(s, ctx.clone()).await.expect("no error");

    assert!(rec.calls().contains(&format!("GET {WORKSPACES}/ws-1")));
    assert_eq!(action, kube::runtime::controller::Action::requeue(rustic_git_agent::controller::TICK));
}

/// A volume the store has not seen yet is NOT "not mine": the store is a cache, and a Volume
/// created seconds ago may not have reached it. Keep-biased — fall through to the real lookup.
#[tokio::test]
async fn an_unknown_volume_falls_through_to_the_worktree_lookup() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        not_found(format!("{WORKSPACES}/ws-1")),
        not_found(format!("{ENVIRONMENTS}/ws-1")),
    ];
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
    // Store deliberately EMPTY — not yet populated, which is not evidence of anything.
    seed_volume_store(&ctx, vec![]);

    let _ = rustic_git_agent::snapshot::reconcile_commit(Arc::new(snapshot_cr("push-1", "vol-x", "ws-1")), ctx.clone()).await;

    assert!(rec.calls().contains(&format!("GET {WORKSPACES}/ws-1")), "an empty store must not be read as 'not mine'");
}
```

`seed_volume_store` writes into the `Ctx`'s shared reflector store. If `Ctx` exposes no seam for it, add one beside the writer it already holds:

```rust
    /// Test seam: the Volume store is filled by a live watch in production. A reconciler that
    /// READS the store needs one that is filled, and standing up a watcher against a mock client
    /// tests `kube`'s watcher rather than this rule.
    #[doc(hidden)]
    pub fn seed_volume_store(&self, volumes: Vec<crd::Volume>) { /* apply each to the held writer */ }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-agent-bin --test reconcile a_snapshot_on_another_nodes_volume -- --test-threads=1; echo exit=$?`
Expected: FAIL — `rec.calls()` contains the `Workspace` GET and the `Environment` GET.

- [ ] **Step 3: Read the store first**

In `bins/agent/src/snapshot.rs`, in `reconcile_commit`, immediately after the phase check and before `worktree_node`:

```rust
    // Every node watches every Snapshot in the cluster (a Snapshot carries no node of its own, so
    // there is nothing for a field selector to select on), and the ~(N-1)/N that are not ours were
    // each paying a Workspace GET — plus an Environment GET on the miss — purely to discover that.
    // The shared Volume store is ALREADY scoped to this node's volumes (`run.rs`'s
    // `spec.nodeName={me}` watch), so the same answer is an in-memory read.
    //
    // Keep-biased: a store that has not SEEN the volume is not a store that says it is foreign — a
    // Volume created seconds ago may not have reached the cache — so only a store holding at least
    // one volume, and not this one, short-circuits. `worktree_node` stays as the second check, and
    // it is the one that decides.
    {
        let store = ctx.volumes.state();
        if !store.is_empty() && !store.iter().any(|v| v.name_any() == s.spec.volume) {
            return Ok(Action::await_change());
        }
    }
```

- [ ] **Step 4: Say why the watch stays unfiltered**

In `bins/agent/src/controller/run.rs:248-255`, replace the `Snapshot` controller's comment:

```rust
    // The `Snapshot` kind: no finalizer (see `snapshot::reconcile_commit`'s module doc), so a
    // plain watch over every one in the cluster is enough — and it has to be one: a Snapshot
    // carries no node of its own, so there is no field to select on, and a label would be a second
    // copy of `Volume.spec.nodeName` that some write path forgets to stamp. `reconcile_commit`
    // filters against the node-scoped Volume store instead, at no API cost.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/snapshot.rs bins/agent/src/controller/run.rs bins/agent/tests/reconcile.rs
git commit -m "Filter foreign snapshots against the node's own volume store"
```

**RBAC / admission:** none. The watch's grant is unchanged — a field selector narrows a watch, never authorization, and this task adds no selector anyway.

---

## Task 10: M1 — `kept_conditions` keeps `Replicated` and `Decommissioning`

**Files:**
- Modify: `bins/agent/src/controller/workspace.rs:413-418`
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub(crate) fn kept_conditions(prev: &[Condition], ready: Condition) -> Vec<Condition>` — unchanged signature, two more types kept.

**Why:** it keeps only `PackagesReady` and `ATTACHED`. Every `Resolved::Wait` arm, the namespace gate, `HomeNotReady`, `HeadUnknown` and `CommitPending` go through `ws_conditions` → `kept_conditions`, so a starting workspace loses the `Replicated` condition the per-volume sweep reads and the drain notice `with_drain_notice` writes. The sweep then reads `replicated: false`, which is the keep-biased direction, so nothing breaks today — but spec simplification 2's "one place computes it, everywhere reads it" is not actually held, and the next reader of `Replicated` will not be keep-biased by luck.

- [ ] **Step 1: Write the failing test**

```rust
/// M1: a workspace parked in a Wait arm keeps the conditions OTHER writers own. `Replicated` is
/// computed in one place (`replicated_condition`) and read by the per-volume sweep;
/// `Decommissioning` is the drain notice. Dropping either on every wait arm makes the sweep read
/// a false it did not compute.
#[test]
fn kept_conditions_preserves_replicated_and_decommissioning() {
    let prev = vec![
        cond("PackagesReady", true),
        cond("Attached", true),
        cond("Replicated", true),
        cond("Decommissioning", true),
        cond("Ready", false),
    ];
    let kept = rustic_git_agent::controller::workspace::kept_conditions(&prev, cond("Ready", true));
    let types: Vec<&str> = kept.iter().map(|c| c.type_.as_str()).collect();
    assert!(types.contains(&"Replicated"), "the sweep reads this and does not write it: {types:?}");
    assert!(types.contains(&"Decommissioning"), "the drain notice is not the wait arm's to drop: {types:?}");
    assert_eq!(types.iter().filter(|t| **t == "Ready").count(), 1, "the new condition replaces, never doubles");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-agent-bin kept_conditions_preserves -- --test-threads=1; echo exit=$?`
Expected: FAIL — neither type survives.

- [ ] **Step 3: Keep them**

```rust
pub(crate) fn kept_conditions(prev: &[Condition], ready: Condition) -> Vec<Condition> {
    // Every type here is owned by a writer that is NOT this path: `PackagesReady` by the profile
    // step, `Attached` by the pod path, `Replicated` by `replicated_condition` (which the
    // per-volume sweep reads and never computes), `Decommissioning` by the drain notice. A wait
    // arm dropping one makes its reader see a value nobody computed.
    let keep = [crd::PACKAGES_READY, crd::ATTACHED, "Replicated", "Decommissioning"];
    let mut c: Vec<Condition> = prev.iter().filter(|c| keep.contains(&c.type_.as_str()) && c.type_ != ready.type_).cloned().collect();
    c.push(ready);
    c
}
```

(The `c.type_ != ready.type_` guard is new and load-bearing: with `Replicated` now kept, a `ready` condition of that same type would otherwise appear twice.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/controller/workspace.rs bins/agent/tests/reconcile.rs
git commit -m "Keep the conditions other writers own across a workspace wait arm"
```

**RBAC / admission:** none.

---

## Task 11: M2 — one dead-node floor, one number

**Files:**
- Modify: `bins/agent/src/peer.rs:387-392` (`node_dead_secs`'s default and doc)
- Modify: `deploy/k3s/agent.yaml` (delete the `WS_NODE_DEAD_SECS` override, if it is now redundant)
- Modify: `bins/agent/src/peer.rs:198-202` (the stale `ponytail:` marker — see Task 22 if it is cleaner to land it there; do it here, since the number is the marker's subject)
- Test: `bins/agent/src/peer.rs` `mod reconcile_tests`

**Why:** the code default says 600 ("how long a node must be observed NotReady"), `controller/mod.rs:my_node`'s doc says "The `WS_NODE_DEAD_SECS` floor (180 s)", and commit `5319f67d` ("Declare a node dead after 180 s instead of 600") moved the cluster to 180 without moving the env default. One number, one place.

- [ ] **Step 1: Write the failing test**

```rust
    /// M2: the code default IS the cluster's floor. Two numbers — a 600 s default with a 180 s
    /// deploy override — is a node declared dead at one interval in production and another in
    /// every test, and the comments disagreed about which.
    #[test]
    fn the_dead_node_floor_defaults_to_the_number_the_cluster_runs() {
        std::env::remove_var("WS_NODE_DEAD_SECS");
        assert_eq!(node_dead_secs(), 180);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-agent-bin the_dead_node_floor_defaults -- --test-threads=1; echo exit=$?`
Expected: FAIL — `600 != 180`. (`--test-threads=1` matters here: this test mutates process env.)

- [ ] **Step 3: Move the default**

```rust
/// `WS_NODE_DEAD_SECS`, default 180 — how long a node must be observed NotReady before its
/// `VolumeReplica` rows are reaped and its volumes swept. Long enough that a rolling restart or a
/// brief kubelet hiccup never costs a replica row; the row is cheap to recreate, a wrongly-reaped
/// one is not. It was 600 with a 180 deploy override, which meant the number every test and every
/// other doc comment saw was not the one production ran.
pub(crate) fn node_dead_secs() -> i64 {
    std::env::var("WS_NODE_DEAD_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(180)
}
```

- [ ] **Step 4: Drop the now-redundant deploy override and fix the marker**

In `deploy/k3s/agent.yaml`, remove the `WS_NODE_DEAD_SECS: "180"` env entry — the code default is the same value, and an env restating a default is the second copy this task exists to remove. (If the manifest sets a value **other** than 180, keep it and say why in a comment instead.)

In `bins/agent/src/peer.rs:198-202`, the marker's ceiling has moved:

```rust
    // ponytail: `now` is THIS node's own clock against another node's `lastTransitionTime`, so
    // the 180 s floor absorbs NTP drift rather than measuring it. At 600 s that was ample slack;
    // at 180 it is three minutes of margin, which is still far more than a healthy fleet drifts.
    // The upgrade is an apiserver-relative delta (compare against the API server's own clock via
    // a `Lease` renewal, as the server tier's ownership lease already does) if drift ever shows up
    // as a spurious sweep.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`. Any test that assumed a 600 s default needs its fixture timestamps checked — `grep -rn "WS_NODE_DEAD_SECS\|600" bins/agent/src bins/agent/tests` before you commit.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/peer.rs deploy/k3s/agent.yaml
git commit -m "Default the dead-node floor to the 180 s the cluster runs"
```

**RBAC / admission:** none. Deploy touchpoint: one env removed from `deploy/k3s/agent.yaml`; the roll order in `CLAUDE.md` applies (image repin before the yaml, or the old image reads a default of 600 with no override — harmless in the safe direction, but say so in the PR).

---

## Task 12: M3 — halve the drain poll rate

**Files:**
- Modify: `bins/agent/src/controller/environment.rs:608-616`
- Test: `bins/agent/tests/reconcile.rs` (if the file has a drain test; if not, this task ships without one — see below)

**Why:** 40 × 250 ms polls, each a namespaced pod LIST. On a stop or restore of an environment with slow services that is 40 LISTs and 10 s of a reconcile slot. The doc justifies the wait; the polling rate is the part worth halving — 500 ms × 20 is the same 10 s ceiling at half the LISTs.

**On the test:** this is a rate change with an unchanged ceiling and an unchanged outcome. The existing `drain_services` tests (which assert the outcome, not the poll count) are the regression net; do not add a timing test — a test that asserts "twenty LISTs, not forty" pins an implementation detail and fails on any future change to the same ceiling. If no `drain_services` test exists at all, add one asserting only that a drain returning zero writing pods on the first poll makes exactly one LIST.

- [ ] **Step 1: Change the poll**

```rust
    let mut remaining = writing_pods(ns, ctx).await?;
    // 20 × 500 ms, not 40 × 250: the same 10 s ceiling at half the pod LISTs. A service that
    // finishes its writes 250 ms sooner is not worth a doubled API cost on every stop and every
    // restore of every environment.
    for _ in 0..20 {
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        remaining = writing_pods(ns, ctx).await?;
    }
    Ok(remaining)
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0` — if a test hard-codes 40 route repetitions for the drain, adjust it to 20 and say so in the diff.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 3: Commit**

```bash
git add bins/agent/src/controller/environment.rs
git commit -m "Halve the drain poll rate at the same wait ceiling"
```

**RBAC / admission:** none.

---

## Task 13: M4 — `btrfs_delete` stops unwrapping a path

**Files:**
- Modify: `bins/agent/src/janitor.rs:313`
- Test: none — this is a one-line removal of an unreachable `unwrap` whose replacement is strictly more general, and `Command::arg` taking `AsRef<OsStr>` is the whole argument. The existing `cleanup_local` tests cover the call.

**Why:** `path.to_str().unwrap()` panics on a non-UTF-8 path. Unreachable with today's names (all `valid_segment`), but this runs inside `cleanup_local`, which the finalizer path depends on — a panic there wedges a delete. `Command::arg` takes `AsRef<OsStr>`, so the conversion is not needed at all.

- [ ] **Step 1: Make the change**

```rust
fn btrfs_delete(path: &std::path::Path, id: &str) {
    // `arg`, not `args([… path.to_str().unwrap()])`: `Command::arg` takes `AsRef<OsStr>`, so the
    // UTF-8 conversion — and its panic, inside the path a finalizer depends on — was never needed.
    match std::process::Command::new("btrfs").arg("subvolume").arg("delete").arg(path).output() {
```

Body below unchanged.

- [ ] **Step 2: Run the tests**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 3: Commit**

```bash
git add bins/agent/src/janitor.rs
git commit -m "Pass the subvolume path to btrfs without a UTF-8 unwrap"
```

**RBAC / admission:** none.

---

## Task 14: M7 — a pull target must be the agent's own ServiceAccount; M5 and M6 are recorded, not changed

**Files:**
- Modify: `bins/agent/src/peer.rs:250-260` (`agent_pod_addr`)
- Modify: `bins/agent/src/peer.rs:113-114` (`secret_ok` — comment only)
- Modify: `bins/agent/src/peer.rs:467-481` (`sweep_dead_nodes` — comment only)
- Test: `bins/agent/src/peer.rs` `mod reconcile_tests`

**Why (M7):** the peer address for a node is whichever pod in `kube-system` carries `app=rustic-git-agent` and `spec.nodeName={node}`. Anyone who can create a pod in `kube-system` can redirect a pull. That is already a cluster-admin-adjacent capability, so this is hardening rather than a hole — and a `spec.serviceAccountName` check closes it for one line.

**Why M5 and M6 change nothing:**
- **M5** (the dead-node sweep runs identically on every live node, paying `N ×` the writes and `N ×` the GET-per-parent) is correct as written; only one node wins the release CAS and the `mark_parent_of` idle check keeps the marks from churning. A rendezvous over `live` keyed by volume id is the named upgrade if the write volume ever shows up — it is not worth a lease. Record it as a `ponytail:` marker rather than building it.
- **M6** (`secret_ok` comparing digests with `==`) is correct: the comparison is over SHA-256 digests, so its timing leaks nothing about the secret. The note exists so a future reader does not "fix" it into a constant-time crate and believe something changed. Record it in the comment.

- [ ] **Step 1: Write the failing test**

```rust
    /// M7: a pull target must be the agent's own ServiceAccount, not merely a pod wearing its
    /// label in `kube-system`. Creating a pod there is cluster-admin-adjacent already, so this is
    /// depth, not a hole — but the check is one line and the alternative is a redirected pull.
    #[tokio::test]
    async fn a_pod_wearing_the_label_but_not_the_service_account_is_not_a_peer() {
        let tmp = tempfile::tempdir().unwrap();
        let impostor = serde_json::json!({
            "apiVersion": "v1", "kind": "PodList",
            "items": [{
                "apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": "not-us", "namespace": "kube-system", "labels": {"app": "rustic-git-agent"}},
                "spec": {"nodeName": "node-b", "serviceAccountName": "default"},
                "status": {"podIp": "10.0.0.9"},
            }],
        });
        let routes = vec![get("/api/v1/namespaces/kube-system/pods", impostor)];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);

        assert!(agent_pod_addr(&ctx.client, "node-b").await.is_err(), "an impostor pod is not a peer address");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-agent-bin a_pod_wearing_the_label -- --test-threads=1; echo exit=$?`
Expected: FAIL — `Ok("10.0.0.9:8444")`.

- [ ] **Step 3: Check the ServiceAccount**

```rust
    let ip = pods
        .items
        .into_iter()
        // The label and the node are a selector, not an identity: a pod created in `kube-system`
        // by anyone can wear both. The ServiceAccount is the thing only our DaemonSet has, and a
        // pull redirected to an impostor is a root `btrfs receive` of whatever it answers with.
        .filter(|p| p.spec.as_ref().and_then(|s| s.service_account_name.as_deref()) == Some("rustic-git-agent"))
        .find_map(|p| p.status.and_then(|s| s.pod_ip))
        .ok_or_else(|| format!("no ready rustic-git-agent pod on {node}"))?;
```

- [ ] **Step 4: Record M5 and M6 where a reader will hit them**

At `secret_ok`, extend the existing doc:

```rust
/// `==` on the digests, deliberately, and NOT a constant-time-compare crate: the values compared
/// are SHA-256 digests of both sides, so an early-exit `memcmp` over them leaks where two DIGESTS
/// differ, which says nothing about the secret. The length-independence above is the property that
/// matters and it is already held. Changing this buys nothing.
```

Above `sweep_dead_nodes`:

```rust
/// ponytail: every live node computes the same dead set and runs this same sweep. Only one wins
/// the release CAS and `mark_parent_of`'s idle check absorbs the duplicate marks, so it is
/// correct — it just pays `N ×` the parent GETs and status writes. The upgrade is a rendezvous
/// over `live` keyed by volume id (`preferred_node`, already in this file), not a lease; take it
/// if the dead-node write volume ever shows up in an API server's audit log.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/peer.rs
git commit -m "Require a peer pod to carry the agent service account"
```

**RBAC / admission:**

| file | rule | why |
|---|---|---|
| `deploy/k3s/agent-rbac.yaml` | `pods: get, list` in `kube-system` (existing, unchanged) | `spec.serviceAccountName` is on the Pod object the agent already reads; no new verb or resource. |

Admission: none — no spec write.

---

## Task 15: Delete the agent-side `VolumeSource::RestoreOf` arms

**Files:**
- Modify: `bins/agent/src/controller/volume.rs:268-271` (the two materialize arms) and `:593` (the `check_source` arm)
- Modify: `crates/workspaces/src/engine/ops.rs:18-24` (`RESTORE_OF_GONE`)
- Test: `bins/agent/tests/reconcile.rs` — delete any test asserting the `RESTORE_OF_GONE` outcome

**Interfaces:**
- **Consumes:** `crd::VolumeSource` **without** a `RestoreOf` variant. The variant is removed from the CRD by the **workspaces-api plan**, not by this one. This task's code must compile against the enum after that removal, so it must land **after** that plan's variant-removal task. Until then the arms below are still required to compile and this task is blocked. The remaining variants are `CloneOf { volume, .. }`, `SeededFrom { volume, snapshot }`, `GitRepo { repo, branch }`, and `None`.
- **Produces:** `volume_work`'s `match &source` becomes exhaustive over four cases with no `RestoreOf` arm; `check_source`'s match loses its `RestoreOf` arm. `RESTORE_OF_GONE` is deleted from `crates/workspaces/src/engine/ops.rs`.

**Why:** the variant is already dead — `/v1` translates a restore-to-new into `CloneOf{volume, commit: Some(id)}` at write time and never writes it. The arms exist only so a pre-cutover stored spec deserializes into a fixed permanent condition. Once the variant leaves the CRD, a stored object carrying it fails to deserialize into `VolumeSource` at all and the field parses as absent (`Option<VolumeSource>` with an unknown tag) — which is the `None` arm, "a fresh subvolume", and for a volume whose bytes are already on disk (`voldir(id).exists()`) `create_subvol` is a no-op. That is the same outcome the `if voldir exists` arm produced, reached without the enum variant.

- [ ] **Step 1: Confirm the dependency has landed**

Run: `grep -rn "RestoreOf" crates/workspaces/src/crd.rs; echo exit=$?`
Expected: no match. If the variant is still there, **stop** — this task is blocked on the workspaces-api plan's variant removal. Do not remove it yourself: the CRD schema and `/v1`'s parse path are that plan's scope.

- [ ] **Step 2: Delete the arms**

In `bins/agent/src/controller/volume.rs`, delete both `Some(VolumeSource::RestoreOf { .. })` arms and the seven-line comment above them. In `check_source`, delete its `Some(VolumeSource::RestoreOf { .. }) => Ok(())` arm and the comment above it.

In `crates/workspaces/src/engine/ops.rs`, delete `RESTORE_OF_GONE` and its doc.

- [ ] **Step 3: Note what a legacy object now does**

Add, above `volume_work`'s `match &source`:

```rust
        // A pre-cutover object whose `source` named the deleted `restoreOf` variant no longer
        // deserializes into a `VolumeSource` at all, so its `source` reads as absent — the `None`
        // arm, a fresh subvolume, which for a volume already on disk is `create_subvol`'s no-op.
        // That is the same outcome the old dedicated arm produced, minus the variant.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0` after deleting any test naming `RESTORE_OF_GONE` (`grep -rn "RESTORE_OF_GONE" bins crates`).
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/controller/volume.rs crates/workspaces/src/engine/ops.rs bins/agent/tests/reconcile.rs
git commit -m "Delete the agent's dead restore-of arms"
```

**RBAC / admission:** none.

---

## Task 16: One `cas` helper for every guarded spec/metadata patch

**Files:**
- Modify: `bins/agent/src/controller/volume.rs:410-540` (`take_volume`, `release_volume`, `detach_volume`, `attach_volume`)
- Modify: `bins/agent/src/peer.rs` (`sweep_volumes`'s release arm — the fifth copy)
- Test: `bins/agent/src/controller/volume.rs` test module

**Interfaces:**
- Consumes: Task 3's `sweep_volumes` (the release arm is the caller being rewritten).
- Produces:
  ```rust
  pub(crate) async fn cas(
      api: &Api<crd::Volume>,
      name: &str,
      path: &str,
      from: serde_json::Value,
      to: serde_json::Value,
  ) -> Result<bool, kube::Error>
  ```
  `Ok(true)` = the patch landed; `Ok(false)` = a 409 or 422, meaning "lost, not broken"; `Err` = anything else. `path` is a JSON Pointer (`"/spec/nodeName"`, `"/metadata/ownerReferences"`).

**Why:** five hand-built `json_patch::Patch(vec![Test, Replace])` blocks with identical `.parse().expect("static pointer parses")` boilerplate and identical 409/422 handling. Four of them are in one file. The construction is the safety property (the API server applies the patch atomically, so exactly one of two claimants sees 200) and five copies of a safety property is five places it can be got subtly wrong.

- [ ] **Step 1: Write the failing test**

```rust
    /// The whole point of the helper: a 409 or a 422 is "lost, not broken", and anything else is
    /// an error the caller must see. Five copies of this rule is five places it can drift.
    #[tokio::test]
    async fn cas_reads_a_conflict_as_lost_and_anything_else_as_an_error() {
        let ok = mock_client(vec![Route { method: "PATCH", path: format!("{VOLUMES}/v1"), status: 200, body: volume_json("v1", "node-a") }]);
        let api: Api<crd::Volume> = Api::all(ok.0);
        assert!(cas(&api, "v1", "/spec/nodeName", json!(""), json!("node-a")).await.unwrap());

        let lost = mock_client(vec![Route { method: "PATCH", path: format!("{VOLUMES}/v1"), status: 422, body: json!({"message": "test failed"}) }]);
        let api: Api<crd::Volume> = Api::all(lost.0);
        assert!(!cas(&api, "v1", "/spec/nodeName", json!(""), json!("node-a")).await.unwrap());

        let broken = mock_client(vec![Route { method: "PATCH", path: format!("{VOLUMES}/v1"), status: 500, body: json!({"message": "etcd is down"}) }]);
        let api: Api<crd::Volume> = Api::all(broken.0);
        assert!(cas(&api, "v1", "/spec/nodeName", json!(""), json!("node-a")).await.is_err(), "an outage is not a lost race");
    }

    /// The body must be exactly Test-then-Replace on the given pointer: it is the atomicity of
    /// that pair that makes two claimants safe, and a Replace alone would let both win.
    #[tokio::test]
    async fn cas_sends_a_test_then_a_replace() {
        let (client, rec) = mock_client(vec![Route { method: "PATCH", path: format!("{VOLUMES}/v1"), status: 200, body: volume_json("v1", "node-a") }]);
        let api: Api<crd::Volume> = Api::all(client);
        cas(&api, "v1", "/spec/nodeName", json!(""), json!("node-a")).await.unwrap();
        let sent = rec.sent("PATCH", &format!("{VOLUMES}/v1"));
        assert_eq!(sent[0], json!([
            {"op": "test", "path": "/spec/nodeName", "value": ""},
            {"op": "replace", "path": "/spec/nodeName", "value": "node-a"},
        ]));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-agent-bin cas_reads_a_conflict -- --test-threads=1; echo exit=$?`
Expected: FAIL to compile — `cas` does not exist.

- [ ] **Step 3: Write the helper**

In `bins/agent/src/controller/volume.rs`, above `take_volume`:

```rust
/// Compare-and-set one JSON pointer on a Volume, atomically. `test` proves the value we decided
/// against is still there and `replace` writes the new one; the API server applies the pair as a
/// unit, so of two claimants exactly one sees 200 and the other a 409/422 it reads as "lost, not
/// broken" rather than as a failure. That reading is the safety property, which is why there is
/// one of these and not five.
///
/// `Ok(false)` is a lost race and the caller re-decides on its next pass. An `Err` is an outage —
/// never "lost": a caller that treated an unreachable API server as a lost race would silently
/// skip work forever.
pub(crate) async fn cas(
    api: &Api<crd::Volume>,
    name: &str,
    path: &str,
    from: serde_json::Value,
    to: serde_json::Value,
) -> Result<bool, kube::Error> {
    let pointer = || path.parse().expect("callers pass static pointers");
    let ops = json_patch::Patch(vec![
        json_patch::PatchOperation::Test(json_patch::TestOperation { path: pointer(), value: from }),
        json_patch::PatchOperation::Replace(json_patch::ReplaceOperation { path: pointer(), value: to }),
    ]);
    match api.patch(name, &PatchParams::default(), &Patch::Json::<crd::Volume>(ops)).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(s)) if s.code == 422 || s.code == 409 => Ok(false),
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 4: Route the five call sites through it**

```rust
pub(crate) async fn take_volume(ctx: &Arc<Ctx>, name: &str, node: &str) -> Result<bool, kube::Error> {
    cas(&Api::all(ctx.client.clone()), name, "/spec/nodeName", serde_json::json!(""), serde_json::json!(node)).await
}

pub(crate) async fn release_volume(ctx: &Arc<Ctx>, name: &str, owner: &str) -> Result<bool, kube::Error> {
    cas(&Api::all(ctx.client.clone()), name, "/spec/nodeName", serde_json::json!(owner), serde_json::json!("")).await
}
```

Keep both doc comments — they explain the two-claimant rule at the point a reader meets it.

`detach_volume`: keep the read, the already-detached early return and the `kept` computation; replace the patch block with

```rust
    cas(
        &api,
        name,
        "/metadata/ownerReferences",
        serde_json::to_value(&current).expect("owner references serialize"),
        serde_json::to_value(&kept).expect("owner references serialize"),
    )
    .await
```

`attach_volume`: keep the `current.is_empty()` Add branch **verbatim, with its comment** — a detached volume has no `ownerReferences` key at all and a `test` against `[]` 422s forever on it, which `cas` cannot express. The non-empty branch becomes a `cas` call, and the function keeps returning `Result<bool, ReconcileErr>` (map the `kube::Error` with `?`/`.map_err(Into::into)`).

`peer.rs`'s release arm in `sweep_volumes`:

```rust
        if release && !stranded {
            // The pin FIRST, before anything is un-placed: a failed CAS with parents already
            // cleared would leave them claimable on a node that does not own the volume — the
            // exact bug this whole function exists to make impossible.
            match crate::controller::volume::cas(&api, &name, "/spec/nodeName", serde_json::json!(owner), serde_json::json!("")).await {
                // `cur` stays the stale copy on purpose: the status PUT below carries a
                // `resourceVersion` the patch just bumped, so its first attempt 409s and its
                // existing re-read arm fetches the fresh object. One round trip, not two.
                Ok(true) => {}
                Ok(false) => continue, // a survivor's takeover landed between our list and this patch
                Err(e) => {
                    tracing::warn!(volume = %name, error = %e, "sweep: releasing an unavailable owner's volume");
                    continue;
                }
            }
        }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`. Watch the sweep tests: the release arm now makes one more `PUT .../status` attempt (the 409 + re-read) on the release path than it did when it adopted the patched object. If a test asserts an exact PUT count on a release, update it and note the extra round trip in the diff.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/controller/volume.rs bins/agent/src/peer.rs
git commit -m "Share one compare-and-set for every guarded volume patch"
```

**RBAC / admission:**

| file | rule | why |
|---|---|---|
| `deploy/k3s/agent-rbac.yaml` | `volumes: patch` (existing, unchanged) | Same verb, same two pointers as before. |
| `deploy/k3s/agent-admission.yaml` | the policy's allowed spec changes (unchanged) | The policy allows the agent `patch` on the main resources only for labels, finalizers and the two spec fields a parent's reconciler copies into its child; `spec.nodeName` under `take_volume` is the one other spec write it permits. This task changes the code path, not the field set — verify by re-reading the policy's field list before you commit, and if any pointer here is not in it, the task is wrong, not the policy. |

---

## Task 17: Split `peer.rs` into four modules — mechanical, no behaviour change

**Files:**
- Create: `bins/agent/src/peer/pull.rs`, `bins/agent/src/peer/sweeps.rs`, `bins/agent/src/peer/wake.rs`, `bins/agent/src/peer/placement.rs`
- Modify: `bins/agent/src/peer.rs` → `bins/agent/src/peer/mod.rs` (or keep `peer.rs` with `mod` declarations; take whichever the crate's existing `controller/` layout uses — `controller/mod.rs`, so use `peer/mod.rs`)
- Modify: every `crate::peer::…` reference in `bins/agent/src` and `bins/agent/tests`

**Interfaces:**
- Consumes: Tasks 1, 3–7, 14 and 16 have all landed in `peer.rs` — this task moves their result, unchanged.
- Produces: **every public and `pub(crate)` path stays identical.** `peer/mod.rs` re-exports each moved item so `crate::peer::pull_beat`, `crate::peer::sweep_volumes`, `crate::peer::wake_peers`, `crate::peer::node_dead_secs` and the rest keep working with no call-site edits:
  ```rust
  mod placement;
  mod pull;
  mod sweeps;
  mod wake;
  pub use pull::{pull_beat, replica_interval};
  pub use wake::wake_peers;
  pub(crate) use placement::{decommissioning, live_nodes, newest_transient, node_dead_secs, node_is_dead, placeable_nodes, pool_nodes, preferred_node, unplaceable, up_to_date, up_to_date_nodes};
  pub(crate) use sweeps::{mark_parent, sweep_volumes, unplace_parent, volume_decision, VolumeVerdict};
  pub(crate) use wake::{after_pass, Next, MIN_WAKE_GAP, RETRY_SOON};
  pub(crate) use crd::newest_transient_of;
  ```

**Why:** 3444 lines holding the transport, both sweeps, placement arithmetic and the wake protocol. The module doc's own claim ("Replication's transport, both halves") stopped being true several tasks ago. This is the same mechanical split `api.rs` is getting in the workspaces-api plan.

**Rule for this task: not one line of logic changes.** Move functions verbatim, adjust `use` lines and visibility (`fn` → `pub(super) fn` where a sibling module calls it), move each test into the module whose code it tests. If a reviewer can see a behaviour difference in the diff, the task is wrong.

- [ ] **Step 1: Record the baseline**

Run: `cargo test -p rustic-git-agent-bin -- --test-threads=1 2>&1 | tail -5; echo exit=$?`
Write down the test count. It must be identical after the split — a test lost in a move is the failure mode of this kind of task.

- [ ] **Step 2: Create `peer/placement.rs`**

Move, verbatim: `pool_nodes`, `placeable_nodes`, `live_nodes`, `standby_count`, `preferred_node`, `up_to_date`, `up_to_date_nodes`, `newest_transient`, `node_is_dead`, `decommissioning`, `unplaceable`, `node_dead_secs`, and their tests. Module doc:

```rust
//! Who may hold a volume, and who is alive enough to be asked. Pure arithmetic over a Node list
//! and a set of `VolumeReplica` rows, with one rule under all of it: a decision is made by NAME
//! (`up_to_date`), never by comparing clocks across nodes — which is what makes placement
//! skew-proof.
```

- [ ] **Step 3: Create `peer/wake.rs`**

Move: `wake_peers`, `Next`, `RETRY_SOON`, `MIN_WAKE_GAP`, `retry_delay`, `after_pass`, and their tests. The `wake` HTTP handler stays in `mod.rs` with the router. Module doc:

```rust
//! The wake protocol: "something you replicate just changed, pull now." A wake can only make a
//! pass happen SOONER, never change what it pulls — which is why the handler trusts nothing past
//! the secret, and why the floor (`MIN_WAKE_GAP`) is the whole defence against a peer driving
//! this node's beat.
```

- [ ] **Step 4: Create `peer/pull.rs`**

Move: `pull_beat`, `pull_beat_with`, `interesting_volumes`, `nearest_held_ancestor`, `retired`, `pull_volume`, `pull_one`, `receive_ceiling`, `write_replica_status`, `subvolume_names`, `delete_subvolume`, `replica_interval`, `send_timeout`, `peer_http_client`, `agent_pod_addr`, and their tests.

- [ ] **Step 5: Create `peer/sweeps.rs`**

Move: `reap_dead_replicas`, `volume_decision`, `VolumeVerdict`, `sweep_volumes`, `unplace_parent`, `mark_parent`, `mark_parent_of`, `sweep_dead_nodes`, `should_retire`, `orphan_voldirs`, `sweep_orphan_snapshots`, `orphan_snaps`, `sweep_orphan_snap_bytes`, `retire_pass`, `collect_unreferenced_volumes`, and their tests. Module doc:

```rust
//! Every sweep in the agent, and the one rule they all hold: keep-biased. A fresh read
//! immediately before any delete, a partial listing acts on nothing, and an unreadable answer
//! keeps rather than guesses. `volume_decision` + `sweep_volumes` are ONE function for both the
//! dead-node sweep and the drain on purpose (spec simplification 9) — two copies of those arms is
//! how a drain starts libelling a healthy workspace.
```

- [ ] **Step 6: Leave `peer/mod.rs` as the listener**

`PeerState`, `router`, `serve`, `secret_ok`, `commit`, `wake`, `KillOnDrop`, `spawn_send_tokio`, `serve_timeout`, `tail_str`, `CommitQuery`, the `use` re-exports above, and the module doc (updated to say what the module root now is and where the four halves live).

- [ ] **Step 7: Move the shared test fixtures**

`test_ctx`, `NoopNix`, `beat_of`, `parent_at`, `ready_snapshot`, `node_json`, `replica_of`, the path consts — these are used by tests in more than one of the new modules. Do **not** copy them four times; that is exactly K8's finding, and Task 22 fixes it. For this task, put them in `bins/agent/src/peer/testsupport.rs` behind `#[cfg(test)]` and `use super::testsupport::*;` from each module's test block. Task 22 then lifts that file to the crate root.

- [ ] **Step 8: Verify nothing moved but the code**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0` **and the same test count as Step 1**.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning. `git diff --stat` should show near-zero net line change outside the module docs.

- [ ] **Step 9: Commit**

```bash
git add bins/agent/src/peer.rs bins/agent/src/peer/
git commit -m "Split the peer module into pull, sweeps, wake and placement"
```

**RBAC / admission:** none.

---

## Task 18: `worktree_gate` — the start/head/checkout sequence shared by both parents

**Files:**
- Create: `bins/agent/src/controller/worktree.rs`
- Modify: `bins/agent/src/controller/workspace.rs:117-127` and `:189-312`
- Modify: `bins/agent/src/controller/environment.rs:110-117` and `:249-350`
- Modify: `bins/agent/src/controller/mod.rs` (declare and re-export)
- Test: `bins/agent/tests/reconcile.rs` — the existing tests for both parents ARE the test

**Interfaces:**
- Consumes: Task 10's `kept_conditions`.
- Produces:
  ```rust
  /// What the shared gate decided. The two callers turn it into their own status type — that
  /// difference is the only real one between the two ~120-line blocks this replaces.
  pub(crate) enum WorktreeGate {
      /// Nothing to wait for: the worktree is checked out at this head and quota is set.
      Ready { head: Option<String> },
      /// Wait, with the condition reason and message the caller must write.
      Wait { reason: &'static str, message: String, action: Action },
      /// Handed over: another node was preferred at start and now owns the volume.
      HandedOver { node: String },
  }

  pub(crate) async fn worktree_gate(
      parent_name: &str,
      parent_kind: &'static str,
      volume: &crd::Volume,
      storage: &crd::Storage,
      prev_head: Option<&str>,
      prev_phase: crd::Phase,
      ctx: &Arc<Ctx>,
  ) -> Result<WorktreeGate, ReconcileErr>
  ```

**Why:** `workspace.rs:117-127` and `environment.rs:110-117` are the same eight lines (`if prev.phase == Stopped { parents_on_volume → start_placement → await_change }`) with a different log field, and `workspace.rs:189-312` vs `environment.rs:249-350` are ~120 duplicated lines (`migrate_and_seed_baseline` → `latest_transient` → `effective_head` → `HeadUnknown` → `clone_commit`/`CommitPending`/`NoSuchCommit` → `checkout` + `set_quota_worktree` → first graft) with the status struct as the only real difference. The comments say so themselves ("`apply_workspace`'s twin arm, verbatim in shape"). It is the largest duplication in the crate and the largest place the two kinds can drift.

**Rule for this task: behaviour-preserving.** Every existing test for both parents must pass unchanged. If one needs editing, you changed behaviour — stop and find out which.

- [ ] **Step 1: Record the baseline**

Run: `cargo test -p rustic-git-agent-bin --test reconcile -- --test-threads=1 2>&1 | tail -5; echo exit=$?`
Note the count. The two parents' existing tests are what proves this task.

- [ ] **Step 2: Move the start-spread block**

Add to `bins/agent/src/controller/worktree.rs`:

```rust
/// The start-time spread, for either parent kind. Only the OWNER may give a volume away, only on
/// a start (the one moment the parent's status still says `Stopped`), and never when anything on
/// the volume is running — `start_placement` holds all three rules; this is the shared call.
pub(crate) async fn start_spread(
    parent_kind: &'static str,
    parent_name: &str,
    volume_id: &str,
    volume: &crd::Volume,
    prev_phase: crd::Phase,
    ctx: &Arc<Ctx>,
) -> Result<Option<String>, ReconcileErr> {
    if prev_phase != crd::Phase::Stopped {
        return Ok(None);
    }
    let Some(siblings) = crate::listing::parents_on_volume(ctx, volume_id).await else { return Ok(None) };
    let Some(node) = super::stop::start_placement(ctx, volume, &siblings).await? else { return Ok(None) };
    tracing::info!(kind = %parent_kind, parent = %parent_name, %node, "handed over on start");
    Ok(Some(node))
}
```

Replace both call sites with it, each keeping its own `return Ok(Action::await_change())`.

- [ ] **Step 3: Move the worktree sequence**

Lift `workspace.rs:189-312` into `worktree_gate` verbatim, replacing each write of `crd::WorkspaceStatus` with the corresponding `WorktreeGate::Wait { reason, message, action }`. Then rewrite the workspace call site as a `match` that turns each variant into the status it wrote before, and rewrite the environment call site the same way against `crd::EnvironmentStatus`. Do the workspace first, run the tests, then the environment — two commits' worth of risk in one step is how a behaviour change hides.

Module doc for the new file:

```rust
//! The worktree gate: everything a Workspace and an Environment do IDENTICALLY between "my
//! Volume is Ready" and "my pod may start" — migration baseline, newest transient, effective
//! head, checkout, quota. The two kinds differ only in which status struct carries the answer,
//! which is why the gate returns a decision and the callers write their own status.
//!
//! It exists because the two copies of this sequence had already begun to drift, and every
//! divergence between them is a bug in whichever kind was not the one being edited.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`, same count as Step 1, **no test edited**.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/controller/worktree.rs bins/agent/src/controller/workspace.rs bins/agent/src/controller/environment.rs bins/agent/src/controller/mod.rs
git commit -m "Share the worktree gate between workspaces and environments"
```

**RBAC / admission:** none — the same API calls, from one place.

---

## Task 19: K1 + K2 — delete `compatibleNodes`

**Files:**
- Modify: `crates/workspaces/src/crd.rs:629` and `:736` (the two status structs)
- Modify: `bins/agent/src/controller/workspace.rs:1284` and `bins/agent/src/controller/environment.rs:641` (the status-equality predicates)
- Modify: `bins/agent/src/decommission.rs:234`, `bins/agent/src/peer.rs` fixtures (three sites), `bins/agent/tests/reconcile.rs` fixtures
- Test: `crates/workspaces/src/crd.rs:1242` — the tolerated-unknown parse test already covers old objects; extend its assertion message, do not add a test

**Why:** spec simplifications 6 and 11 say it goes. Nothing writes it (`claim.rs:40` documents its removal), yet both status structs still declare it and both status-equality predicates still compare it — comparing a field neither side ever sets.

- [ ] **Step 1: Delete the field**

Remove `pub compatible_nodes: Vec<String>,` from both `WorkspaceStatus` and `EnvironmentStatus` in `crates/workspaces/src/crd.rs`. Confirm the struct's `serde` attributes tolerate unknown fields — `crd.rs:1242`'s test asserts exactly that for `durable`, `compatibleNodes` and `lastSyncAt`, so a stored object still parses.

- [ ] **Step 2: Delete the two comparisons**

Remove `&& a.compatible_nodes == b.compatible_nodes` from `workspace.rs:1284` and `environment.rs:641`.

- [ ] **Step 3: Strip it from the fixtures**

`grep -rn "compatibleNodes" bins crates` and delete the key from every test fixture. Keep `bins/agent/tests/reconcile.rs:465`'s assertion (`compatibleNodes is dead and never written`) — it is now enforcing the absence rather than a policy, and its message still reads correctly. Keep the three comments at `reconcile.rs:540`, `:702`, `:776` that say "the old `compatibleNodes` arm": they are explaining what a rule replaced, which is why the test exists.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`, including `crd.rs`'s tolerated-unknown parse test.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/crd.rs bins/agent/src bins/agent/tests
git commit -m "Delete the dead compatibleNodes status field"
```

**RBAC / admission:** the CRD schema changes, so `deploy/k3s/` CRD yaml must be regenerated and re-applied by hand per `deploy/k3s/README.md`. Removing a field from a status schema does not orphan stored objects (the API server keeps unknown status fields it was given only if the schema preserves them; here they are dropped on the next status write, which is the intent). Note it in the PR body: **apply the regenerated CRDs before rolling the agent.**

---

## Task 20: K3 + K4 — the RBAC file's table is the role, so make it true

**Files:**
- Modify: `deploy/k3s/agent-rbac.yaml:49-50`, `:102`, `:168-169`, `:243-249`

**Why:** the file's own header says the table **is** the role. `apps/deployments: get,delete` is granted "legacy migration only" and `grep -n 'Deployment' bins/agent/src` returns nothing. The table also names `home_commit_beat` and `stop-home-{ws}`, both gone since the home moved to ZeroFS (spec 2026-09-01). Stale rows in load-bearing documentation are worse than none.

- [ ] **Step 1: Confirm the grant is dead**

Run: `grep -rn "Deployment" bins/agent/src; echo exit=$?`
Expected: no match. If there is one, stop — the row is not dead and the review is wrong about it.

Run: `grep -rn "home_commit_beat\|stop-home" bins/agent/src crates/workspaces/src; echo exit=$?`
Expected: no match.

- [ ] **Step 2: Delete the rule and the rows**

Remove the `apps`/`deployments` rule (`:243-249`) and its table row (`:102`). In the table's `snapshots` row (`:49-50`) and the rule comment at `:168-169`, delete `home_commit_beat` and `stop-home-{ws}`, leaving `stop_push`'s `stop-{env}`:

```yaml
#                                          create                       stop_push (stop-{env})
```

- [ ] **Step 3: Verify the manifest still parses and the role still covers the code**

Run: `kubectl apply --dry-run=client -f deploy/k3s/agent-rbac.yaml; echo exit=$?`
Expected: `exit=0`. If no cluster credentials are available, `--dry-run=client` still parses locally; if `kubectl` is absent, at minimum run a YAML parse.

Cross-check every remaining row against the code: for each `resources:` entry in the file, `grep -rn "Api::<K>" bins/agent/src` for the matching kind. A row with no caller is the next K3.

- [ ] **Step 4: Commit**

```bash
git add deploy/k3s/agent-rbac.yaml
git commit -m "Drop the agent's dead deployments grant and three stale table rows"
```

**RBAC / admission:**

| file | rule | why |
|---|---|---|
| `deploy/k3s/agent-rbac.yaml` | **remove** `apiGroups: ["apps"], resources: ["deployments"], verbs: ["get","delete"]` | No code path reads or deletes a Deployment; an environment's services are StatefulSets. A grant with no caller is standing authority for a future bug. |
| `deploy/k3s/agent-rbac.yaml` | table rows for `home_commit_beat`, `stop-home-{ws}`, `deployments` | The header says the table is the role; three rows named concepts that no longer exist. |

Apply by hand per `deploy/k3s/README.md`. Removing a grant is safe to apply **before** the agent rolls (nothing calls it).

---

## Task 21: K5 + K6 + K7 — say snapshot, and retire the stale markers

**Files:**
- Modify: `bins/agent/src/claim.rs:23`, `:76-110` (`Placement::has_commits`, `commit_phase`, `has_commits`)
- Modify: `bins/agent/src/peer/pull.rs`, `bins/agent/src/peer/sweeps.rs` (log strings and doc comments saying "commit")
- Modify: `bins/agent/src/binding.rs:8-13` (the module doc), `:15` (the `ponytail:` marker)
- Modify: `bins/agent/src/peer/pull.rs` (`retired`'s marker), `bins/agent/src/peer/placement.rs` (the `node_dead_secs` marker, if Task 11 did not already correct it)

**Why:** the durable-snapshots vocabulary note is explicit — read **snapshot** for **commit** — and the API, the web and the CLI all say snapshot now. `claim.rs`'s four helpers and `pull_volume`/`retire_pass`'s log strings are the last places that do not. `binding.rs`'s module doc explains a `spec.nodeName` the struct no longer has, and its marker defers a node-retirement path that now exists (`decommission.rs`) and deliberately does not touch bindings.

**Do not rename** `crd::Snapshot`, `reconcile_commit`, `commit_worktree`, `local_commits`, `drop_commit` or the `/peer/v1/commit/...` route in this task — those are the engine's and the wire's names, they are load-bearing across crates and the peer protocol, and renaming them is a separate decision. This task is the **agent-local** vocabulary: `claim.rs`'s four helpers and log strings.

- [ ] **Step 1: Rename the four `claim.rs` helpers**

`Placement::has_commits` → `Placement::has_snapshots`; `commit_phase` → `snapshot_phase`; the free `has_commits` → `has_snapshots`. Update every caller (`grep -rn "has_commits\|commit_phase" bins/agent`).

- [ ] **Step 2: Fix the log strings and docs**

In `peer/pull.rs` and `peer/sweeps.rs`, replace in comments, doc comments and `tracing::` messages:
- "Every volume this node must hold a commit-model replica of" → "Every volume this node must hold a replica of"
- "retired commit" → "retired snapshot"
- "orphaned commit bytes" → "orphaned snapshot bytes"
- "dropping a retired commit failed" → "dropping a retired snapshot failed"

Leave `local_commits`, `drop_commit` and `commit_worktree` as-is where they are engine method names being called.

- [ ] **Step 3: Fix `binding.rs`**

Replace the module doc's two archaeology sentences:

```rust
//! It is NOT node-scoped. It used to be — the owner's home was a btrfs subvolume on one node, so
//! the binding pinned every workspace of theirs to it. The home is a directory on a region-shared
//! NFS mount now, so every node reconciles every binding (the objects below are all
//! server-side-applied, which is convergent under concurrent appliers) and placement is the
//! claim's own business.
```

(deleting the trailing "An old stored object may still carry `spec.nodeName`…" sentence — the field is one of the spec's four dropped fields and the struct's absence of it is the whole story.)

Replace the marker, whose deferred work now exists and is deliberately not done:

```rust
//! ponytail: bindings are never deleted, and the node-retirement path (`decommission.rs`) does
//! not collect them — deliberately. A binding is an OWNER's namespaces, not a node's: draining
//! the last node an owner happened to run on must not delete the namespace their workspaces come
//! back to. The upgrade, if orphaned bindings ever cost anything, is an owner-deletion path in
//! `/v1`, not a node-side sweep.
```

- [ ] **Step 4: Retire `retired`'s marker if Task 1 left it standing**

`peer/pull.rs`'s `retired` marker ("all-or-nothing rather than transients-only") is still accurate after Task 1 — Task 1 added a guard, it did not split the reclaim. Keep it verbatim. (The review's K7 note that it "goes with" a deleted `retired()` applied to the alternative fix, which this plan did not take. Say so in the commit body.)

- [ ] **Step 5: Run the tests**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src
git commit -m "Say snapshot where the agent still said commit"
```

**RBAC / admission:** none. No CRD field, no route, no wire name changed.

---

## Task 22: K8 — one test fixture, and the btrfs-gated tests named

**Files:**
- Create: `bins/agent/src/testsupport.rs`
- Modify: `bins/agent/src/lib.rs` (declare it `#[cfg(test)] mod testsupport;`)
- Modify: `bins/agent/src/listing.rs`, `claim.rs`, `sync.rs`, `decommission.rs`, `controller/volume.rs`, `peer/*.rs` — delete each local `NoopNix` + `test_ctx` and `use crate::testsupport::*;`
- Modify: `bins/agent/src/janitor.rs:523`, `snapshot.rs`, `bins/agent/tests/reconcile.rs`, `bins/agent/tests/peer.rs` — mark the gated tests
- Create: `bins/agent/tests/README.md`? **No.** Put the list in `bins/agent/src/lib.rs`'s module doc, where a reader editing the engine will hit it.

**Why (K8):** six near-identical copies of the same ~30 lines (`NoopNix` + `test_ctx`). One shared fixture deletes ~150 lines. Note the review's suggestion of `rustic_git_workspaces::kube_test` is **not** taken: `Ctx` and `Nix` are the agent binary's own types, and a workspaces-crate helper cannot name them without the crate depending on the binary. A `#[cfg(test)]` module in the agent crate is the right home.

**Why (the gated tests):** CI runs on a container with no loopback btrfs and no root, so `janitor.rs::cleanup_local_deletes_nested_commit_model_subvolumes` — the **only** test of `cleanup_local` against real subvolumes — never runs there, and every other `cleanup_local`/`drop_stale_worktrees` test exercises `btrfs_delete`'s `#[cfg(test)]` `remove_dir_all` fallback, proving the fallback rather than the production path. Several more pass on a Mac only because the code short-circuits before touching btrfs. A reader editing the engine has no way to know which.

- [ ] **Step 1: Write the shared fixture**

`bins/agent/src/testsupport.rs`:

```rust
//! The one test `Ctx`. Six copies of these thirty lines had drifted apart — a route list here, a
//! different pool root there — which is how two tests of the same rule end up asserting against
//! two different worlds.

use crate::controller::Ctx;
use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
use rustic_git_workspaces::kube_test::{mock_client, Recorder, Route};
use std::sync::Arc;

pub(crate) struct NoopNix;

#[async_trait::async_trait]
impl crate::nix::Nix for NoopNix {
    async fn build(&self, _expr: &str, _timeout: std::time::Duration) -> Result<std::path::PathBuf, String> {
        Ok(std::path::PathBuf::from("/tmp"))
    }
    async fn ping(&self) -> Result<(), String> {
        Ok(())
    }
    async fn collect_garbage(&self) -> Result<u64, String> {
        Ok(0)
    }
}

pub(crate) fn test_ctx(pool: &std::path::Path, node: &str, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
    let (client, rec) = mock_client(routes);
    let engine = Engine::new(EnginePool::new(pool));
    // Set, not read from the environment: a pod spec built without an image is a reconcile error,
    // and every test in this binary shares one process env — hence `--test-threads=1`.
    std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
    (
        Arc::new(Ctx::new(
            client,
            Arc::new(engine),
            node.into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            vec![],
            Some("test:/".into()),
            Arc::new(NoopNix),
            pool.join("profiles"),
        )),
        rec,
    )
}
```

- [ ] **Step 2: Delete the six copies**

`grep -rn "struct NoopNix" bins/agent/src` and delete each, plus each local `fn test_ctx`. Add `use crate::testsupport::{test_ctx, NoopNix};` to each test module. If a copy differs from the shared one in any way that matters, **that difference is the bug** — reconcile it explicitly in the diff, do not preserve it as a second fixture.

- [ ] **Step 3: Mark every btrfs-gated test**

Give the one explicitly gated test a named gate rather than an ad-hoc `if`:

```rust
    /// GATED ON REAL BTRFS. Skipped in CI (no loopback btrfs, no root) — the only test of
    /// `cleanup_local` against real subvolumes. Every other `cleanup_local` test exercises
    /// `btrfs_delete`'s `#[cfg(test)]` `remove_dir_all` fallback, i.e. proves the fallback and not
    /// the production path. Run it on the Linux VM, or through `tests/ws_e2e.sh`.
    #[test]
    fn cleanup_local_deletes_nested_commit_model_subvolumes() {
        if !have_btrfs() {
            eprintln!("SKIPPED (needs loopback btrfs + root): cleanup_local_deletes_nested_commit_model_subvolumes");
            return;
        }
```

Add a one-line `/// IMPLICITLY GATED: passes on a Mac only because …` doc to each test the review names, saying which short-circuit carries it:

- `snapshot.rs::cut_on_my_node_sets_ready_and_advances_head_preserving_other_status_fields` — "`snap/{name}` is pre-created, so `commit_worktree`'s `dst.exists()` returns before any btrfs call."
- `reconcile.rs::commit_model_checkout_converges_on_an_existing_worktree`
- `reconcile.rs::commit_model_environment_bootstrap_materializes_its_worktree`
- `reconcile.rs::commit_model_clone_checks_out_its_graft_commit_and_records_it_as_head`
- `reconcile.rs::the_sync_beat_cuts_a_transient_only_when_the_worktree_generation_moved`
  — all four: "pre-created directories stand in for subvolumes, so no `btrfs` binary is invoked. A change that makes the engine actually shell out will pass here and fail on a node."
- `bins/agent/tests/peer.rs` (file-level doc): "drives the router with a fake `btrfs send` script — good coverage of auth, `valid_segment` and streaming; **zero** coverage of the receive half (`pull_one`) against a real `btrfs receive`. That half is only ever exercised by `tests/ws_e2e.sh`."

- [ ] **Step 4: Give the receive half a Mac-runnable seam**

`pull_one` already takes `btrfs_bin`, which the send-side tests point at a fake script. Add the receive-side test that seam makes possible — it needs no btrfs and it is the gap the review names:

```rust
/// The receive half against a fake `btrfs receive`: a truncated body must delete the partial and
/// return an error, so the puller tries the next source rather than keeping a half-received
/// subvolume that `local_commits` would then advertise. The real `btrfs receive` is only ever
/// exercised by `tests/ws_e2e.sh`; this covers the code AROUND it, which is where the bugs were.
#[tokio::test]
async fn a_truncated_receive_deletes_the_partial_and_fails() {
    let tmp = tempfile::tempdir().unwrap();
    // A fake btrfs whose `receive` arm creates the subvolume directory and then exits non-zero,
    // exactly as a real one does on a bad `-p`.
    let fake = write_fake_btrfs(tmp.path(), "receive-fails-after-creating");
    let engine = Engine::new(EnginePool::new(tmp.path()));
    let server = serve_one_body(b"partial stream").await; // a oneshot HTTP server, addr returned

    let err = pull_one(&engine, &fake, &peer_http_client().unwrap(), &server.addr, "s3cret", "v1", "c1", None, receive_ceiling(0))
        .await
        .expect_err("a failed receive is an error");

    assert!(err.contains("btrfs receive failed"), "{err}");
    assert!(!engine.pool.snap("v1", "c1").exists(), "the partial must not survive a failed receive");
}
```

`write_fake_btrfs` and `serve_one_body` are new local helpers in `bins/agent/tests/peer.rs`; the file already writes fake scripts for the send side — extend that builder rather than adding a second one.

- [ ] **Step 5: Strengthen the two weak absence-assertions**

`sync.rs::a_failed_parent_listing_cuts_no_sync_points` asserts only "no POST", and `decommission.rs::a_drain_leaves_a_running_parent_completely_alone` asserts "no DELETE" plus the absence of a path substring — both against a mock whose route list would 404 anyway, so they pass for the wrong reason if the code calls a different API. Give each a positive assertion alongside:

```rust
    // A negative alone passes for the wrong reason against a mock that 404s everything. Pin what
    // the pass DID do, so a code change that calls something else fails here rather than silently
    // satisfying the absence.
    assert_eq!(rec.calls(), vec![format!("GET {WORKSPACES}"), format!("GET {ENVIRONMENTS}")], "the beat lists, and stops");
```

(adjust each to the calls its own pass actually makes — run the test and read `rec.calls()` to get the real list, then assert it.)

- [ ] **Step 6: Write the list where an engine editor will see it**

Add to `bins/agent/src/lib.rs`'s module doc:

```rust
//! # Tests that do not run in CI
//!
//! CI has no loopback btrfs and no root. `janitor::cleanup_local_deletes_nested_commit_model_subvolumes`
//! is the only test gated explicitly on `have_btrfs()` and the only one exercising `cleanup_local`
//! against real subvolumes; every other test of that path proves `btrfs_delete`'s test-only
//! `remove_dir_all` fallback instead. Several more pass on a Mac only because the code
//! short-circuits before touching btrfs (each carries an `IMPLICITLY GATED` doc line saying which
//! short-circuit). If you change the engine so a path that used to return early now shells out,
//! those tests keep passing here and fail on a node — run `tests/ws_e2e.sh` on the Linux VM.
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: `exit=0`, and a test count **higher** than before by the tests added in Steps 4 and 5.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 8: Commit**

```bash
git add bins/agent/src bins/agent/tests
git commit -m "Share one test context and name the btrfs-gated tests"
```

**RBAC / admission:** none.

---

## Self-Review

**1. Spec and review coverage.** Every finding in `agent.md` maps to a task: C1→1, C3→2, C2→3, I1→4, I2→5, I3→6, I4→7, I5→8, I6→9, M1→10, M2→11, M3→12, M4→13, M5/M6/M7→14, K1/K2→19, K3/K4→20, K5/K6/K7→21, K8→22, K9→18. The per-beat cost table's two named costs are addressed: `retire_pass`'s reactor-blocking walk (Task 4) and I6, "the largest avoidable one" (Task 9); `retire_pass`'s cluster-wide `Snapshot` LIST — "the single largest recurring cost" — is deliberately **not** changed, because both byte sweeps read that one listing and splitting it is how two sweeps end up acting on different views (`listing.rs`'s rule, item 1 of "what is good"). The two audit cuts are Tasks 16 and 15. Every item in "What is good, and should not be touched" survives: `listing::Beat`'s partial rule, `volume_decision` + `sweep_volumes` as one function (Task 3 adds an arm, never a second copy), the claim's 409 re-decide, `up_to_date` by name, `my_node`'s dead-guard, `lib.rs`'s mount hygiene, `write_resolv_conf`'s in-place write, `wake_worthy`'s coalescing (Task 5 adds a floor beside it, not in place of it), `mkdir_env_mounts`' validate-before-mkdir, `secret_ok`'s empty guard (Task 14 documents it rather than touching it), `KillOnDrop` (Task 6 drops it on a deadline, which is the mechanism it already implements), and `janitor_sweep_profiles`' asymmetric strictness.

**2. Placeholder scan.** No task says "add error handling" or "write tests for the above"; every code step carries the code and every test step carries the assertions. Two tasks deliberately ship without a new test and say why: Task 12 (a rate change under an unchanged ceiling, where a poll-count assertion would pin an implementation detail) and Task 13 (a one-line removal of an unreachable `unwrap`, covered by existing callers). Task 4's test asserts a proxy for "not on the reactor" and says so.

**3. Type consistency.** `after_pass` takes four arguments everywhere after Task 5. `pull_one` takes `max_bytes: u64` last, after `parent`, everywhere after Task 7. `interesting_volumes` is `async` after Task 4 and its one caller awaits it. `cas` has one signature, used by five call sites in Task 16. `seeded_from_cuts` is `pub(crate)` from Task 2 onward and both its readers use the same set. `newest_recorded` (Task 8) and `newest_transient_of` (`crd`) use the same `(generation, name)` key. Task 17 changes no path: every `crate::peer::…` name is re-exported from `peer/mod.rs`.

**4. Ordering dependencies.** Task 15 is **blocked** on the workspaces-api plan removing `VolumeSource::RestoreOf` from the CRD and has a Step 1 that stops if it has not. Task 16 rewrites the release arm Task 3 edits, so it must follow it. Task 17 moves code from Tasks 1 and 3–7 and 14, so it must follow all of them. Task 22 lifts the fixture file Task 17 creates. Everything else is independent.
