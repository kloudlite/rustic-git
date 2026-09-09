//! snapshot-model clone/restore (Task 6b).

use super::*;


/// A `Ready` `Snapshot` of the volume a `cloneOf` names — the precondition `the phase check` checks
/// before ever letting a clone check out. Kept separate from `snapshot_cr` (worktree/parent don't
/// matter here) so the volume and phase are the only things a test has to vary.
pub(crate) fn ready_snapshot(name: &str, volume: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name, "uid": "snapshot-uid"},
        "spec": {"volume": volume, "owner": "alice", "worktree": volume, "parent": ""},
        "status": {"phase": "ready"},
    })
}

/// `check_source` proves the clone SOURCE object exists (Workspace, then Environment) before
/// anything else — independent of, and ahead of, the volume-level checks below.
pub(crate) fn source_workspace_exists(id: &str) -> Route {
    kloudlite_workspaces::kube_test::get(
        format!("/apis/kloudlite.io/v1alpha1/workspaces/{id}"),
        serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
                           "metadata": {"name": id},
                           "spec": {"owner": "alice", "team": "", "name": id, "region": "r1",
                                    "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"},
                           "status": {"phase": "ready", "nodeName": "node-a"}}),
    )
}

pub(crate) fn ready_source_volume(id: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": id, "uid": "src-vol-uid"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true},
    })
}

/// A workspace whose `cloneOf` carries a graft snapshot and no worktree yet — a fresh clone.
pub(crate) fn cloned_workspace(snapshot: &str, head: Option<&str>) -> crd::Workspace {
    let mut status = serde_json::json!({"phase": "creating", "nodeName": "node-a"});
    if let Some(h) = head {
        status["head"] = serde_json::json!(h);
    }
    let mut w = workspace(status);
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into(), commit: Some(snapshot.into()) }),
    });
    w
}

/// A clone with no head of its own yet checks out the GRAFTED snapshot (never bootstraps empty next
/// to the source's real history) and records it as its own `head` on the very first pass — the
/// same preserve-pattern write `snapshot::advance_head` uses for a push, so retention's
/// `worktree_heads` sees it from here on. `resolve_volume` also proves clone PLACEMENT here: the
/// SOURCE's volume (`ws-src`), not a freshly created child, is what gets read — the route list has
/// no `POST /volumes` at all, so `ensure_child_volume` was never called.
///
/// IMPLICITLY GATED: pre-created directories stand in for subvolumes, so no `btrfs` binary is
/// invoked. A change that makes the engine actually shell out will pass here and fail on a node.
#[tokio::test]
async fn snapshot_model_clone_checks_out_its_graft_snapshot_and_records_it_as_head() {
    let tmp = tempfile::tempdir().unwrap();
    // The worktree name is the WORKSPACE's own id, on the SOURCE volume's snap tree — never
    // `vol/ws-1/...`, which would be a fresh (and wrong) child volume.
    std::fs::create_dir_all(tmp.path().join("vol/ws-src/live/ws-1")).unwrap();
    let routes = vec![
        source_workspace_exists("ws-src"),
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-src", ready_source_volume("ws-src")),
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/ws-src".into(), status: 200, body: ready_source_volume("ws-src") },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/snapshots/ws-src-aaaaaaaa", ready_snapshot("ws-src-aaaaaaaa", "ws-src")),
        ready_binding(),
        ready_namespace(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = cloned_workspace("ws-src-aaaaaaaa", None);

    // The pass runs past the checkout arm and then fails on the next unmocked route (namespace,
    // profile, ...) — not the point here; the head write already landed by then.
    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await;

    let sent = rec.sent("PATCH", WS_STATUS);
    assert!(
        sent.iter().any(|s| s["status"]["head"] == "ws-src-aaaaaaaa"),
        "the graft snapshot must be recorded as this clone's own head: {sent:?}"
    );
    assert!(!rec.calls().iter().any(|c| c.contains("POST") && c.contains("/volumes")), "a shared-volume clone creates no child Volume");
}

/// An environment RESTORED onto a snapshot (`/v1`'s `CloneOf { volume, commit }`) records that
/// snapshot as its own `status.head` instead of parking forever in `HeadUnknown` — the live repro of
/// 2026-09-03. The workspace twin is
/// `snapshot_model_clone_checks_out_its_graft_snapshot_and_records_it_as_head`.
#[tokio::test]
async fn a_restored_environment_records_its_graft_snapshot_as_head() {
    let tmp = tempfile::tempdir().unwrap();
    // The environment's OWN worktree of the SOURCE's volume — `live/env-1`, never `live/env-src`,
    // which is the source environment's live subvolume. It exists already so `checkout` converges
    // on WORKTREE_EXISTS instead of shelling out to btrfs, the same trick the workspace twin uses.
    std::fs::create_dir_all(tmp.path().join("vol/env-src/live/env-1")).unwrap();
    // The source's own worktree, which this pass must not touch.
    std::fs::create_dir_all(tmp.path().join("vol/env-src/live/env-src/marker")).unwrap();
    let (ctx, rec) = ctx(tmp.path(), restored_env_routes(ready_snapshot("env-src-aaaa", "env-src")));

    // Runs past the checkout arm and then fails on the next unmocked route — the head write has
    // already landed by then, same as the workspace twin.
    let _ = kloudlite_agent::controller::apply_environment(&restored_env(), &ctx).await;

    let sent = rec.sent("PATCH", ENV_STATUS_PATH);
    assert!(
        sent.iter().any(|s| s["status"]["head"] == "env-src-aaaa"),
        "the graft snapshot must be recorded as this environment's head: {sent:?}"
    );
    assert!(
        !sent.iter().any(|s| s["status"]["conditions"][0]["reason"] == "HeadUnknown"),
        "never HeadUnknown: the head is known from the spec: {sent:?}"
    );
    // Design rule 6, the environment twin: the restore becomes an owner of the source's Volume.
    let attach = rec.sent("PATCH", "/apis/kloudlite.io/v1alpha1/volumes/env-src");
    assert_eq!(attach.len(), 1, "one attach patch: {:?}", rec.calls());
    assert_eq!(attach[0][0]["value"][0]["uid"], "env-uid-1");
    assert_eq!(attach[0][0]["value"][0]["kind"], "Environment");
    // Task 2c: the restore holds its own worktree of the source's volume. Nothing it does may
    // reach the SOURCE's live subvolume — two environments writing one is the bug this proves gone.
    assert!(tmp.path().join("vol/env-src/live/env-src/marker").exists(), "the source's live worktree was touched");
    // `mkdir_env_mounts`' root is the worktree the pod mounts, not `live/` one level above it — a
    // folder made there is invisible to every service's subPath.
    assert!(
        tmp.path().join("vol/env-src/live/env-1/volumes/dbdata").is_dir(),
        "the declared mount folder must be made inside this environment's own worktree: {:?}", rec.calls()
    );
    assert!(!tmp.path().join("vol/env-src/live/volumes").exists(), "nothing is made above the worktree");
    // Its StatefulSets and Services go in ITS namespace, not the source volume's.
    assert!(
        rec.calls().iter().all(|c| !c.contains(&format!("/namespaces/{}/", crd::env_namespace("env-src")))),
        "wrote into the source environment's namespace: {:?}",
        rec.calls()
    );
}

/// The cut `/v1` made microseconds before the object is almost always still `Working` on the first
/// reconcile: `Creating` + `Ready=False/SnapshotPending` and a requeue, never a checkout and never a
/// permanent settle.
#[tokio::test]
async fn a_restored_environment_waits_while_its_snapshot_is_still_working() {
    let tmp = tempfile::tempdir().unwrap();
    let mut working = ready_snapshot("env-src-aaaa", "env-src");
    working["status"]["phase"] = serde_json::json!("working");
    let (ctx, rec) = ctx(tmp.path(), restored_env_routes(working));

    let action = kloudlite_agent::controller::apply_environment(&restored_env(), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)), "requeued, not settled");

    let last = rec.sent("PATCH", ENV_STATUS_PATH).pop().expect("a status write");
    assert_eq!(last["status"]["phase"], "creating");
    assert_eq!(last["status"]["conditions"][0]["reason"], "SnapshotPending");
    assert!(last["status"]["head"].is_null(), "no head recorded while the snapshot is uncut: {last}");
}

/// An environment whose `cloneOf` carries a graft snapshot — what `POST /v1/environments/restore`
/// writes — claimed on this node, with no head of its own yet.
pub(crate) fn restored_env() -> crd::Environment {
    let mut e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    // One declared mount, so the pass exercises `mkdir_env_mounts` — whose root must be the
    // worktree the pod mounts, not `live/` one level above it.
    e.spec.services = vec![kloudlite_workspaces::model::Service {
        name: "db".into(),
        image: "mongo".into(),
        mounts: vec![kloudlite_workspaces::model::Mount { folder: "dbdata".into(), path: "/data/db".into() }],
        command: vec![],
        env: Default::default(),
        ports: vec![],
        resources: None,
    }];
    e.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "env-src".into(), commit: Some("env-src-aaaa".into()) }),
    });
    e
}

/// `check_source` (Workspace 404 then Environment), the SOURCE's Volume, and the graft snapshot in
/// whatever state the test wants it.
pub(crate) fn restored_env_routes(snapshot: serde_json::Value) -> Vec<Route> {
    vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/workspaces/env-src"),
        kloudlite_workspaces::kube_test::get(
            "/apis/kloudlite.io/v1alpha1/environments/env-src",
            serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
                               "metadata": {"name": "env-src"},
                               "spec": {"owner": "acme", "name": "src", "region": "r1", "services": [],
                                        "storage": {"quotaGb": 20}, "desiredState": "running"}}),
        ),
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-src", ready_source_volume("env-src")),
        // The restore's own attach: it becomes an OWNER of the source's Volume (design rule 6).
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/env-src".into(), status: 200, body: ready_source_volume("env-src") },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/snapshots/env-src-aaaa", snapshot),
        Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        // The children `run_environment` applies before it makes the mount folders. Named for the
        // environment's OWN namespace: writing into `env-src`'s is the collision Task 2c removed.
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "Namespace"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/default-deny", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-dns", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-internet-egress", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-same-namespace", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{}/rolebindings/api-secrets", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "RoleBinding"}) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}/limitranges/slot", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "LimitRange"}) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}/resourcequotas/owner-quota", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "ResourceQuota"}) },
    ]
}

/// A clone naming a snapshot that is not a `Ready` `Snapshot` of its source volume — swept by
/// retention, or simply never existed — settles PERMANENTLY with its own reason, distinct from a
/// bad clone SOURCE (`NoSuchSource`, settled earlier by `check_source`): retrying at TICK would
/// spin on the same missing snapshot forever.
#[tokio::test]
async fn snapshot_model_clone_with_a_missing_snapshot_settles_as_no_such_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        source_workspace_exists("ws-src"),
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-src", ready_source_volume("ws-src")),
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/ws-src".into(), status: 200, body: ready_source_volume("ws-src") },
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/snapshots/ws-src-gone"),
        ready_binding(),
        ready_namespace(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = cloned_workspace("ws-src-gone", None);

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "settled permanently, not requeued");

    let sent = rec.sent("PATCH", WS_STATUS);
    let last = sent.last().expect("a status write");
    assert_eq!(last["status"]["phase"], "error");
    let cond = &last["status"]["conditions"][0];
    assert_eq!(cond["reason"], "NoSuchSnapshot");
}

/// The same missing snapshot, but the clone's worktree is already on disk: the cut was consumed
/// at checkout and retention pruned it once the clone was `Ready` (as it should), so a later pass
/// must not re-judge it. The live failure: a clone ran, its source cut a newer sync point, and the
/// clone's next reconcile settled `NoSuchSnapshot` over a worktree that was perfectly fine.
#[tokio::test]
async fn snapshot_model_a_materialised_clone_survives_its_pruned_graft_cut() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/ws-src/live/ws-1")).unwrap();
    let routes = vec![
        source_workspace_exists("ws-src"),
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-src", ready_source_volume("ws-src")),
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/ws-src".into(), status: 200, body: ready_source_volume("ws-src") },
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/snapshots/ws-src-gone"),
        ready_binding(),
        ready_namespace(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = cloned_workspace("ws-src-gone", None);

    // Runs past the checkout arm and then fails on the next unmocked route — the point is what
    // it did NOT write on the way.
    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await;

    let sent = rec.sent("PATCH", WS_STATUS);
    assert!(
        !sent.iter().any(|s| s["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["reason"] == "NoSuchSnapshot"))),
        "a clone with its worktree must not be judged on a cut it no longer needs: {sent:?}"
    );
    assert!(!rec.calls().iter().any(|c| c.contains("snapshots/ws-src-gone")), "the cut is not even asked about: {:?}", rec.calls());
}

/// Restoring a snapshot of a DELETED workspace — the case durable snapshots exist for. No
/// `Workspace` named by `cloneOf.volume` exists any more; the detached `Volume` does, and that is
/// what `check_source` must look at. The live failure was a permanent `NoSuchSource` here.
#[tokio::test]
async fn a_restore_onto_a_detached_volume_is_not_no_such_source() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/ws-src/live/ws-1")).unwrap();
    let routes = vec![
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/workspaces/ws-src"),
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/environments/ws-src"),
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-src", ready_source_volume("ws-src")),
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/ws-src".into(), status: 200, body: ready_source_volume("ws-src") },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/snapshots/ws-src-aaaaaaaa", ready_snapshot("ws-src-aaaaaaaa", "ws-src")),
        ready_binding(),
        ready_namespace(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = cloned_workspace("ws-src-aaaaaaaa", None);

    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await;

    let sent = rec.sent("PATCH", WS_STATUS);
    assert!(
        !sent.iter().any(|s| s["status"]["conditions"][0]["reason"] == "NoSuchSource"),
        "a deleted source workspace must not settle a restore: {sent:?}"
    );
    // The shared-worktree arm ran: the graft snapshot became this clone's own head.
    assert!(sent.iter().any(|s| s["status"]["head"] == "ws-src-aaaaaaaa"), "the shared-arm path must run: {sent:?}");
}

/// The other half: a restore naming a `Volume` that is really gone stays permanently wrong.
#[tokio::test]
async fn a_restore_whose_volume_is_gone_settles_as_no_such_source() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/volumes/ws-src"),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = cloned_workspace("ws-src-aaaaaaaa", None);

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "settled permanently, not requeued");
    let sent = rec.sent("PATCH", WS_STATUS);
    assert_eq!(sent.last().expect("a status write")["status"]["conditions"][0]["reason"], "NoSuchSource");
}

/// A LIVE clone (`cloneOf` with no snapshot) copies from the source's live worktree, so its source
/// parent must still exist — unchanged by the restore carve-out above.
#[tokio::test]
async fn a_live_clone_of_a_deleted_workspace_still_settles_as_no_such_source() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/workspaces/ws-src"),
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/environments/ws-src"),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let mut w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into(), commit: None }),
    });

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "settled permanently, not requeued");
    let sent = rec.sent("PATCH", WS_STATUS);
    assert_eq!(sent.last().expect("a status write")["status"]["conditions"][0]["reason"], "NoSuchSource");
}

/// The same deleted source, but this clone's OWN subvolume is already on disk: the bytes were
/// copied at materialize and the source is never read again, so deleting the source workspace must
/// not settle a healthy clone. The twin of the pruned-graft-cut case of 2026-09-08.
#[tokio::test]
async fn a_materialised_live_clone_survives_its_deleted_source() {
    let tmp = tempfile::tempdir().unwrap();
    // The clone's own volume (`ws-1`, named after the parent) exists: the copy has happened.
    std::fs::create_dir_all(tmp.path().join("vol/ws-1/live")).unwrap();
    let routes = vec![Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) }];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let mut w = workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a"}));
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into(), commit: None }),
    });

    // Runs past `check_source` and then fails on the next unmocked route — the point is what it
    // did NOT write, and that the source was not even asked about.
    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await;

    let sent = rec.sent("PATCH", WS_STATUS);
    assert!(
        !sent.iter().any(|s| s["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["reason"] == "NoSuchSource"))),
        "a clone that already holds its bytes must not be judged on its source: {sent:?}"
    );
    assert!(!rec.calls().iter().any(|c| c.contains("workspaces/ws-src")), "the source is not even asked about: {:?}", rec.calls());
}

/// F6: the interrupted clone. The source's Volume is pinned to the node that DIED, so the
/// shared-worktree path settles `Degraded=NodeMismatch` on whichever peer holds the cut. A
/// `seededFrom` parent instead authors its OWN child Volume on this node — pinned here, carrying
/// the source reference for the materialize step — and never touches the source Volume at all.
#[tokio::test]
async fn a_seeded_clone_creates_its_own_volume_and_leaves_the_dead_owners_pin_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let mut dead_owner = ready_source_volume("ws-src");
    dead_owner["spec"]["nodeName"] = serde_json::json!("node-b");
    let routes = vec![
        source_workspace_exists("ws-src"),
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-src", dead_owner),
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/volumes/ws-1"),
        Route {
            method: "POST",
            path: "/apis/kloudlite.io/v1alpha1/volumes".into(),
            status: 201,
            body: serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
                                     "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
                                     "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20}}),
        },
        ready_binding(),
        ready_namespace(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let mut w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::SeededFrom { volume: "ws-src".into(), snapshot: "sync-ws-src-bbbb".into() }),
    });

    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await;

    let made = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/volumes").remove(0);
    assert_eq!(made["metadata"]["name"], "ws-1", "its own volume, named after itself — not a worktree of ws-src");
    assert_eq!(made["spec"]["nodeName"], "node-a", "pinned HERE: the node that holds the cut, not the dead one");
    assert_eq!(made["spec"]["source"]["seededFrom"]["snapshot"], "sync-ws-src-bbbb");
    // The whole point: the dead node's pin is untouched, so nothing settles NodeMismatch and the
    // source volume is still the unclaim sweep's to release on its own terms.
    assert!(
        !rec.calls().iter().any(|c| c.contains("/volumes/ws-src") && !c.starts_with("GET")),
        "a seeded clone only READS the source volume: {:?}",
        rec.calls()
    );
}

/// A clone that already has its own `head` (it pushed since being grafted) never re-derives it
/// from `cloneOf` — the graft is a ONE-TIME starting point, not a value this pass keeps re-reading.
#[tokio::test]
async fn snapshot_model_clone_with_a_head_of_its_own_does_not_rewrite_it() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/ws-src/live/ws-1")).unwrap();
    let routes = vec![
        source_workspace_exists("ws-src"),
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-src", ready_source_volume("ws-src")),
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/ws-src".into(), status: 200, body: ready_source_volume("ws-src") },
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = cloned_workspace("ws-src-aaaaaaaa", Some("ws-1-own-snapshot"));

    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await;

    // No snapshot GET at all: an already-owned head skips `the phase check`'s validation of the
    // graft snapshot entirely, and no status write ever names the graft snapshot as `head`.
    assert!(!rec.calls().iter().any(|c| c.contains("/snapshots/")), "{:?}", rec.calls());
    assert!(rec.sent("PATCH", WS_STATUS).iter().all(|s| s["status"]["head"] != "ws-src-aaaaaaaa"));
}

/// Restore-in-place never touches the registry (the old `get_history`/`restore` HTTP calls, gone
/// with the object-store subsystem) — the checkout-swap (`Engine::swap_worktree`) is entirely
/// local. Real btrfs is unavailable in this test environment, so the swap itself errors past the
/// point this asserts; the point is that nothing here ever reaches for a network call.
#[tokio::test]
async fn snapshot_model_restore_in_place_never_calls_the_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "env-1", "uid": "vol-uid-1", "generation": 2},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20,
                 "restoreTo": {"snapshotId": "env-1-bbbbbbbb", "volume": "env-1", "requestedAt": "2026-09-01T00:00:00Z"}},
        "status": {"phase": "ready", "subvolumePresent": true},
    });
    let routes = vec![
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/env-1/status".into(), status: 200, body: vol.clone() },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let v: crd::Volume = serde_json::from_value(vol).unwrap();

    let _ = kloudlite_agent::controller::apply_volume(&v, &ctx).await;
    wait_idle(&ctx).await;

    assert!(
        !rec.calls().iter().any(|c| c.contains("get_history") || c.contains("registry")),
        "snapshot-model restore must never fetch from the registry: {:?}", rec.calls()
    );
}

/// The sync beat is keep-biased about not knowing: `Engine::generation` shells out to `btrfs
/// subvolume show`, which cannot work here, so this asserts the beat WARNS AND CREATES NOTHING on
/// a generation error — cutting on "we do not know" would cut a redundant sync point every single
/// pass. The decision itself (has the generation moved?) is a pure function tested in `sync.rs`;
/// no fake-`generation` seam is worth carrying for a second test of it.
///
/// IMPLICITLY GATED: pre-created directories stand in for subvolumes, so no `btrfs` binary is
/// invoked. A change that makes the engine actually shell out will pass here and fail on a node.
#[tokio::test]
async fn the_sync_beat_cuts_a_transient_only_when_the_worktree_generation_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a",
                                             "volumeRef": "vol-1", "podRef": "ws-1"}))]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            kloudlite_workspaces::kube_test::get(
                "/apis/kloudlite.io/v1alpha1/environments",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "EnvironmentList",
                                   "metadata": {}, "items": []}),
            ),
        ],
    );

    kloudlite_agent::sync::sync_beat(&ctx).await;

    assert!(
        rec.calls().iter().any(|c| c == "GET /apis/kloudlite.io/v1alpha1/snapshots"),
        "the beat must look for this worktree's existing sync point: {:?}", rec.calls()
    );
    assert!(
        rec.sent("POST", SNAPSHOTS_LIST).is_empty(),
        "an unreadable generation must cut nothing: {:?}", rec.sent("POST", SNAPSHOTS_LIST)
    );
}

/// The owner guard: `spec.owner` becomes `{pool}/homes/{owner}` and is chowned by a privileged
/// process, so a traversing owner must settle Permanent before `ensure_shared_home` runs — not be
/// caught by accident because `heal_labels` patches the same string as a label first.
#[tokio::test]
async fn a_workspace_with_a_traversing_owner_settles_permanent_and_makes_no_directory() {
    let tmp = tempfile::tempdir().unwrap();
    // `patch_ok` answers with a Volume; the status write here deserializes as a Workspace.
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) }],
    );
    let w: crd::Workspace = serde_json::from_value(serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-1", "uid": "ws-uid", "generation": 1},
        "spec": {"owner": "../../etc", "team": "", "name": "ws", "region": "r1",
                 "image": "", "packages": [], "desiredState": "running"},
        "status": {"phase": "ready", "nodeName": "node-a",
                   "volumeRef": "vol-1", "conditions": []},
    }))
    .unwrap();

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent, never retried");
    let sent = rec.sent("PATCH", WS_STATUS);
    let reason = sent.last().expect("a status write")["status"]["conditions"][0]["reason"].clone();
    assert_eq!(reason, "InvalidSpec");
    assert!(!tmp.path().join("homes").exists(), "nothing under the pool root was created");
    assert!(rec.calls().iter().all(|c| !c.starts_with("POST")), "nothing was created: {:?}", rec.calls());
    // `patch_status` is a forced apply, so an omitted field is pruned — placement must survive.
    let st = &sent.last().unwrap()["status"];
    assert_eq!(st["nodeName"], "node-a");
    assert_eq!(st["volumeRef"], "vol-1");
}

/// `write_attach_label` patches `spec.attachedEnvironment` verbatim into a label value — unchecked,
/// a hostile value 422s that patch and wedges the reconcile forever with no Permanent settle.
#[tokio::test]
async fn a_workspace_with_a_traversing_attached_environment_settles_permanent() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) }],
    );
    let w: crd::Workspace = serde_json::from_value(serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-1", "uid": "ws-uid", "generation": 1},
        "spec": {"owner": "alice", "team": "", "name": "ws", "region": "r1",
                 "image": "", "packages": [], "desiredState": "running", "attachedEnvironment": "../evil"},
        "status": {"phase": "ready", "nodeName": "node-a",
                   "volumeRef": "vol-1", "conditions": []},
    }))
    .unwrap();

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent, never retried");
    let sent = rec.sent("PATCH", WS_STATUS);
    let reason = sent.last().expect("a status write")["status"]["conditions"][0]["reason"].clone();
    assert_eq!(reason, "InvalidSpec");
    assert!(
        rec.calls().iter().all(|c| c == &format!("PATCH {WS_STATUS}") || !c.starts_with("PATCH") && !c.starts_with("POST")),
        "no POST/PATCH beyond the status write: {:?}", rec.calls()
    );
}

/// The same guard on an Environment, whose `spec.owner` reaches `{pool}/homecache/{owner}`.
#[tokio::test]
async fn an_environment_with_a_traversing_owner_settles_permanent_and_keeps_its_placement() {
    const ENV_STATUS: &str = "/apis/kloudlite.io/v1alpha1/environments/env-1/status";
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![Route { method: "PATCH", path: ENV_STATUS.into(), status: 200, body: env_json(serde_json::json!({})) }],
    );
    let mut json = env_json(serde_json::json!({
        "phase": "ready", "nodeName": "node-a", "volumeRef": "vol-1",
        "serviceStatus": [], "conditions": []
    }));
    json["spec"]["owner"] = serde_json::json!("../../etc");
    let e: crd::Environment = serde_json::from_value(json).unwrap();

    let action = kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent, never retried");
    let sent = rec.sent("PATCH", ENV_STATUS);
    let st = &sent.last().expect("a status write")["status"];
    assert_eq!(st["conditions"][0]["reason"], "InvalidSpec");
    assert_eq!(st["nodeName"], "node-a");
    assert_eq!(st["volumeRef"], "vol-1");
    assert!(rec.calls().iter().all(|c| !c.starts_with("POST")), "nothing was created: {:?}", rec.calls());
    assert!(!tmp.path().join("homecache").exists(), "nothing under the pool root was created");
}


/// The gate is a per-NODE fact read off a cluster-scoped condition: node A's pass sets
/// `NamespaceReady=True` after creating the namespaces ITS workspaces need, and a workspace in a
/// team this node has never seen would sail past it into a 60 s `ensure_ssh` retry. The namespace
/// itself is what must answer.
#[tokio::test]
async fn the_namespace_gate_asks_about_this_workspace_s_own_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let binding = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
        "metadata": {"name": "r1-alice", "uid": "b-uid", "generation": 1},
        "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"},
        "status": {"observedGeneration": 1,
                   "conditions": [{"type": "NamespaceReady", "status": "True", "reason": "Converged",
                                   "message": "", "lastTransitionTime": "2000-01-01T00:00:00Z"}]},
    });
    let routes = vec![
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/ownerbindings/r1-alice", binding),
        kloudlite_workspaces::kube_test::not_found(format!(
            "/api/v1/namespaces/{}",
            kloudlite_workspaces::crd::ws_namespace("alice", "eng")
        )),
    ];
    let (ctx, _rec) = ctx(tmp.path(), routes);

    assert!(!kloudlite_agent::binding::namespace_ready(&ctx, "r1", "alice", "eng").await.unwrap(),
            "a True condition from another node must not pass a namespace this node has not made");
}

/// The pod bind-mounts this file BY INODE (`type: File`, no subPath), so a rewrite that replaces
/// the inode — `rename(2)`, the usual way to write a file atomically — leaves every running pod
/// reading the old one and attachment silently stops working. Verified on a live cluster; this
/// test is what stops someone "fixing" it into an atomic write.
#[test]
fn rewriting_a_resolv_conf_keeps_the_same_inode() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().to_string_lossy().to_string();
    // The template the agent reads is its own `/etc/resolv.conf`; skip where that is unreadable.
    if std::fs::read_to_string("/etc/resolv.conf").is_err() {
        return;
    }

    kloudlite_agent::controller::write_resolv_conf(&pool, "ws-1", "ws-alice", None).unwrap();
    let path = kloudlite_workspaces::k8s::attach_file(&pool, "ws-1");
    let before = std::fs::metadata(&path).unwrap().ino();

    kloudlite_agent::controller::write_resolv_conf(&pool, "ws-1", "ws-alice", Some("env-abc")).unwrap();
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(before, after.ino(), "the file was replaced, not truncated: every running pod now reads the old inode");
    assert!(std::fs::read_to_string(&path).unwrap().contains("env-abc"), "and the new content did land");
}

/// A pre-migration pod mounted this path with a `subPath`, which kubernetes created as a
/// DIRECTORY. A node upgraded from that shape must clear it rather than leave the workspace with
/// no DNS for as long as the pod lives.
#[test]
fn a_directory_left_by_the_old_subpath_mount_is_replaced_by_the_file() {
    if std::fs::read_to_string("/etc/resolv.conf").is_err() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().to_string_lossy().to_string();
    let path = kloudlite_workspaces::k8s::attach_file(&pool, "ws-1");
    std::fs::create_dir_all(&path).unwrap();

    kloudlite_agent::controller::write_resolv_conf(&pool, "ws-1", "ws-alice", None).unwrap();
    assert!(std::fs::metadata(&path).unwrap().is_file());
}
