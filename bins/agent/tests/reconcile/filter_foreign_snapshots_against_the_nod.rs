//! I6: filter foreign Snapshots against the node's Volume store, not a GET.

use super::*;


pub(crate) fn snapshot_working(name: &str, volume: &str, worktree: &str) -> Arc<crd::Snapshot> {
    Arc::new(
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": name, "uid": "snap-uid"},
            "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": ""},
        }))
        .unwrap(),
    )
}

/// I6: a Snapshot on another node's volume costs NO API calls. Every node watches every
/// Snapshot, so the ~(N-1)/N that are not ours were each paying a Workspace GET (and an
/// Environment GET on the miss) just to discover that.
#[tokio::test]
async fn a_snapshot_on_another_nodes_volume_makes_no_api_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    // The node-scoped Volume store holds only this node's volumes; `vol-elsewhere` is absent.
    ctx.remember_volume(volume(1));

    let action = kloudlite_agent::snapshot::reconcile_snapshot(snapshot_working("push-1", "vol-elsewhere", "ws-1"), ctx.clone())
        .await
        .expect("no error");

    assert!(rec.calls().is_empty(), "a foreign volume's snapshot must cost nothing: {:?}", rec.calls());
    assert_eq!(action, kube::runtime::controller::Action::await_change());
}

/// The volume IS ours but the worktree cannot be resolved yet (a push racing `volumeRef`
/// visibility): still a requeue, still through `worktree_node`. The pre-filter must not turn a
/// racing push into a silently hung one.
#[tokio::test]
async fn a_snapshot_on_my_volume_still_resolves_its_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/workspaces/ws-1"),
            kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/environments/ws-1"),
        ],
    );
    ctx.remember_volume(volume(1)); // "vol-1", this node

    let action = kloudlite_agent::snapshot::reconcile_snapshot(snapshot_working("push-1", "vol-1", "ws-1"), ctx.clone())
        .await
        .expect("no error");

    assert!(rec.calls().iter().any(|c| c == "GET /apis/kloudlite.io/v1alpha1/workspaces/ws-1"));
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
}

/// A volume the store has not seen yet is NOT "not mine": the store is a cache, and a Volume
/// created seconds ago may not have reached it. Keep-biased — fall through to the real lookup.
#[tokio::test]
async fn an_unknown_volume_falls_through_to_the_worktree_lookup() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/workspaces/ws-1"),
            kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/environments/ws-1"),
        ],
    );
    // Store deliberately EMPTY — not yet populated, which is not evidence of anything.

    let _ = kloudlite_agent::snapshot::reconcile_snapshot(snapshot_working("push-1", "vol-x", "ws-1"), ctx.clone()).await;

    assert!(
        rec.calls().iter().any(|c| c == "GET /apis/kloudlite.io/v1alpha1/workspaces/ws-1"),
        "an empty store must not be read as 'not mine'"
    );
}

/// M1: a workspace parked in a Wait arm keeps the conditions OTHER writers own. `Replicated` is
/// computed in one place (`replicated_condition`) and read by the per-volume sweep;
/// `Decommissioning` is the drain notice. Dropping either on every wait arm makes the sweep read
/// a false it did not compute.
#[test]
fn kept_conditions_preserves_replicated_and_decommissioning() {
    let cond = |kind: &str, status: bool| crd::condition(kind, status, "r", "m", 1);
    let prev = vec![
        cond("PackagesReady", true),
        cond("Attached", true),
        cond("Replicated", true),
        cond("Decommissioning", true),
        cond("Ready", false),
    ];
    let kept = kloudlite_agent::controller::kept_conditions(&prev, cond("Ready", true));
    let types: Vec<&str> = kept.iter().map(|c| c.type_.as_str()).collect();
    assert!(types.contains(&"Replicated"), "the sweep reads this and does not write it: {types:?}");
    assert!(types.contains(&"Decommissioning"), "the drain notice is not the wait arm's to drop: {types:?}");
    assert_eq!(types.iter().filter(|t| **t == "Ready").count(), 1, "the new condition replaces, never doubles");
}

// ---------------------------------------------------------------------------------------------
// Service intercept: three states, and the third — the wish KEPT while it is not in force — is
// the one the design was rewritten around. A controller never writes spec and never clears an
// intercept; what it changes is the rendering and the status.
// ---------------------------------------------------------------------------------------------

pub(crate) const WS_OBJ: &str = "/apis/kloudlite.io/v1alpha1/workspaces/ws-1";
pub(crate) const WEB_STS: &str = "/apis/apps/v1/namespaces/env-1/statefulsets/web";
pub(crate) const WEB_SVC: &str = "/api/v1/namespaces/env-1/services/web";
pub(crate) const WEB_SLICE: &str = "/apis/discovery.k8s.io/v1/namespaces/env-1/endpointslices/web-intercept";
pub(crate) const ENV_POLICY: &str = "/apis/networking.k8s.io/v1/namespaces/env-1/networkpolicies/intercept-ws-1";
pub(crate) const WS_POLICY: &str = "/apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/intercept-ws-1";

pub(crate) fn secs_ago(n: i64) -> String {
    k8s_openapi::jiff::Timestamp::from_second(k8s_openapi::jiff::Timestamp::now().as_second() - n)
        .unwrap()
        .to_string()
}

/// The environment every intercept test runs: one service `web` on 80, wished onto `ws-1`'s 3000.
pub(crate) fn intercept_env(intercepts: serde_json::Value, prev_by: Option<&str>) -> crd::Environment {
    let mut e = environment(serde_json::json!({
        "phase": "running", "nodeName": "node-a", "volumeRef": "env-1",
        "serviceStatus": [{"name": "web", "ready": true, "interceptedBy": prev_by}],
    }));
    e.spec.services = vec![kloudlite_workspaces::model::Service {
        name: "web".into(),
        image: "nginx".into(),
        command: vec![],
        env: Default::default(),
        mounts: vec![],
        ports: vec![80],
        resources: None,
    }];
    e.spec.intercepts = serde_json::from_value(intercepts).unwrap();
    e
}

pub(crate) fn one_intercept() -> serde_json::Value {
    serde_json::json!([{"service": "web", "workspace": "ws-1", "ports": [{"service": 80, "workspace": 3000}]}])
}

/// Everything a running environment reconcile needs before it reaches the services, plus write
/// routes for both halves of an intercept. The Workspace and pod routes are per-test.
pub(crate) fn intercept_routes(extra: Vec<Route>) -> Vec<Route> {
    let ns = crd::env_namespace("env-1");
    let ready_sts = serde_json::json!({
        "apiVersion": "apps/v1", "kind": "StatefulSet",
        "metadata": {"name": "web", "namespace": "env-1"},
        "status": {"readyReplicas": 1},
    });
    let mut r = vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![]) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{ns}"), status: 200, body: serde_json::json!({"kind": "Namespace"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/default-deny"), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/allow-dns"), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/allow-internet-egress"), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/allow-same-namespace"), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings/api-secrets"), status: 200, body: serde_json::json!({"kind": "RoleBinding"}) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{ns}/limitranges/slot"), status: 200, body: serde_json::json!({"kind": "LimitRange"}) },
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/quotas/acme"),
        kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/quotas/default-user"),
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{ns}/resourcequotas/owner-quota"), status: 200, body: serde_json::json!({"kind": "ResourceQuota"}) },
        Route { method: "PATCH", path: WEB_STS.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
        Route { method: "PATCH", path: WEB_SVC.into(), status: 200, body: serde_json::json!({"kind": "Service"}) },
        Route { method: "PATCH", path: WEB_SLICE.into(), status: 200, body: serde_json::json!({"kind": "EndpointSlice"}) },
        Route { method: "DELETE", path: WEB_SLICE.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "PATCH", path: ENV_POLICY.into(), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: WS_POLICY.into(), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        kloudlite_workspaces::kube_test::get(WEB_STS, ready_sts),
        Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
    ];
    // The slice list an in-force intercept reads to find what Kubernetes abandoned. Only added
    // when the test has not supplied its own — a test about the abandoned slices needs the list to
    // answer with them, and the first route for a path is the one that answers first.
    let slice_list = format!("/apis/discovery.k8s.io/v1/namespaces/{ns}/endpointslices");
    if !extra.iter().any(|e| e.method == "GET" && e.path == slice_list) {
        r.push(kloudlite_workspaces::kube_test::get(
            slice_list,
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSliceList", "metadata": {},
                "items": [{"metadata": {"name": "web-intercept", "namespace": "env-1"}, "addressType": "IPv4", "endpoints": []}],
            }),
        ));
    }
    r.extend(extra);
    r
}

/// A workspace attached to `env-1`, its pod named — `pod` decides what the pod GET answers.
pub(crate) fn attached_ws(desired: &str, attached: Option<&str>, ready_since: i64) -> serde_json::Value {
    attached_ws_ready(desired, attached, ready_since, false)
}

/// `ready`: what the WORKSPACE's own `Ready` condition says. `True` is not a clock — see
/// `not_ready_since` — and a test that wants the grace measured has to say `False`.
///
/// `podRef` is `{namespace}/{name}`, the shape the workspace's controller really writes. A bare
/// name here let every intercept test pass while the live decision could not find a pod at all.
pub(crate) fn attached_ws_ready(desired: &str, attached: Option<&str>, ready_since: i64, ready: bool) -> serde_json::Value {
    let mut o = ws_json(serde_json::json!({
        "phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1", "podRef": "ws-alice/ws-1-0",
        "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" },
                        "reason": "PodNotReady", "message": "",
                        "lastTransitionTime": secs_ago(ready_since), "observedGeneration": 1}],
    }));
    o["spec"]["desiredState"] = serde_json::json!(desired);
    if let Some(a) = attached {
        o["spec"]["attachedEnvironment"] = serde_json::json!(a);
    }
    o
}

pub(crate) fn ready_pod(ready_since: i64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "ws-1-0", "namespace": "ws-alice"},
        "status": {"podIP": "10.42.3.231",
                   "conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": secs_ago(ready_since)}]},
    })
}

pub(crate) fn env_tmp() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/env-1/live/env-1")).unwrap();
    tmp
}

/// (a) In force: the real service is STOPPED, its Service loses the selector Kubernetes would
/// otherwise maintain, and the slice carries the workspace pod's live IP on the MAPPED port.
#[tokio::test]
async fn an_intercept_in_force_stops_the_real_service_and_points_the_slice_at_the_workspace() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1-0", ready_pod(600)),
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), None), &ctx).await.unwrap();

    let sts = rec.sent("PATCH", WEB_STS);
    assert_eq!(sts.last().unwrap()["spec"]["replicas"], 0, "the real service is stopped: {:?}", sts.last());
    let svc = rec.sent("PATCH", WEB_SVC);
    assert!(svc.last().unwrap()["spec"]["selector"].is_null(), "a selector can never name another namespace: {:?}", svc.last());
    let slice = rec.sent("PATCH", WEB_SLICE);
    assert_eq!(slice.last().unwrap()["endpoints"][0]["addresses"][0], "10.42.3.231");
    assert_eq!(slice.last().unwrap()["ports"][0]["name"], "p80", "matched to the Service port BY NAME");
    assert_eq!(slice.last().unwrap()["ports"][0]["port"], 3000, "delivered on the workspace's port");
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["serviceStatus"][0]["interceptedBy"], "ws-1");
    assert!(
        rec.calls().iter().any(|c| c == &format!("PATCH {ENV_POLICY}")) && rec.calls().iter().any(|c| c == &format!("PATCH {WS_POLICY}")),
        "both halves of the grant, or the traffic is denied: {:?}", rec.calls()
    );
}

/// (b) The wish REMOVED: the real service comes back and the slice goes.
#[tokio::test]
async fn removing_the_wish_restores_the_real_service_and_deletes_the_slice() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        Route { method: "DELETE", path: ENV_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: WS_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    kloudlite_agent::controller::apply_environment(&intercept_env(serde_json::json!([]), Some("ws-1")), &ctx).await.unwrap();

    let sts = rec.sent("PATCH", WEB_STS);
    assert_eq!(sts.last().unwrap()["spec"]["replicas"], 1, "back to its declared replicas");
    assert!(!sts.last().unwrap()["spec"]["selector"].is_null());
    let svc = rec.sent("PATCH", WEB_SVC);
    assert!(!svc.last().unwrap()["spec"]["selector"].is_null(), "the selector is back: {:?}", svc.last());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}")), "the slice goes: {:?}", rec.calls());
    // The ordinary release: the wish is out of spec and status is the only record of the grant, so
    // BOTH halves have to be found from it — an ingress rule left behind opens this environment's
    // namespace to that pod until somebody deletes the workspace.
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {ENV_POLICY}")), "env-side grant goes: {:?}", rec.calls());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WS_POLICY}")), "workspace-side grant goes: {:?}", rec.calls());
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert!(st.last().unwrap()["status"]["serviceStatus"][0]["interceptedBy"].is_null());
}

/// (c) THE CENTRAL RULE. The workspace is stopped, the wish is untouched, and the real service is
/// back — a controller never writes spec, so the person's intercept survives the night.
#[tokio::test]
async fn a_stopped_workspace_releases_the_intercept_without_touching_the_wish() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("stopped", Some("env-1"), 60)),
        Route { method: "DELETE", path: WS_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: ENV_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);
    let e = intercept_env(one_intercept(), Some("ws-1"));

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    // Non-vacuous on purpose: `heal_labels` patches the object every pass, so an empty list here
    // would mean the loop below proved nothing.
    assert!(!rec.sent("PATCH", ENV_PATCH).is_empty(), "the object IS patched: {:?}", rec.calls());
    for body in rec.sent("PATCH", ENV_PATCH) {
        assert!(body.get("spec").is_none(), "a controller never writes an Environment's spec: {body}");
    }
    assert!(!e.spec.intercepts.is_empty(), "the wish is the person's and stays");
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 1, "the real service is back");
    assert!(!rec.sent("PATCH", WEB_SVC).last().unwrap()["spec"]["selector"].is_null(), "the selector is back");
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}")), "the slice goes: {:?}", rec.calls());
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    let st = st.last().unwrap();
    assert!(st["status"]["serviceStatus"][0]["interceptedBy"].is_null(), "not in force: {st}");
    let cond = st["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Intercepted").unwrap();
    assert_eq!(cond["status"], "False");
    assert_eq!(cond["reason"], "WorkspaceStopped", "status says why: {cond}");
}

/// (d) The pod gone for LESS than the grace: an ordinary restart must not bounce the real
/// StatefulSet up and down, so nothing moves and the pass looks again.
#[tokio::test]
async fn a_pod_missing_for_less_than_the_grace_moves_nothing() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 5)),
        kloudlite_workspaces::kube_test::not_found("/api/v1/namespaces/ws-alice/pods/ws-1-0"),
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    let action = kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), Some("ws-1")), &ctx)
        .await
        .unwrap();

    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)), "it has to look again");
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "the real service stays down");
    assert!(
        !rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}") || c == &format!("PATCH {WEB_SLICE}")),
        "the slice is left exactly as it is: {:?}", rec.calls()
    );
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["serviceStatus"][0]["interceptedBy"], "ws-1", "still in force");
}

/// The other side of the same clock: unreachable for longer than the grace DOES fall back, which
/// is what makes the grace a delay rather than a permanent hold.
#[tokio::test]
async fn a_pod_missing_for_longer_than_the_grace_brings_the_real_service_back() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 120)),
        kloudlite_workspaces::kube_test::not_found("/api/v1/namespaces/ws-alice/pods/ws-1-0"),
        Route { method: "DELETE", path: WS_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: ENV_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), Some("ws-1")), &ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 1);
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}")));
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    let cond = st.last().unwrap()["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Intercepted").cloned().unwrap();
    assert_eq!(cond["reason"], "PodUnreachable", "{cond}");
}

/// (e) An ERROR reading the Workspace is not evidence of anything. Conflating it with "the
/// workspace is gone" would turn a blip in the API server into a service flap.
#[tokio::test]
async fn an_unreadable_workspace_changes_nothing_and_requeues() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![Route {
        method: "GET",
        path: WS_OBJ.into(),
        status: 500,
        body: serde_json::json!({"kind": "Status", "code": 500, "message": "etcd leader changed"}),
    }]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    let action = kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), Some("ws-1")), &ctx)
        .await
        .unwrap();

    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "the previous rendering stands");
    assert!(
        !rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}") || c == &format!("PATCH {WEB_SLICE}")),
        "nothing is written on an answer that says nothing: {:?}", rec.calls()
    );
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["serviceStatus"][0]["interceptedBy"], "ws-1");
    assert!(
        !st.last().unwrap()["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Intercepted"),
        "an undecided pass states nothing: {:?}", st.last()
    );
}

/// The grace's own trap: a workspace that has been `Ready=True` for ten minutes and has just lost
/// its pod must be HELD, not dated from when it came up. Reading that transition time as an outage
/// yields `waited = 600` and an immediate fallback — in exactly the ordinary pod restart the grace
/// exists to ride out.
#[tokio::test]
async fn a_long_ready_workspace_that_has_just_lost_its_pod_is_held_not_dated_from_its_start() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws_ready("running", Some("env-1"), 600, true)),
        kloudlite_workspaces::kube_test::not_found("/api/v1/namespaces/ws-alice/pods/ws-1-0"),
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    let action = kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), Some("ws-1")), &ctx)
        .await
        .unwrap();

    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "the real service stays down");
    assert!(
        !rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}")),
        "a ten-minute-old start time is not a ten-minute-old outage: {:?}", rec.calls()
    );
}


/// The ORDER, which is the whole difference between a failed slice write costing nothing and
/// costing the service: a Service with its selector dropped and no slice behind it has no
/// endpoints at all, and the wish stays, so every retry repeats it.
#[tokio::test]
async fn the_slice_is_written_before_the_service_loses_its_selector() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1-0", ready_pod(600)),
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), None), &ctx).await.unwrap();

    let calls = rec.calls();
    let slice = calls.iter().position(|c| c == &format!("PATCH {WEB_SLICE}")).expect("the slice is written");
    let svc = calls.iter().position(|c| c == &format!("PATCH {WEB_SVC}")).expect("the Service is written");
    assert!(slice < svc, "the endpoints go in before the selector goes: {calls:?}");
}

/// A Force pass whose status write failed, then an unreadable Workspace: `prev` records no
/// intercept, so the Service is rendered WITH its selector again — and the slice from that first
/// pass has to go with it. kube-proxy unions the two, so a survivor splits the service's traffic
/// at random between the real pod and the workspace.
#[tokio::test]
async fn a_held_pass_that_rendered_no_intercept_last_time_deletes_the_stale_slice() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![Route {
        method: "GET",
        path: WS_OBJ.into(),
        status: 500,
        body: serde_json::json!({"kind": "Status", "code": 500, "message": "etcd leader changed"}),
    }]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), None), &ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 1, "the real service is up");
    assert!(!rec.sent("PATCH", WEB_SVC).last().unwrap()["spec"]["selector"].is_null(), "with its selector");
    assert!(
        rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}")),
        "a slice beside a selectored Service splits the traffic: {:?}", rec.calls()
    );
}

/// Kubernetes does not clean up after a Service that loses its selector: the endpointslice
/// controller's slice and the legacy `Endpoints` object both keep naming the stopped pod, and
/// kube-proxy unions them with ours. Measured on the fleet before this: two dials in six reached
/// nothing. Our own slice must survive — deleting it is the outage this feature exists to avoid.
#[tokio::test]
async fn an_intercept_deletes_the_endpoints_kubernetes_abandoned_and_keeps_its_own() {
    let tmp = env_tmp();
    let abandoned = "/apis/discovery.k8s.io/v1/namespaces/env-1/endpointslices/web-x9k2p";
    let endpoints = "/api/v1/namespaces/env-1/endpoints/web";
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1-0", ready_pod(600)),
        kloudlite_workspaces::kube_test::get(
            "/apis/discovery.k8s.io/v1/namespaces/env-1/endpointslices",
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSliceList", "metadata": {},
                "items": [
                    {"metadata": {"name": "web-intercept", "namespace": "env-1"}, "addressType": "IPv4", "endpoints": []},
                    {"metadata": {"name": "web-x9k2p", "namespace": "env-1"}, "addressType": "IPv4", "endpoints": []},
                ],
            }),
        ),
        Route { method: "DELETE", path: abandoned.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: endpoints.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), None), &ctx).await.unwrap();

    let calls = rec.calls();
    assert!(calls.iter().any(|c| c == &format!("DELETE {abandoned}")), "the abandoned slice goes: {calls:?}");
    assert!(calls.iter().any(|c| c == &format!("DELETE {endpoints}")), "the legacy Endpoints goes: {calls:?}");
    assert!(!calls.iter().any(|c| c == &format!("DELETE {WEB_SLICE}")), "ours stays: {calls:?}");
    // AFTER the selector is gone, or the controllers that own them write them straight back.
    let svc = calls.iter().position(|c| c == &format!("PATCH {WEB_SVC}")).expect("the Service is written");
    let gone = calls.iter().position(|c| c == &format!("DELETE {abandoned}")).unwrap();
    assert!(svc < gone, "the selector goes first: {calls:?}");
}

/// The clock-less hold, BOUNDED. No pod, and a workspace whose own controller has stopped stamping
/// — its node died — dates nothing at all, and the service would otherwise stay scaled to zero
/// behind a slice pointing at a pod that no longer exists, on every pass, forever. The clock is
/// the one this controller stamped itself on the pass that first saw the outage.
#[tokio::test]
async fn an_intercept_with_no_clock_anywhere_falls_back_once_our_own_record_is_older_than_the_grace() {
    let tmp = env_tmp();
    let mut ws = attached_ws("running", Some("env-1"), 0);
    ws["status"]["conditions"] = serde_json::json!([]);
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, ws),
        kloudlite_workspaces::kube_test::not_found("/api/v1/namespaces/ws-alice/pods/ws-1-0"),
        Route { method: "DELETE", path: WS_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: ENV_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);
    let mut e = intercept_env(one_intercept(), Some("ws-1"));
    e.status.as_mut().unwrap().service_status[0].unreachable_since =
        Some(k8s_openapi::jiff::Timestamp::now().as_second() - 600);

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 1, "the real service comes back");
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}")), "the slice goes: {:?}", rec.calls());
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    let cond = st.last().unwrap()["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Intercepted").cloned().unwrap();
    assert_eq!(cond["reason"], "PodUnreachable", "{cond}");
}

/// The pass that DISCOVERS a clock-less outage stamps the clock and holds. Without the stamp the
/// next pass would find nothing again and hold again — forever — and borrowing an older timestamp
/// from elsewhere would skip the grace entirely on this very pass.
#[tokio::test]
async fn the_pass_that_first_sees_no_clock_records_one_and_holds() {
    let tmp = env_tmp();
    let mut ws = attached_ws("running", Some("env-1"), 0);
    ws["status"]["conditions"] = serde_json::json!([]);
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, ws),
        kloudlite_workspaces::kube_test::not_found("/api/v1/namespaces/ws-alice/pods/ws-1-0"),
    ]);
    let (ctx, rec) = ctx(tmp.path(), routes);

    kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), Some("ws-1")), &ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "the intercept is held, not dropped");
    assert!(!rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}")), "the slice stays: {:?}", rec.calls());
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    let svc = st.last().unwrap()["status"]["serviceStatus"][0].clone();
    let stamped = svc["unreachableSince"].as_i64().expect("the outage is dated on the pass that finds it");
    assert!(
        (k8s_openapi::jiff::Timestamp::now().as_second() - stamped) < 5,
        "dated now, not borrowed from an older object: {svc}"
    );
}
