# Stop, Interruption and Node Decommission Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a stop take seconds instead of minutes by moving the replica wait out of `stop_push` and into placement; make an interrupted (node-dead) parent a state a person can act on rather than a wedge; and make a node's retirement a labelled, observable drain that never stops anyone's running work.

**Architecture:** One truth per fact, computed once and read everywhere. `up_to_date(replica, worktree, newest_transient)` (Task 1) is the single placement bar — a replica is up to date for a worktree when its `status.branches[worktree]` names that worktree's newest Ready transient, falling back to plain `Synced` when the worktree has no transient at all. `unplaceable(node)` (Task 3) is the single "not a place to run" predicate, true for a node dead past `WS_NODE_DEAD_SECS` and for one labelled `kloudlite.io/decommission=true`. The `Replicated` condition on a stopped parent (Task 4) is the single "is it replicated" truth, written by the owner's reconcile and read by the sweep, `/v1` and the web. `volume_decision(...)` (Task 6) is the single per-volume sweep arm, called with the dead set or with the decommission set. Stop cuts and tears down at once, then POSTs `/peer/v1/wake` to every placeable node so the peers pull within seconds instead of at the next five-minute beat.

**Tech Stack:** Rust (axum, kube-rs, tokio), Kubernetes CRDs in `kloudlite.io/v1alpha1`, btrfs send/receive over the agent peer listener, Next.js app router + bun test for the web half.

**Spec:** docs/superpowers/specs/2026-09-03-stop-interrupt-decommission-design.md

## Global Constraints
- `Replicated` is written `False / Running` by the owner the instant a parent starts (in the same status write that records the pod), and recomputed only while the parent is stopped; the spread chooser, the sweep and `/v1` read the condition and never recompute it (spec, 2026-09-03 amendment).
- Up to date, for a worktree: the replica row's `status.branches[worktree]` equals that worktree's newest Ready transient's NAME; never a clock — `lastSyncAt` is the puller's clock and `readyAt` the owner's.
- A worktree with no transient at all (never ran, or a fresh restore) falls back to plain `phase == "Synced"`.
- `status.branches` semantics: `worktree → snapshot name`, the newest Ready transient of that worktree this node actually holds locally, written by the pull pass that holds it.
- One predicate for placement: `unplaceable(node)` is true when the node is dead (NotReady past `WS_NODE_DEAD_SECS`) OR carries the label `kloudlite.io/decommission=true`. `live_nodes`, `owner_alive`, `standby_count`, `may_claim` and both sweeps call it; nothing tests the two conditions separately.
- The wake route is exactly `POST /peer/v1/wake` on the peer listener, authenticated by `x-peer-secret` like `/peer/v1/commit/{volume}/{name}`; a wake that cannot be delivered is a `tracing::warn!`, never an error — the ticker still comes.
- The wake `Notify` coalesces: a pull pass already running finishes and runs once more via a pending flag, never concurrently.
- `Replicated` condition on a stopped parent, written on every reconcile of a stopped parent: `False / AwaitingReplica` with message `"no other node holds the final sync point yet"`; `False / AwaitingReplica` with message `"no replica is configured for this volume"` when `spec.replicas == 1`; `True / Replicated` with message `"another node holds the final sync point"`.
- No `NoReplica` reason and no `AwaitingReplica` reason on `NodeDead`: a parent waiting for both shows `NodeDead` plus `Replicated=False`.
- No `Released` reason: `Unavailable` with an empty pin IS the released state.
- Dead-node / decommission volume conditions: running parent → `Available=False / NodeDead`, pin kept; not replicated → `Available=False / NodeDead`, pin kept; releasable → pin cleared, `Available=False / NodeDead` (dead sweep) or `Available=False / Decommissioned` (decommission beat).
- The interrupted-parent condition stays `Degraded=True / NodeDead`.
- `/v1` start answers 409 with exactly: `"workspace is interrupted: its node is down; it resumes when the node returns"` / `"environment is interrupted: its node is down; it resumes when the node returns"`.
- Every clone response carries `basedOn: {snapshot, at, age}` and the web renders the sentence shape `"cloned from the sync point of 14:32:07, 6 minutes before the node went down"` for an interrupted source, `"cloned from the sync point of 14:32:07"` otherwise.
- Clone cuts `clone-{ws}-{hex}` (`crd::short_hex()`), a transient `Snapshot`, exactly like a sync point.
- Decommission label key: `kloudlite.io/decommission`, value `"true"`.
- One decommission annotation key, `kloudlite.io/decommission-status`, with values `"draining running=N owned=N copies=N"` while in progress and `"drained <RFC 3339>"` when done.
- The decommission beat runs every 30 s, on the decommissioning node's own agent only, and stops nothing.
- `Decommissioning=True / NodeLeaving` on each running parent of a decommissioning node, message exactly: `"this node is being retired; stop when convenient and the next start lands elsewhere"`.
- Deleted outright: `flush_gate`, `flush_expired`, `flush_timeout`, `NO_PEERS`, `NO_READY_AT`, `FLUSH_TIMED_OUT`, `FlushUnreplicated`, `WS_STOP_FLUSH_TIMEOUT_SECS`, `STOP_GENERATION`, `source_nodes`, `release_dead_volumes`, `unclaim_kind`'s `releasable` closures and its `running_volumes` plumbing.
- `StopPush::Landed` is a unit variant.
- `status.compatibleNodes` is no longer written and is dropped from the CRD schemas; the Rust structs keep the field with `#[serde(default)]` so old stored objects still parse.
- The two-step volume move stays (owner clears the pin, taker CASes it) — the admission policy in `deploy/k3s/agent-admission.yaml` allows only `nodeName` owned→`""` and `""`→node.
- The 180 s dead-node floor, the stop snapshot, sync points, retention, the pull protocol and the takeover CAS are unchanged.

---
### Task 1: Replica rows record the newest held transient per worktree

**Files:**
- Modify `bins/agent/src/peer.rs`: `pull_volume` (lines 440–557, the `have`/`ready` sets are already in scope at line 552), `write_replica_status` (lines 625–661, `branches: Default::default()` at line 651 is the hole this fills); add `up_to_date` and `newest_transient_of` beside them.
- Test `bins/agent/src/peer.rs` `mod reconcile_tests` (harness at lines 1006–1118: `NoopNix`, `test_ctx`, `list_of`, `beat_of`, `replica_of`, `ready_transient` at line 1788).

**Interfaces:**
- Consumes: `crd::VolumeReplicaStatus::branches` (`crates/workspaces/src/crd.rs:310`), `crate::sync::SYNCED_GENERATION` (`bins/agent/src/sync.rs:27`).
- Produces:
  - `pub(crate) fn newest_transient_of(snaps: &[crd::Snapshot], worktree: &str) -> Option<String>` — the highest-`SYNCED_GENERATION` Ready transient of `worktree` among `snaps`, ties broken by name so two nodes agree.
  - `pub(crate) fn up_to_date(replica: &crd::VolumeReplica, worktree: &str, newest_transient: Option<&str>) -> bool`
  - `async fn write_replica_status(ctx: &Arc<Ctx>, volume: &str, synced: bool, listed_at: &str, branches: BTreeMap<String, String>) -> Result<(), kube::Error>` (one added argument).
  - `pub(crate) async fn newest_transient(ctx: &Arc<Ctx>, volume: &str, worktree: &str) -> Result<Option<String>, kube::Error>` — the same answer for a caller that has no beat listing (the stop path, `/v1`-adjacent code in Tasks 4 and 6 use it).

- [ ] **Step 1: Write the failing test** — append to `mod reconcile_tests` in `bins/agent/src/peer.rs`:

```rust
    // ---------------------------------------------------------------------------------------
    // Task 1: `status.branches` is the newest Ready transient this node HOLDS, per worktree —
    // the one thing placement is allowed to read, because a name cannot be skewed by a clock.
    // ---------------------------------------------------------------------------------------

    fn transient_gen(name: &str, volume: &str, worktree: &str, generation: u64) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1",
            "kind": "Snapshot",
            "metadata": {"name": name, "uid": format!("uid-{name}"),
                         "annotations": {"kloudlite.io/synced-generation": generation.to_string()}},
            "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": "",
                     "pinned": false, "transient": true},
            "status": {"phase": "ready"},
        })
    }

    fn snaps_of(items: Vec<serde_json::Value>) -> Vec<crd::Snapshot> {
        items.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect()
    }

    /// Generation, not creation time, and not the name's suffix: the annotation is the btrfs
    /// generation the sync beat actually replicated, and it is the only ordering that survives
    /// clock skew between the owner and a puller.
    #[test]
    fn newest_transient_is_the_highest_generation_of_that_worktree() {
        let snaps = snaps_of(vec![
            transient_gen("sync-ws-1-aaaa", "vol-1", "ws-1", 10),
            transient_gen("sync-ws-1-bbbb", "vol-1", "ws-1", 42),
            transient_gen("sync-ws-2-cccc", "vol-1", "ws-2", 99),
            ready_snapshot("vol-1-commit", "vol-1", ""),
        ]);
        assert_eq!(newest_transient_of(&snaps, "ws-1").as_deref(), Some("sync-ws-1-bbbb"));
        assert_eq!(newest_transient_of(&snaps, "ws-2").as_deref(), Some("sync-ws-2-cccc"));
        assert_eq!(newest_transient_of(&snaps, "ws-none"), None, "a worktree with no transient has none");
    }

    /// The stop transient carries no generation annotation at all (the stop path cuts it before
    /// the post-cut re-stamp), so it reads as 0 — and must still LOSE to an annotated one rather
    /// than winning by being newest-created. Ties break by name so two nodes agree.
    #[test]
    fn an_unannotated_transient_reads_as_generation_zero() {
        let mut stop = transient_gen("stop-ws-1-7", "vol-1", "ws-1", 0);
        stop["metadata"]["annotations"] = serde_json::json!({});
        let snaps = snaps_of(vec![stop.clone(), transient_gen("sync-ws-1-aaaa", "vol-1", "ws-1", 5)]);
        assert_eq!(newest_transient_of(&snaps, "ws-1").as_deref(), Some("sync-ws-1-aaaa"));
        assert_eq!(newest_transient_of(&snaps_of(vec![stop]), "ws-1").as_deref(), Some("stop-ws-1-7"));
    }

    fn replica_with_branches(volume: &str, node: &str, phase: &str, branches: serde_json::Value) -> crd::VolumeReplica {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": format!("{volume}.{node}"), "uid": format!("uid-{node}")},
            "spec": {"volume": volume, "node": node},
            "status": {"phase": phase, "branches": branches},
        }))
        .unwrap()
    }

    /// The whole placement bar, in one function: the NAME must match. A `Synced` row whose
    /// branches still name the previous sync point is a replica that has not pulled the stop cut
    /// — exactly the retention case the spec calls out — and must not be allowed to start it.
    #[test]
    fn up_to_date_compares_names_never_phases_or_clocks() {
        let holding = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({"ws-1": "sync-ws-1-bbbb"}));
        let behind = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({"ws-1": "sync-ws-1-aaaa"}));
        assert!(up_to_date(&holding, "ws-1", Some("sync-ws-1-bbbb")));
        assert!(!up_to_date(&behind, "ws-1", Some("sync-ws-1-bbbb")));
        assert!(!up_to_date(&holding, "ws-2", Some("sync-ws-2-cccc")), "another worktree's branch is not this one's");
    }

    /// No transient at all (never ran, or a fresh restore): plain `Synced` is the right bar —
    /// a Synced replica holds every Ready commit, which is all there is to hold.
    #[test]
    fn with_no_transient_plain_synced_is_up_to_date() {
        let synced = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({}));
        let syncing = replica_with_branches("vol-1", "node-b", "Syncing", serde_json::json!({}));
        assert!(up_to_date(&synced, "ws-1", None));
        assert!(!up_to_date(&syncing, "ws-1", None));
        assert!(!up_to_date(&syncing, "ws-1", Some("sync-ws-1-bbbb")), "mid-pull is never up to date");
    }

    /// The pull pass writes what it HOLDS, not what it listed: a transient whose subvolume never
    /// landed here must not appear in `branches`, or this node advertises data it cannot serve.
    #[tokio::test]
    async fn a_pull_pass_records_only_the_transients_it_actually_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let created = serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": "vol-1.node-b", "uid": "r-uid"},
            "spec": {"volume": "vol-1", "node": "node-b"},
            "status": {"phase": "Syncing", "branches": {}},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![
                transient_gen("sync-ws-1-aaaa", "vol-1", "ws-1", 5),
            ])},
            Route { method: "GET", path: "/api/v1/namespaces/kube-system/pods".into(), status: 200, body: list_of("Pod", vec![]) },
            Route { method: "GET", path: format!("{VOLREPLICAS}/vol-1.node-b"), status: 200, body: created.clone() },
            Route { method: "PUT", path: format!("{VOLREPLICAS}/vol-1.node-b/status"), status: 200, body: created },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        // No local commits: nothing was pulled, so nothing is held.
        std::fs::create_dir_all(ctx.engine.pool.snap_dir("vol-1")).unwrap();

        let http = peer_http_client().unwrap();
        pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1").await;

        let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["status"]["phase"], "Syncing");
        assert!(
            sent[0]["status"]["branches"].as_object().is_none_or(|b| b.is_empty()),
            "a transient this node does not hold must never appear in branches: {:?}",
            sent[0]["status"]["branches"]
        );
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent --lib peer::reconcile_tests` fails to compile with `cannot find function `newest_transient_of` in this scope` and `cannot find function `up_to_date` in this scope`.

- [ ] **Step 3: Implement** — in `bins/agent/src/peer.rs`, add above `write_replica_status` (line 625):

```rust
/// The newest Ready transient of `worktree` among `snaps` — ordered by the sync beat's
/// `SYNCED_GENERATION` annotation, never by creation time, because the annotation is the btrfs
/// generation actually replicated and it is the one ordering that survives clock skew between the
/// owner that cut it and the node that pulled it. A stop cut carries no annotation (it is stamped
/// post-cut on the owner only) and so reads as 0: it loses to any annotated one and still beats
/// nothing. Ties break by NAME so two nodes computing this independently never disagree.
pub(crate) fn newest_transient_of(snaps: &[crd::Snapshot], worktree: &str) -> Option<String> {
    snaps
        .iter()
        .filter(|s| {
            s.spec.transient
                && s.spec.worktree == worktree
                && s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready)
        })
        .map(|s| {
            let gen = s.annotations().get(crate::sync::SYNCED_GENERATION).and_then(|g| g.parse::<u64>().ok()).unwrap_or(0);
            (gen, s.name_any())
        })
        .max()
        .map(|(_, name)| name)
}

/// THE placement bar, and the only one: a replica is up to date for a worktree when it HOLDS that
/// worktree's newest Ready transient, by name. Names, not clocks — `lastSyncAt` is stamped by the
/// pulling node and `readyAt` by the owner, and a skewed clock must never make an old copy look
/// current. A worktree with no transient at all (never ran, or a fresh restore) has nothing to
/// name, so plain `Synced` is the right bar: a Synced replica holds every Ready commit.
pub(crate) fn up_to_date(replica: &crd::VolumeReplica, worktree: &str, newest_transient: Option<&str>) -> bool {
    let Some(st) = replica.status.as_ref() else { return false };
    match newest_transient {
        None => st.phase == "Synced",
        Some(want) => st.branches.get(worktree).is_some_and(|held| held == want),
    }
}

/// `newest_transient_of` for a caller with no beat listing of its own — one field-selected list.
pub(crate) async fn newest_transient(ctx: &Arc<Ctx>, volume: &str, worktree: &str) -> Result<Option<String>, kube::Error> {
    let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let list = api.list(&ListParams::default().fields(&format!("spec.volume={volume}"))).await?;
    Ok(newest_transient_of(&list.items, worktree))
}
```

Then in `pull_volume`, replace the final status write (line 552–556) with one that computes the per-worktree branches from what this pass actually holds:

```rust
    let missing_at_end = ready.iter().any(|s| !have.contains(&s.name_any()));
    // What this node HOLDS, per worktree — not what it listed. A transient whose subvolume never
    // landed here would otherwise advertise data this node cannot serve, and placement would then
    // start a worktree on a node with no bytes for it. `have` is the disk, after the pull loop and
    // after the retire sweep above, so this is the honest answer for this pass.
    let held: Vec<crd::Snapshot> = ready.iter().filter(|s| have.contains(&s.name_any())).cloned().collect();
    let mut branches: std::collections::BTreeMap<String, String> = Default::default();
    for worktree in held.iter().map(|s| s.spec.worktree.clone()).collect::<HashSet<_>>() {
        if let Some(newest) = newest_transient_of(&held, &worktree) {
            branches.insert(worktree, newest);
        }
    }
    if let Err(e) = write_replica_status(ctx, volume, !missing_at_end, &listed_at, branches).await {
        tracing::warn!(%volume, error = %e, "pull: writing VolumeReplica status");
    }
```

And `write_replica_status` (line 625) takes and writes them:

```rust
/// Create-or-update THIS node's own `VolumeReplica` — the sole writer, per the module doc.
/// `branches` is `worktree -> the newest Ready transient this node holds`, which is what every
/// placement decision reads; `phase` is `Synced` iff nothing was missing at the end of this pass.
async fn write_replica_status(
    ctx: &Arc<Ctx>,
    volume: &str,
    synced: bool,
    listed_at: &str,
    branches: std::collections::BTreeMap<String, String>,
) -> Result<(), kube::Error> {
    // ... unchanged create-or-get ...
    let status = crd::VolumeReplicaStatus {
        phase: if synced { "Synced" } else { "Syncing" }.to_string(),
        branches,
        last_sync_at: Some(listed_at.to_string()),
    };
    // ... unchanged two-attempt replace_status ...
}
```

Update the two existing callers' expectations in `write_replica_status_stamps_the_volume_label_on_create` (line 1135) and `write_replica_status_stamps_the_listing_instant_not_the_write_instant` (line 1166) to pass `Default::default()`.

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`** — `cargo test -p kloudlite-agent` then the clippy line.
- [ ] **Step 5: Commit** — `git add bins/agent/src/peer.rs && git commit -m "Record the newest held transient per worktree in replica rows"`

---
### Task 2: `/peer/v1/wake` and a coalescing pull notify

**Files:**
- Modify `bins/agent/src/peer.rs`: `router` (lines 70–74), add the `wake` handler beside `commit` (lines 117–166), add `wake_peers`; `PeerState` (lines 36–51) gains the notify.
- Modify `bins/agent/src/controller/mod.rs`: `Ctx` (lines 60–131) gains `pub pull_wake: Arc<tokio::sync::Notify>`, initialised in `Ctx::new` (lines 135–176).
- Modify `bins/agent/src/controller/run.rs`: `spawn_pull` (lines 321–330).
- Test `bins/agent/src/peer.rs` `mod reconcile_tests`, and `bins/agent/tests/peer.rs` for the router half (it already drives the peer listener with a fake `btrfs`).

**Interfaces:**
- Consumes: `PeerState::secret` / `secret_ok` (line 88), `pool_nodes` (line 223), `agent_pod_addr` (line 235), `peer_http_client` (line 258), `Task 3::unplaceable`.
- Produces:
  - `pub async fn wake_peers(ctx: &Arc<Ctx>, live: &[String])` — one `POST /peer/v1/wake` per node in `live` that is not me; every failure is a warn.
  - `async fn wake(State(state): State<Arc<PeerState>>, headers: HeaderMap) -> impl IntoResponse`
  - `Ctx::pull_wake: Arc<tokio::sync::Notify>`

- [ ] **Step 1: Write the failing test** — append to `mod reconcile_tests` in `bins/agent/src/peer.rs`:

```rust
    // ---------------------------------------------------------------------------------------
    // Task 2: the wake. A stop or a clone pokes every placeable peer so the pull happens in
    // seconds instead of at the next `WS_REPLICA_SECS` beat.
    // ---------------------------------------------------------------------------------------

    /// One POST per live peer, never to myself, and an unreachable peer is a warn — the ticker
    /// still comes, so a wake that cannot be delivered must never fail the stop that sent it.
    #[tokio::test]
    async fn wake_peers_posts_once_per_live_peer_and_skips_me() {
        let tmp = tempfile::tempdir().unwrap();
        let pod = |node: &str, ip: &str| serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": format!("agent-{node}"), "namespace": "kube-system"},
            "spec": {"nodeName": node},
            "status": {"podIP": ip},
        });
        let routes = vec![Route {
            method: "GET",
            path: "/api/v1/namespaces/kube-system/pods".into(),
            status: 200,
            body: list_of("Pod", vec![pod("node-a", "127.0.0.1"), pod("node-b", "127.0.0.1")]),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        std::env::set_var("WS_PEER_SECRET", "s3cret");

        wake_peers(&ctx, &["node-a".to_string(), "node-b".to_string()]).await;

        // node-a is me: no address is ever resolved for it, so exactly one pod lookup happens.
        let looked_up = rec.requests().into_iter().filter(|r| r.contains("/pods?")).count();
        assert_eq!(looked_up, 1, "one address lookup, for the peer only: {:?}", rec.requests());
    }

    /// The notify coalesces: N wakes arriving during one pass produce exactly one more pass, not
    /// N. `Notify::notify_one` stores a single permit, which is the whole mechanism.
    #[tokio::test]
    async fn many_wakes_in_a_burst_coalesce_into_one_more_pass() {
        let n = Arc::new(tokio::sync::Notify::new());
        for _ in 0..5 {
            n.notify_one();
        }
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), n.notified()).await.is_ok());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), n.notified()).await.is_err(),
            "five wakes are one pending permit, not five passes"
        );
    }
```

And, in `bins/agent/tests/peer.rs`, the router half (copy the file's existing `serve`-on-a-port harness):

```rust
/// The wake route is authenticated exactly like the commit route, and answers 204 with no body:
/// it is a poke, not a transfer. An unauthenticated wake is a 401, or any pod on the cluster
/// could drive every agent's pull beat at will.
#[tokio::test]
async fn wake_requires_the_peer_secret_and_answers_204() {
    let tmp = tempfile::tempdir().unwrap();
    let (client, _rec) = mock_client(vec![]);
    let state = kloudlite_agent::peer::PeerState::new(
        client,
        tmp.path().to_string_lossy().into(),
        "node-a".into(),
        "s3cret".into(),
        "btrfs".into(),
    );
    let notify = state.pull_wake.clone();
    let app = kloudlite_agent::peer::router(state);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let http = reqwest::Client::new();

    let bad = http.post(format!("http://{addr}/peer/v1/wake")).send().await.unwrap();
    assert_eq!(bad.status(), 401);

    let ok = http.post(format!("http://{addr}/peer/v1/wake")).header("x-peer-secret", "s3cret").send().await.unwrap();
    assert_eq!(ok.status(), 204);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), notify.notified()).await.is_ok(),
        "an authenticated wake fires the pull notify"
    );
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent wake` fails with `cannot find function `wake_peers` in this scope` and `no field `pull_wake` on type `PeerState``.

- [ ] **Step 3: Implement** —

`bins/agent/src/controller/mod.rs`, in `Ctx` (after `applied`, line 130):

```rust
    /// Fired by `/peer/v1/wake` and awaited by `spawn_pull`. A `Notify` and not a channel because
    /// the payload is nothing at all: "something changed, pull now". `notify_one` stores exactly
    /// one permit, so a burst of stops coalesces into one extra pass instead of N.
    pub pull_wake: Arc<tokio::sync::Notify>,
```

and in `Ctx::new`, `pull_wake: Arc::new(tokio::sync::Notify::new()),`.

`bins/agent/src/peer.rs`, `PeerState` gains `pub pull_wake: Arc<tokio::sync::Notify>` (set to a fresh `Notify` in `new`, and to `ctx.pull_wake.clone()` in `from_ctx`), and:

```rust
pub fn router(state: PeerState) -> Router {
    Router::new()
        .route("/peer/v1/commit/{volume}/{name}", get(commit))
        // A poke, not a transfer: the body is empty and the answer is 204. Same secret as the
        // commit route and the same NetworkPolicy, because it drives the same root-run machinery.
        .route("/peer/v1/wake", axum::routing::post(wake))
        .with_state(Arc::new(state))
}

/// "Something you replicate just changed; pull now." The whole handler is one `notify_one`: the
/// puller decides what to fetch, exactly as it does on its own ticker. Nothing here is trusted
/// beyond the secret — a wake can only make a pass happen sooner, never change what it pulls.
async fn wake(State(state): State<Arc<PeerState>>, headers: HeaderMap) -> impl IntoResponse {
    if !secret_ok(&headers, &state.secret) {
        return StatusCode::UNAUTHORIZED;
    }
    state.pull_wake.notify_one();
    StatusCode::NO_CONTENT
}

/// POST `/peer/v1/wake` to every placeable node but me. Every failure is a warn and never an
/// error: the wake is an optimisation on top of the ticker, and a stop that failed because a peer
/// was unreachable would be strictly worse than a stop that replicates a beat later.
pub async fn wake_peers(ctx: &Arc<Ctx>, live: &[String]) {
    let secret = std::env::var("WS_PEER_SECRET").unwrap_or_default();
    if secret.is_empty() {
        return; // fail-closed, same rule as every other dial in this file
    }
    let Ok(http) = peer_http_client() else { return };
    for node in live.iter().filter(|n| *n != &ctx.node) {
        let addr = match agent_pod_addr(&ctx.client, node).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(%node, error = %e, "wake: no peer address; the ticker will get it");
                continue;
            }
        };
        let url = format!("http://{addr}/peer/v1/wake");
        match http.post(&url).header("x-peer-secret", &secret).timeout(Duration::from_secs(5)).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::warn!(%node, status = %r.status(), "wake: peer refused"),
            Err(e) => tracing::warn!(%node, error = %e, "wake: peer unreachable; the ticker will get it"),
        }
    }
}
```

`bins/agent/src/controller/run.rs`, `spawn_pull` selects on both:

```rust
/// The commit model's puller: its own beat, so a slow pull never delays a reconcile — plus the
/// wake, so a stop or a clone is replicated in seconds instead of at the next tick. A pass already
/// running finishes and then runs ONCE more (the pending flag), never concurrently: two receives of
/// the same volume buy nothing but disk contention.
fn spawn_pull(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::peer::replica_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let wake = ctx.pull_wake.clone();
        let mut pending = false;
        loop {
            if !pending {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = wake.notified() => {}
                }
            }
            pending = false;
            crate::peer::pull_beat(&ctx).await;
            // Wakes that arrived DURING the pass: `notify_one` left one permit, so this takes it
            // without waiting and runs exactly one more pass however many arrived.
            if wake.notified().now_or_never().is_some() {
                pending = true;
            }
        }
    });
}
```

(`use futures::FutureExt;` for `now_or_never`, already a dependency of this binary.)

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`** — `cargo test -p kloudlite-agent` and `cargo test -p kloudlite-agent --test peer`.
- [ ] **Step 5: Commit** — `git add bins/agent/src && git commit -m "Add a peer wake route and coalesce it into the pull beat"`

---
### Task 3: One `unplaceable(node)` predicate for dead and decommissioning nodes

**Files:**
- Modify `bins/agent/src/peer.rs`: `node_is_dead` (lines 663–678), `live_nodes` (lines 308–310), `standby_count` (lines 314–316), `interesting_volumes` (line 388's `owner_alive`), `retire_pass` (line 976's `owner_alive`).
- Test `bins/agent/src/peer.rs` `mod reconcile_tests` (`dead_nodes_leave_the_candidate_list` at line 1398 and `a_dead_owner_is_not_a_copy` at line 1409 are the two to extend).

**Interfaces:**
- Consumes: `k8s_openapi::api::core::v1::Node`, `node_dead_secs()` (line 289).
- Produces:
  - `pub(crate) const DECOMMISSION_LABEL: &str = "kloudlite.io/decommission";` in `crates/workspaces/src/crd.rs` (beside `VOLUME_LABEL`), re-exported as `crd::DECOMMISSION_LABEL`.
  - `pub(crate) fn decommissioning(node: Option<&Node>) -> bool`
  - `pub(crate) fn unplaceable(node: Option<&Node>, floor: i64, now: k8s_openapi::jiff::Timestamp) -> bool`
  - `node_is_dead` stays, unchanged and still the reaper's rule — a decommissioning node's replica rows are NOT reaped.

- [ ] **Step 1: Write the failing test** — append to `mod reconcile_tests` in `bins/agent/src/peer.rs`:

```rust
    fn node_decommissioning(name: &str) -> Node {
        let mut n = node_ready_obj(name);
        n.metadata.labels.get_or_insert_with(Default::default).insert(crd::DECOMMISSION_LABEL.into(), "true".into());
        n
    }

    /// Dead and decommissioning are the SAME thing to placement, and nothing downstream is allowed
    /// to tell them apart: one predicate, or the sweep and the rendezvous eventually disagree
    /// about whether a node is a place to run and a volume ends up owned by nobody.
    #[test]
    fn decommissioning_is_unplaceable_but_not_dead() {
        let now = k8s_openapi::jiff::Timestamp::now();
        let floor = 180;
        let leaving = node_decommissioning("node-b");
        assert!(unplaceable(Some(&leaving), floor, now), "a decommissioning node takes no new work");
        assert!(!node_is_dead(Some(&leaving), floor, now), "but it is alive: its rows are not reaped and it still serves pulls");
        assert!(unplaceable(Some(&node_dead_obj("node-c", "2000-01-01T00:00:00Z")), floor, now));
        assert!(unplaceable(None, floor, now), "absent from a positive listing is unplaceable");
        assert!(!unplaceable(Some(&node_ready_obj("node-a")), floor, now));
    }

    /// A label value other than exactly "true" is not a decommission: a half-typed `kubectl label`
    /// must not silently drain a node.
    #[test]
    fn only_the_exact_true_value_decommissions() {
        let mut n = node_ready_obj("node-b");
        n.metadata.labels.get_or_insert_with(Default::default).insert(crd::DECOMMISSION_LABEL.into(), "yes".into());
        assert!(!decommissioning(Some(&n)));
        assert!(!decommissioning(Some(&node_ready_obj("node-a"))));
        assert!(decommissioning(Some(&node_decommissioning("node-b"))));
    }

    /// Rendezvous must stop naming a decommissioning node, or its copies never re-home: the whole
    /// "copies settle on their own" half of a drain is this one line.
    #[test]
    fn a_decommissioning_node_leaves_the_candidate_list_and_is_not_a_copy() {
        let pool: Vec<String> = ["node-a", "node-b", "node-c"].iter().map(|s| s.to_string()).collect();
        let nodes = vec![node_ready_obj("node-a"), node_decommissioning("node-b"), node_ready_obj("node-c")];
        let live = live_nodes(&pool, &nodes, 180, k8s_openapi::jiff::Timestamp::now());
        assert_eq!(live, vec!["node-a".to_string(), "node-c".to_string()]);
        // A decommissioning OWNER is not a copy either, so the volume asks for one standby more
        // and rendezvous places the replacement while the original is still serving pulls.
        assert_eq!(standby_count(false, 2), 3);
        assert_eq!(standby_count(true, 2), 2);
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent --lib peer::reconcile_tests::decommissioning` fails with `cannot find function `unplaceable` in this scope` and `cannot find value `DECOMMISSION_LABEL` in `crd``.

- [ ] **Step 3: Implement** —

`crates/workspaces/src/crd.rs`, beside `VOLUME_LABEL`:

```rust
/// Set on a `Node` by an operator (`kubectl label node <n> kloudlite.io/decommission=true`) to
/// retire it. A LABEL and not an annotation because it is a selector-worthy fact about the node,
/// and because removing it is the documented abort. Only the exact value `"true"` counts: a
/// half-typed label must never drain a node.
pub const DECOMMISSION_LABEL: &str = "kloudlite.io/decommission";
```

`bins/agent/src/peer.rs`, beside `node_is_dead` (line 663):

```rust
/// Whether an operator has asked for this node to be retired. Exact value only — see the constant.
pub(crate) fn decommissioning(node: Option<&Node>) -> bool {
    node.and_then(|n| n.metadata.labels.as_ref())
        .and_then(|l| l.get(crd::DECOMMISSION_LABEL))
        .is_some_and(|v| v == "true")
}

/// "Not a place to run", the ONE predicate every placement decision uses. Dead (NotReady past the
/// floor, or absent from a listing we did get) and decommissioning are the same answer here: both
/// mean nothing new may land, and keeping them as two tests is how the rendezvous and the sweep
/// eventually disagree about whether a node still owns anything.
///
/// It is deliberately NOT `node_is_dead`, which stays the reaper's rule: a decommissioning node is
/// alive, keeps serving pulls, and its replica rows must not be reaped out from under a peer that
/// is mid-transfer from it.
pub(crate) fn unplaceable(node: Option<&Node>, floor: i64, now: k8s_openapi::jiff::Timestamp) -> bool {
    node_is_dead(node, floor, now) || decommissioning(node)
}
```

Then swap the four call sites: `live_nodes` (line 309) filters on `!unplaceable(...)`; `interesting_volumes`'s `owner_alive` (line 388) and `retire_pass`'s (line 976) already read `live`, so they follow for free; the doc comment on `standby_count` gains "or is being decommissioned".

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`**
- [ ] **Step 5: Commit** — `git add crates/workspaces/src/crd.rs bins/agent/src/peer.rs && git commit -m "Treat a decommissioning node as unplaceable for placement only"`

---
### Task 4: Stop tears down at once, wakes the peers, and reports `Replicated`

**Files:**
- Modify `bins/agent/src/controller/stop.rs`: delete lines 15–29's `unreplicated` field, 79–92's flush arms, 145–163 (`flush_timeout`, `flush_expired`), 165–203 (`flush_gate`), 205–212 (the three constants), and line 36 + line 120's `STOP_GENERATION`.
- Modify `bins/agent/src/controller/workspace.rs`: `stop_workspace` (lines 427–493) — `unreplicated` binding at 458, `stopped_condition(unreplicated, gen)` at 484; add the `Replicated` write.
- Modify `bins/agent/src/controller/environment.rs`: `stop_environment` (lines 103–184) — 152–174; `stopped_condition` (lines 186–193).
- Modify `crates/workspaces/src/api.rs`: `ws_doc` (lines 391–431) and `env_doc` (lines 430–456) gain `replicated`; `stop_ws` (lines 910–937) and `stop_env` (lines 1443–1459) already read conditions for `node_dead_warning`.
- Modify `deploy/k3s/agent-daemonset.yaml` lines 202–207 (delete the env var and its comment).
- Test `bins/agent/tests/reconcile.rs`, `crates/workspaces/tests/api_commit_model.rs`.

**Interfaces:**
- Consumes: `peer::up_to_date`, `peer::newest_transient` (Task 1), `peer::wake_peers` + `peer::live_nodes`/`unplaceable` (Tasks 2–3), `crd::condition_since` (`crates/workspaces/src/crd.rs:841`).
- Produces:
  - `pub(crate) enum StopPush { Landed, Waiting }`
  - `pub(crate) async fn replicated_condition(ctx: &Arc<Ctx>, volume: &str, worktree: &str, replicas: u32, prev: &[Condition], gen: i64) -> Result<Condition, ReconcileErr>` in `bins/agent/src/controller/stop.rs`
  - `pub(crate) fn stopped_condition(gen: i64) -> Condition` (loses its argument)
  - `ApiWorkspace.replicated: Option<{ ready: bool, reason: String, message: String }>`, same shape on `ApiEnvironment`.

- [ ] **Step 1: Write the failing test** — append to `bins/agent/tests/reconcile.rs`:

```rust
/// A stop no longer waits for anybody: the cut turns Ready and the pod goes in the SAME pass.
/// The whole flush gate — ten minutes of a person's time in the bad case — moved into placement,
/// where the decision it was making actually belongs.
#[tokio::test]
async fn a_stop_tears_down_as_soon_as_the_cut_is_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let stop = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": "stop-ws-1-3", "uid": "stop-uid"},
        "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "",
                 "pinned": false, "transient": true},
        "status": {"phase": "ready", "readyAt": "2026-09-03T10:00:00Z"},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots/stop-ws-1-3".into(), status: 200, body: stop },
        // NO VolumeReplica list for a gate, and no wait: the pod delete happens on this pass.
        Route { method: "DELETE", path: "/api/v1/namespaces/ws-alice/pods/ws-1".into(), status: 200, body: json!({}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": []}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "SnapshotList", "items": []}) },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200,
                body: stopping_ws("ws-1") },
        // the pod lookup and volume reads the workspace path already makes
    ];
    let (ctx, rec) = ctx_with(tmp.path(), routes);
    kloudlite_agent::controller::apply_workspace(&stopping_ws_obj("ws-1"), &ctx).await.unwrap();

    assert_eq!(rec.calls().iter().filter(|c| c.starts_with("DELETE /api/v1/namespaces/ws-alice/pods/ws-1")).count(), 1);
    let st = rec.sent("PUT", "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status").remove(0);
    assert_eq!(st["status"]["phase"], "stopped");
    let conds = st["status"]["conditions"].as_array().unwrap();
    assert!(conds.iter().any(|c| c["type"] == "Ready" && c["reason"] == "Stopped"));
    assert!(
        !conds.iter().any(|c| c["reason"] == "FlushUnreplicated"),
        "FlushUnreplicated is gone: a stop is never unreplicated, it is merely not-yet-replicated"
    );
}

/// The `Replicated` condition is the ONE truth about whether a stopped parent can start
/// elsewhere, written by its owner on every reconcile of it. `False/AwaitingReplica` until some
/// other node's replica holds the stop cut by name.
#[tokio::test]
async fn a_stopped_parent_reports_awaiting_replica_until_a_peer_holds_the_cut() {
    let tmp = tempfile::tempdir().unwrap();
    let behind = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-1.node-b"},
        "spec": {"volume": "vol-1", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "sync-ws-1-old"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-1", "ws-1", 7)]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [behind]}) },
    ];
    let (ctx, _rec) = ctx_with(tmp.path(), routes);

    let c = kloudlite_agent::controller::replicated_condition(&ctx, "vol-1", "ws-1", 2, &[], 3).await.unwrap();
    assert_eq!(c.type_, "Replicated");
    assert_eq!(c.status, "False");
    assert_eq!(c.reason, "AwaitingReplica");
    assert_eq!(c.message, "no other node holds the final sync point yet");
}

#[tokio::test]
async fn a_stopped_parent_reports_replicated_once_a_peer_holds_the_cut_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let holding = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-1.node-b"},
        "spec": {"volume": "vol-1", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "stop-ws-1-3"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-1", "ws-1", 7)]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [holding]}) },
    ];
    let (ctx, _rec) = ctx_with(tmp.path(), routes);
    let c = kloudlite_agent::controller::replicated_condition(&ctx, "vol-1", "ws-1", 2, &[], 3).await.unwrap();
    assert_eq!((c.status.as_str(), c.reason.as_str()), ("True", "Replicated"));
    assert_eq!(c.message, "another node holds the final sync point");
}

/// `replicas: 1` is not a separate reason — it is the same `False/AwaitingReplica` with a message
/// that names why it will never become true. One reason, one place to read it.
#[tokio::test]
async fn replicas_one_says_so_in_the_message_not_in_a_second_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "SnapshotList", "items": []}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": []}) },
    ];
    let (ctx, _rec) = ctx_with(tmp.path(), routes);
    let c = kloudlite_agent::controller::replicated_condition(&ctx, "vol-1", "ws-1", 1, &[], 3).await.unwrap();
    assert_eq!((c.status.as_str(), c.reason.as_str()), ("False", "AwaitingReplica"));
    assert_eq!(c.message, "no replica is configured for this volume");
}
```

and to `crates/workspaces/tests/api_commit_model.rs`:

```rust
fn stopped_ws_replicated(name: &str, owner: &str, status: &str, reason: &str, message: &str) -> Value {
    let mut w = placed_ws(name, owner);
    w["status"]["phase"] = json!("stopped");
    w["status"]["conditions"] = json!([{
        "type": "Replicated", "status": status, "reason": reason, "message": message,
        "lastTransitionTime": "2026-09-03T10:00:00Z", "observedGeneration": 3
    }]);
    w
}

/// The condition the owner wrote is what `/v1` answers with, verbatim: the UI's "safe to start
/// anywhere" vs "still copying" is that one field, and re-deriving it here would be a second
/// truth that can disagree with the node's.
#[tokio::test]
async fn get_and_stop_expose_the_replicated_condition() {
    let ws = stopped_ws_replicated("ws-1", "karthik", "False", "AwaitingReplica", "no other node holds the final sync point yet");
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), ws.clone()),
        get(format!("{API}/snapshots"), json!({"apiVersion": "v1", "kind": "SnapshotList", "metadata": {}, "items": []})),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: ws },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let http = reqwest::Client::new();

    let got: Value = http.get(format!("{}/v1/workspaces/ws-1", s.base)).bearer_auth(&tok).send().await.unwrap().json().await.unwrap();
    assert_eq!(got["replicated"]["ready"], false);
    assert_eq!(got["replicated"]["message"], "no other node holds the final sync point yet");

    let stopped: Value = http.post(format!("{}/v1/workspaces/ws-1/stop", s.base)).bearer_auth(&tok).send().await.unwrap().json().await.unwrap();
    assert_eq!(stopped["replicated"]["reason"], "AwaitingReplica");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent --test reconcile a_stop_tears_down` fails with `cannot find function `replicated_condition``; `cargo test -p kloudlite-workspaces --test api_commit_model get_and_stop_expose` fails on `replicated` being `null`.

- [ ] **Step 3: Implement** —

`bins/agent/src/controller/stop.rs` — the enum, the gate's removal, and the new condition:

```rust
/// What a fixed-name stop request says about its push: landed, or still being cut. The caller
/// writes ITS OWN status — the two parent kinds share no status type.
///
/// `Landed` carries nothing now. It used to carry WHY the final sync point had not been
/// replicated, because the stop waited for that and gave up after ten minutes; the wait moved into
/// placement (a stopped parent starts on its own node until a peer is up to date), so a stop is
/// never "unreplicated", only not-yet-replicated — which the `Replicated` condition says, on the
/// object, for as long as it is true.
pub(crate) enum StopPush {
    Landed,
    Waiting,
}
```

`stop_push`'s `Ready` arm becomes `Some(crd::Phase::Ready) => Ok(StopPush::Landed)`, its `Some(_)` arm `Ok(StopPush::Waiting)` with the `flush_expired` guards gone; the `None` arm drops the `STOP_GENERATION` annotation insert (line 120) and the `gen` binding (line 75). Delete `flush_timeout`, `flush_expired`, `flush_gate`, `NO_PEERS`, `NO_READY_AT`, `FLUSH_TIMED_OUT`, and the `Duration` import.

Add, in the same file:

```rust
/// THE "is it replicated" truth, computed in exactly one place — the owner's reconcile of a
/// stopped parent — and read everywhere else (the dead-node sweep, `/v1`, the web). One
/// field-selected `VolumeReplica` list plus one field-selected `Snapshot` list per reconcile of a
/// stopped parent; both are cheap and neither runs for a running one.
///
/// `replicas: 1` is not a second reason: no standby can ever be up to date, so the answer is the
/// same `False/AwaitingReplica` with a message that says it will never change. An operator reads
/// one condition either way.
pub(crate) async fn replicated_condition(
    ctx: &Arc<Ctx>,
    volume: &str,
    worktree: &str,
    replicas: u32,
    prev: &[Condition],
    gen: i64,
) -> Result<Condition, ReconcileErr> {
    let was = prev.iter().find(|c| c.type_ == "Replicated");
    if replicas <= 1 {
        return Ok(crd::condition_since(was, "Replicated", false, "AwaitingReplica",
            "no replica is configured for this volume", gen));
    }
    let newest = crate::peer::newest_transient(ctx, volume, worktree).await?;
    let lp = ListParams::default().fields(&format!("spec.volume={volume}"));
    let rows = Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&lp).await?;
    // Never my own row: the point of the final sync point is that ANOTHER node holds it.
    let held = rows
        .items
        .iter()
        .filter(|r| r.spec.volume == volume && r.spec.node != ctx.node)
        .any(|r| crate::peer::up_to_date(r, worktree, newest.as_deref()));
    Ok(if held {
        crd::condition_since(was, "Replicated", true, "Replicated", "another node holds the final sync point", gen)
    } else {
        crd::condition_since(was, "Replicated", false, "AwaitingReplica", "no other node holds the final sync point yet", gen)
    })
}
```

`bins/agent/src/controller/environment.rs`:

```rust
/// The stop's own Ready condition. No `FlushUnreplicated` arm: whether the last sync point has
/// reached another node is the `Replicated` condition's job, written on every reconcile of a
/// stopped parent and true for as long as it is true — not a one-shot record of one bad moment.
pub(crate) fn stopped_condition(gen: i64) -> Condition {
    crd::condition("Ready", true, "Stopped", "pushed and stopped", gen)
}
```

`stop_environment`: `StopPush::Landed => {}` instead of binding `unreplicated`; after the StatefulSet deletes, and before the status write:

```rust
    // Poke every placeable peer: the stop cut exists NOW, and waiting out the five-minute pull
    // beat is what used to make a cross-node start take minutes. Best-effort by construction.
    let live = crate::peer::placeable_nodes(ctx).await;
    crate::peer::wake_peers(ctx, &live).await;
    let replicated = replicated_condition(ctx, &id, &e.name_any(), vol.spec.replicas, &prev.conditions, gen).await?;
    let st = crd::EnvironmentStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        service_status: vec![],
        conditions: vec![stopped_condition(gen), replicated],
        ..prev
    };
```

and the already-stopped early return at line 119 re-writes `Replicated` each pass rather than returning immediately, so the condition tracks a peer catching up:

```rust
    // Already stopped at this generation: the teardown is done, but `Replicated` is not a
    // one-shot fact — a peer catches up minutes later, and the condition is what tells the UI
    // (and the sweep) that the volume may now move. Re-computed each pass, written only when it
    // actually changed, so a converged environment is idle.
    if e.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Stopped && s.observed_generation == Some(gen)) {
        let replicated = replicated_condition(ctx, &id, &e.name_any(), vol.spec.replicas, &prev.conditions, gen).await?;
        if !conditions_eq(&prev.conditions, &replaced(&prev.conditions, replicated.clone())) {
            let st = crd::EnvironmentStatus { conditions: replaced(&prev.conditions, replicated), ..prev };
            write_env_status(e, st, ctx).await?;
        }
        return Ok(Action::requeue(TICK));
    }
```

with a small helper beside `kept_conditions` in `workspace.rs`:

```rust
/// One condition replaced by type, the rest kept in order. The `Replicated` condition is rewritten
/// on every reconcile of a stopped parent, and a naive push would grow the list without bound.
pub(crate) fn replaced(prev: &[Condition], c: Condition) -> Vec<Condition> {
    prev.iter().filter(|p| p.type_ != c.type_).cloned().chain(std::iter::once(c)).collect()
}
```

`bins/agent/src/controller/workspace.rs`'s `stop_workspace`: the same three edits — `StopPush::Landed => {}`, the wake + `replicated_condition` before the status write (`conditions: with_replicated(ws_conditions(&prev, stopped_condition(gen)), replicated)` using `replaced`), and the already-stopped arm at line 436 re-computing `Replicated` the same way.

`bins/agent/src/peer.rs` gains the small helper both stop paths call:

```rust
/// The nodes a stopped parent could start on, from this node's own view — `pool_nodes` minus the
/// unplaceable. Keep-biased: a listing error is an empty list, which wakes nobody and places
/// nothing, rather than a guess.
pub async fn placeable_nodes(ctx: &Arc<Ctx>) -> Vec<String> {
    let (Ok(pool), Ok(nodes)) = (
        pool_nodes(&ctx.client).await,
        Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await,
    ) else {
        return Vec::new();
    };
    live_nodes(&pool, &nodes.items, node_dead_secs(), k8s_openapi::jiff::Timestamp::now())
}
```

`crates/workspaces/src/api.rs`: add to both docs

```rust
/// The `Replicated` condition, verbatim from the owner's own write — the UI's "safe to start
/// anywhere" vs "still copying". Absent while the parent is running: it is only computed for a
/// stopped one, and inventing a value here would be a second truth that can disagree with the node.
#[serde(skip_serializing_if = "Option::is_none")]
pub replicated: Option<ConditionDoc>,
```

filled in `ws_doc`/`env_doc` by `st.and_then(|s| s.conditions.iter().find(|c| c.type_ == "Replicated")).map(ConditionDoc::from)` (reuse the shape `packages_status` already builds at line 419), and returned by `stop_ws`/`stop_env` as part of the doc rather than the bare `{}`.

`deploy/k3s/agent-daemonset.yaml`: delete lines 202–207.

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`** — `cargo test -p kloudlite-agent && cargo test -p kloudlite-workspaces`.
- [ ] **Step 5: Commit** — `git add bins/agent/src crates/workspaces/src deploy/k3s/agent-daemonset.yaml && git commit -m "Tear a stop down at the cut and report Replicated instead of waiting"`

---
### Task 5: `may_claim` is the up-to-date rule; `source_nodes` and `compatibleNodes` go

**Files:**
- Modify `bins/agent/src/claim.rs`: `CommitPlacement` (lines 21–26), `may_claim` (lines 28–48), `commit_placement` (lines 50–65), `source_nodes` (lines 89–108, DELETED), `decide` (lines 140–173), `Parts` (lines 183–193) and both `claim_*` (lines 268–299).
- Modify `crates/workspaces/src/crd.rs`: `WorkspaceStatus::compatible_nodes` (line 502) and `EnvironmentStatus::compatible_nodes` (line 611) — kept as fields, marked dead.
- Modify `bins/agent/src/controller/volume.rs`: `resolve_volume`'s `compatible_nodes` argument (line 481) and the `settle` builder at lines 496–512.
- Test `bins/agent/src/claim.rs` `mod tests` (add one) and `bins/agent/tests/reconcile.rs`.

**Interfaces:**
- Consumes: `peer::up_to_date`, `peer::newest_transient` (Task 1), `peer::unplaceable` (Task 3), `claim::has_commits` (line 71).
- Produces:
  - `struct Placement { has_commits: bool, my_replica: Option<crd::VolumeReplica>, newest_transient: Option<String>, worktree: String }`
  - `fn may_claim(me: &str, owner: &str, p: &Placement) -> bool`
  - `async fn placement(ctx: &Arc<Ctx>, volume: Option<&str>, worktree: &str) -> Result<Placement, ReconcileErr>`
  - `source_nodes` and `CommitPlacement` are gone; `Parts::compatible` is gone.

- [ ] **Step 1: Write the failing test** — replace `bins/agent/src/claim.rs`'s `mod tests` additions with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn replica(node: &str, phase: &str, branches: &[(&str, &str)]) -> crd::VolumeReplica {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": format!("vol-1.{node}")},
            "spec": {"volume": "vol-1", "node": node},
            "status": {"phase": phase,
                       "branches": branches.iter().cloned().collect::<std::collections::BTreeMap<_, _>>()},
        }))
        .unwrap()
    }

    fn p(has_commits: bool, my_replica: Option<crd::VolumeReplica>, newest: Option<&str>) -> Placement {
        Placement {
            has_commits,
            my_replica,
            newest_transient: newest.map(str::to_string),
            worktree: "ws-1".into(),
        }
    }

    /// The owner is ALWAYS allowed: it holds the bytes by construction, and a rule that could
    /// refuse the owner is a rule that can strand a workspace with nowhere at all to start.
    #[test]
    fn the_owner_may_always_claim_even_with_no_replica_row() {
        assert!(may_claim("node-a", "node-a", &p(true, None, Some("stop-ws-1-3"))));
    }

    /// Another node needs the NAME, not the phase: this is the check that used to live in the
    /// flush gate, moved to where the decision is actually made.
    #[test]
    fn another_node_may_claim_only_when_it_holds_the_newest_transient() {
        let holding = Some(replica("node-b", "Synced", &[("ws-1", "stop-ws-1-3")]));
        let behind = Some(replica("node-b", "Synced", &[("ws-1", "sync-ws-1-old")]));
        assert!(may_claim("node-b", "node-a", &p(true, holding, Some("stop-ws-1-3"))));
        assert!(!may_claim("node-b", "node-a", &p(true, behind, Some("stop-ws-1-3"))));
        assert!(!may_claim("node-b", "node-a", &p(true, None, Some("stop-ws-1-3"))), "no row is not up to date");
    }

    /// No transient at all: a restore-to-new, or a worktree that never ran. A `Synced` replica
    /// holds every Ready commit, so plain `Synced` is the right bar — and the spec says so.
    #[test]
    fn with_no_transient_a_synced_replica_may_claim() {
        assert!(may_claim("node-b", "node-a", &p(true, Some(replica("node-b", "Synced", &[])), None)));
        assert!(!may_claim("node-b", "node-a", &p(true, Some(replica("node-b", "Syncing", &[])), None)));
    }

    /// Bootstrap is unchanged and is the reason `has_commits` survives: a volume nothing has ever
    /// committed to is claimable by any node, because there are no bytes anywhere to be near.
    #[test]
    fn a_volume_with_no_commits_is_claimable_by_anyone() {
        assert!(may_claim("node-b", "node-a", &p(false, None, None)));
        assert!(may_claim("node-b", "", &p(false, None, None)), "and by anyone when nothing owns it yet");
    }
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent --lib claim::tests` fails with `cannot find struct `Placement` in this scope`.

- [ ] **Step 3: Implement** — in `bins/agent/src/claim.rs`:

```rust
/// What the claim needs to know about the volume behind an unplaced object, gathered once in
/// `decide` (async) and handed to the pure, testable `may_claim` below.
struct Placement {
    /// Any `Snapshot` CR for this volume, Ready or not — "a commit was ever started" is enough to
    /// leave the never-started-dataless guard armed; only a volume with none at all is bootstrap.
    has_commits: bool,
    /// THIS node's own replica row, or `None` when it has never pulled the volume.
    my_replica: Option<crd::VolumeReplica>,
    /// The worktree's newest Ready transient, cluster-wide — the name `my_replica` must hold.
    newest_transient: Option<String>,
    worktree: String,
}

/// Whether THIS node may claim the object, and the ONE placement rule in the system: the owner
/// always, any other node only when it is up to date for the worktree being claimed.
///
/// This is the check that used to sit in `stop_push`'s flush gate, holding a person's stop open
/// for up to ten minutes to answer a question nobody was asking yet. It belongs here, where the
/// answer is actually used — and it is why a stop is now instant and a cross-node START is what
/// waits.
///
/// `compatibleNodes` is gone: it was a memory of "who held this once", and holding it once is not
/// holding it now. A volume with no commits at all is still bootstrap, claimable by anyone,
/// because there are no bytes anywhere for a claim to be near.
fn may_claim(me: &str, owner: &str, p: &Placement) -> bool {
    if !p.has_commits {
        return true;
    }
    if owner == me {
        return true;
    }
    p.my_replica.as_ref().is_some_and(|r| crate::peer::up_to_date(r, &p.worktree, p.newest_transient.as_deref()))
}

/// Gathers `Placement` for `volume` (`None` when the child `Volume` has not been created yet —
/// every workspace/environment starts that way, and that IS the bootstrap case). Errors propagate
/// rather than being swallowed: a claim decided on a partial read of "does anyone have this" is
/// exactly the never-started-dataless bug the guard exists to prevent.
async fn placement(ctx: &Arc<Ctx>, volume: Option<&str>, worktree: &str) -> Result<Placement, ReconcileErr> {
    let Some(volume) = volume else {
        return Ok(Placement { has_commits: false, my_replica: None, newest_transient: None, worktree: worktree.into() });
    };
    let has_commits = has_commits(ctx, volume).await?;
    let my_replica = Api::<crd::VolumeReplica>::all(ctx.client.clone())
        .get_opt(&crd::replica_name(volume, &ctx.node))
        .await?;
    let newest_transient = crate::peer::newest_transient(ctx, volume, worktree).await?;
    Ok(Placement { has_commits, my_replica, newest_transient, worktree: worktree.into() })
}
```

`decide` loses `compatible` and `storage`, gains the volume's owner and the worktree name:

```rust
async fn decide(
    ctx: &Arc<Ctx>,
    name: &str,
    node_name: &str,
    volume: Option<&str>,
    phase: crd::Phase,
    gen: i64,
) -> Result<Option<serde_json::Value>, ReconcileErr> {
    if !node_name.is_empty() || ctx.homes_export.is_none() {
        return Ok(None);
    }
    // A node being retired takes no new work: the label is the operator's decision and the claim
    // is where it has to bite, or a drain never finishes because new workspaces keep landing.
    let me = Api::<Node>::all(ctx.client.clone()).get_opt(&ctx.node).await?;
    if crate::peer::unplaceable(me.as_ref(), crate::peer::node_dead_secs(), k8s_openapi::jiff::Timestamp::now()) {
        return Ok(None);
    }
    // The volume's CURRENT owner, not a remembered one: `source_nodes` used to pin a clone to the
    // source volume's `nodeName` unconditionally, which is why a clone of a released or dead-node
    // source could never start anywhere at all.
    let owner = match volume {
        Some(v) => Api::<crd::Volume>::all(ctx.client.clone()).get_opt(v).await?.map(|x| x.spec.node_name).unwrap_or_default(),
        None => String::new(),
    };
    // The worktree is the parent's OWN name (`Pool::worktree`), never the volume's — a clone is a
    // second worktree of the same volume and has its own transients.
    let p = placement(ctx, volume, name).await?;
    if !may_claim(&ctx.node, &owner, &p) {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({
        "phase": phase,
        "nodeName": ctx.node,
        "conditions": [crd::condition("Placed", true, "Claimed", &format!("claimed by {}", ctx.node), gen)],
    })))
}
```

`Parts` drops `compatible` and `storage`; both `claim_*` drop them; `source_nodes`, `storage_source` and `with_me` are deleted along with `with_me`'s test. `resolve_volume` drops the `compatible_nodes` parameter and the `"compatibleNodes": nodes` line in its `settle` builder; both parent call sites drop the argument.

`crates/workspaces/src/crd.rs`, on both `compatible_nodes` fields:

```rust
    /// DEAD as of the 2026-09-03 stop/decommission design: placement reads the replica rows'
    /// `branches` now, so "who held this once" is never consulted. Kept as a tolerated field so a
    /// stored object written before the cutover still parses; nothing writes it, and it is gone
    /// from the CRD schema (Task 12).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compatible_nodes: Vec<String>,
```

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`**
- [ ] **Step 5: Commit** — `git add bins/agent/src crates/workspaces/src && git commit -m "Decide placement by the up-to-date rule and drop compatibleNodes"`

---
### Task 6: One per-volume decision replaces `release_dead_volumes` and `unclaim_kind`

**Files:**
- Modify `bins/agent/src/peer.rs`: `unclaim_dead_nodes` (lines 698–745), `unclaim_kind` (lines 747–820, the `releasable` closure and `running_volumes` argument DELETED), `release_dead_volumes` (lines 841–910, DELETED), `already_marked_dead` (lines 829–839), `volume_ref_of` (lines 822–827).
- Modify `bins/agent/src/listing.rs`: `Parent` (lines 29–41) gains `replicated: bool` and `conditions`, filled from both parent listings (lines 85–94, 114–123).
- Test `bins/agent/src/peer.rs` `mod reconcile_tests` (the four existing sweep tests at lines 1594, 1624, 1657, 1697 are rewritten against the new function).

**Interfaces:**
- Consumes: `listing::Beat` (`bins/agent/src/listing.rs:54`), `peer::unplaceable` (Task 3), the `Replicated` condition (Task 4), `Parent::is_live_worktree` (line 47).
- Produces:
  - `pub(crate) enum VolumeVerdict { Keep, Mark { why: String }, Release { why: String, reason: &'static str } }`
  - `pub(crate) fn volume_decision(volume: &str, owner: &str, parents: &[&crate::listing::Parent], reason: &'static str) -> VolumeVerdict` — pure, takes the parents ON that volume from the beat.
  - `async fn sweep_volumes(ctx: &Arc<Ctx>, beat: &Beat, owners: &HashSet<String>, reason: &'static str)` — applies the verdict for every volume whose owner is in `owners`, un-placing parents on a `Release`.
  - `unclaim_dead_nodes` becomes `sweep_dead_nodes(ctx, beat, nodes, floor, now)`, building the dead owner set and calling `sweep_volumes(..., "NodeDead")`. Task 11's decommission beat calls the same `sweep_volumes` with `"Decommissioned"`.

- [ ] **Step 1: Write the failing test** — in `bins/agent/src/peer.rs` `mod reconcile_tests`:

```rust
    fn parent_at(kind: &'static str, name: &str, volume: &str, phase: crd::Phase, replicated: bool) -> crate::listing::Parent {
        crate::listing::Parent {
            kind,
            name: name.into(),
            volume: volume.into(),
            owner: "alice".into(),
            head: None,
            phase,
            pod_ref: (kind == "Workspace").then(|| format!("ws-alice/{name}")),
            owner_ref: Default::default(),
            replicated,
        }
    }

    /// Arm one: a Running parent pins the volume, full stop. Nothing on it moves — stopped
    /// siblings included, which is the bug this rule exists to make impossible: the parent is
    /// never looked at alone.
    #[test]
    fn a_running_parent_pins_the_whole_volume() {
        let running = parent_at("Workspace", "ws-run", "vol-1", crd::Phase::Ready, false);
        let stopped = parent_at("Workspace", "ws-stop", "vol-1", crd::Phase::Stopped, true);
        match volume_decision("vol-1", "node-b", &[&running, &stopped], "NodeDead") {
            VolumeVerdict::Mark { why } => assert!(why.contains("Running worktree"), "{why}"),
            other => panic!("a running sibling must keep the pin, got {other:?}"),
        }
    }

    /// Arm two: everything stopped, but one of them is not replicated anywhere — the volume waits
    /// for the node. Every parent must be covered, or a start elsewhere would lose that one's
    /// last edits.
    #[test]
    fn one_unreplicated_stopped_parent_holds_the_whole_volume() {
        let ok = parent_at("Workspace", "ws-a", "vol-1", crd::Phase::Stopped, true);
        let waiting = parent_at("Workspace", "ws-b", "vol-1", crd::Phase::Stopped, false);
        match volume_decision("vol-1", "node-b", &[&ok, &waiting], "NodeDead") {
            VolumeVerdict::Mark { why } => assert!(why.contains("ws-b"), "the message names the holder: {why}"),
            other => panic!("expected a mark, got {other:?}"),
        }
    }

    /// Arm three: everything stopped and every one replicated — the pin is cleared and every
    /// parent un-placed, so an up-to-date node claims them on the next start.
    #[test]
    fn a_fully_replicated_stopped_volume_is_released() {
        let a = parent_at("Workspace", "ws-a", "vol-1", crd::Phase::Stopped, true);
        let b = parent_at("Environment", "env-b", "vol-1", crd::Phase::Stopped, true);
        match volume_decision("vol-1", "node-b", &[&a, &b], "NodeDead") {
            VolumeVerdict::Release { reason, .. } => assert_eq!(reason, "NodeDead"),
            other => panic!("expected a release, got {other:?}"),
        }
        // A volume with no parents at all is releasable too: nothing on it can lose anything.
        assert!(matches!(volume_decision("vol-1", "node-b", &[], "NodeDead"), VolumeVerdict::Release { .. }));
    }

    /// The drill from the spec, exactly: one volume, one stopped workspace and one RUNNING clone
    /// of it. Today's code un-places the stopped one while the running sibling keeps the pin —
    /// which leaves it claimable on a node that does not own the volume. Nothing moves.
    #[tokio::test]
    async fn a_stopped_parent_beside_a_running_clone_on_one_volume_never_moves() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)];
        let routes = vec![
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-stop/status".into(), status: 200, body: ws_placed_stopped("ws-stop", "node-b") },
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-clone/status".into(), status: 200, body: ws_placed("ws-clone", "node-b") },
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1/status".into(), status: 200, body: vol_owned("vol-1", "node-b") },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        let mut beat = beat_of(vec![vol_owned("vol-1", "node-b")], vec![], vec![]);
        beat.parents = vec![
            parent_at("Workspace", "ws-stop", "vol-1", crd::Phase::Stopped, true),
            parent_at("Workspace", "ws-clone", "vol-1", crd::Phase::Ready, false),
        ];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        assert!(
            !rec.calls().iter().any(|c| c.starts_with("PATCH /apis/kloudlite.io/v1alpha1/volumes/vol-1")),
            "the pin is never cleared while a sibling runs: {:?}",
            rec.calls()
        );
        let stop_writes = rec.sent("PUT", "/apis/kloudlite.io/v1alpha1/workspaces/ws-stop/status");
        assert!(
            stop_writes.iter().all(|w| w["status"]["nodeName"] == "node-b"),
            "the stopped sibling keeps its placement: {stop_writes:?}"
        );
        // Both parents carry NodeDead so the API can say why neither will start.
        for name in ["ws-stop", "ws-clone"] {
            let sent = rec.sent("PUT", &format!("/apis/kloudlite.io/v1alpha1/workspaces/{name}/status"));
            assert!(sent.iter().any(|w| w["status"]["conditions"].as_array().unwrap().iter().any(|c| c["reason"] == "NodeDead")), "{name}");
        }
    }
```

The three sweep tests at lines 1594/1624/1697 are rewritten to build `beat.parents` the same way and call `sweep_dead_nodes`; the `PATCH`+`PUT` assertions in `a_stopped_worktree_on_a_dead_node_is_released_with_its_volume` (line 1697) are unchanged except that the volume condition's reason stays `NodeDead` (there is no `Released` reason).

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent --lib peer::reconcile_tests::a_running_parent_pins` fails with `cannot find function `volume_decision``.

- [ ] **Step 3: Implement** —

`bins/agent/src/listing.rs`, `Parent` gains the two fields, filled from the parent's own status:

```rust
    /// The `Replicated` condition's answer, as the OWNER wrote it. Read, never recomputed: the
    /// sweep runs on every node, and a second computation of "is it replicated" on a node that is
    /// not the owner is a second truth that can disagree with the one the UI shows.
    pub replicated: bool,
```

filled with `st.conditions.iter().any(|c| c.type_ == "Replicated" && c.status == "True")` in both loops.

`bins/agent/src/peer.rs` — the three arms as one function, and one applier:

```rust
/// What a sweep decides about one volume. `Keep` is "not mine to touch"; `Mark` writes the
/// condition and keeps the pin; `Release` clears the pin and un-places every parent.
#[derive(Debug)]
pub(crate) enum VolumeVerdict {
    Keep,
    Mark { why: String },
    Release { why: String, reason: &'static str },
}

/// THE per-volume decision, for both sweeps. Ownership is per volume, so moving is decided per
/// volume — never per parent, which is exactly the bug this replaces: un-placing a stopped
/// workspace while a running clone of it kept the same volume pinned left the stopped one
/// claimable on a node that owns nothing.
///
/// The three arms, in the spec's order:
///   1. any parent Running        → nothing moves, pin kept, every parent marked;
///   2. some stopped parent not   → nothing moves yet, pin kept — every parent must be covered,
///      replicated                  or starting elsewhere loses that one's last edits;
///   3. otherwise                 → pin cleared, parents un-placed, an up-to-date node takes it.
///
/// `reason` is the condition reason the caller wants (`NodeDead` for the dead-node sweep,
/// `Decommissioned` for a drain) — the arms are identical, only the word differs.
pub(crate) fn volume_decision(
    volume: &str,
    owner: &str,
    parents: &[&crate::listing::Parent],
    reason: &'static str,
) -> VolumeVerdict {
    if let Some(running) = parents.iter().find(|p| p.is_live_worktree()) {
        return VolumeVerdict::Mark {
            why: format!(
                "owner {owner} is unavailable; a Running worktree ({}) still names volume {volume}, so it stays pinned",
                running.name
            ),
        };
    }
    if let Some(waiting) = parents.iter().find(|p| !p.replicated) {
        return VolumeVerdict::Mark {
            why: format!(
                "owner {owner} is unavailable; {} on volume {volume} is not replicated to any other node yet",
                waiting.name
            ),
        };
    }
    VolumeVerdict::Release {
        why: format!("owner {owner} is unavailable; released, waiting for an up-to-date node to take it"),
        reason,
    }
}

/// Applies `volume_decision` to every volume whose owner is in `owners`. One place, called by the
/// dead-node sweep and by the decommission beat with different sets and different reasons — the
/// arms must never drift, and two copies of them is how they would.
async fn sweep_volumes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, owners: &HashSet<String>, reason: &'static str) {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    for vol in beat.volumes.iter().cloned() {
        let owner = vol.spec.node_name.clone();
        if owner.is_empty() || !owners.contains(&owner) {
            continue;
        }
        let name = vol.name_any();
        let parents: Vec<&crate::listing::Parent> = beat.parents.iter().filter(|p| p.volume == name).collect();
        let verdict = volume_decision(&name, &owner, &parents, reason);
        let (why, release) = match verdict {
            VolumeVerdict::Keep => continue,
            VolumeVerdict::Mark { why } => (why, false),
            VolumeVerdict::Release { why, .. } => (why, true),
        };
        // Every parent on the volume carries the condition, whatever the verdict — that is how
        // the API answers "why will this not start", and on a release it is written BEFORE the
        // un-place so the object is never briefly unplaced with no explanation.
        for p in &parents {
            mark_parent(ctx, p, reason, &why, release).await;
        }
        let mut cur = vol;
        if release {
            // `test` first: a survivor's takeover landing between our list and this patch must not
            // be clobbered back to "" — a failed test (409/422) means we lost that race.
            let ops = json_patch::Patch(vec![
                json_patch::PatchOperation::Test(json_patch::TestOperation {
                    path: "/spec/nodeName".parse().expect("static pointer parses"),
                    value: serde_json::json!(owner),
                }),
                json_patch::PatchOperation::Replace(json_patch::ReplaceOperation {
                    path: "/spec/nodeName".parse().expect("static pointer parses"),
                    value: serde_json::json!(""),
                }),
            ]);
            match api.patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Json::<crd::Volume>(ops)).await {
                // The patched object, not our stale copy: the PUT below carries a resourceVersion.
                Ok(v) => cur = v,
                Err(kube::Error::Api(s)) if s.code == 409 || s.code == 422 => continue,
                Err(e) => {
                    tracing::warn!(volume = %name, error = %e, "sweep: releasing an unavailable owner's volume");
                    continue;
                }
            }
        }
        let mut st = cur.status.clone().unwrap_or_default();
        if st.phase == crd::Phase::Unavailable && !release && st.conditions.first().is_some_and(|c| c.message == why) {
            continue; // already marked, same reason, still pinned: nothing changed since last beat
        }
        st.phase = crd::Phase::Unavailable;
        let gen = cur.metadata.generation.unwrap_or(0);
        // No `Released` reason: `Unavailable` with an empty pin IS released, and a third word
        // would restate the pin the object already carries.
        st.conditions = vec![crd::condition("Available", false, reason, &why, gen)];
        if let Err(e) = replace_status(&api, &cur, "Volume", serde_json::to_value(st).expect("VolumeStatus serializes")).await {
            tracing::warn!(volume = %name, error = %e, "sweep: marking an unavailable owner's volume");
        }
    }
}

/// One parent's status write for the sweep: the condition always, `nodeName: ""` only on a
/// release. The same guarded primitive the claim uses (`replace_status`, a PUT carrying
/// `resourceVersion`, one re-read on a 409) — clearing a claim races the same way winning one does.
async fn mark_parent(ctx: &Arc<Ctx>, p: &crate::listing::Parent, reason: &str, why: &str, release: bool) {
    match p.kind {
        "Workspace" => mark_parent_of::<crd::Workspace>(ctx, &p.name, "Workspace", reason, why, release).await,
        _ => mark_parent_of::<crd::Environment>(ctx, &p.name, "Environment", reason, why, release).await,
    }
}

async fn mark_parent_of<K>(ctx: &Arc<Ctx>, name: &str, kind: &'static str, reason: &str, why: &str, release: bool)
where
    K: kube::Resource<DynamicType = ()> + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let api: Api<K> = Api::all(ctx.client.clone());
    let Ok(Some(mut cur)) = api.get_opt(name).await else { return };
    for attempt in 0..2 {
        let mut status = serde_json::to_value(&cur).unwrap_or_default()["status"].take();
        if status.is_null() {
            status = serde_json::json!({});
        }
        let gen = cur.meta().generation.unwrap_or(0);
        let prev: Vec<crd::Condition> = serde_json::from_value(status["conditions"].clone()).unwrap_or_default();
        let cond = crd::condition_since(
            prev.iter().find(|c| c.type_ == "Degraded"),
            "Degraded",
            true,
            reason,
            why,
            gen,
        );
        // Idle when nothing changed: this runs on every beat of every node, and rewriting an
        // identical status every 300 s per volume is churn the API server pays for.
        if !release && prev.iter().any(|c| c.type_ == "Degraded" && c.reason == cond.reason && c.message == cond.message) {
            return;
        }
        status["conditions"] = serde_json::to_value(replaced_conditions(&prev, cond)).expect("conditions serialize");
        if release {
            status["nodeName"] = serde_json::json!("");
        }
        match replace_status(&api, &cur, kind, status).await {
            Ok(()) => return,
            Err(kube::Error::Api(s)) if s.code == 409 && attempt == 0 => match api.get(name).await {
                Ok(fresh) => cur = fresh,
                Err(e) => {
                    tracing::warn!(%kind, %name, error = %e, "sweep: re-read after conflict");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!(%kind, %name, error = %e, "sweep: marking an unavailable node's parent");
                return;
            }
        }
    }
}

/// The dead half: build the set of owners that are dead and hand it to `sweep_volumes`. The
/// parents come from the beat's own listing, which is why the per-kind list-and-decide plumbing
/// (`unclaim_kind`, its `releasable` closures, and the `running_volumes` set threaded between it
/// and the release pass) is gone — the listing already knows every parent on this node's volumes.
async fn sweep_dead_nodes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp) {
    let dead: HashSet<String> = beat
        .volumes
        .iter()
        .map(|v| v.spec.node_name.clone())
        .filter(|n| !n.is_empty() && node_is_dead(nodes.iter().find(|k| k.name_any() == *n), floor, now))
        .collect();
    sweep_volumes(ctx, beat, &dead, "NodeDead").await;
}
```

with `replaced_conditions` the JSON-side twin of `workspace::replaced`. Delete `unclaim_dead_nodes`, `unclaim_kind`, `release_dead_volumes`, `volume_ref_of` and `already_marked_dead`; `pull_beat_with`'s line 342 becomes `sweep_dead_nodes(ctx, &beat, &nodes, floor, now).await;`.

**Caveat to carry into review:** `beat.parents` is scoped to THIS node (`listing::parents_on_node`), so a sweeping node sees only its own parents. `sweep_volumes` therefore lists the dead owner's parents cluster-wide once per pass instead — add to `listing::beat` a fourth field `all_parents: Vec<Parent>` from the same two listings without the `status.nodeName` selector, and have `sweep_volumes` read `beat.all_parents`. `parents_on_node` keeps its selector for every other consumer.

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`**
- [ ] **Step 5: Commit** — `git add bins/agent/src && git commit -m "Decide dead-node moves per volume in one function"`

---
### Task 7: Mismatch self-heal in `resolve_volume`

**Files:**
- Modify `bins/agent/src/controller/volume.rs`: the `NodeMismatch` arm (lines 574–581).
- Test `bins/agent/tests/reconcile.rs`.

**Interfaces:**
- Consumes: `peer::unplaceable` (Task 3), `controller::replace_status` (`bins/agent/src/controller/status.rs:121`), `Resolved::Wait` (`volume.rs:458`).
- Produces: no new symbol — one new branch inside `resolve_volume`, plus `Resolved::Settled` reuse.

- [ ] **Step 1: Write the failing test** — append to `bins/agent/tests/reconcile.rs`:

```rust
/// Two up-to-date nodes race for a released volume: the CAS picks one, and the LOSER has to
/// notice. Today it writes `Degraded/NodeMismatch` and sits in `error` forever, holding a
/// `status.nodeName` that contradicts the volume — so the winner's own sweep never sees it as
/// unplaced and nothing ever starts it. Clearing my own claim is the whole fix.
#[tokio::test]
async fn a_mismatch_against_a_live_owner_un_places_me() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1".into(), status: 200, body: vol_owned("vol-1", "node-b") },
        Route { method: "GET", path: "/api/v1/nodes/node-b".into(), status: 200, body: node_ready("node-b") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: placed_ws("ws-1", "node-a") },
    ];
    let (ctx, rec) = ctx_with(tmp.path(), routes);

    let out = kloudlite_agent::controller::apply_workspace(&placed_ws_obj("ws-1", "node-a"), &ctx).await.unwrap();

    let sent = rec.sent("PUT", "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"]["nodeName"], "", "I un-place myself so the real owner reclaims it");
    assert!(out.requeue_after().is_some(), "and come back rather than awaiting a change I just caused");
}

/// When the owner is DEAD the arm stays exactly as it was: refuse and wait. Un-placing here would
/// be a second thing allowed to release a volume, and the per-volume sweep is the only one — it
/// is the thing that knows whether a running sibling still pins it.
#[tokio::test]
async fn a_mismatch_against_a_dead_owner_still_refuses_and_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1".into(), status: 200, body: vol_owned("vol-1", "node-b") },
        Route { method: "GET", path: "/api/v1/nodes/node-b".into(), status: 200, body: node_dead("node-b", "2000-01-01T00:00:00Z") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: placed_ws("ws-1", "node-a") },
    ];
    let (ctx, rec) = ctx_with(tmp.path(), routes);

    kloudlite_agent::controller::apply_workspace(&placed_ws_obj("ws-1", "node-a"), &ctx).await.unwrap();

    let sent = rec.sent("PUT", "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status");
    assert_eq!(sent[0]["status"]["nodeName"], "node-a", "a dead owner's volume is the sweep's to release, not mine");
    assert_eq!(sent[0]["status"]["conditions"][0]["reason"], "NodeMismatch");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent --test reconcile a_mismatch_against_a_live_owner` fails: `assertion `left == right` failed: left: "node-a", right: ""`.

- [ ] **Step 3: Implement** — replace the arm at `bins/agent/src/controller/volume.rs:574`:

```rust
    if vol.spec.node_name != node_name {
        let why = format!("status.nodeName {node_name} disagrees with volume {id}'s node {}", vol.spec.node_name);
        // The owner is alive and is somebody else: I lost the takeover CAS (two up-to-date nodes
        // raced for a released volume, and exactly one won). Clear MY OWN claim and requeue, so
        // the winner's reconcile picks the parent up. Without this the loser sits in `error`
        // forever holding a `status.nodeName` nobody will ever clear — the object is neither
        // placed nor unplaced, so no claim watch matches it.
        //
        // Only for a LIVE owner. A dead owner's volume is released by the per-volume sweep and by
        // nothing else: it is the only code that knows whether a Running sibling still pins it.
        let owner_node = Api::<Node>::all(ctx.client.clone()).get_opt(&vol.spec.node_name).await?;
        let alive = !crate::peer::unplaceable(
            owner_node.as_ref(),
            crate::peer::node_dead_secs(),
            k8s_openapi::jiff::Timestamp::now(),
        );
        if alive {
            tracing::info!(volume = %id, owner = %vol.spec.node_name, "lost the volume; un-placing myself so the owner reclaims it");
            return Ok(Resolved::Wait {
                volume_ref: None,
                phase: crd::Phase::Pending,
                cond: crd::condition("Placed", false, "NodeMismatch", &why, gen),
                action: Action::requeue(std::time::Duration::from_secs(5)),
            });
        }
        return Ok(Resolved::Wait {
            volume_ref: None,
            phase: crd::Phase::Error,
            cond: crd::condition("Degraded", true, "NodeMismatch", &why, gen),
            action: Action::await_change(),
        });
    }
```

`Resolved::Wait` gains one field so the parent knows to blank its own placement — `unplace: bool`, default `false`, set `true` only here; both parents' `Wait` arms write `node_name: String::new()` when it is set (`write_ws_status` / `write_env_status` already own the whole status struct).

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`**
- [ ] **Step 5: Commit** — `git add bins/agent/src/controller/volume.rs bins/agent/src/controller && git commit -m "Un-place myself when a live node owns the volume I claimed"`

---
### Task 8: `/v1` refuses to start an interrupted parent

**Files:**
- Modify `crates/workspaces/src/api.rs`: `start_ws` (lines 889–901), `start_env` (lines 1432–1441), beside `node_dead_warning` (lines 903–908).
- Test `crates/workspaces/tests/api_commit_model.rs`.

**Interfaces:**
- Consumes: `crd::Condition`, `node_dead_warning`'s condition read.
- Produces: `fn interrupted(conditions: &[crd::Condition]) -> bool` and `fn interrupted_409(kind: &str) -> Response` in `crates/workspaces/src/api.rs`.

- [ ] **Step 1: Write the failing test** — append to `crates/workspaces/tests/api_commit_model.rs`:

```rust
fn interrupted_ws(name: &str, owner: &str) -> Value {
    let mut w = placed_ws(name, owner);
    w["status"]["conditions"] = json!([{
        "type": "Degraded", "status": "True", "reason": "NodeDead",
        "message": "node node-a is down", "lastTransitionTime": "2026-09-03T10:00:00Z"
    }]);
    w
}

/// There is no way to start an interrupted parent elsewhere and no way to abandon its edits:
/// reaching that state is a system failure, never a workflow. The 409 says what will happen
/// instead of leaving a start silently pending forever.
#[tokio::test]
async fn starting_an_interrupted_workspace_is_a_409_that_explains_itself() {
    let routes = vec![get(format!("{API}/workspaces/ws-1"), interrupted_ws("ws-1", "karthik"))];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/start", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(
        r.text().await.unwrap(),
        "workspace is interrupted: its node is down; it resumes when the node returns"
    );
    assert!(
        !s.rec.calls().iter().any(|c| c.starts_with("PATCH")),
        "and desiredState is never flipped: {:?}",
        s.rec.calls()
    );
}

#[tokio::test]
async fn starting_an_interrupted_environment_is_a_409_too() {
    let mut e = placed_env("env-1", "karthik");
    e["status"]["conditions"] = json!([{
        "type": "Degraded", "status": "True", "reason": "NodeDead",
        "message": "node node-a is down", "lastTransitionTime": "2026-09-03T10:00:00Z"
    }]);
    let routes = vec![get(format!("{API}/environments/env-1"), e)];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new().post(format!("{}/v1/environments/env-1/start", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(r.text().await.unwrap(), "environment is interrupted: its node is down; it resumes when the node returns");
}

/// A stopped parent on a dead node is NOT interrupted — it was flushed on the way down, so it
/// starts as soon as an up-to-date node claims it. Only a parent carrying `NodeDead` is refused.
#[tokio::test]
async fn a_plain_stopped_workspace_still_starts() {
    let mut w = placed_ws("ws-1", "karthik");
    w["status"]["phase"] = json!("stopped");
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), w.clone()),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: w },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new().post(format!("{}/v1/workspaces/ws-1/start", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(r.status(), 202);
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-workspaces --test api_commit_model starting_an_interrupted` fails: `assertion `left == right` failed: left: 202, right: 409`.

- [ ] **Step 3: Implement** — in `crates/workspaces/src/api.rs`, beside `node_dead_warning`:

```rust
/// Interrupted: the node died while this was RUNNING, so its live edits exist only there. The
/// sweep writes `Degraded/NodeDead` and keeps the pin; nothing in the system may move it.
fn interrupted(conditions: &[crd::Condition]) -> bool {
    conditions.iter().any(|c| c.reason == "NodeDead" && c.status == "True")
}

/// The one answer a start gets while a parent is interrupted. There is deliberately no force
/// flag: abandoning someone's edits is not a thing this API can offer, and the way forward is a
/// clone from the last synced point — which `clone` allows, with its age stated.
fn interrupted_409(kind: &str) -> Response {
    (
        StatusCode::CONFLICT,
        format!("{kind} is interrupted: its node is down; it resumes when the node returns"),
    )
        .into_response()
}
```

`start_ws` (line 889) and `start_env` (line 1432) gain, right after the `my_ws`/`find_env` read and BEFORE `set_desired`:

```rust
    if w.status.as_ref().is_some_and(|st| interrupted(&st.conditions)) {
        return Err(interrupted_409("workspace"));
    }
```

(`"environment"` in `start_env`, reading `e.status`.) `start_ws` currently discards the object (`my_ws(&s, &owner, &id).await?;`) — bind it.

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`**
- [ ] **Step 5: Commit** — `git add crates/workspaces/src/api.rs && git commit -m "Refuse to start an interrupted parent with the reason"`

---
### Task 9: Clone cuts a transient, wakes the peers, and always reports what it is based on

**Files:**
- Modify `crates/workspaces/src/api.rs`: `clone_ws` (lines 1044–1084), `clone_env` (lines 1502–1536), the `Workspace`/`Environment` docs (lines 391–456).
- Modify `bins/agent/src/controller/workspace.rs` and `environment.rs`: the clone-request arm that cuts the `clone-{ws}-{hex}` transient and wakes (reuse `stop_push`'s create shape at `stop.rs:94–141`).
- Test `crates/workspaces/tests/api_commit_model.rs`, `bins/agent/tests/reconcile.rs`.

**Interfaces:**
- Consumes: `crd::short_hex()` (`crates/workspaces/src/crd.rs:335`), `peer::wake_peers` + `peer::placeable_nodes` (Tasks 2, 4), `peer::newest_transient` (Task 1), `claim::may_claim` (Task 5).
- Produces:
  - `#[derive(Serialize)] pub struct BasedOn { pub snapshot: String, pub at: Option<String>, pub interrupted: bool }` in `crates/workspaces/src/api.rs`, on both clone responses and on `ApiWorkspace`/`ApiEnvironment` as `based_on`.
  - `async fn clone_cut(c: &kube::Client, src: &crd::Workspace, volume: &str) -> Result<crd::Snapshot, Response>` — creates `clone-{ws}-{hex}` as a `Working` transient parented on the source's newest transient.

- [ ] **Step 1: Write the failing test** — append to `crates/workspaces/tests/api_commit_model.rs`:

```rust
/// A clone cuts a snapshot NOW rather than leaning on whatever the last beat happened to leave:
/// `clone-{ws}-{hex}`, a transient, parented on the source's newest sync point so the puller
/// sends a delta. The clone's spec then names that cut, not the source's `head`.
#[tokio::test]
async fn clone_cuts_a_transient_and_bases_the_clone_on_it() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws_with_head("ws-1", "karthik", "ws-1-aaaaaaaa")),
        no_workspaces(),
        get(format!("{API}/snapshots"), json!({"apiVersion": "v1", "kind": "SnapshotList", "metadata": {}, "items": [
            snapshot("sync-ws-1-bbbb", "ws-1", "karthik", "ws-1", "", "ready")
        ]})),
        get(format!("{API}/volumes/ws-1"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
            "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201,
                body: snapshot("clone-ws-1-cafe", "ws-1", "karthik", "ws-1", "sync-ws-1-bbbb", "working") },
        Route { method: "POST", path: format!("{API}/workspaces"), status: 201, body: placed_ws("ws-2", "karthik") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);

    let cut = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert!(cut["metadata"]["name"].as_str().unwrap().starts_with("clone-ws-1-"), "{cut}");
    assert_eq!(cut["spec"]["transient"], true, "a clone cut is a sync point, not a commit in anyone's history");
    assert_eq!(cut["spec"]["parent"], "sync-ws-1-bbbb", "parented on the newest transient so the send is a delta");
    assert_eq!(cut["spec"]["worktree"], "ws-1");

    let made = s.rec.sent("POST", &format!("{API}/workspaces")).remove(0);
    assert_eq!(made["spec"]["storage"]["source"]["cloneOf"]["commit"], cut["metadata"]["name"]);

    // basedOn is on EVERY clone response, not only an interrupted one: a clone is always based
    // on a cut, and the person is always entitled to know which.
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["based_on"]["snapshot"], cut["metadata"]["name"]);
    assert_eq!(body["based_on"]["interrupted"], false);
}

/// Cloning an INTERRUPTED source is allowed — it is the one way forward — and the response says
/// exactly what it is based on, so choosing it is choosing the gap knowingly.
#[tokio::test]
async fn cloning_an_interrupted_source_is_allowed_and_states_the_cut_it_used() {
    let mut src = interrupted_ws("ws-1", "karthik");
    src["status"]["head"] = json!("ws-1-aaaaaaaa");
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), src),
        no_workspaces(),
        get(format!("{API}/snapshots"), json!({"apiVersion": "v1", "kind": "SnapshotList", "metadata": {}, "items": [
            snapshot("sync-ws-1-bbbb", "ws-1", "karthik", "ws-1", "", "ready")
        ]})),
        get(format!("{API}/volumes/ws-1"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
            "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/workspaces"), status: 201, body: placed_ws("ws-2", "karthik") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    // No new cut: the owner is down, so the newest transient a peer HOLDS is the only thing to
    // graft onto — and that is exactly what the response names.
    assert!(s.rec.sent("POST", &format!("{API}/snapshots")).is_empty(), "an interrupted source cannot be cut");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["based_on"]["snapshot"], "sync-ws-1-bbbb");
    assert_eq!(body["based_on"]["interrupted"], true);
}
```

and, in `bins/agent/src/peer.rs` `mod reconcile_tests`, the placement half (the spec's ruling 4):

```rust
    /// A running source's clone lands on the OWNER by arithmetic, not by policy: at the instant
    /// of the cut the owner is the only node up to date for that worktree. There is no same-node
    /// rule in the code, and this test asserts the reason, not just the result.
    #[test]
    fn a_running_sources_clone_lands_on_the_owner_because_nothing_else_is_up_to_date_yet() {
        let newest = Some("clone-ws-1-cafe");
        let peer = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({"ws-1": "sync-ws-1-old"}));
        assert!(!up_to_date(&peer, "ws-1", newest), "the peer has not pulled the fresh cut yet");
        // The owner needs no row at all: it holds the bytes by construction (Task 5's may_claim).
        assert_eq!(up_to_date_nodes("ws-1", newest, &[peer]), Vec::<String>::new());
    }

    /// Once the peer HAS pulled the cut, both nodes qualify and rendezvous decides — the same
    /// deterministic hash a start uses, so a retry lands on the same answer.
    #[test]
    fn once_a_peer_holds_the_cut_rendezvous_decides_between_them() {
        let newest = Some("clone-ws-1-cafe");
        let peer = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({"ws-1": "clone-ws-1-cafe"}));
        assert_eq!(up_to_date_nodes("ws-1", newest, &[peer]), vec!["node-b".to_string()]);
        let candidates = vec!["node-a".to_string(), "node-b".to_string()];
        assert_eq!(
            preferred_node("vol-1", &candidates),
            preferred_node("vol-1", &candidates),
            "deterministic: a retry lands on the same node"
        );
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-workspaces --test api_commit_model clone_cuts_a_transient` fails on the missing `POST /snapshots` (the recorder has none) and `body["based_on"]` being `null`.

- [ ] **Step 3: Implement** — in `crates/workspaces/src/api.rs`:

```rust
/// What a clone was grafted onto, on every clone response and on every workspace/environment doc.
/// Always present: a clone is always based on a cut, and only the interrupted case makes the cut
/// older than "now" — which is the one thing a person needs to weigh before accepting it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BasedOn {
    pub snapshot: String,
    /// The cut's `readyAt`, when it has one — absent for a cut this request just created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// The source's node was down, so this is the newest cut a peer HOLDS rather than a fresh one.
    pub interrupted: bool,
}

/// The clone's own cut: `clone-{ws}-{hex}`, a transient — the same shape the sync beat produces,
/// so the puller sends a delta against what a replica already holds and retention sweeps it like
/// any other sync point. Created here rather than left to the next beat because a clone that
/// leaned on the last beat could be up to five minutes stale, which is a silent data loss the
/// person never asked for.
async fn clone_cut(c: &kube::Client, owner: &str, volume: &str, worktree: &str, parent: String) -> Result<crd::Snapshot, Response> {
    let api: Api<crd::Snapshot> = Api::all(c.clone());
    let name = format!("clone-{worktree}-{}", crd::short_hex());
    let mut snap = crd::Snapshot::new(&name, crd::SnapshotSpec {
        volume: volume.to_string(),
        owner: owner.to_string(),
        worktree: worktree.to_string(),
        parent,
        message: Some("cloning".to_string()),
        pinned: false,
        transient: true,
    });
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
    snap.metadata.labels = Some(crd::commit_labels(owner, volume));
    api.create(&PostParams::default(), &snap).await.map_err(kube_err)
}
```

`clone_ws` replaces its `head`-pinning block (lines 1061–1067) with:

```rust
    // Placement is the ONE up-to-date rule (Task 5): the clone starts on a node up to date for
    // the source worktree, the owner always being one. There is no "same node" rule — at the
    // instant of a fresh cut the owner is simply the only node that qualifies.
    let newest = newest_transient(c, &volume, &id).await?;
    let based_on = if src.status.as_ref().is_some_and(|st| interrupted(&st.conditions)) {
        // The source's node is down: nothing can be cut there, so graft onto the newest cut a
        // peer actually HOLDS and say how old it is. This is the one way forward for an
        // interrupted workspace, and the person chooses it knowing the gap.
        let held = newest_replicated_transient(c, &volume, &id).await?.ok_or_else(|| {
            (StatusCode::CONFLICT, "the source's node is down and no other node holds a sync point of it yet").into_response()
        })?;
        BasedOn { snapshot: held.name_any(), at: held.status.as_ref().and_then(|s| s.ready_at.clone()), interrupted: true }
    } else {
        let cut = clone_cut(c, &owner, &volume, &id, newest.unwrap_or_default()).await?;
        BasedOn { snapshot: cut.name_any(), at: None, interrupted: false }
    };
    let source = VolumeSource::CloneOf { volume, commit: Some(based_on.snapshot.clone()) };
```

and the response becomes `Json(json!({ "workspace": ws_doc(&w, &HashSet::new()), "based_on": based_on }))` — matching the shape `stop_ws` already uses for `warning`. `clone_env` gets the identical treatment against `env_volume`/`find_env`.

`bins/agent/src/peer.rs` gains the two helpers the placement tests name:

```rust
/// Which of these replica rows are up to date for `worktree` — the candidate set a start or a
/// clone chooses among, the owner being added by the caller (it holds the bytes by construction).
pub(crate) fn up_to_date_nodes(worktree: &str, newest: Option<&str>, rows: &[crd::VolumeReplica]) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().filter(|r| up_to_date(r, worktree, newest)).map(|r| r.spec.node.clone()).collect();
    out.sort();
    out
}

/// Rendezvous over the candidate set, keyed by the volume id — `replicate::targets`' own hash, so
/// the spread is deterministic and even by count and a retry lands on the same answer. Every node
/// computes the same result with no coordinator.
///
/// ponytail: by COUNT, not by load. Weighting by free CPU or pool space is the named upgrade and
/// needs an input every node computes identically — a per-node metric every agent can read the
/// same way, not one node's opinion.
pub(crate) fn preferred_node(volume: &str, candidates: &[String]) -> Option<String> {
    // `targets(volume, me = "", candidates, total = 2)` is "the top-scoring candidate", which is
    // the same ordering the replication spread already uses.
    replicate::targets(volume, "", candidates, 2).into_iter().next()
}
```

The agent side of the cut needs nothing new: `snapshot::reconcile_commit` already materialises a `Working` transient on the volume's owning node, and Task 2's wake is fired by the same `apply_workspace` pass that turns it `Ready` (`wake_peers(ctx, &placeable_nodes(ctx).await)` beside the stop path's call).

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`** — `cargo test -p kloudlite-workspaces && cargo test -p kloudlite-agent`.
- [ ] **Step 5: Commit** — `git add crates/workspaces/src bins/agent/src && git commit -m "Cut a snapshot at clone time and report what the clone is based on"`

---
### Task 10: Starts spread — the owner gives a movable volume away

**Files:**
- Modify `bins/agent/src/controller/workspace.rs` and `environment.rs`: the start path, right after `resolve_volume` returns `Resolved::Ready` and before the pod/StatefulSet work.
- Modify `bins/agent/src/controller/volume.rs`: reuse `take_volume` (line 399) unchanged; add `release_volume` beside it (the mirror CAS).
- Test `bins/agent/tests/reconcile.rs`, `bins/agent/src/peer.rs` `mod reconcile_tests` (the pure chooser).

**Interfaces:**
- Consumes: `peer::up_to_date_nodes`, `peer::preferred_node` (Task 9), `peer::newest_transient` (Task 1), `listing::parents_on_node`, `take_volume`.
- Produces:
  - `pub(crate) async fn release_volume(ctx: &Arc<Ctx>, name: &str, owner: &str) -> Result<bool, kube::Error>` in `controller/volume.rs` — `test` owner + `replace ""`, the exact mirror of `take_volume`.
  - `pub(crate) async fn start_placement(ctx: &Arc<Ctx>, volume: &crd::Volume, parents: &[Parent]) -> Result<Option<String>, ReconcileErr>` in `bins/agent/src/controller/stop.rs` — `Some(node)` when the volume should MOVE there, `None` to start here.

- [ ] **Step 1: Write the failing test** — in `bins/agent/tests/reconcile.rs`:

```rust
/// A movable volume — nothing on it running — spreads: the OWNER computes the preferred node over
/// {itself} ∪ {nodes up to date for every stopped parent on it}, and hands the volume over when
/// that is not itself. Only the owner may give a volume away; it is the one node that certainly
/// is not mid-takeover.
#[tokio::test]
async fn a_movable_volume_whose_preferred_node_is_a_peer_is_released_and_un_placed() {
    let tmp = tempfile::tempdir().unwrap();
    let peer_holds = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-1.node-b"},
        "spec": {"volume": "vol-1", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "stop-ws-1-3", "ws-2": "stop-ws-2-1"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [peer_holds]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [
                    transient("stop-ws-1-3", "vol-1", "ws-1", 7), transient("stop-ws-2-1", "vol-1", "ws-2", 4)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "NodeList", "items": [node_ready("node-a"), node_ready("node-b")]}) },
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1".into(), status: 200, body: vol_owned("vol-1", "") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: placed_ws("ws-1", "") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-2/status".into(), status: 200, body: placed_ws("ws-2", "") },
    ];
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", routes);
    // Both parents stopped: the volume is movable.
    let parents = vec![stopped_parent("ws-1", "vol-1"), stopped_parent("ws-2", "vol-1")];

    let chosen = kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-1", "node-a"), &parents).await.unwrap();

    // node-b wins the rendezvous for "vol-1" over {node-a, node-b} — deterministic, so this is a
    // fixed expectation, not a coin flip.
    assert_eq!(chosen.as_deref(), Some("node-b"));
    let ops = rec.sent("PATCH", "/apis/kloudlite.io/v1alpha1/volumes/vol-1").remove(0);
    assert_eq!(ops[0], json!({"op": "test", "path": "/spec/nodeName", "value": "node-a"}));
    assert_eq!(ops[1], json!({"op": "replace", "path": "/spec/nodeName", "value": ""}));
    for name in ["ws-1", "ws-2"] {
        let sent = rec.sent("PUT", &format!("/apis/kloudlite.io/v1alpha1/workspaces/{name}/status"));
        assert_eq!(sent[0]["status"]["nodeName"], "", "every parent on the volume follows, not just the started one");
    }
}

/// A volume with a RUNNING parent is not movable: a stopped sibling starts on the owner, because
/// that is where the volume is and nothing is ever moved out from under a running pod.
#[tokio::test]
async fn a_volume_with_a_running_sibling_never_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", vec![]);
    let parents = vec![running_parent("ws-1", "vol-1"), stopped_parent("ws-2", "vol-1")];

    assert_eq!(kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-1", "node-a"), &parents).await.unwrap(), None);
    assert!(rec.calls().is_empty(), "not movable is decided locally, with no API calls at all: {:?}", rec.calls());
}

/// No up-to-date replica: the candidate set is exactly {owner}, so it starts here. This is the
/// `replicas: 1` case and the "the stop cut has not landed anywhere yet" case, both.
#[tokio::test]
async fn with_no_up_to_date_replica_the_owner_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let behind = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-1.node-b"}, "spec": {"volume": "vol-1", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "sync-ws-1-old"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [behind]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-1", "ws-1", 7)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "NodeList", "items": [node_ready("node-a"), node_ready("node-b")]}) },
    ];
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", routes);
    let parents = vec![stopped_parent("ws-1", "vol-1")];

    assert_eq!(kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-1", "node-a"), &parents).await.unwrap(), None);
    assert!(!rec.calls().iter().any(|c| c.starts_with("PATCH")), "nothing is released when there is nowhere to go");
}

/// Preferred == owner: the common case, and it must cost nothing but the read.
#[tokio::test]
async fn when_the_owner_is_preferred_it_starts_here_with_no_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let holds = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-2.node-b"}, "spec": {"volume": "vol-2", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "stop-ws-1-3"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [holds]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-2", "ws-1", 7)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: json!({"apiVersion": "v1", "kind": "NodeList", "items": [node_ready("node-a"), node_ready("node-b")]}) },
    ];
    // "vol-2" is the id whose rendezvous over {node-a, node-b} scores node-a top — asserted
    // directly in peer.rs's `preferred_node` test, and used here so this stays deterministic.
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", routes);
    let parents = vec![stopped_parent("ws-1", "vol-2")];
    assert_eq!(kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-2", "node-a"), &parents).await.unwrap(), None);
    assert!(!rec.calls().iter().any(|c| c.starts_with("PATCH") || c.starts_with("PUT")));
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent --test reconcile a_movable_volume` fails with `cannot find function `start_placement` in `kloudlite_agent::controller``.

- [ ] **Step 3: Implement** — in `bins/agent/src/controller/stop.rs` (it already owns the stop/placement seam):

```rust
/// Where a volume should start next. `None` means "here" — the owner keeps it; `Some(node)` means
/// this pass has already released the pin and un-placed every parent, so the named node's claim
/// takes it over.
///
/// Only the OWNER runs this, and only when the volume is MOVABLE (no parent on it running).
/// Nothing is ever moved while running and nothing is copied from a live tree; a stopped sibling
/// on a volume with a running parent therefore starts on the owner, because that is where the
/// volume is.
///
/// The candidate set is `{owner} ∪ {nodes up to date for EVERY stopped parent on the volume}` —
/// every parent, because a node that holds one worktree's cut and not another's would strand the
/// other. The choice is rendezvous on the volume id, so it is deterministic (a retry lands on the
/// same answer), even by count, and computed identically by every node with no coordinator.
///
/// If the preferred node never claims (it died in between), nothing is stuck: the volume is
/// released, so the dead-node sweep's own rule lets any up-to-date node take it.
pub(crate) async fn start_placement(
    ctx: &Arc<Ctx>,
    volume: &crd::Volume,
    parents: &[crate::listing::Parent],
) -> Result<Option<String>, ReconcileErr> {
    let id = volume.name_any();
    // Not movable: decided locally, with no API calls at all — this runs on every start.
    if parents.iter().any(|p| p.is_live_worktree()) {
        return Ok(None);
    }
    let lp = ListParams::default().fields(&format!("spec.volume={id}"));
    let rows = Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&lp).await?.items;
    let nodes = Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await?.items;
    let (floor, now) = (crate::peer::node_dead_secs(), k8s_openapi::jiff::Timestamp::now());

    // Intersection across every parent: a candidate must be up to date for ALL of them.
    let mut candidates: Option<HashSet<String>> = None;
    for p in parents {
        let newest = crate::peer::newest_transient(ctx, &id, &p.name).await?;
        let ok: HashSet<String> = rows
            .iter()
            .filter(|r| r.spec.node != ctx.node)
            .filter(|r| !crate::peer::unplaceable(nodes.iter().find(|n| n.name_any() == r.spec.node), floor, now))
            .filter(|r| crate::peer::up_to_date(r, &p.name, newest.as_deref()))
            .map(|r| r.spec.node.clone())
            .collect();
        candidates = Some(match candidates {
            None => ok,
            Some(prev) => prev.intersection(&ok).cloned().collect(),
        });
    }
    let mut set: Vec<String> = candidates.unwrap_or_default().into_iter().collect();
    // The owner is always a candidate: it holds the bytes by construction.
    set.push(ctx.node.clone());
    set.sort();
    let Some(preferred) = crate::peer::preferred_node(&id, &set) else { return Ok(None) };
    if preferred == ctx.node {
        return Ok(None);
    }
    // The two-step move, deliberately kept over an owner-writes-the-target handoff: a handoff
    // would need the admission policy to allow ANY `nodeName` change, and this reuses the CAS the
    // takeover path already proved. Pin first, parents second — a cleared pin with placed parents
    // self-heals through the mismatch branch, the reverse strands them.
    if !crate::controller::volume::release_volume(ctx, &id, &ctx.node).await? {
        return Ok(None); // someone else moved it first; next pass re-decides against the new owner
    }
    for p in parents {
        crate::peer::unplace_parent(ctx, p).await;
    }
    tracing::info!(volume = %id, to = %preferred, "start: spreading a movable volume");
    Ok(Some(preferred))
}
```

`controller/volume.rs` gains the mirror CAS beside `take_volume`:

```rust
/// The mirror of `take_volume`: compare-and-set the owner pin from `owner` to empty. Same `test`
/// construction and the same "lost, not broken" reading of a 409/422 — a start that raced the
/// dead-node sweep just re-decides on its next pass.
pub(crate) async fn release_volume(ctx: &Arc<Ctx>, name: &str, owner: &str) -> Result<bool, kube::Error> {
    // ... the two-op patch, `owner` -> "" ...
}
```

Both parents' start paths call it right after `Resolved::Ready(vol)`:

```rust
    // Starts spread. The owner is alive (it is running this reconcile), and only the owner may
    // give a volume away — so this is the one place the decision can be made at all.
    let siblings = crate::listing::parents_on_volume(ctx, &vol.name_any()).await?;
    if let Some(node) = stop::start_placement(ctx, &vol, &siblings).await? {
        // Nothing left to do here: this object is unplaced now and `node`'s claim watch will pick
        // it up. Await the change rather than requeueing at an object that is no longer ours.
        tracing::info!(workspace = %w.name_any(), %node, "handed over on start");
        return Ok(Action::await_change());
    }
```

with `listing::parents_on_volume` a small cluster-wide sibling read (the same two listings, selected by nothing, filtered on `volumeRef`).

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`**
- [ ] **Step 5: Commit** — `git add bins/agent/src && git commit -m "Spread starts across the owner and its up-to-date replicas"`

---
### Task 11: Node decommission

**Files:**
- Modify `bins/agent/src/controller/run.rs`: add `spawn_decommission(ctx)` beside `spawn_sync` (lines 334–343).
- Create `bins/agent/src/decommission.rs`; register in `bins/agent/src/lib.rs`.
- Modify `deploy/k3s/agent-rbac.yaml`: the header table's `nodes` row (lines 85–88) and the rule (lines 214–216).
- Test `bins/agent/src/decommission.rs` `mod tests` (the `test_ctx`/`Route`/`list_of` harness, copied from `peer.rs:1006–1090`).

**Interfaces:**
- Consumes: `peer::decommissioning` (Task 3), `peer::sweep_volumes` (Task 6, made `pub(crate)`), `listing::beat`, `crd::DECOMMISSION_LABEL`.
- Produces:
  - `pub const DECOMMISSION_STATUS: &str = "kloudlite.io/decommission-status";`
  - `pub(crate) fn drain_status(running: usize, owned: usize, copies: usize, now: &str) -> String`
  - `pub async fn decommission_beat(ctx: &Arc<Ctx>)`
  - `fn spawn_decommission(ctx: Arc<Ctx>)` on a 30 s ticker.

- [ ] **Step 1: Write the failing test** — `bins/agent/src/decommission.rs`, `mod tests`:

```rust
    /// One annotation key, not two: an operator greps `decommission-status` and gets the whole
    /// story, in progress or finished. Two keys is two things to remember and one to forget.
    #[test]
    fn the_status_line_carries_progress_then_the_drained_stamp() {
        assert_eq!(drain_status(2, 3, 1, "2026-09-03T10:00:00Z"), "draining running=2 owned=3 copies=1");
        assert_eq!(drain_status(0, 0, 0, "2026-09-03T10:00:00Z"), "drained 2026-09-03T10:00:00Z");
    }

    /// A running parent is NEVER stopped: it is the person's, and the node waits. It is told, in
    /// the one place a person looks, that the next start lands elsewhere.
    #[tokio::test]
    async fn running_parents_are_told_not_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_decommissioning("node-a")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-a")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws_placed("ws-run", "node-a")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-run/status".into(), status: 200, body: ws_placed("ws-run", "node-a") },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_decommissioning("node-a") },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        decommission_beat(&ctx).await;

        assert!(
            !rec.calls().iter().any(|c| c.starts_with("DELETE")),
            "a drain stops nothing, ever: {:?}",
            rec.calls()
        );
        let sent = rec.sent("PUT", "/apis/kloudlite.io/v1alpha1/workspaces/ws-run/status").remove(0);
        let cond = sent["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Decommissioning").expect("the condition");
        assert_eq!(cond["reason"], "NodeLeaving");
        assert_eq!(cond["message"], "this node is being retired; stop when convenient and the next start lands elsewhere");
        let ann = rec.sent("PATCH", "/api/v1/nodes/node-a").remove(0);
        assert_eq!(ann["metadata"]["annotations"]["kloudlite.io/decommission-status"], "draining running=1 owned=1 copies=0");
    }

    /// Drained is a conjunction of four facts, and the annotation is the operator's gate on
    /// deleting the VM. Nothing else may stamp it.
    #[tokio::test]
    async fn a_node_with_nothing_left_is_stamped_drained() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_decommissioning("node-a"), node_ready_json("node-b")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-b")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![replica_of("vol-1", "node-b", "Synced")]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_decommissioning("node-a") },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        decommission_beat(&ctx).await;

        let ann = rec.sent("PATCH", "/api/v1/nodes/node-a").remove(0);
        let v = ann["metadata"]["annotations"]["kloudlite.io/decommission-status"].as_str().unwrap();
        assert!(v.starts_with("drained "), "{v}");
        assert!(chrono::DateTime::parse_from_rfc3339(v.trim_start_matches("drained ")).is_ok(), "{v}");
    }

    /// Abort: the label is gone, so the beat does nothing at all — not even a status rewrite.
    /// Parents already stopped stay stopped and copies already re-homed stay re-homed.
    #[tokio::test]
    async fn removing_the_label_stops_the_beat() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_ready_json("node-a")]) }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        decommission_beat(&ctx).await;
        assert!(rec.calls().iter().all(|c| c.starts_with("GET")), "{:?}", rec.calls());
    }

    /// A volume with everything stopped and replicated is released by the SAME arm the dead-node
    /// sweep uses — one function, called with a different owner set and a different word.
    #[tokio::test]
    async fn a_releasable_volume_is_released_with_the_decommissioned_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_decommissioning("node-a")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-a")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws_placed_stopped_replicated("ws-1", "node-a")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: ws_placed_stopped("ws-1", "") },
            Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1".into(), status: 200, body: vol_at_rv("vol-1", "", "10") },
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1/status".into(), status: 200, body: vol_owned("vol-1", "") },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_decommissioning("node-a") },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        decommission_beat(&ctx).await;

        let vol = rec.sent("PUT", "/apis/kloudlite.io/v1alpha1/volumes/vol-1/status").remove(0);
        assert_eq!(vol["status"]["conditions"][0]["reason"], "Decommissioned");
        assert_eq!(rec.sent("PUT", "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status")[0]["status"]["nodeName"], "");
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-agent --lib decommission` fails with `unresolved module `decommission``.

- [ ] **Step 3: Implement** — `bins/agent/src/decommission.rs`:

```rust
//! The decommission beat: the PLANNED version of node death, with one difference — whatever is
//! running here keeps running.
//!
//! It stops nothing. The node takes no new work (that is `peer::unplaceable`, which every node
//! applies to it), its copies are re-homed by ordinary rendezvous, and each volume it owns is
//! released as the people using it stop. Draining therefore takes as long as the people take;
//! an operator who needs the node sooner stops those workspaces through `/v1` like anyone else.
//!
//! Runs only on the node that carries the label, and only that node writes its own annotation.

use crate::controller::Ctx;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::ResourceExt;
use kloudlite_workspaces::crd;
use std::collections::HashSet;
use std::sync::Arc;

/// The operator's one window into a drain, rewritten each beat and readable with
/// `kubectl describe node`. ONE key: `draining …` while there is work left, `drained <RFC 3339>`
/// when there is not. Two keys would be two things to check and one to forget.
pub const DECOMMISSION_STATUS: &str = "kloudlite.io/decommission-status";

/// `WS_DECOMMISSION_SECS`, default 30 — fast, because everything it does is idempotent and cheap,
/// and because the thing it is waiting for (a person stopping their workspace) deserves a prompt
/// answer when it happens.
fn beat_interval() -> std::time::Duration {
    std::time::Duration::from_secs(std::env::var("WS_DECOMMISSION_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30))
}

/// Drained is a conjunction: no parent hosted here, no volume owned here, no replica row here, and
/// every volume this node ever touched has its `spec.replicas` Synced elsewhere. Anything short of
/// that is progress, and the counts say which of the four is holding it.
pub(crate) fn drain_status(running: usize, owned: usize, copies: usize, now: &str) -> String {
    if running == 0 && owned == 0 && copies == 0 {
        format!("drained {now}")
    } else {
        format!("draining running={running} owned={owned} copies={copies}")
    }
}

pub async fn decommission_beat(ctx: &Arc<Ctx>) {
    let nodes = match Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(error = %e, "decommission: listing nodes; doing nothing this beat");
            return;
        }
    };
    let me = nodes.iter().find(|n| n.name_any() == ctx.node);
    // Abort semantics, for free: the label gone means the beat does nothing at all — not even a
    // status rewrite. Copies already re-homed stay; this node is a rendezvous candidate again.
    if !crate::peer::decommissioning(me) {
        return;
    }
    // Keep-biased like every other beat: a half-listed cluster releases nothing.
    let Some(beat) = crate::listing::beat(ctx).await else { return };

    // 1. Running parents keep running, and are told why the next start lands elsewhere.
    for p in beat.parents.iter().filter(|p| p.is_live_worktree()) {
        crate::peer::set_parent_condition(
            ctx,
            p,
            "Decommissioning",
            true,
            "NodeLeaving",
            "this node is being retired; stop when convenient and the next start lands elsewhere",
        )
        .await;
    }

    // 2. Release owned volumes as they become releasable — the dead-node sweep's own three arms,
    //    the same function, called with this node as the "unavailable" owner and a different word.
    let mine: HashSet<String> = [ctx.node.clone()].into_iter().collect();
    crate::peer::sweep_volumes(ctx, &beat, &mine, "Decommissioned").await;

    // 3. Copies settle on their own: `unplaceable` already dropped this node from every other
    //    node's rendezvous, and its own retire pass drops each copy once the replacement is
    //    Synced. Nothing to do here — deliberately.

    // 4. Progress, or the stamp that gates deleting the VM.
    let running = beat.parents.iter().filter(|p| p.is_live_worktree()).count();
    let owned = beat.volumes.iter().filter(|v| v.spec.node_name == ctx.node).count();
    let copies = beat.replicas.iter().filter(|r| r.spec.node == ctx.node).count();
    let status = drain_status(running, owned, copies, &chrono::Utc::now().to_rfc3339());
    let patch = serde_json::json!({"metadata": {"annotations": {DECOMMISSION_STATUS: status}}});
    if let Err(e) = Api::<Node>::all(ctx.client.clone())
        .patch(&ctx.node, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        tracing::warn!(error = %e, "decommission: annotating my own node");
    }
}
```

`bins/agent/src/controller/run.rs`, beside `spawn_sync`:

```rust
/// The decommission beat (`decommission.rs`), same shape as the others. It costs one node list per
/// 30 s on every node and returns immediately unless THIS node carries the label — cheaper than a
/// watch on Nodes, and a beat is the right shape anyway: what it waits for (a person stopping their
/// workspace) is observed through the same listing everything else already reads.
fn spawn_decommission(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::decommission::beat_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            crate::decommission::decommission_beat(&ctx).await;
        }
    });
}
```

called from `run` beside `spawn_sync(ctx.clone());` (line 65).

`deploy/k3s/agent-rbac.yaml` — the header table row (lines 85–88) becomes:

```
#   nodes                                  get,list                     node_roles (startup);
#                                                                       replication rendezvous
#                                                                       (peer.rs) — every agent must
#                                                                       see the same candidate list;
#                                                                       the decommission label
#                                          patch                        decommission_beat, THIS
#                                                                       node's own
#                                                                       `kloudlite.io/decommission-
#                                                                       status` annotation only
```

and the rule (lines 214–216):

```yaml
  # `patch` is annotations in practice — the decommission beat writes this node's own
  # `kloudlite.io/decommission-status`, which is how an operator watches a drain and learns when
  # the VM is safe to delete. RBAC cannot narrow `patch` to one annotation on one node, and the
  # admission policy does not match `nodes`, so this is a broad verb taken KNOWINGLY: the agent
  # already runs as root with the host PID namespace on that node, so a compromised agent could
  # do anything a node can do regardless of this line. The narrowing that WOULD work — an
  # admission policy binding `request.name` to the agent's own node — is the upgrade if the
  # agent ever stops being root.
  - apiGroups: [""]
    resources: ["nodes"]
    verbs: ["get", "list", "patch"]
```

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`**
- [ ] **Step 5: Commit** — `git add bins/agent/src deploy/k3s/agent-rbac.yaml && git commit -m "Drain a labelled node without stopping anyone's work"`

---
### Task 12: Drop the four dead CRD fields and regenerate the CRDs

**Files:** (plus, for the three fields below, every struct literal and status write that names them — grep `durable`, `last_sync_at`, `node_name` in binding.rs/OwnerBindingSpec)

Also removed in this task, each with zero readers after Tasks 1–11 (spec Simplifications item 11): `WorkspaceStatus.durable` and `EnvironmentStatus.durable` (never written; drop the field, its doc, and the two `a.durable == b.durable` equality terms in the status-equality helpers), `VolumeReplicaStatus.last_sync_at` (Task 1 replaced its one reader; drop the field and the `listed_at` plumbing in `write_replica_status` that only fed it — keep the listing-instant ordering comment if `branches` still depends on it), and `OwnerBindingSpec.node_name` (drop the field; `claim::ensure_binding` stops passing it). Serde tolerates the fields on old objects because nothing sets `deny_unknown_fields`; assert that with one test that deserializes a Workspace status JSON carrying `durable` and `compatibleNodes` and a VolumeReplica status carrying `lastSyncAt` without error.

**Original files:**
- Modify `deploy/k3s/crds.yaml` (regenerated, `compatibleNodes` gone from both parent status schemas).
- Test `crates/workspaces/tests/crd_yaml.rs` (it already asserts the checked-in yaml matches what the code generates).

**Interfaces:**
- Consumes: the `kube::CustomResource` derives in `crates/workspaces/src/crd.rs` after Task 5.
- Produces: no Rust symbol — the checked-in schema.

- [ ] **Step 1: Write the failing test** — extend `crates/workspaces/tests/crd_yaml.rs`:

```rust
/// The status field placement stopped reading is gone from the SCHEMA too, not merely unwritten:
/// a schema that still advertises it invites the next reader to trust it. Old stored objects keep
/// parsing because the Rust struct tolerates the field on read (`#[serde(default)]`) and the CRD
/// prunes what it does not declare — which is exactly the wanted behaviour: the value disappears
/// on the first write of an old object, and nothing ever reads it again.
#[test]
fn compatible_nodes_is_gone_from_the_published_schema() {
    let yaml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/k3s/crds.yaml")).unwrap();
    assert!(!yaml.contains("compatibleNodes"), "regenerate deploy/k3s/crds.yaml");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-workspaces --test crd_yaml` fails: `regenerate deploy/k3s/crds.yaml`, and the existing round-trip assertion fails first with a diff on the two parent schemas.

- [ ] **Step 3: Implement** — regenerate with the crate's own generator (the same one `crd_yaml.rs` compares against) and commit the result:

```sh
cargo run -p kloudlite-workspaces --bin crdgen > deploy/k3s/crds.yaml
```

If no `crdgen` binary exists, `crd_yaml.rs`'s failure message names the generating expression; write the file from that same `serde_yaml::to_string(&crd::Workspace::crd())` sequence rather than hand-editing the yaml — a hand edit is how the checked-in schema and the code drift.

Note in `deploy/k3s/README.md`'s upgrade list that this CRD apply is safe in either order relative to the agent roll: an agent that still writes `compatibleNodes` has it pruned, and one that does not never sets it.

- [ ] **Step 4: Run tests and `cargo clippy --workspace --all-targets --locked -- -D warnings`**
- [ ] **Step 5: Commit** — `git add deploy/k3s/crds.yaml crates/workspaces/tests/crd_yaml.rs && git commit -m "Regenerate the CRDs without compatibleNodes"`

---
### Task 13: The web shows `Replicated`, `NodeDead`, `Decommissioning` and `basedOn`

**Files:**
- Create `web/apps/web/src/lib/ws-status.ts` and `web/apps/web/src/lib/ws-status.test.ts` (the repo's test shape: `bun:test`, pure functions in `src/lib/*.test.ts` — see `web/apps/web/src/lib/env-page.test.ts`).
- Modify `web/apps/web/src/lib/api.ts`: `ApiWorkspace` (lines 695–718) and `ApiEnvironment` (lines 722–741) gain `replicated` and `based_on`.
- Modify `web/apps/web/src/components/app/workspace-list.tsx` (the `Packages` sibling at line 140 is the pattern) and `web/apps/web/src/components/app/environment-list.tsx` (line 37's badge row).
- Modify the two `actions.ts` files only where the start action must surface the 409's sentence: `startWorkspace` (`workspaces/actions.ts:69–81`) and `startEnvironment` (`environments/actions.ts:21–34`) already return `r.message`, so the 409 text reaches the dialog unchanged — assert it rather than rewriting it.

**Interfaces:**
- Consumes: `ApiWorkspace.replicated`, `ApiWorkspace.based_on`, `WsState` (`lib/api.ts`).
- Produces:
  - `export type WsNotice = { tone: "info" | "warning"; text: string }`
  - `export function noticesFor(w: Pick<ApiWorkspace, "state" | "replicated" | "based_on"> & { conditions?: ... }): WsNotice[]`
  - `export function basedOnSentence(b: { snapshot: string; at?: string | null; interrupted: boolean }, now?: Date): string`

- [ ] **Step 1: Write the failing test** — `web/apps/web/src/lib/ws-status.test.ts`:

```ts
import { describe, expect, test } from "bun:test";
import { basedOnSentence, noticesFor } from "./ws-status";

describe("noticesFor", () => {
  test("a stopped workspace still copying says so, and says when it will be safe", () => {
    const n = noticesFor({
      state: "stopped",
      replicated: { ready: false, reason: "AwaitingReplica", message: "no other node holds the final sync point yet" },
    });
    expect(n).toEqual([{ tone: "info", text: "Still copying to another node — it can only start on its current node until that finishes." }]);
  });

  test("replicated says it is safe to start anywhere", () => {
    const n = noticesFor({
      state: "stopped",
      replicated: { ready: true, reason: "Replicated", message: "another node holds the final sync point" },
    });
    expect(n).toEqual([{ tone: "info", text: "Copied to another node — safe to start anywhere." }]);
  });

  test("replicas: 1 says why it will never finish copying", () => {
    const n = noticesFor({
      state: "stopped",
      replicated: { ready: false, reason: "AwaitingReplica", message: "no replica is configured for this volume" },
    });
    expect(n[0].text).toBe("No replica is configured, so this can only ever start on its current node.");
  });

  test("an interrupted workspace is a warning, and offers the clone rather than a start", () => {
    const n = noticesFor({ state: "ready", conditions: [{ type: "Degraded", reason: "NodeDead", status: "True" }] });
    expect(n).toEqual([{
      tone: "warning",
      text: "Its node is down. It resumes when the node returns — or clone it from the last synced point.",
    }]);
  });

  test("a node being retired is stated once, without alarm", () => {
    const n = noticesFor({ state: "ready", conditions: [{ type: "Decommissioning", reason: "NodeLeaving", status: "True" }] });
    expect(n).toEqual([{ tone: "info", text: "This node is being retired; stop when convenient and the next start lands elsewhere." }]);
  });

  test("a running workspace with nothing to say says nothing", () => {
    expect(noticesFor({ state: "ready" })).toEqual([]);
  });
});

describe("basedOnSentence", () => {
  const now = new Date("2026-09-03T14:38:07Z");

  test("an ordinary clone names the cut it was made from", () => {
    expect(basedOnSentence({ snapshot: "clone-ws-1-cafe", at: null, interrupted: false }, now))
      .toBe("Cloned from a sync point taken just now.");
  });

  test("an interrupted clone states the gap, because that is the whole decision", () => {
    expect(basedOnSentence({ snapshot: "sync-ws-1-bbbb", at: "2026-09-03T14:32:07Z", interrupted: true }, now))
      .toBe("Cloned from the sync point of 14:32:07, 6 minutes before the node went down.");
  });
});
```

- [ ] **Step 2: Run it, expect failure** — `cd web && bun test apps/web/src/lib/ws-status.test.ts` fails with `Cannot find module './ws-status'`.

- [ ] **Step 3: Implement** — `web/apps/web/src/lib/ws-status.ts`:

```ts
/** One place the four states a workspace or environment can be *waiting on* become sentences.
 *
 *  Pure and in `lib` so it is testable without rendering: the list components render `noticesFor`
 *  and nothing decides these words twice. The API answers with conditions the node wrote; this
 *  file is the only translation of them into English, and the messages deliberately say what the
 *  person can DO rather than restating the condition's reason. */
export type WsNotice = { tone: "info" | "warning"; text: string };

type Cond = { type: string; reason: string; status: string };
type ConditionDoc = { ready: boolean; reason: string; message: string };

export function noticesFor(x: {
  state: string;
  replicated?: ConditionDoc | null;
  conditions?: Cond[] | null;
}): WsNotice[] {
  const on = (t: string, r: string) => (x.conditions ?? []).some((c) => c.type === t && c.reason === r && c.status === "True");

  // Interrupted first: it is the only one that changes what the buttons can do (start is refused,
  // clone is the way forward), so it must not be buried under a copying notice.
  if (on("Degraded", "NodeDead")) {
    return [{ tone: "warning", text: "Its node is down. It resumes when the node returns — or clone it from the last synced point." }];
  }
  if (on("Decommissioning", "NodeLeaving")) {
    return [{ tone: "info", text: "This node is being retired; stop when convenient and the next start lands elsewhere." }];
  }
  const r = x.replicated;
  if (x.state === "stopped" && r) {
    if (r.ready) return [{ tone: "info", text: "Copied to another node — safe to start anywhere." }];
    // The `replicas: 1` case shares a reason with "not yet" on purpose (one condition, one place
    // to read it) and is told apart by the message the node wrote.
    if (r.message.startsWith("no replica is configured")) {
      return [{ tone: "info", text: "No replica is configured, so this can only ever start on its current node." }];
    }
    return [{ tone: "info", text: "Still copying to another node — it can only start on its current node until that finishes." }];
  }
  return [];
}

/** What a clone was grafted onto. Always shown: a clone is always based on a cut, and the
 *  interrupted case differs only in that the cut is older than "now" — which is precisely the
 *  thing the person accepted when they chose it. */
export function basedOnSentence(
  b: { snapshot: string; at?: string | null; interrupted: boolean },
  now: Date = new Date(),
): string {
  if (!b.at) return "Cloned from a sync point taken just now.";
  const at = new Date(b.at);
  const time = at.toISOString().slice(11, 19);
  if (!b.interrupted) return `Cloned from the sync point of ${time}.`;
  const mins = Math.max(0, Math.round((now.getTime() - at.getTime()) / 60000));
  const ago = mins === 1 ? "1 minute" : `${mins} minutes`;
  return `Cloned from the sync point of ${time}, ${ago} before the node went down.`;
}
```

`web/apps/web/src/lib/api.ts`, on both docs:

```ts
  /** The `Replicated` condition, verbatim from the node that wrote it — "safe to start anywhere"
   *  vs "still copying". Absent while running: it is only computed for a stopped parent. */
  replicated?: { ready: boolean; reason: string; message: string } | null;
  /** What a clone was grafted onto, and whether that cut predates the source's node going down. */
  based_on?: { snapshot: string; at?: string | null; interrupted: boolean } | null;
  /** The raw conditions the two notices above read — `Degraded/NodeDead`, `Decommissioning/NodeLeaving`. */
  conditions?: { type: string; reason: string; status: string }[] | null;
```

`workspace-list.tsx` renders them beside `Packages` (line 140's shape):

```tsx
/** The waiting-on notices: at most one, rendered where the person is already looking for state.
 *  `text-warning` only for the interrupted case — everything else here is information, and a page
 *  where every line is orange is a page nobody reads. */
function Notices({ w }: { w: ApiWorkspace }) {
  const notices = noticesFor(w);
  if (notices.length === 0) return null;
  return (
    <>
      {notices.map((n) => (
        <p key={n.text} className={`text-caption ${n.tone === "warning" ? "text-warning" : "text-muted"}`}>
          {n.text}
        </p>
      ))}
      {w.based_on && <p className="text-caption text-muted">{basedOnSentence(w.based_on)}</p>}
    </>
  );
}
```

and `environment-list.tsx` renders the same component beside its `WsEnvStateBadge` (line 37).

- [ ] **Step 4: Run tests** — `cd web && bun run typecheck && bun run lint && bun test`
- [ ] **Step 5: Commit** — `git add web/apps/web/src && git commit -m "Show what a workspace is waiting on and what a clone is based on"`

---
### Task 14: Docs and the e2e's stop assertions

**Files:**
- Modify `CLAUDE.md` lines 214–220 (the stop paragraph) and 146–152 (the dead-node paragraph).
- Modify `deploy/k3s/README.md`: "Sync points" (lines 570–584), "Node death" (lines 586–618), and a new "### Decommissioning a node" after it.
- Modify `README.md` line 106's row only if it names the flush (it does not — check and leave alone).
- Modify `tests/ws_e2e.sh`: the workspace stop at lines 647–655 and the environment stop at lines 935–944.

**Interfaces:**
- Consumes: everything Tasks 1–13 shipped.
- Produces: no code — the operator-facing description of it.

- [ ] **Step 1: Write the failing test** — in `tests/ws_e2e.sh`, tighten the two stop blocks so the old behaviour would fail:

```sh
# A stop is now seconds, not minutes: the cut turns Ready and the pod goes in the same pass, and
# the replica wait moved into placement. 60s, not 300s, is the assertion — a stop that takes
# longer than that is the flush gate having come back.
kubectl wait --for=jsonpath='{.status.phase}'=stopped "workspace/$WS_ID" --timeout=60s \
  || fail "workspace $WS_ID never reached phase=stopped"
kubectl -n "$WS_NS" get "pod/$WS_ID" >/dev/null 2>&1 && fail "the pod is still there after the stop"

# `FlushUnreplicated` is gone entirely: a stop is never "unreplicated", only not-yet-replicated,
# and that is the `Replicated` condition's job for as long as it is true.
kubectl get "workspace/$WS_ID" -o json | grep -q FlushUnreplicated \
  && fail "FlushUnreplicated is gone; a stop no longer records a one-shot flush verdict"

# And the condition that replaced it is there, with one of the two reasons and nothing else.
REPL=$(kubectl get "workspace/$WS_ID" -o jsonpath='{.status.conditions[?(@.type=="Replicated")].reason}')
case "$REPL" in
  Replicated|AwaitingReplica) : ;;
  *) fail "expected a Replicated condition on the stopped workspace, got '$REPL'" ;;
esac
```

with the same three assertions after `wait_env_stopped "$ENV_ID"` (line 937).

- [ ] **Step 2: Run it, expect failure** — `./tests/ws_e2e.sh` on a k3s box against the pre-change agent fails at `expected a Replicated condition on the stopped workspace, got ''`. On this Mac it exits 77 (no btrfs, no cluster), which is a SKIP and not a pass — the assertion is verified on the cluster the roll targets.

- [ ] **Step 3: Implement** — `CLAUDE.md`, replacing lines 214–220:

```
Stopping a workspace or environment cuts a `stop-{ws}-{gen}`/`stop-{env}-{gen}` sync point, named by
the parent's generation so every stop is a fresh snapshot (skipped if the pod never ran), and tears
the pod (or the StatefulSets) down as soon as that cut is Ready — a stop is seconds, and it never
waits for a replica. Right after the cut the owner POSTs `/peer/v1/wake` to every placeable node so
the peers pull within seconds instead of at the next `WS_REPLICA_SECS` beat. Whether the bytes have
landed elsewhere is the `Replicated` condition on the stopped object (`True/Replicated`, or
`False/AwaitingReplica` with a message that says whether it will ever become true), rewritten on
every reconcile of a stopped parent and read by `/v1`, the web and the dead-node sweep. **The wait
moved into placement**: a stopped parent may start on ANOTHER node only once that node is up to
date for the worktree — its `VolumeReplica.status.branches[worktree]` names that worktree's newest
Ready transient — and until then the only place it can start is its own node. `may_claim` is that
one rule; `compatibleNodes` is gone. Starts also SPREAD: when a volume is movable (nothing on it
running) its owner computes the preferred node by rendezvous over `{owner} ∪ {up-to-date nodes}`,
keyed by the volume id, and hands the volume over (release CAS + un-place every parent) when that
is not itself.
```

and, replacing the dead-node sentence at lines 146–152:

```
When a node is unplaceable — dead for `WS_NODE_DEAD_SECS`, or labelled
`kloudlite.io/decommission=true` (`peer::unplaceable`, one predicate for both) — the sweep decides
PER VOLUME, never per parent: any Running parent on it pins the whole volume (`Unavailable/NodeDead`,
`Degraded/NodeDead` on every parent, nothing moves); otherwise any stopped parent that is not yet
`Replicated` pins it too; otherwise the pin is cleared and every parent un-placed, so an up-to-date
node claims them on the next start. A running worktree is *interrupted*, not moved: `/v1` refuses to
start it (409, "its node is down; it resumes when the node returns") and the way forward is a clone
from the last synced point, whose response and page state the cut's age. A decommission is the
planned version of the same thing with one difference — whatever runs there keeps running; the
node's own agent beats every 30 s, tells its running parents (`Decommissioning/NodeLeaving`),
releases each volume as it becomes releasable, and stamps
`kloudlite.io/decommission-status: draining running=N owned=N copies=N` until it can stamp
`drained <RFC 3339>`, which is the operator's gate on deleting the VM.
```

`deploy/k3s/README.md`, "Sync points" (lines 577–584) loses the `WS_STOP_FLUSH_TIMEOUT_SECS` and `FlushUnreplicated` sentences and gains:

```
`WS_SYNC_SECS` (default 60) is how often the beat checks. A stop no longer waits for anything: the
cut turns Ready, the pod goes, and the owner pokes every placeable peer with `/peer/v1/wake` so the
copy happens in seconds. Whether it HAS happened is the `Replicated` condition on the stopped
object — `kubectl get workspace X -o jsonpath='{.status.conditions[?(@.type=="Replicated")]}'`.
`False/AwaitingReplica` with "no other node holds the final sync point yet" is normal for a few
seconds after a stop and is only worth investigating if it persists; with the message "no replica is
configured for this volume" it will never become true (that is `spec.replicas: 1`), and the
workspace simply always starts on its own node.
```

and a new section after "Node death":

```
### Decommissioning a node

The planned version of node death. It never stops anyone's work; the node drains at the people's
pace, and an operator in a hurry stops those workspaces through `/v1` like anyone else.

1. `kubectl label node <n> kloudlite.io/decommission=true`. From that moment every other agent
   treats it as unplaceable — it wins no rendezvous slot, counts as no copy, and refuses claims —
   while it keeps serving pulls and keeps running everything already on it.
2. Watch the one annotation: `kubectl describe node <n> | grep decommission-status`, or
   `kubectl get node <n> -o jsonpath='{.metadata.annotations.kloudlite\.io/decommission-status}'`.
   It reads `draining running=N owned=N copies=N` and is rewritten every 30 s. `running` is people's
   workspaces — it only falls when they stop them. `owned` falls as each volume becomes releasable
   (everything on it stopped AND replicated); `copies` falls as its replicas re-home and its own
   retire pass drops them.
3. When all three reach zero the annotation becomes `drained <RFC 3339>`.
4. Only then delete the VM, and remove that node's flannel `/32` from the `ipBlock` list in
   `deploy/k3s/system-netpol.yaml` (read one off a node with `ip -4 addr show flannel.1`; the list
   is hand-maintained, see the comment at line 32).

Deleting the VM before `drained` is the dead-node path: copies still heal, but any volume not yet
released waits for a node that will never return. That is the whole reason `drained` is a gate and
not just a progress line.

To abort, remove the label: the beat stops immediately, parents already stopped stay stopped (start
them — they run here again if the volume was not released, elsewhere if it was), copies already
re-homed stay re-homed, and the node becomes a rendezvous candidate again.
```

- [ ] **Step 4: Run tests** — `shellcheck tests/ws_e2e.sh` (as CI runs it) and `cargo test --workspace`; the e2e itself runs on the k3s box, where exit 77 is a skip and not a pass.
- [ ] **Step 5: Commit** — `git add CLAUDE.md deploy/k3s/README.md tests/ws_e2e.sh && git commit -m "Document the instant stop, the placement rule and node decommission"`

---

## Self-review

Spec section → task:

| spec section | task(s) |
|---|---|
| "Words used here" — *up to date*, `status.branches` | 1 |
| "Rules" 1 (a running parent never moves) | 6, 8 |
| "Rules" 2 (stop flushes then tears down; replication poked) | 2, 4 |
| "Rules" 3 (a stopped parent starts elsewhere only when that node is up to date) | 5 |
| "Rules" 4 (ownership per volume, so moving is per volume) | 6 |
| "Rules" 5 (decommission = planned death, running work keeps running) | 3, 11 |
| "Stop (both kinds)" — teardown at the cut, `flush_*` deleted | 4 |
| "Stop (both kinds)" — `/peer/v1/wake` to every live node | 2, 4 |
| "Stop (both kinds)" — the `Replicated` condition, `/v1` exposure | 4 |
| "Where a stopped parent starts (spread)" — `may_claim` | 5 |
| "Where a stopped parent starts (spread)" — rendezvous, release, un-place | 10 |
| "Interrupted parents" — 409 on start | 8 |
| "Interrupted parents" — clone allowed, age stated | 9, 13 |
| "Clone" — cut `clone-{ws}-{hex}` + wake; `source_nodes` replaced by the up-to-date check | 9 |
| "Dead-node sweep, per volume" — the three arms as one function | 6 |
| "Mismatch self-heal" | 7 |
| "Node decommission" — placement treats it as unplaceable | 3 |
| "Node decommission" — the 30 s beat, the four numbered points, RBAC | 11 |
| "Node decommission" — the runbook, the flannel `/32` | 14 |
| Simplification 1 (one `unplaceable`) | 3 |
| Simplification 2 (one `Replicated` truth, no `NoReplica`) | 4, 6 |
| Simplification 3 (no `Released` reason) | 6 |
| Simplification 4 (`STOP_GENERATION` deleted) | 4 |
| Simplification 5 (`Landed` is a unit variant) | 4 |
| Simplification 6 (`compatibleNodes` dead) | 5, 12 |
| Simplification 7 (one decommission annotation) | 11 |
| Simplification 8 (`basedOn` on every clone response) | 9, 13 |
| Simplification 9 (one per-volume decision function) | 6 |
| Simplification 10 (`source_nodes` deleted) | 5, 9 |
| "Costs, named" — the VolumeReplica list per stopped reconcile | 4 |
| "Rulings" 1–4 | 10, 11, 8, 9 |

"Cases checked" → the task whose test covers it:

| case | covered by |
|---|---|
| Stop cut done, node dies before any replica pulled it | 6 — `one_unreplicated_stopped_parent_holds_the_whole_volume` |
| Node dies between the stop request and the cut | 6 — `a_running_parent_pins_the_whole_volume` (the parent is still Running from the system's view until the teardown) |
| Person stops an interrupted parent | covered by design: `desiredState: Stopped` is a spec write `/v1` always accepts, and the pinned parent's own controller runs the stop when the node returns — Task 8 refuses only `start`, and Task 6's arm one keeps the pin until then |
| Clone of a parent whose volume is released or whose node is dead | 9 — `cloning_an_interrupted_source_is_allowed_and_states_the_cut_it_used` |
| Restore-to-new from a commit | 1 — `with_no_transient_plain_synced_is_up_to_date`; 5 — `with_no_transient_a_synced_replica_may_claim` |
| `replicas: 1` | 4 — `replicas_one_says_so_in_the_message_not_in_a_second_reason`; 10 — `with_no_up_to_date_replica_the_owner_keeps_it`; 13 — the `replicas: 1` notice test |
| Two stopped parents on one volume, one replicated and one not | 6 — `one_unreplicated_stopped_parent_holds_the_whole_volume` (asserts the message names the holder) |
| Retention deletes the sync transient before a replica pulled the stop one | 1 — `up_to_date_compares_names_never_phases_or_clocks` (the `behind` row is `Synced` and still refused) |
| Two up-to-date nodes race to take a released volume | 7 — `a_mismatch_against_a_live_owner_un_places_me` |
| Start of a parent whose volume is pinned to a decommissioning node (a sibling runs there) | 10 — `a_volume_with_a_running_sibling_never_moves` (the owner path applies; the node takes no new work only for volumes it does not own) |
| Decommissioning node dies mid-drain | covered by design: `unplaceable` is already true for it, so every other node's `sweep_dead_nodes` (Task 6) picks its volumes up under the same three arms — nothing in Task 11 is load-bearing for correctness, only for progress reporting |
| Node NotReady for less than the 180 s floor | 3 — `decommissioning_is_unplaceable_but_not_dead` asserts the floor is what `node_is_dead` reads; the existing `reaper_deletes_dead_keeps_young_keeps_absent_condition` (peer.rs:1484) is unchanged and covers the young case |
| Stop transient cut fails (btrfs error) | 4 — `a_stop_tears_down_as_soon_as_the_cut_is_ready` covers the Ready path; the `Working` path is `StopPush::Waiting`, unchanged and covered by the existing stop tests |
| Many stops in a burst | 2 — `many_wakes_in_a_burst_coalesce_into_one_more_pass` |
| Deleting an interrupted parent | covered by design: delete is untouched by this plan — the ownerReference cascade is unchanged, and Task 8 gates only `start` |
| Environment with several services | 4 — the environment half of the stop tests; 6 — `a_fully_replicated_stopped_volume_is_released` includes an `Environment` parent |
