//! the stop-before-teardown snapshot.

use super::*;


pub(crate) const STOP_REQ: &str = "/apis/kloudlite.io/v1alpha1/snapshots/stop-env-1-1";
pub(crate) const ENV_PATCH: &str = "/apis/kloudlite.io/v1alpha1/environments/env-1";
pub(crate) const DEP_DEL: &str = "/apis/apps/v1/namespaces/env-1/statefulsets/db";
pub(crate) const REPLICAS: &str = "/apis/kloudlite.io/v1alpha1/volumereplicas";

/// `VolumeReplica` declares only `.spec.node`/`.status.phase` as selectable; a `spec.volume=`
/// field selector is a 400 from a real API server on every reconcile — unseen by this mock, which
/// accepts any selector. So pin the request shape itself.
#[tokio::test]
async fn the_replicated_condition_lists_replicas_by_spec_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let w = stopping_ws();
    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await;
    // `requests()`, not `calls()`: only the former keeps the query string the selector lives in.
    let requests = rec.requests();
    let lists: Vec<&String> = requests.iter().filter(|c| c.contains("volumereplicas")).collect();
    assert!(!lists.is_empty(), "the condition must list replicas: {requests:?}");
    for c in lists {
        assert!(c.contains("fieldSelector=spec.volume"), "expected a spec.volume field selector: {c}");
    }
}
pub(crate) const WS_STOP_REQ: &str = "/apis/kloudlite.io/v1alpha1/snapshots/stop-ws-1-1";

pub(crate) fn rfc3339_ago(secs: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
}

/// A `VolumeReplicaList` as the flush gate lists it.
pub(crate) fn replica_list(rows: &[(&str, &str, Option<&str>)]) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplicaList",
        "metadata": {"resourceVersion": "1"},
        "items": rows.iter().map(|(node, phase, last)| serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": crd::replica_name("env-1", node), "uid": format!("vr-{node}")},
            "spec": {"volume": "env-1", "node": node},
            "status": {"phase": phase, "branches": {}, "lastSyncAt": last},
        })).collect::<Vec<_>>(),
    })
}

/// The environment's live worktree on disk. Its existence is what tells the stop path there is
/// something to cut — every test that expects a cut must materialise it.
pub(crate) fn materialise_env(tmp: &std::path::Path) {
    std::fs::create_dir_all(tmp.join("vol").join("env-1").join("live").join("env-1")).unwrap();
}

/// Everything a stopping environment touches before the flush gate: the drain, and the volume.
pub(crate) fn env_flush_routes(stop: serde_json::Value, replicas: serde_json::Value) -> Vec<Route> {
    vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
        kloudlite_workspaces::kube_test::get(POD_LIST, pod_list(&[])),
        kloudlite_workspaces::kube_test::get(STOP_REQ, stop),
        kloudlite_workspaces::kube_test::get(REPLICAS, replicas),
        Route { method: "DELETE", path: DEP_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
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
    ]
}

/// A stop no longer waits for anybody: the cut turns Ready and the StatefulSets go in the SAME
/// pass. The whole flush gate — ten minutes of a person's time in the bad case — moved into
/// placement, where the decision it was making actually belongs.
#[tokio::test]
async fn a_stop_tears_down_as_soon_as_the_cut_is_ready() {
    let tmp = tempfile::tempdir().unwrap();
    materialise_env(tmp.path());
    let ready = stop_snapshot(serde_json::json!({"phase": "ready", "readyAt": rfc3339_ago(1)}));
    // NOBODY holds it: under the old gate this was a ten-minute wait, and now it is a condition.
    let (ctx, rec) = ctx(tmp.path(), env_flush_routes(ready, replica_list(&[])));

    kloudlite_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {DEP_DEL}")), "no wait: {:?}", rec.calls());
    // C1: the stop CR is the stopped worktree's ONE remaining sync point — `status.head` never
    // names it, the last beat transient was reclaimed when it turned Ready, and every replica
    // drops a CR-less subvolume within a cycle.
    assert!(!rec.calls().iter().any(|c| c == &format!("DELETE {STOP_REQ}")), "the stop CR must survive teardown: {:?}", rec.calls());
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    let last = st.last().unwrap();
    assert_eq!(last["status"]["phase"], "stopped");
    let conds = last["status"]["conditions"].as_array().unwrap();
    assert!(conds.iter().any(|c| c["type"] == "Ready" && c["reason"] == "Stopped"));
    assert!(
        !conds.iter().any(|c| c["reason"] == "FlushUnreplicated"),
        "FlushUnreplicated is gone: a stop is never unreplicated, it is merely not-yet-replicated: {conds:?}"
    );
    assert!(conds.iter().any(|c| c["type"] == "Replicated" && c["reason"] == "AwaitingReplica"), "{conds:?}");
    // The peers are woken on the way out — the cut exists NOW, and the pull ticker alone is what
    // used to make a cross-node start take minutes. The node list is `placeable_nodes` deciding
    // whom to poke; the dials themselves are best-effort and unasserted.
    assert!(rec.calls().iter().any(|c| c.starts_with("GET /api/v1/nodes")), "the stop must wake the peers: {:?}", rec.calls());
}

/// The one thing that still parks a teardown: a cut that has not landed. A parent torn down
/// without one loses its last state for good, so `Waiting` keeps the services up — unbounded now,
/// because the bound existed only to cap the replica wait that is gone.
#[tokio::test]
async fn a_stop_whose_cut_is_not_ready_still_tears_nothing_down() {
    let tmp = tempfile::tempdir().unwrap();
    materialise_env(tmp.path());
    let wedged = stop_snapshot(serde_json::json!({"phase": "working"}));
    let (ctx, rec) = ctx(tmp.path(), env_flush_routes(wedged, replica_list(&[])));

    kloudlite_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();

    assert!(!rec.calls().iter().any(|c| c == &format!("DELETE {DEP_DEL}")), "no cut, no teardown: {:?}", rec.calls());
    assert_eq!(rec.sent("PATCH", ENV_STATUS_PATH).last().unwrap()["status"]["conditions"][0]["reason"], "FlushBeforeStop");
}

/// A builder is an `Environment` created `Stopped` from birth: no pod ever ran, no worktree was
/// ever materialised, and `btrfs subvolume snapshot` of a path that does not exist fails forever.
/// It must reach `Stopped` in one pass, cut nothing, and clear the unfulfillable stop request an
/// earlier build of the agent left behind.
#[tokio::test]
async fn an_environment_that_never_materialised_stops_without_a_cut() {
    let tmp = tempfile::tempdir().unwrap();
    // No `materialise_env`: nothing on disk, which is the whole point.
    let pending = stop_snapshot(serde_json::json!({"phase": "working"}));
    let mut routes = env_flush_routes(pending.clone(), replica_list(&[]));
    routes.push(Route { method: "DELETE", path: STOP_REQ.into(), status: 200, body: pending });
    let (ctx, rec) = ctx(tmp.path(), routes);

    // Exactly the fleet's shape: the `Waiting` arm wrote `phase: running` itself on an earlier
    // pass, which is why phase is not evidence that anything ever ran.
    kloudlite_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();

    assert!(
        !rec.calls().iter().any(|c| c.starts_with("POST /apis/kloudlite.io/v1alpha1/snapshots")),
        "nothing to cut: {:?}",
        rec.calls()
    );
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {STOP_REQ}")), "the unfulfillable request goes: {:?}", rec.calls());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {DEP_DEL}")), "the teardown still runs: {:?}", rec.calls());
    assert_eq!(rec.sent("PATCH", ENV_STATUS_PATH).last().unwrap()["status"]["phase"], "stopped");
}

/// The `Replicated` condition is the ONE truth about whether a stopped parent can start
/// elsewhere, written by its owner on every reconcile of it. `False/AwaitingReplica` until some
/// other node's replica holds the stop cut BY NAME.
#[tokio::test]
async fn a_stopped_parent_reports_awaiting_replica_until_a_peer_holds_the_cut() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), replicated_routes("sync-ws-1-old"));

    let c = kloudlite_agent::controller::replicated_condition(&ctx, "vol-1", "ws-1", 2, &[], 3).await.unwrap();
    assert_eq!(c.type_, "Replicated");
    assert_eq!(c.status, "False");
    assert_eq!(c.reason, "AwaitingReplica");
    assert_eq!(c.message, "no other node holds the final sync point yet");
}

#[tokio::test]
async fn a_stopped_parent_reports_replicated_once_a_peer_holds_the_cut_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), replicated_routes("stop-ws-1-3"));

    let c = kloudlite_agent::controller::replicated_condition(&ctx, "vol-1", "ws-1", 2, &[], 3).await.unwrap();
    assert_eq!((c.status.as_str(), c.reason.as_str()), ("True", "Replicated"));
    assert_eq!(c.message, "another node holds the final sync point");
}

/// `replicas: 1` is not a separate reason — it is the same `False/AwaitingReplica` with a message
/// that names why it will never become true. One reason, one place to read it.
#[tokio::test]
async fn replicas_one_says_so_in_the_message_not_in_a_second_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), replicated_routes("stop-ws-1-3"));

    let c = kloudlite_agent::controller::replicated_condition(&ctx, "vol-1", "ws-1", 1, &[], 3).await.unwrap();
    assert_eq!((c.status.as_str(), c.reason.as_str()), ("False", "AwaitingReplica"));
    assert_eq!(c.message, "no replica is configured for this volume");
    assert!(rec.calls().is_empty(), "no standby can ever hold it: nothing to ask the API: {:?}", rec.calls());
}

/// The cluster's newest Ready transient for `ws-1` on `vol-1`, and one peer replica holding
/// `held` for it — the two lists `replicated_condition` reads and nothing else.
pub(crate) fn replicated_routes(held: &str) -> Vec<Route> {
    vec![
        kloudlite_workspaces::kube_test::get(
            "/apis/kloudlite.io/v1alpha1/snapshots",
            serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "metadata": {"resourceVersion": "1"}, "items": [
                {"apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
                 "metadata": {"name": "stop-ws-1-3", "uid": "stop-uid",
                              "annotations": {"kloudlite.io/synced-generation": "7"}},
                 "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "transient": true},
                 "status": {"phase": "ready"}}]}),
        ),
        kloudlite_workspaces::kube_test::get(
            REPLICAS,
            serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplicaList",
                               "metadata": {"resourceVersion": "1"}, "items": [
                {"apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
                 "metadata": {"name": crd::replica_name("vol-1", "node-b"), "uid": "vr-b"},
                 "spec": {"volume": "vol-1", "node": "node-b"},
                 "status": {"phase": "Synced", "branches": {"ws-1": held}}}]}),
        ),
    ]
}

/// The workspace half of the same gate: the pod is what keeps the worktree changing, so the cut
/// happens first and the delete waits on it landing.
#[tokio::test]
async fn a_workspace_stop_cuts_a_sync_point_before_deleting_the_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx1, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::not_found(WS_STOP_REQ),
            kloudlite_workspaces::kube_test::post(
                "/apis/kloudlite.io/v1alpha1/snapshots",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
                                   "metadata": {"name": "stop-ws-1-1", "uid": "stop-ws-uid"},
                                   "spec": {"volume": "ws-1", "owner": "alice", "worktree": "ws-1", "transient": true}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        ],
    );

    kloudlite_agent::controller::apply_workspace(&stopping_ws(), &ctx1).await.unwrap();
    let cut = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/snapshots");
    assert_eq!(cut.len(), 1, "one sync point: {:?}", rec.calls());
    assert_eq!(cut[0]["spec"]["transient"], true, "a sync point, not a snapshot a user sees");
    assert_eq!(cut[0]["spec"]["worktree"], "ws-1", "the PARENT's name, not the volume's");
    assert_eq!(cut[0]["spec"]["state"]["kind"], "workspace", "the parent's definition rides along on the cut");
    assert_eq!(cut[0]["spec"]["state"]["image"], "nginx:alpine");
    assert!(!rec.calls().iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")), "no delete before it lands: {:?}", rec.calls());

    // Second pass: the cut is Ready. Nobody holds it yet — that is the condition's business now,
    // not a gate on the teardown.
    drop(rec);
    let tmp2 = tempfile::tempdir().unwrap();
    let replicas = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplicaList", "metadata": {"resourceVersion": "1"},
        "items": [{"apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
                   "metadata": {"name": crd::replica_name("ws-1", "node-b"), "uid": "vr-b"},
                   "spec": {"volume": "ws-1", "node": "node-b"},
                   "status": {"phase": "Synced", "branches": {}, "lastSyncAt": rfc3339_ago(1)}}],
    });
    let (ctx2, rec) = ctx(
        tmp2.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                WS_STOP_REQ,
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
                                   "metadata": {"name": "stop-ws-1-1", "uid": "stop-ws-uid"},
                                   "spec": {"volume": "ws-1", "owner": "alice", "worktree": "ws-1", "transient": true},
                                   "status": {"phase": "ready", "readyAt": rfc3339_ago(30)}}),
            ),
            kloudlite_workspaces::kube_test::get(REPLICAS, replicas),
            Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    kloudlite_agent::controller::apply_workspace(&stopping_ws(), &ctx2).await.unwrap();
    let calls = rec.calls();
    assert!(calls.iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")), "landed: the pod goes: {calls:?}");
    // C1: the pod goes, the stop CR STAYS — it is the stopped worktree's only sync point now.
    assert!(!calls.iter().any(|c| c == &format!("DELETE {WS_STOP_REQ}")), "the stop CR must survive teardown: {calls:?}");
    assert_eq!(rec.sent("PATCH", WS_STATUS).last().unwrap()["status"]["phase"], "stopped");
}















pub(crate) const WS_POD_DEL: &str = "/api/v1/namespaces/ws-alice/pods/ws-1";














pub(crate) const ENV_STATUS_PATH: &str = "/apis/kloudlite.io/v1alpha1/environments/env-1/status";

/// An environment placed on this node authors its OWN Volume child, named after itself and
/// ownerReferenced to it — same skeleton as a workspace, so `DELETE environment` reclaims the disk.
#[tokio::test]
async fn a_placed_environment_creates_its_volume_child_on_its_own_node() {
    let tmp = tempfile::tempdir().unwrap();
    let mut fresh_vol = env_vol();
    fresh_vol["status"] = serde_json::json!({"phase": "working", "subvolumePresent": false});
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/volumes/env-1"),
            // The freshly created child has no disk yet, so the pass stops at the readiness wait.
            kloudlite_workspaces::kube_test::post("/apis/kloudlite.io/v1alpha1/volumes", fresh_vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    let sent = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/volumes");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["metadata"]["name"], "env-1", "the child takes the parent's name");
    assert_eq!(sent[0]["spec"]["nodeName"], "node-a", "the Volume is created FROM status.nodeName");
    let refs = sent[0]["metadata"]["ownerReferences"].as_array().expect("an ownerReference");
    assert_eq!(refs[0]["kind"], "Environment");
    assert_eq!(refs[0]["name"], "env-1");
}

/// No Deployment may exist before the disk does: a pod bound to an unmaterialized subvolume wedges
/// forever on `path … does not exist`.
#[tokio::test]
async fn an_environment_with_an_unready_volume_creates_no_deployment() {
    let tmp = tempfile::tempdir().unwrap();
    let mut vol = env_vol();
    vol["status"] = serde_json::json!({"phase": "working", "subvolumePresent": false});
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    let action = kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    assert!(
        !rec.calls().iter().any(|c| c.contains("/statefulsets")),
        "no deployment before its disk exists: {:?}",
        rec.calls()
    );
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["phase"], "creating");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "VolumeNotReady");
}

/// A service declaring no ports (a worker with nothing to connect to, e.g. `sleep 1d`) gets a
/// StatefulSet like any other service but no ClusterIP Service — the live repro of 2026-09-03,
/// where the API server rejected `spec.ports: Required value` on every reconcile forever. Both
/// services still converge and the environment reaches Running.
#[tokio::test]
async fn a_portless_service_gets_a_statefulset_but_no_clusterip() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/env-1/live/env-1")).unwrap();
    let ready_sts = |name: &str| {
        serde_json::json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": {"name": name, "namespace": "env-1"},
            "status": {"readyReplicas": 1},
        })
    };
    let routes = vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![]) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "Namespace"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/default-deny", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-dns", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-internet-egress", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-same-namespace", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{}/rolebindings/api-secrets", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "RoleBinding"}) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}/limitranges/slot", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "LimitRange"}) },
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/quotas/acme"),
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/quotas/default-user"),
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}/resourcequotas/owner-quota", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "ResourceQuota"}) },
        Route { method: "PATCH", path: "/apis/apps/v1/namespaces/env-1/statefulsets/web".into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
        Route { method: "PATCH", path: "/api/v1/namespaces/env-1/services/web".into(), status: 200, body: serde_json::json!({"kind": "Service"}) },
        Route { method: "PATCH", path: "/apis/apps/v1/namespaces/env-1/statefulsets/worker".into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
        kloudlite_workspaces::kube_test::get("/apis/apps/v1/namespaces/env-1/statefulsets/web", ready_sts("web")),
        kloudlite_workspaces::kube_test::get("/apis/apps/v1/namespaces/env-1/statefulsets/worker", ready_sts("worker")),
        Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let mut e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    e.spec.services = vec![
        kloudlite_workspaces::model::Service {
            name: "web".into(),
            image: "nginx".into(),
            command: vec![],
            env: Default::default(),
            mounts: vec![],
            ports: vec![80],
            resources: None,
        },
        kloudlite_workspaces::model::Service {
            name: "worker".into(),
            image: "alpine".into(),
            command: vec!["sh".into(), "-c".into(), "sleep 1d".into()],
            env: Default::default(),
            mounts: vec![],
            ports: vec![],
            resources: None,
        },
    ];

    let action = kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "both services ready: fully converged");

    assert!(rec.calls().iter().any(|c| c == "PATCH /apis/apps/v1/namespaces/env-1/statefulsets/web"));
    assert!(rec.calls().iter().any(|c| c == "PATCH /apis/apps/v1/namespaces/env-1/statefulsets/worker"));
    assert!(
        rec.calls().iter().any(|c| c == "PATCH /api/v1/namespaces/env-1/services/web"),
        "the ported service gets a ClusterIP: {:?}", rec.calls()
    );
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("PATCH") && c.contains("/services/worker")),
        "a portless service must never be given a ClusterIP: {:?}", rec.calls()
    );
    assert!(
        rec.calls().iter().any(|c| c == "DELETE /api/v1/namespaces/env-1/services/worker"),
        "a stale ClusterIP from an earlier ported definition must be cleaned up: {:?}", rec.calls()
    );

    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["phase"], "running", "both services converge: {:?}", st.last());
}

/// `EnvironmentSpec` carries one owner string, no separate "is this a team" bit — the agent has no
/// directory to ask either. The binding reconciler already worked this out for every owner it has
/// ever seen (`binding::is_team_owner`, stamped as `OwnerBinding.status.team`), so a team-owned
/// environment must read THAT rather than being sized off the smaller, person-shaped default.
#[tokio::test]
async fn a_team_owned_environments_quota_reads_the_bindings_team_flag() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/env-1/live/env-1")).unwrap();
    let ready_sts = |name: &str| {
        serde_json::json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": {"name": name, "namespace": "env-1"},
            "status": {"readyReplicas": 1},
        })
    };
    let binding = kloudlite_workspaces::kube_test::get(
        format!("/apis/kloudlite.io/v1alpha1/ownerbindings/{}", crd::binding_name("r1", "acme")),
        serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
                           "metadata": {"name": "r1-acme"},
                           "spec": {"owner": "acme", "region": "r1"},
                           "status": {"team": true, "conditions": []}}),
    );
    let quota = kloudlite_workspaces::kube_test::get(
        "/apis/kloudlite.io/v1alpha1/quotas/acme",
        serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "acme"},
            "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 12, "memoryGb": 48}
        }),
    );
    let routes = vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![]) },
        binding,
        quota,
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "Namespace"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/default-deny", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-dns", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-internet-egress", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-same-namespace", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{}/rolebindings/api-secrets", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "RoleBinding"}) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}/limitranges/slot", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "LimitRange"}) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}/resourcequotas/owner-quota", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "ResourceQuota"}) },
        Route { method: "PATCH", path: "/apis/apps/v1/namespaces/env-1/statefulsets/web".into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
        Route { method: "PATCH", path: "/api/v1/namespaces/env-1/services/web".into(), status: 200, body: serde_json::json!({"kind": "Service"}) },
        kloudlite_workspaces::kube_test::get("/apis/apps/v1/namespaces/env-1/statefulsets/web", ready_sts("web")),
        Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let mut e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    e.spec.services = vec![kloudlite_workspaces::model::Service {
        name: "web".into(),
        image: "nginx".into(),
        command: vec![],
        env: Default::default(),
        mounts: vec![],
        ports: vec![80],
        resources: None,
    }];

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", &format!("/api/v1/namespaces/{}/resourcequotas/owner-quota", crd::env_namespace("env-1")));
    assert!(!sent.is_empty(), "{:?}", rec.calls());
    assert_eq!(sent[0]["spec"]["hard"]["limits.cpu"], "12", "the TEAM's quota, not the person default of 8");
    assert_eq!(sent[0]["spec"]["hard"]["limits.memory"], "48Gi");
}

/// A NEW environment with no `storage` can never build a disk, and no retry adds a field.
#[tokio::test]
async fn a_new_environment_without_storage_fails_permanently() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let mut e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    e.spec.storage = None;

    let action = kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent: never retried");
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["phase"], "error");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "NoStorage");
}

/// A converged environment whose only status delta is `volumeRef` must still write it — the child
/// pointer is how everything else finds the disk, and a guard that ignored it left it unset forever.
#[tokio::test]
async fn an_environment_whose_only_delta_is_its_volume_ref_still_writes_status() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", env_vol()),
            kloudlite_workspaces::kube_test::get(STOP_REQ, stop_snapshot(serde_json::json!({"phase": "done"}))),
            Route { method: "DELETE", path: DEP_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "DELETE", path: STOP_REQ.into(), status: 200, body: stop_snapshot(serde_json::json!({"phase": "done"})) },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let mut e = stopping_env();
    // Everything the guard used to compare is already correct; only `volumeRef` is missing.
    e.status = Some(crd::EnvironmentStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(1),
        node_name: "node-a".into(),
        conditions: vec![kloudlite_workspaces::crd::condition("Ready", true, "Stopped", "pushed and stopped", 1)],
        ..Default::default()
    });
    // Not the idempotency guard's case: that one needs `observedGeneration` AND a volumeRef-free
    // status to be indistinguishable, so bump the generation to force the stop path to run.
    e.metadata.generation = Some(2);

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.len(), 1, "one status write: {:?}", rec.calls());
    assert_eq!(st[0]["status"]["volumeRef"], "env-1");
}
