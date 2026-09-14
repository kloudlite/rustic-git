//! An intercept backed by a PROXY POD, not a hand-written EndpointSlice: the order the switch is
//! made in, the order it is taken back in, and the conversion of an intercept left in force by the
//! previous mechanism.
//!
//! The one rule every test here is about: the intercepted service is never without a ready
//! endpoint. Taking it, the proxy is Ready before the selector moves; giving it back, the selector
//! and the replicas are back before anything is deleted.

use super::*;


/// The proxy `web`'s intercept renders, as the pass finds it: `ready` completes the switch.
fn in_force_routes(ready: bool) -> Vec<Route> {
    intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1-0", ready_pod(600)),
        kloudlite_workspaces::kube_test::get(PROXY_POD, proxy_pod(ready)),
    ])
}

fn position(calls: &[String], call: &str) -> usize {
    calls.iter().position(|c| c == call).unwrap_or_else(|| panic!("{call} never happened: {calls:?}"))
}

/// What the pass recorded in `status.services[0].proxy`.
fn status_proxy(rec: &Recorder) -> Option<String> {
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    st.last()?["status"]["serviceStatus"][0]["proxy"].as_str().map(str::to_string)
}


/// The order IS the behaviour: the workspace-side target the proxy dials, then the proxy, then —
/// only because it came back Ready — the ClusterIP whose selector now names it, and the real
/// StatefulSet at zero.
#[tokio::test]
async fn in_force_writes_the_target_and_the_proxy_before_the_selector_moves() {
    let tmp = env_tmp();
    let (ctx, rec) = intercept_ctx(tmp.path(), in_force_routes(true));

    let action = kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), None), &ctx).await.unwrap();

    // Nothing in this namespace announces a pod that lives in ws-alice, so the pass that hands the
    // service back when the workspace vanishes has to be a TIMED one.
    assert_ne!(action, kube::runtime::controller::Action::await_change(), "an intercept in force keeps the tick");
    let calls = rec.calls();
    let target = position(&calls, &format!("PATCH {TARGET_SVC}"));
    let proxy = position(&calls, &format!("GET {PROXY_POD}"));
    let svc = position(&calls, &format!("PATCH {WEB_SVC}"));
    assert!(target < proxy, "the proxy dials the target: {calls:?}");
    assert!(proxy < svc, "the proxy is Ready before the selector moves: {calls:?}");

    let svc = rec.sent("PATCH", WEB_SVC);
    assert_eq!(svc.last().unwrap()["spec"]["selector"]["kloudlite.io/kind"], "intercept", "{:?}", svc.last());
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "the real service is stopped");
    assert_eq!(status_proxy(&rec).as_deref(), Some("ready"));
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["serviceStatus"][0]["interceptedBy"], "ws-1");
    assert!(
        calls.iter().any(|c| c == &format!("PATCH {ENV_POLICY}")) && calls.iter().any(|c| c == &format!("PATCH {WS_POLICY}")),
        "both halves of the grant, or the traffic is denied: {calls:?}"
    );
}


/// A half-done switch is a service with no endpoints at all, which is worse than an intercept that
/// has not taken yet. So on a FIRST take — nothing behind the proxy yet — a `starting` proxy
/// changes nothing and the real service is still the one serving.
#[tokio::test]
async fn a_proxy_that_is_not_ready_leaves_the_real_service_serving() {
    let tmp = env_tmp();
    let (ctx, rec) = intercept_ctx(tmp.path(), in_force_routes(false));

    kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), None), &ctx).await.unwrap();

    let svc = rec.sent("PATCH", WEB_SVC);
    assert_eq!(svc.last().unwrap()["spec"]["selector"]["kloudlite.io/service"], "web", "{:?}", svc.last());
    assert_ne!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "the real pods keep answering");
    assert_eq!(status_proxy(&rec).as_deref(), Some("starting"));
    // Not in force until the selector has moved — the web reads this, and a workspace that is not
    // being dialled must not be shown as if it were.
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert!(st.last().unwrap()["status"]["serviceStatus"][0]["interceptedBy"].is_null(), "{:?}", st.last());
    // The grant and the proxy STAY: this is an intercept being taken, not one being released.
    assert!(
        !rec.calls().iter().any(|c| c == &format!("DELETE {PROXY_POD}") || c == &format!("DELETE {ENV_POLICY}")),
        "a starting proxy must not be swept by its own pass: {:?}", rec.calls()
    );
}


/// The mirror of the order above. Everything the intercept left behind goes only after the service
/// is serving itself again — and every delete is forgotten by the apply-hash cache, or the next
/// intercept renders nothing because this process still believes it applied them.
#[tokio::test]
async fn a_release_restores_the_service_before_it_deletes_the_proxy() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        Route { method: "DELETE", path: ENV_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: WS_POLICY.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let (ctx, rec) = intercept_ctx(tmp.path(), routes);
    // As the pass that took the intercept left the cache: without the `forget_applied` calls the
    // assertion below is vacuous, since a release pass applies none of these itself.
    for key in ["Pod/env-1/intercept-web", "Service/ws-alice/intercept-target-ws-1",
                "NetworkPolicy/env-1/intercept-ws-1-web", "NetworkPolicy/ws-alice/intercept-ws-1"] {
        ctx.applied.lock().unwrap().insert(key.to_string(), (0, std::time::Instant::now()));
    }

    kloudlite_agent::controller::apply_environment(&intercept_env(serde_json::json!([]), Some("ws-1")), &ctx).await.unwrap();

    let calls = rec.calls();
    let svc = position(&calls, &format!("PATCH {WEB_SVC}"));
    let sts = position(&calls, &format!("PATCH {WEB_STS}"));
    let proxy = position(&calls, &format!("DELETE {PROXY_POD}"));
    assert!(svc < proxy, "the selector is back first: {calls:?}");
    assert!(sts < proxy, "and the replicas with it: {calls:?}");
    for gone in [PROXY_POD, TARGET_SVC, ENV_POLICY, WS_POLICY] {
        assert!(calls.iter().any(|c| c == &format!("DELETE {gone}")), "{gone} survives: {calls:?}");
    }
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 1, "back to its declared replicas");
    let applied = ctx.applied.lock().unwrap();
    for name in ["intercept-web", "intercept-target-ws-1", "intercept-ws-1-web", "intercept-ws-1"] {
        assert!(
            !applied.keys().any(|k| k.ends_with(&format!("/{name}"))),
            "{name} was deleted without forget_applied: {:?}", applied.keys().collect::<Vec<_>>()
        );
    }
}


/// The previous mechanism left a selector-less Service and a `web-intercept` slice. One pass under
/// this build converts it: the proxy comes up, the selector comes back naming it, and the slice —
/// which kube-proxy would otherwise union with the proxy's own endpoints — goes.
#[tokio::test]
async fn an_intercept_in_force_under_the_old_mechanism_is_converted_in_place() {
    let tmp = env_tmp();
    let (ctx, rec) = intercept_ctx(tmp.path(), in_force_routes(true));

    kloudlite_agent::controller::apply_environment(&intercept_env(one_intercept(), Some("ws-1")), &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WEB_SLICE}")), "the legacy slice goes: {:?}", rec.calls());
    let svc = rec.sent("PATCH", WEB_SVC);
    assert_eq!(svc.last().unwrap()["spec"]["selector"]["kloudlite.io/kind"], "intercept", "{:?}", svc.last());
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0);
}


/// An intercept left in force by the previous mechanism: the Service is selector-less and a
/// `web-intercept` slice is what delivers its traffic. Until the proxy is actually up, that is
/// still the only thing serving — so this pass must leave BOTH alone. Deleting the slice here, or
/// handing the service back to a StatefulSet that has to scale up first, is a real outage in the
/// middle of a conversion nobody asked for.
#[tokio::test]
async fn a_conversion_holds_the_legacy_slice_until_the_proxy_is_up() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1-0", ready_pod(600)),
        // First pass: no proxy pod at all. Second: the one it created, Ready.
        kloudlite_workspaces::kube_test::not_found(PROXY_POD),
        kloudlite_workspaces::kube_test::get(PROXY_POD, proxy_pod(true)),
    ]);
    let (ctx, rec) = intercept_ctx(tmp.path(), routes);
    // As the previous build left it: in force, and nothing recorded about a proxy.
    let e = intercept_env(one_intercept(), Some("ws-1"));

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    let calls = rec.calls();
    assert!(!calls.iter().any(|c| c == &format!("DELETE {WEB_SLICE}")), "the slice is still serving: {calls:?}");
    assert!(!calls.iter().any(|c| c == &format!("PATCH {WEB_SVC}")), "the Service keeps no selector: {calls:?}");
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "not handed back either");
    assert_eq!(status_proxy(&rec).as_deref(), Some("starting"));

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    let calls = rec.calls();
    assert!(calls.iter().any(|c| c == &format!("DELETE {WEB_SLICE}")), "the converted pass deletes it: {calls:?}");
    let svc = rec.sent("PATCH", WEB_SVC);
    assert_eq!(svc.last().unwrap()["spec"]["selector"]["kloudlite.io/kind"], "intercept", "{:?}", svc.last());
}


/// The target Service and the ingress grant are per WORKSPACE, so they are a union over every
/// service that workspace serves — and a `Keep` is being served just as much as a `Force` is.
/// Counting only the `Force` would let the sibling's pass rewrite the shared Service with its own
/// ports alone and cut the kept service's live traffic until the next pass.
#[tokio::test]
async fn a_kept_sibling_keeps_its_port_on_the_shared_target_and_grant() {
    let tmp = env_tmp();
    let ws_pod = "/api/v1/namespaces/ws-alice/pods/ws-1-0";
    let mut routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        // `api` is decided first and its pod GET fails — the transient unreadable answer that is
        // `Keep`. `web`'s own GET, right after, succeeds: one `Keep`, one `Force`, one pass.
        Route { method: "GET", path: ws_pod.into(), status: 500, body: serde_json::json!({"kind": "Status"}) },
        kloudlite_workspaces::kube_test::get(ws_pod, ready_pod(600)),
        kloudlite_workspaces::kube_test::get(PROXY_POD, proxy_pod(true)),
    ]);
    routes.extend([
        Route { method: "PATCH", path: "/apis/apps/v1/namespaces/env-1/statefulsets/api".into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
        Route { method: "PATCH", path: "/api/v1/namespaces/env-1/services/api".into(), status: 200, body: serde_json::json!({"kind": "Service"}) },
    ]);
    let (ctx, rec) = intercept_ctx(tmp.path(), routes);

    let mut e = intercept_env(
        serde_json::json!([
            {"service": "api", "workspace": "ws-1", "ports": [{"service": 8080, "workspace": 3001}]},
            {"service": "web", "workspace": "ws-1", "ports": [{"service": 80, "workspace": 3000}]},
        ]),
        Some("ws-1"),
    );
    let mut api = e.spec.services[0].clone();
    api.name = "api".into();
    api.ports = vec![8080];
    e.spec.services.insert(0, api);
    // Both were in force last pass; `api`'s record is the only thing that says so this pass.
    e.status.as_mut().unwrap().service_status.insert(
        0,
        serde_json::from_value(serde_json::json!({"name": "api", "ready": true, "interceptedBy": "ws-1", "proxy": "ready"})).unwrap(),
    );

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    let target = rec.sent("PATCH", TARGET_SVC);
    let ports: Vec<u64> = target.last().expect("the target Service is written")["spec"]["ports"]
        .as_array()
        .expect("ports")
        .iter()
        .map(|p| p["port"].as_u64().expect("a port"))
        .collect();
    assert!(ports.contains(&3001), "the kept sibling's port is dropped from the shared target: {ports:?}");
    assert!(ports.contains(&3000), "and the forced one's is still there: {ports:?}");
    let grant = rec.sent("PATCH", WS_POLICY);
    let text = grant.last().expect("the ingress grant is written").to_string();
    assert!(text.contains("3001"), "the kept sibling's port is not admitted: {text}");
    assert!(text.contains("3000"), "and the forced one's is: {text}");
}


/// A proxy that WAS serving and has gone NotReady is a restart, not a handover. Handing the
/// service back costs a StatefulSet scale-up and then a re-take — two real gaps — for a pod that
/// is back in seconds, so the selector stays on it for the grace.
#[tokio::test]
async fn a_restarting_proxy_keeps_the_service_within_the_grace() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1-0", ready_pod(600)),
        kloudlite_workspaces::kube_test::get(PROXY_POD, proxy_pod_since(false, 5)),
    ]);
    let (ctx, rec) = intercept_ctx(tmp.path(), routes);
    let mut e = intercept_env(one_intercept(), Some("ws-1"));
    e.status.as_mut().unwrap().service_status[0].proxy = Some("ready".into());

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    assert!(!rec.calls().iter().any(|c| c == &format!("PATCH {WEB_SVC}")), "the selector stays on the proxy: {:?}", rec.calls());
    assert_eq!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "and the real service stays down");
    assert_eq!(status_proxy(&rec).as_deref(), Some("starting"));
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["serviceStatus"][0]["interceptedBy"], "ws-1", "still in force");
}


/// The other side of that clock: a proxy NotReady for longer than the grace is not coming back on
/// its own, and the service goes back to its own pods — a hold that never expires is a service
/// that never returns.
#[tokio::test]
async fn a_proxy_notready_past_the_grace_hands_the_service_back() {
    let tmp = env_tmp();
    let routes = intercept_routes(vec![
        kloudlite_workspaces::kube_test::get(WS_OBJ, attached_ws("running", Some("env-1"), 600)),
        kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1-0", ready_pod(600)),
        kloudlite_workspaces::kube_test::get(PROXY_POD, proxy_pod_since(false, 600)),
    ]);
    let (ctx, rec) = intercept_ctx(tmp.path(), routes);
    let mut e = intercept_env(one_intercept(), Some("ws-1"));
    e.status.as_mut().unwrap().service_status[0].proxy = Some("ready".into());

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    let svc = rec.sent("PATCH", WEB_SVC);
    assert_eq!(svc.last().unwrap()["spec"]["selector"]["kloudlite.io/service"], "web", "{:?}", svc.last());
    assert_ne!(rec.sent("PATCH", WEB_STS).last().unwrap()["spec"]["replicas"], 0, "its own pods come back");
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert!(st.last().unwrap()["status"]["serviceStatus"][0]["interceptedBy"].is_null(), "{:?}", st.last());
}
