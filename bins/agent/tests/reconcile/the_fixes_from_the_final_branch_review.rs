//! the fixes from the final branch review.

use super::*;


/// A `cloneOf` source is resolved as a VOLUME, so it works for both parent kinds. `clone_env`
/// writes the environment's id there, and a workspace-only lookup meant a cloned environment was
/// never claimed by anyone.
pub(crate) fn src_volume(node: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "env-src", "uid": "env-src-uid"},
        "spec": {"owner": "acme", "team": "", "nodeName": node, "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    })
}

pub(crate) fn cloned_env(source: &str) -> crd::Environment {
    let mut e = environment(serde_json::json!({}));
    e.spec.storage =
        Some(crd::WorkspaceStorage { quota_gb: 20, source: Some(crd::VolumeSource::CloneOf { volume: source.into(), commit: None }) });
    e
}

pub(crate) const ENV_STATUS: &str = "/apis/kloudlite.io/v1alpha1/environments/env-1/status";
pub(crate) const SRC_VOL: &str = "/apis/kloudlite.io/v1alpha1/volumes/env-src";

#[tokio::test]
async fn a_cloned_environment_is_claimed_by_its_source_volumes_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(SRC_VOL, src_volume("node-a")),
            Route { method: "PUT", path: ENV_STATUS.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::post(
                BINDINGS,
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-acme"},
                                   "spec": {"owner": "acme", "region": "r1", "nodeName": "node-a"}}),
            ),
        ],
    );

    kloudlite_agent::claim::claim_environment(&cloned_env("env-src"), &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", ENV_STATUS).len(), 1, "this node owns the source volume: {:?}", rec.calls());
}

#[tokio::test]
async fn a_cloned_environment_is_not_claimed_off_its_sources_node() {
    let tmp = tempfile::tempdir().unwrap();
    // A source with snapshots: bootstrap would claim anywhere, so the rule only bites once there is
    // something to be up to date WITH — and node-a has no replica row for env-src at all.
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(SRC_VOL, src_volume("node-b")),
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200,
                    body: snapshot_list_of("Snapshot", vec![snapshot_cr("env-src-a", "env-src")]) },
        ],
    );

    kloudlite_agent::claim::claim_environment(&cloned_env("env-src"), &ctx).await.unwrap();
    assert!(rec.sent("PUT", ENV_STATUS).is_empty(), "node-a is not up to date for env-src: {:?}", rec.calls());
}

/// The permanent path is a status write like any other, so it needs the same no-op guard: without
/// it every reconcile re-stamps `lastTransitionTime`, the write is its own watch event, and a
/// permanently-broken object spins against the API server until someone fixes its spec.
#[tokio::test]
async fn a_second_reconcile_of_a_settled_workspace_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let mut w = workspace(serde_json::json!({
        "phase": "error",
        "nodeName": "node-a",
        "conditions": [{"type": "Ready", "status": "False", "reason": "NoStorage",
                        "message": "spec.storage is required", "observedGeneration": 1,
                        "lastTransitionTime": "2026-08-27T00:00:00Z"}]
    }));
    w.spec.storage = None;

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert_eq!(
        rec.calls(),
        vec!["GET /api/v1/nodes/node-a".to_string()],
        "an already-settled object writes nothing — only the self-dead read: {:?}",
        rec.calls()
    );
}


/// Stopping needs neither the disk nor the namespace — only a pod delete. Gated on the Volume, a
/// workspace whose subvolume failed could never be stopped, so it kept its pod forever.
#[tokio::test]
async fn a_stopped_workspace_with_a_broken_volume_still_loses_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let ns = crd::ws_namespace("alice", "");
    let pod_del = format!("/api/v1/namespaces/{ns}/pods/ws-1");
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                WS_STOP_REQ,
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
                                   "metadata": {"name": "stop-ws-1-1", "uid": "stop-ws-uid"},
                                   "spec": {"volume": "ws-1", "owner": "alice", "worktree": "ws-1", "transient": true},
                                   "status": {"phase": "ready", "readyAt": rfc3339_ago(30)}}),
            ),
            Route { method: "DELETE", path: pod_del.clone(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    // With a `podRef`: a workspace that never ran skips the flush entirely, which would make this
    // test pass without ever exercising the gate.
    let mut w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a", "volumeRef": "ws-1",
                                             "podRef": "ws-alice/ws-1"}));
    w.spec.desired_state = crd::DesiredState::Stopped;

    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {pod_del}")), "{:?}", rec.calls());
    assert!(
        !rec.calls().iter().any(|c| c.contains("/volumes/")),
        "the stop must not depend on the Volume at all: {:?}",
        rec.calls()
    );
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["phase"], "stopped");
}

/// The home is on the shared NFS mount now (spec 2026-09-01): a stop deletes the pod straight
/// away, with no `stop-home-{ws}` snapshot request gating it.
#[tokio::test]
async fn a_stop_deletes_the_pod_without_any_home_push_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let w = stopping_ws();
    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")));
    assert!(rec.calls().iter().all(|c| !c.contains("snapshots/stop-home")), "no stop-home gate: {:?}", rec.calls());
}
