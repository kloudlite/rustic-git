//! snapshot-model placement.

use super::*;


pub(crate) const SNAPSHOTS_LIST: &str = "/apis/kloudlite.io/v1alpha1/snapshots";

pub(crate) fn snapshot_list_of(kind: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({"apiVersion": "v1", "kind": format!("{kind}List"), "items": items})
}

pub(crate) fn snapshot_cr(name: &str, volume: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name, "uid": "snap-uid"},
        "spec": {"volume": volume, "owner": "alice", "worktree": "ws-1", "parent": ""},
        "status": {"phase": "ready"},
    })
}

pub(crate) fn volume_replica(volume: &str, node: &str, phase: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": crd::replica_name(volume, node), "uid": "vr-uid"},
        "spec": {"volume": volume, "node": node},
        "status": {"phase": phase, "branches": {}},
    })
}

/// A workspace whose volume already has snapshots and whose replica on THIS node reports Synced is
/// claimed exactly like the old `compatibleNodes` arm — ruling A.
#[tokio::test]
async fn snapshot_model_a_synced_replica_claims_a_workspace_with_snapshots() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![snapshot_cr("vol-1-a", "vol-1")]) },
            kloudlite_workspaces::kube_test::get(
                format!("/apis/kloudlite.io/v1alpha1/volumereplicas/{}", crd::replica_name("vol-1", "node-a")),
                volume_replica("vol-1", "node-a", "Synced"),
            ),
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "Synced: claimed");
}

/// An up-to-date non-owner must NOT claim a parent whose volume is pinned to a LIVE owner, even
/// though `may_claim`'s up-to-date rule would otherwise allow it — that combination is exactly
/// the mismatch arm's self-heal turned into a ping-pong: un-place, re-claim, mismatch, un-place,
/// every 5s, forever, with the real owner's own reconcile never getting a turn.
#[tokio::test]
async fn an_up_to_date_non_owner_does_not_claim_a_parent_whose_volume_has_a_live_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                "/apis/kloudlite.io/v1alpha1/volumes/vol-1",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "vol-1", "uid": "uid-1"},
                                   "spec": {"owner": "alice", "nodeName": "node-b", "region": "r1", "quotaGb": 20}}),
            ),
            kloudlite_workspaces::kube_test::get(
                "/api/v1/nodes/node-b",
                serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "node-b"},
                                   "status": {"conditions": [{"type": "Ready", "status": "True",
                                                              "lastTransitionTime": rfc3339_ago(60)}]}}),
            ),
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "only the live owner may claim its own volume's parent");
}

/// The same volume, but this node's replica is Syncing (or absent) — ruling A's other half: no
/// claim, and the object is left unplaced for whichever node IS Synced.
#[tokio::test]
async fn snapshot_model_a_syncing_replica_does_not_claim_a_workspace_with_snapshots() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![snapshot_cr("vol-1-a", "vol-1")]) },
            kloudlite_workspaces::kube_test::not_found(format!(
                "/apis/kloudlite.io/v1alpha1/volumereplicas/{}",
                crd::replica_name("vol-1", "node-a")
            )),
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "no replica on this node: never started dataless");
}

/// A volume with ZERO `Snapshot` CRs is the bootstrap case (ruling B) — claimable by any pool
/// node with no replica at all, same as the old empty-`compatibleNodes` arm.
#[tokio::test]
async fn snapshot_model_a_zero_snapshot_volume_is_claimable_as_bootstrap() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![]) },
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "zero snapshots: bootstrap, claimable");
}

/// A brand-new workspace has no child `Volume` at all yet (`volumeRef` unset) — the same bootstrap
/// case, reached with no Snapshot list at all since there is no volume to list.
#[tokio::test]
async fn snapshot_model_a_workspace_with_no_volume_yet_is_claimable_as_bootstrap() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );

    kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1);
    assert!(!rec.calls().iter().any(|c| c.starts_with(&format!("GET {SNAPSHOTS_LIST}"))), "nothing to list without a volume");
}

/// F1: the write a claim makes is a PUT of the WHOLE status subresource — building it from only
/// the 4 fields `decide` cares about would silently erase `head`, `volumeRef` and everything else
/// a prior life on another node already put there. This is exactly the shape `unclaim_dead_nodes`
/// leaves behind: `nodeName` cleared, everything else intact.
#[tokio::test]
async fn f1_reclaiming_an_unclaimed_workspace_preserves_head_and_volume_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    let w = workspace(serde_json::json!({
        "phase": "ready", "nodeName": "",
        "volumeRef": "vol-1", "head": "vol-1-snapshot-a",
        "packages": {"base": [], "observed": [], "observedHash": "h1", "profile": "/nix/store/x"},
    }));

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"]["nodeName"], "node-a", "reclaimed by this node");
    assert_eq!(sent[0]["status"]["volumeRef"], "vol-1", "F1: volumeRef must survive the claim write");
    assert_eq!(sent[0]["status"]["head"], "vol-1-snapshot-a", "F1: head must survive the claim write");
    assert_eq!(sent[0]["status"]["packages"]["profile"], "/nix/store/x", "F1: nothing else in status is wiped either");
}

/// F4: a `Snapshot`-list error must never read as "bootstrap, claim it" — that is the exact
/// never-started-dataless failure the snapshot-model arm exists to prevent.
#[tokio::test]
async fn f4_a_snapshot_list_error_claims_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) }],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    let result = kloudlite_agent::claim::claim_workspace(&w, &ctx).await;
    assert!(result.is_err(), "a Snapshot-list error must not resolve to a claim decision");
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "nothing is claimed on a listing error");
}

/// F4's other half: a `VolumeReplica`-get error must not read as "no replica, so not Synced,
/// so no claim" either — that HAPPENS to be the safe answer for this arm, but the point is the
/// error must propagate rather than being silently folded into a boolean.
#[tokio::test]
async fn f4_a_volume_replica_get_error_claims_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![snapshot_cr("vol-1-a", "vol-1")]) },
            Route {
                method: "GET",
                path: format!("/apis/kloudlite.io/v1alpha1/volumereplicas/{}", crd::replica_name("vol-1", "node-a")),
                status: 500,
                body: serde_json::json!({"message": "etcd is down"}),
            },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    let result = kloudlite_agent::claim::claim_workspace(&w, &ctx).await;
    assert!(result.is_err(), "a VolumeReplica-get error must not resolve to a claim decision");
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "nothing is claimed on a lookup error");
}

/// F2: `status.head` has no writer yet in this task (Task 5 snapshots, Task 6 clones/restores) — a
/// workspace whose volume already has snapshots but whose OWN `head` is still `None` must not be
/// handed an empty bootstrap worktree. It waits, and no `checkout` (and so no worktree dir) ever
/// happens.
#[tokio::test]
async fn f2_head_none_with_snapshots_present_requeues_without_a_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ssh_routes();
    // TWICE: a re-host pass lists snapshots once for `latest_transient` and once for
    // `has_snapshots`, and this mock walks a path's routes in order — with one route the second
    // listing would fall through to the empty default and read as a zero-snapshot bootstrap.
    for _ in 0..2 {
        routes.push(Route {
            method: "GET",
            path: SNAPSHOTS_LIST.into(),
            status: 200,
            body: snapshot_list_of("Snapshot", vec![snapshot_cr("ws-1-a", "ws-1")]),
        });
    }
    let (ctx, rec, _fake) = ws_ctx_with_ssh(tmp.path(), routes);
    // `ws_ctx_with_ssh` pre-seeds an empty worktree so tests that expect a normal checkout don't
    // need real btrfs; THIS test's whole point is that no checkout may happen at all, so undo
    // that seeding before exercising it.
    std::fs::remove_dir_all(tmp.path().join("vol/ws-1/live/ws-1")).unwrap();

    let action = kloudlite_agent::controller::apply_workspace(&ready_workspace("ws-1", vec![]), &ctx).await.unwrap();

    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)), "waits for T5/T6 to record a head");
    assert!(!rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")), "no pod without a resolved head: {:?}", rec.calls());
    assert!(!tmp.path().join("vol/ws-1/live/ws-1").exists(), "no bootstrap worktree either");
}

/// The re-host fixture: a placed workspace whose `status.head` is `ws-1-aaaaaaaa`, no worktree on
/// this pool (that is what makes it a re-host), and a snapshot listing of that head plus two sync
/// points — ordered so a pick by listing order, or a last-one-wins pick, lands on the wrong object.
/// `present` names the `snap/` dirs this node actually holds.
pub(crate) async fn rehost_outcome(tmp: &std::path::Path, present: &[&str]) -> String {
    let transient = |name: &str, generation: &str| {
        let mut s = snapshot_cr(name, "ws-1");
        s["spec"]["transient"] = true.into();
        s["metadata"]["annotations"] = serde_json::json!({"kloudlite.io/synced-generation": generation});
        s
    };
    let mut routes = ssh_routes();
    routes.push(Route {
        method: "GET",
        path: SNAPSHOTS_LIST.into(),
        status: 200,
        body: snapshot_list_of(
            "Snapshot",
            vec![snapshot_cr("ws-1-aaaaaaaa", "ws-1"), transient("sync-ws-1-bbbbbbbb", "9"), transient("sync-ws-1-cccccccc", "4")],
        ),
    });
    let (ctx, _rec, _fake) = ws_ctx_with_ssh(tmp, routes);
    std::fs::remove_dir_all(tmp.join("vol/ws-1/live/ws-1")).unwrap();
    for name in present {
        std::fs::create_dir_all(tmp.join("vol/ws-1/snap").join(name)).unwrap();
    }
    let mut w = ready_workspace("ws-1", vec![]);
    w.status.as_mut().unwrap().head = Some("ws-1-aaaaaaaa".into());
    // Which snapshot the checkout was asked for is read off WHICH failure comes back, and that is
    // sharp on any platform: a source that is not on the pool fails `NO_SUCH_RECORD` before any
    // shell-out, and a source that IS there gets past that check — succeeding on a btrfs node and
    // dying in `spawn btrfs` on one without, but never as `NO_SUCH_RECORD`.
    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.err().map(|e| e.0).unwrap_or_default()
}

/// Re-host: a node that has never run this worktree starts from the newest SYNC POINT, not from
/// `status.head` — the sync beat replicated it after the last snapshot, so the loss window on a node
/// death is one `WS_SYNC_SECS`. Only the gen-9 sync point is on the pool, and the head snapshot is
/// NOT, so picking the head (or the older gen-4 point) is a `NO_SUCH_RECORD`.
#[tokio::test]
async fn a_workspace_starting_on_a_new_node_checks_out_its_latest_sync_point_over_its_head() {
    let tmp = tempfile::tempdir().unwrap();

    let outcome = rehost_outcome(tmp.path(), &["sync-ws-1-bbbbbbbb"]).await;

    assert_ne!(
        outcome,
        kloudlite_workspaces::engine::ops::NO_SUCH_RECORD,
        "the checkout must have been asked for the local sync point, not the absent head snapshot"
    );
}

/// The other half, and the reason `latest_transient` intersects with `local_snapshots`: a replica one
/// pull cycle behind sees a `Ready` transient whose subvolume has not landed here yet. Checking
/// that out is a PERMANENT `NO_SUCH_RECORD` with no fallback, where `head` — which this node DOES
/// hold — would have started the worktree perfectly well. Neither sync point is on the pool here,
/// so a sharp fall back to the head is the only acceptable outcome.
#[tokio::test]
async fn a_sync_point_this_node_has_not_pulled_yet_falls_back_to_the_head() {
    let tmp = tempfile::tempdir().unwrap();

    let outcome = rehost_outcome(tmp.path(), &["ws-1-aaaaaaaa"]).await;

    assert_ne!(
        outcome,
        kloudlite_workspaces::engine::ops::NO_SUCH_RECORD,
        "an unpulled sync point must not be checked out; the local head must be"
    );
}

pub(crate) const WS_CLONE_OBJ: &str = "/apis/kloudlite.io/v1alpha1/workspaces/ws-clone";
pub(crate) const WS_1_OBJ: &str = "/apis/kloudlite.io/v1alpha1/workspaces/ws-1";

/// A shared-volume clone workspace (`cloneOf { snapshot: Some(_) }`), whose worktree lives under
/// the SOURCE volume's `live/`, not its own.
pub(crate) fn clone_workspace() -> crd::Workspace {
    let mut w = workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-src"}));
    w.metadata.name = Some("ws-clone".into());
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "vol-src".into(), commit: Some("vol-src-abcd".into()) }),
    });
    w
}

/// (i) Task 7a finding 1 / item 3: a fresh clone reconcile ADDS the finalizer. kube-rs's
/// `finalizer()` combinator patches it on and returns `await_change()` WITHOUT running `Apply` at
/// all ("No point applying here, since the patch will cause a new reconciliation") — so this needs
/// no other route.
#[tokio::test]
async fn a_clone_reconcile_adds_the_worktree_finalizer() {
    let tmp = tempfile::tempdir().unwrap();
    let route = Route { method: "PATCH", path: WS_CLONE_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-src"})) };
    let (ctx, rec) = ctx(tmp.path(), vec![route]);

    kloudlite_agent::controller::reconcile_workspace(Arc::new(clone_workspace()), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WS_CLONE_OBJ).len(), 1, "the finalizer-add patch");
}

/// (ii) An OWNED workspace grows the finalizer too, now that a delete has to decide whether its
/// Volume survives: without one the parent would be gone before anything could look at its
/// snapshots, and the Volume — snapshots and all — would go with it.
#[tokio::test]
async fn an_owned_workspace_reconcile_adds_the_finalizer() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ws_stop_routes();
    routes.push(Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) });
    routes.push(Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) });
    let (ctx, rec) = ctx(tmp.path(), routes);
    let mut w = stopping_ws();
    w.status.as_mut().unwrap().pod_ref = None;
    assert!(w.spec.storage.as_ref().and_then(|s| s.source.as_ref()).is_none());

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WS_1_OBJ).len(), 1, "the finalizer-add patch: {:?}", rec.calls());
}

/// (iii) A deleting clone that already carries the finalizer: `reconcile_workspace` runs the
/// `Cleanup` arm (which calls `drop_worktree` — proved for real by
/// `engine_snapshot.rs`'s `drop_worktree_deletes_the_subvolume_and_is_ok_on_absent_retry`; here the
/// worktree is simply absent, so `drop_worktree`'s own no-op-on-absent path keeps this a pure loop
/// test), then removes the finalizer via kube-rs's Test+Remove JSON patch.
#[tokio::test]
async fn a_deleting_clones_reconcile_drops_its_worktree_then_removes_the_finalizer() {
    let tmp = tempfile::tempdir().unwrap();
    // `ws_lock` needs `vol/` to exist to create its lock file — normally left behind by an
    // earlier checkout; nothing else in this pure-loop test ever creates it.
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let route = Route { method: "PATCH", path: WS_CLONE_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-src"})) };
    let (ctx, rec) = ctx(tmp.path(), vec![route]);
    let mut w = clone_workspace();
    w.metadata.finalizers = Some(vec![crd::WORKTREE_FINALIZER.to_string()]);
    w.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WS_CLONE_OBJ).len(), 1, "the finalizer-remove patch");
}

/// (iv) A non-clone workspace that still carries a finalizer left by an earlier pass (a rollback,
/// or a respec away from `cloneOf`) and is deleting: `reconcile_workspace` must still enter the
/// wrapper — the guard is "nothing to add AND nothing to remove", not "not a clone" alone — run
/// the Cleanup arm (nothing on disk here, and no snapshots for this volume), and remove the
/// finalizer, or the object is stranded in Terminating forever.
#[tokio::test]
async fn a_reconcile_of_an_already_finalized_deleting_non_clone_workspace_removes_it() {
    let tmp = tempfile::tempdir().unwrap();
    // `ws_lock` needs `vol/` to exist to create its lock file; the cleanup now runs for every
    // parent, not only a shared clone.
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let route = Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) };
    let (ctx, rec) = ctx(tmp.path(), vec![route]);
    let mut w = workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"}));
    w.metadata.finalizers = Some(vec![crd::WORKTREE_FINALIZER.to_string()]);
    w.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WS_1_OBJ).len(), 1, "the finalizer-remove patch");
}

pub(crate) const VOL_WS1: &str = "/apis/kloudlite.io/v1alpha1/volumes/ws-1";
pub(crate) const SNAPS: &str = "/apis/kloudlite.io/v1alpha1/snapshots";

/// A Volume owned by `uid`, as the API server returns it.
pub(crate) fn owned_volume(name: &str, uid: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "uid": "vol-uid",
                     "ownerReferences": [{"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
                                          "name": name, "uid": uid, "controller": true}]},
        "spec": {"owner": "alice", "nodeName": "node-a", "region": "r1", "quotaGb": 10},
    })
}

/// A Ready push — a snapshot, the only kind that is never deleted with its parent and the only
/// kind that keeps a Volume alive past it.
pub(crate) fn snapshot_record(name: &str, volume: &str, worktree: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name, "uid": "p-uid"},
        "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": ""},
        "status": {"phase": "ready"},
    })
}

pub(crate) fn ready_transient(name: &str, volume: &str, worktree: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name, "uid": "t-uid"},
        "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": "", "transient": true},
        "status": {"phase": "ready"},
    })
}

pub(crate) fn snap_list(items: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "SnapshotList",
                       "metadata": {"resourceVersion": "1"}, "items": items})
}

pub(crate) fn deleting_ws(mut w: crd::Workspace) -> crd::Workspace {
    w.metadata.finalizers = Some(vec![crd::WORKTREE_FINALIZER.to_string()]);
    w.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));
    w
}

/// Deleting a workspace that holds a snapshot keeps it: the worktree goes, every sync point of it
/// goes, and the Volume is DETACHED (this parent's ownerReference removed) rather than
/// left for GC to take the snapshot with it.
#[tokio::test]
async fn deleting_a_workspace_with_a_snapshot_detaches_its_volume() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let routes = vec![
        kloudlite_workspaces::kube_test::get(
            SNAPS,
            snap_list(vec![snapshot_record("ws-1-aaaaaaaa", "ws-1", "ws-1"), ready_transient("sync-ws-1-aaaa", "ws-1", "ws-1")]),
        ),
        Route { method: "DELETE", path: format!("{SNAPS}/sync-ws-1-aaaa"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        kloudlite_workspaces::kube_test::get(VOL_WS1, owned_volume("ws-1", "ws-uid-1")),
        Route { method: "PATCH", path: VOL_WS1.into(), status: 200, body: owned_volume("ws-1", "ws-uid-1") },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    let patches = rec.sent("PATCH", VOL_WS1);
    assert_eq!(patches.len(), 1, "one detach patch: {:?}", rec.calls());
    // The guarded shape: `test` on the list we read, then `replace` with that list minus us —
    // here empty, which IS the detached state.
    assert_eq!(patches[0][0]["op"], "test");
    assert_eq!(patches[0][0]["path"], "/metadata/ownerReferences");
    assert_eq!(patches[0][0]["value"][0]["uid"], "ws-uid-1");
    assert_eq!(patches[0][1]["op"], "replace");
    assert_eq!(patches[0][1]["value"], serde_json::json!([]));
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/sync-ws-1-aaaa")), "the transient goes: {:?}", rec.calls());
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE /apis/kloudlite.io/v1alpha1/volumes/")), "the Volume itself is never deleted");
    assert!(!rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/ws-1-aaaaaaaa")), "a snapshot is never deleted");
}

/// The owner's rule, the other half: a SYNC POINT is replication state for a worktree that no
/// longer exists, not a snapshot. Every sync point of this parent goes, whatever its phase, and
/// with no snapshot left the Volume keeps its ownerReference and GC takes it.
#[tokio::test]
async fn deleting_a_workspace_with_only_sync_points_deletes_them_and_leaves_the_volume_to_gc() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    // `working` proves phase does not gate the deletion.
    let mut working = ready_transient("sync-ws-1-bbbb", "ws-1", "ws-1");
    working["status"]["phase"] = serde_json::json!("working");
    let routes = vec![
        kloudlite_workspaces::kube_test::get(
            SNAPS,
            snap_list(vec![ready_transient("sync-ws-1-aaaa", "ws-1", "ws-1"), working]),
        ),
        Route { method: "DELETE", path: format!("{SNAPS}/sync-ws-1-aaaa"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: format!("{SNAPS}/sync-ws-1-bbbb"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    for name in ["sync-ws-1-aaaa", "sync-ws-1-bbbb"] {
        assert!(rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/{name}")), "{name} was kept: {:?}", rec.calls());
    }
    assert!(rec.sent("PATCH", VOL_WS1).is_empty(), "no snapshot: the Volume goes with its parent: {:?}", rec.calls());
}

pub(crate) const VOLUMES: &str = "/apis/kloudlite.io/v1alpha1/volumes";

/// C3: deleting an interrupted workspace must not delete the sync point a rescue clone is seeding
/// from. `retain` has this rule; the delete path did not, so an ordinary delete destroyed the
/// documented recovery for an interrupted parent and settled the clone `Permanent/NoSuchSnapshot`.
#[tokio::test]
async fn deleting_a_parent_keeps_a_sync_point_a_seeded_clone_still_names() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    // Not Ready: a materialized clone has copied the bytes and released the pin.
    let seeded_vol = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-rescue", "uid": "v-rescue"},
        "spec": {"owner": "alice", "nodeName": "node-b", "region": "r1", "quotaGb": 5,
                 "source": {"seededFrom": {"volume": "ws-1", "snapshot": "sync-ws-1-aaaa"}}},
        "status": {"phase": "creating", "subvolumePresent": false}
    });
    let routes = vec![
        kloudlite_workspaces::kube_test::get(
            SNAPS,
            snap_list(vec![ready_transient("sync-ws-1-aaaa", "ws-1", "ws-1"), ready_transient("sync-ws-1-bbbb", "ws-1", "ws-1")]),
        ),
        kloudlite_workspaces::kube_test::get(VOLUMES, serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeList",
            "metadata": {"resourceVersion": "1"}, "items": [seeded_vol]})),
        Route { method: "DELETE", path: format!("{SNAPS}/sync-ws-1-bbbb"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert!(
        !rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/sync-ws-1-aaaa")),
        "a cut a SeededFrom volume names must survive its parent's delete: {:?}", rec.calls()
    );
    assert!(
        rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/sync-ws-1-bbbb")),
        "every other sync point of the worktree still goes: {:?}", rec.calls()
    );
    assert!(rec.sent("PATCH", VOL_WS1).is_empty(), "no snapshot: the Volume still goes with its parent: {:?}", rec.calls());
}

/// A failed Volume listing must delete NOTHING: a half-seen set is exactly the case that drops the
/// cut somebody is waiting on, and the finalizer retries the whole cleanup on an Err.
#[tokio::test]
async fn a_failed_seeded_listing_deletes_no_sync_points() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let routes = vec![
        kloudlite_workspaces::kube_test::get(SNAPS, snap_list(vec![ready_transient("sync-ws-1-aaaa", "ws-1", "ws-1")])),
        Route { method: "GET", path: VOLUMES.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})));

    let out = kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await;

    assert!(out.is_err(), "a partial view must requeue the finalizer, not proceed");
    assert!(
        rec.calls().iter().all(|c| !c.starts_with("DELETE ")),
        "nothing is deleted on a partial view: {:?}", rec.calls()
    );
}

/// A push still being CUT when its parent was deleted keeps the Volume too: waiting for `Ready`
/// here let GC delete the subvolume out from under the cut and leave an orphan record naming a
/// Volume that is gone.
#[tokio::test]
async fn a_push_that_is_still_working_detaches_the_volume() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let mut working = snapshot_record("ws-1-dddddddd", "ws-1", "ws-1");
    working["status"]["phase"] = serde_json::json!("working");
    let routes = vec![
        kloudlite_workspaces::kube_test::get(SNAPS, snap_list(vec![working])),
        kloudlite_workspaces::kube_test::get(VOL_WS1, owned_volume("ws-1", "ws-uid-1")),
        Route { method: "PATCH", path: VOL_WS1.into(), status: 200, body: owned_volume("ws-1", "ws-uid-1") },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", VOL_WS1).len(), 1, "an uncut snapshot still keeps the Volume: {:?}", rec.calls());
    assert!(!rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/ws-1-dddddddd")), "a snapshot record is never deleted");
}

/// A snapshot of a SIBLING worktree on the same volume keeps it alive too: the bytes live on the
/// same subvolume tree, and this parent's own records are all sync points so they all go.
#[tokio::test]
async fn a_snapshot_of_another_worktree_still_detaches_the_volume() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let routes = vec![
        kloudlite_workspaces::kube_test::get(
            SNAPS,
            snap_list(vec![snapshot_record("ws-1-cccccccc", "ws-1", "ws-other"), ready_transient("sync-ws-1-aaaa", "ws-1", "ws-1")]),
        ),
        Route { method: "DELETE", path: format!("{SNAPS}/sync-ws-1-aaaa"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        kloudlite_workspaces::kube_test::get(VOL_WS1, owned_volume("ws-1", "ws-uid-1")),
        Route { method: "PATCH", path: VOL_WS1.into(), status: 200, body: owned_volume("ws-1", "ws-uid-1") },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", VOL_WS1).len(), 1, "detached for the sibling's snapshot: {:?}", rec.calls());
    assert!(
        !rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/ws-1-cccccccc")),
        "another worktree's records are not this parent's to delete: {:?}",
        rec.calls()
    );
}

/// A migration baseline an OLDER build wrote as an ordinary record is not a snapshot: nobody asked
/// for it, and treating it as one would keep its Volume — and the pre-model volume's whole
/// subvolume tree — alive forever after the workspace was deleted. It goes with the working copy.
#[tokio::test]
async fn deleting_a_workspace_with_only_a_legacy_baseline_deletes_it_and_leaves_the_volume_to_gc() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let mut baseline = snapshot_record("ws-1-aaaaaaaa", "ws-1", "ws-1");
    baseline["spec"]["message"] = serde_json::json!("migration baseline");
    let routes = vec![
        kloudlite_workspaces::kube_test::get(SNAPS, snap_list(vec![baseline])),
        Route { method: "DELETE", path: format!("{SNAPS}/ws-1-aaaaaaaa"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/ws-1-aaaaaaaa")), "the baseline goes: {:?}", rec.calls());
    assert!(rec.sent("PATCH", VOL_WS1).is_empty(), "a baseline never detaches the Volume: {:?}", rec.calls());
}

/// No snapshot on the Volume: the ownerReference stays and ownerReference GC deletes the Volume with
/// its parent, exactly as before this existed.
#[tokio::test]
async fn deleting_a_workspace_without_a_snapshot_leaves_the_volume_to_gc() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let routes = vec![
        kloudlite_workspaces::kube_test::get(SNAPS, snap_list(vec![ready_transient("sync-ws-1-aaaa", "ws-1", "ws-1")])),
        Route { method: "DELETE", path: format!("{SNAPS}/sync-ws-1-aaaa"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert!(rec.sent("PATCH", VOL_WS1).is_empty(), "ownerReference kept so GC deletes the Volume: {:?}", rec.calls());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/sync-ws-1-aaaa")), "the transient still goes");
}

pub(crate) fn env_json(status: serde_json::Value) -> serde_json::Value {
    let mut o = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1",
        "kind": "Environment",
        "metadata": {"name": "env-1", "uid": "env-uid-1", "generation": 1, "resourceVersion": "7"},
        "spec": {"owner": "acme", "name": "staging", "region": "r1", "services": [],
                 "storage": {"quotaGb": 20}, "desiredState": "running"},
    });
    if status != serde_json::json!({}) {
        o["status"] = status;
    }
    o
}

pub(crate) fn environment(status: serde_json::Value) -> crd::Environment {
    serde_json::from_value(env_json(status)).unwrap()
}

/// The environment twin of `deleting_a_workspace_with_a_snapshot_detaches_its_volume`: same
/// rule, same cleanup, and the worktree is the environment's own id.
#[tokio::test]
async fn deleting_an_environment_with_a_snapshot_detaches_its_volume() {
    const ENV_OBJ: &str = "/apis/kloudlite.io/v1alpha1/environments/env-1";
    const VOL_ENV1: &str = "/apis/kloudlite.io/v1alpha1/volumes/env-1";
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let owner_ref = serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
                                       "name": "env-1", "uid": "env-uid-1", "controller": true});
    let vol = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "env-1", "uid": "vol-uid", "ownerReferences": [owner_ref]},
        "spec": {"owner": "acme", "nodeName": "node-a", "region": "r1", "quotaGb": 10}});
    let routes = vec![
        kloudlite_workspaces::kube_test::get(
            SNAPS,
            snap_list(vec![snapshot_record("env-1-aaaaaaaa", "env-1", "env-1"), ready_transient("sync-env-1-aaaa", "env-1", "env-1")]),
        ),
        Route { method: "DELETE", path: format!("{SNAPS}/sync-env-1-aaaa"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        kloudlite_workspaces::kube_test::get(VOL_ENV1, vol.clone()),
        Route { method: "PATCH", path: VOL_ENV1.into(), status: 200, body: vol },
        Route { method: "PATCH", path: ENV_OBJ.into(), status: 200, body: env_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "env-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let mut e = environment(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "env-1"}));
    e.metadata.finalizers = Some(vec![crd::WORKTREE_FINALIZER.to_string()]);
    e.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));

    kloudlite_agent::controller::reconcile_environment(Arc::new(e), ctx).await.unwrap();

    let patches = rec.sent("PATCH", VOL_ENV1);
    assert_eq!(patches.len(), 1, "the detach patch: {:?}", rec.calls());
    assert_eq!(patches[0][0]["op"], "test");
    assert_eq!(patches[0][0]["path"], "/metadata/ownerReferences");
    assert_eq!(patches[0][0]["value"][0]["uid"], "env-uid-1");
    assert_eq!(patches[0][1]["op"], "replace");
    assert_eq!(patches[0][1]["value"], serde_json::json!([]));
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/sync-env-1-aaaa")), "the transient goes");
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE /apis/kloudlite.io/v1alpha1/volumes/")), "the Volume itself is never deleted");
}

/// The environment twin of `deleting_a_workspace_without_a_snapshot_leaves_the_volume_to_gc`.
#[tokio::test]
async fn deleting_an_environment_without_a_snapshot_leaves_the_volume_to_gc() {
    const ENV_OBJ: &str = "/apis/kloudlite.io/v1alpha1/environments/env-1";
    const VOL_ENV1: &str = "/apis/kloudlite.io/v1alpha1/volumes/env-1";
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let routes = vec![
        kloudlite_workspaces::kube_test::get(SNAPS, snap_list(vec![ready_transient("sync-env-1-aaaa", "env-1", "env-1")])),
        Route { method: "DELETE", path: format!("{SNAPS}/sync-env-1-aaaa"), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "PATCH", path: ENV_OBJ.into(), status: 200, body: env_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "env-1"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let mut e = environment(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "env-1"}));
    e.metadata.finalizers = Some(vec![crd::WORKTREE_FINALIZER.to_string()]);
    e.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));

    kloudlite_agent::controller::reconcile_environment(Arc::new(e), ctx).await.unwrap();

    assert!(rec.sent("PATCH", VOL_ENV1).is_empty(), "ownerReference kept so GC deletes the Volume: {:?}", rec.calls());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {SNAPS}/sync-env-1-aaaa")), "the transient still goes");
}

/// The environment claim is the workspace claim with a different opening phase — an environment has
/// containers to bring up before it is `running`, so it is `creating`, never `pending`.
#[tokio::test]
async fn an_unplaced_environment_is_claimed_as_creating() {
    const ENV_STATUS: &str = "/apis/kloudlite.io/v1alpha1/environments/env-1/status";
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: ENV_STATUS.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::post(
                BINDINGS,
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-acme"},
                                   "spec": {"owner": "acme", "region": "r1", "nodeName": "node-a"}}),
            ),
        ],
    );

    kloudlite_agent::claim::claim_environment(&environment(serde_json::json!({})), &ctx).await.unwrap();
    let sent = rec.sent("PUT", ENV_STATUS);
    assert_eq!(sent.len(), 1, "exactly one status write");
    assert_eq!(sent[0]["status"]["phase"], "creating");
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
    assert_eq!(sent[0]["metadata"]["resourceVersion"], "7", "the claim races or it is not a claim: {}", sent[0]);
    assert!(rec.calls().iter().any(|c| c == &format!("POST {BINDINGS}")), "the winner binds the owner");

}

pub(crate) fn binding_status() -> String {
    format!("/apis/kloudlite.io/v1alpha1/ownerbindings/{}/status", crd::binding_name("r1", "alice"))
}

pub(crate) fn ws_in_team(team: &str, node: &str) -> serde_json::Value {
    let mut o = ws_json(serde_json::json!({"phase": "ready", "nodeName": node}));
    o["spec"]["team"] = serde_json::json!(team);
    o
}

/// No `Quota` object for this owner/team and none for its kind's default either: `quota::effective`
/// falls all the way through to the compiled-in table, which is what most binding tests want —
/// they are not testing quota sizing, and this is the fallback that exercises without needing one.
pub(crate) fn quota_fallback_routes(name: &str, team: bool) -> Vec<Route> {
    vec![
        kloudlite_workspaces::kube_test::not_found(format!("/apis/kloudlite.io/v1alpha1/quotas/{name}")),
        kloudlite_workspaces::kube_test::not_found(format!(
            "/apis/kloudlite.io/v1alpha1/quotas/{}",
            if team { "default-team" } else { "default-user" }
        )),
    ]
}

/// Every object the binding ensures in one namespace, answered with itself.
pub(crate) fn ns_routes(ns: &str) -> Vec<Route> {
    let ok = |path: String, api: &str, kind: &str| Route {
        method: "PATCH",
        path,
        status: 200,
        body: serde_json::json!({"apiVersion": api, "kind": kind, "metadata": {"name": "x"}}),
    };
    let mut r = vec![
        ok(format!("/api/v1/namespaces/{ns}"), "v1", "Namespace"),
        ok(format!("/api/v1/namespaces/{ns}/limitranges/slot"), "v1", "LimitRange"),
        ok(format!("/api/v1/namespaces/{ns}/resourcequotas/owner-quota"), "v1", "ResourceQuota"),
        ok(
            format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings/api-secrets"),
            "rbac.authorization.k8s.io/v1",
            "RoleBinding",
        ),
        // The agent's own per-namespace host-key grant, in place of `secrets` cluster-wide.
        ok(
            format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings/agent-secrets"),
            "rbac.authorization.k8s.io/v1",
            "RoleBinding",
        ),
    ];
    for p in ["default-deny", "allow-dns", "allow-same-namespace", "allow-internet-egress", "allow-gateway-ssh", "allow-builder-gate"] {
        r.push(ok(
            format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/{p}"),
            "networking.k8s.io/v1",
            "NetworkPolicy",
        ));
    }
    r
}

pub(crate) fn binding_json() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
        "metadata": {"name": crd::binding_name("r1", "alice"), "uid": "ob-uid-1", "generation": 1},
        "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"}
    })
}

/// The per-owner shared objects have exactly ONE owner now. They used to be re-ensured by the
/// workspace reconciler and the environment reconciler on every pass, which is two writers for one
/// object and a namespace deleted by whichever ran last.
#[tokio::test]
async fn a_binding_ensures_one_namespace_per_team_in_use_and_reports_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        // A team workspace here, and one on ANOTHER node: the second must not make this node build
        // a namespace it does not host.
        "items": [
            ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"})),
            ws_in_team("acme", "node-a"),
            ws_in_team("elsewhere", "node-b"),
        ]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
        ]
        .into_iter()
        .chain(quota_fallback_routes("alice", false))
        .chain(quota_fallback_routes("acme", true))
        .chain(ns_routes("ws-alice"))
        .chain(ns_routes(&crd::ws_namespace("alice", "acme")))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    kloudlite_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == "PATCH /api/v1/namespaces/ws-alice"), "{:?}", rec.calls());
    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice");
    assert!(
        sent[0]["metadata"].get("ownerReferences").is_none(),
        "a namespace shared by every workspace this user owns must never be GC'd with one binding: {}", sent[0]
    );
    let limit = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/limitranges/slot");
    assert!(limit[0]["metadata"].get("ownerReferences").is_none(), "a quota ceiling must not vanish with a binding rewrite");
    // Everything else the binding vouches for IS owned by it, so a re-homed owner does not strand
    // a grant on the old node.
    let rb = rec.sent("PATCH", "/apis/rbac.authorization.k8s.io/v1/namespaces/ws-alice/rolebindings/api-secrets");
    assert_eq!(rb[0]["metadata"]["ownerReferences"][0]["kind"], "OwnerBinding", "{}", rb[0]);
    let acme = crd::ws_namespace("alice", "acme");
    assert!(rec.calls().iter().any(|c| *c == format!("PATCH /api/v1/namespaces/{acme}")), "{:?}", rec.calls());
    let stranded = crd::ws_namespace("alice", "elsewhere");
    assert!(
        !rec.calls().iter().any(|c| *c == format!("PATCH /api/v1/namespaces/{stranded}")),
        "a workspace on another node must not make namespaces here: {:?}", rec.calls()
    );
    let st = rec.sent("PATCH", &binding_status());
    assert_eq!(st.len(), 1);
    assert!(
        st[0]["status"]["conditions"].as_array().unwrap().iter()
            .any(|c| c["type"] == "NamespaceReady" && c["status"] == "True"),
        "{}", st[0]
    );
}

/// The hot loop this design has to not have: `crd::condition` stamps `lastTransitionTime` with
/// `now`, so a status write on every pass is new bytes, which fires this controller's own watch,
/// which writes again — forever, on an object nothing asked to change.
#[tokio::test]
async fn a_second_reconcile_of_a_ready_binding_writes_no_status() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list)]
            .into_iter()
                .chain(quota_fallback_routes("alice", false))
                .chain(ns_routes("ws-alice"))
            .collect(),
    );
    // What the FIRST reconcile left behind, with an older `lastTransitionTime` than `now`.
    let mut b = binding_json();
    b["status"] = serde_json::json!({
        "observedGeneration": 1,
        "conditions": [{"type": "NamespaceReady", "status": "True", "reason": "Converged",
                        "message": "namespaces exist on this node", "observedGeneration": 1,
                        "lastTransitionTime": "2020-01-01T00:00:00Z"}],
    });
    let b: crd::OwnerBinding = serde_json::from_value(b).unwrap();

    kloudlite_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    assert!(
        rec.sent("PATCH", &binding_status()).is_empty(),
        "a status re-stamped with `now` is not a change: {:?}", rec.calls()
    );
}

/// The owner's ceiling is projected into their namespace on every binding pass, so a raise takes
/// effect without a roll and a namespace made before quotas existed gets one on its next reconcile.
#[tokio::test]
async fn a_binding_pass_writes_the_owners_resource_quota() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let quota = kloudlite_workspaces::kube_test::get(
        "/apis/kloudlite.io/v1alpha1/quotas/alice",
        serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "alice"},
            "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 12, "memoryGb": 48}
        }),
    );
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            quota,
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    kloudlite_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/resourcequotas/owner-quota");
    assert!(!sent.is_empty(), "{:?}", rec.calls());
    assert_eq!(sent[0]["spec"]["hard"]["limits.cpu"], "12");
    assert_eq!(sent[0]["spec"]["hard"]["limits.memory"], "48Gi");
}

/// `run.rs` wakes every `OwnerBinding` on any `Quota` write (`all_in_store`, same pattern as the
/// Node watch) rather than trying to map a `Quota`'s name back to the hashed binding name it does
/// not appear in — this is the reconcile that wake reaches: a raised `Quota` re-read on the very
/// next binding pass, with no roll and no wait for an unrelated event to happen to touch it.
#[tokio::test]
async fn a_quota_change_re_stamps_the_resource_quota_on_the_next_binding_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let raised_quota = kloudlite_workspaces::kube_test::get(
        "/apis/kloudlite.io/v1alpha1/quotas/alice",
        serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "alice"},
            "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 16, "memoryGb": 64}
        }),
    );
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            raised_quota,
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    // The event this test proves the reconcile side of: `run.rs`'s Quota watch requeues this same
    // binding, and the requeued pass is exactly another `apply_binding` call — nothing about the
    // binding itself changed, only the `Quota` object the mock now answers with the raised numbers.
    kloudlite_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/resourcequotas/owner-quota");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["spec"]["hard"]["limits.cpu"], "16", "the raised number, not the old one: {sent:?}");
    assert_eq!(sent[0]["spec"]["hard"]["limits.memory"], "64Gi");
}

pub(crate) fn home_vol_json(quota: u64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "home-alice", "uid": "home-uid-1", "generation": 1,
                     "ownerReferences": [{"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
                                          "name": crd::binding_name("r1", "alice"), "uid": "ob-uid-1",
                                          "controller": true, "blockOwnerDeletion": true}]},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": quota},
        "status": {"phase": "ready", "subvolumePresent": true},
    })
}
