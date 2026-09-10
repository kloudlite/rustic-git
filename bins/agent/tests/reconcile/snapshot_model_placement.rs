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
