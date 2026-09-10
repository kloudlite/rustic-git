//! Deleting a parent: the worktree finalizer detaches the Volume when a snapshot remains and
//! leaves it to GC otherwise, for workspaces and environments alike.

use super::*;

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
