//! `GET /v1/workspaces/{id}/tools`: the owner alone, only while Ready, never by label or claim.

mod common;
use common::{admin_token_as, token};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState};
use kloudlite_workspaces::kube_test::{get, mock_client, not_found, Route};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const API: &str = "/apis/kloudlite.io/v1alpha1";

struct T {
    state: Arc<ApiState>,
    jwt: Arc<Jwt>,
}

fn setup(routes: Vec<Route>) -> T {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, _rec) = mock_client(routes);
    let state = Arc::new(ApiState::new(jwt.clone()).with_kube(client));
    T { state, jwt }
}

impl T {
    async fn call(&self, uri: &str, tok: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = router(self.state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, v)
    }
    fn tok(&self, who: &str) -> String {
        token(&self.jwt, who)
    }
}

fn ws_path(id: &str) -> String {
    format!("{API}/workspaces/{id}")
}

fn ws_obj(id: &str, owner: &str, team: &str, phase: &str, pod_ref: Option<&str>) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": id},
        "spec": {"owner": owner, "team": team, "name": "api", "region": "r1", "image": "i",
                  "desiredState": "running",
                  "resources": {"cpuRequest": "1", "cpuLimit": "1", "memoryRequest": "1Gi", "memoryLimit": "1Gi"}},
        "status": { "phase": phase, "podRef": pod_ref }
    })
}

fn pod(ip: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws"}, "status": {"podIP": ip}})
}

#[tokio::test]
async fn the_tool_address_is_the_owners_alone_and_only_while_ready() {
    let w1 = ws_path("w1");
    let pod_path = "/api/v1/namespaces/wt-alice-acme/pods/ws".to_string();

    // alice GET /v1/workspaces/w1/tools -> 200, body == {"address":"10.42.0.9:7788"}
    let t = setup(vec![
        get(w1.clone(), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws"))),
        get(pod_path.clone(), pod("10.42.0.9")),
    ]);
    let (st, body) = t.call("/v1/workspaces/w1/tools", &t.tok("alice")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"address": "10.42.0.9:7788"}));

    // bob, a member of acme, same request -> 404
    let t = setup(vec![get(w1.clone(), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws")))]);
    let (st, _) = t.call("/v1/workspaces/w1/tools", &t.tok("bob")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // a superadmin who is not alice -> 404
    let t = setup(vec![get(w1.clone(), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws")))]);
    let (st, _) = t.call("/v1/workspaces/w1/tools", &admin_token_as(&t.jwt, "root")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // alice GET /v1/workspaces/w1/tools?team=labs -> 409, error contains "is in team acme"
    let t = setup(vec![get(w1.clone(), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws")))]);
    let (st, body) = t.call("/v1/workspaces/w1/tools?team=labs", &t.tok("alice")).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("is in team acme"), "{body}");

    // alice ?team=acme -> 200
    let t = setup(vec![
        get(w1.clone(), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws"))),
        get(pod_path.clone(), pod("10.42.0.9")),
    ]);
    let (st, _) = t.call("/v1/workspaces/w1/tools?team=acme", &t.tok("alice")).await;
    assert_eq!(st, StatusCode::OK);

    // re-seed w1 with phase Stopped; alice -> 409, error == "workspace api is stopped; start it to run tools"
    let t = setup(vec![get(w1.clone(), ws_obj("w1", "alice", "acme", "stopped", Some("wt-alice-acme/ws")))]);
    let (st, body) = t.call("/v1/workspaces/w1/tools", &t.tok("alice")).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["error"], "workspace api is stopped; start it to run tools");

    // re-seed Ready with the Pod absent; alice -> 409, error contains "between pods"
    let t = setup(vec![
        get(w1.clone(), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws"))),
        not_found(pod_path.clone()),
    ]);
    let (st, body) = t.call("/v1/workspaces/w1/tools", &t.tok("alice")).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("between pods"), "{body}");
}

/// The address alone was not enough to USE the tool server: it requires the workspace's own
/// token, and a caller in another pod (the bench) cannot read the file the token is projected
/// into. So this route hands over both — it is already owner-only, already the one place a
/// caller learns where the server is, and the token is worth exactly what the address is.
///
/// Read back from the Secret the keys beat wrote, never minted here: the tool server compares
/// against the FILE in the pod, so a freshly minted token would be a valid JWT that the server
/// refuses — the most confusing possible failure.
#[tokio::test]
async fn the_tools_route_hands_over_the_token_the_pod_actually_holds() {
    let secret_path = "/api/v1/namespaces/wt-alice-acme/secrets/user-key".to_string();
    let t = setup(vec![
        get(ws_path("w1"), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws"))),
        get("/api/v1/namespaces/wt-alice-acme/pods/ws".to_string(), pod("10.42.0.9")),
        get(secret_path.clone(), secret(Some("THE-POD-TOKEN"))),
    ]);
    let (st, body) = t.call("/v1/workspaces/w1/tools", &t.tok("alice")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"address": "10.42.0.9:7788", "token": "THE-POD-TOKEN"}));
}

/// A workspace whose Secret has not been written yet — a pod that started before the keys beat
/// caught up. The address still answers, without a `token` key: a caller that got `null` would
/// send the word "null" as a bearer and read the 401 as a rejected credential rather than as one
/// that is not there yet.
#[tokio::test]
async fn a_token_the_beat_has_not_written_yet_is_absent_not_null() {
    for missing in [None, Some("")] {
        let t = setup(vec![
            get(ws_path("w1"), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws"))),
            get("/api/v1/namespaces/wt-alice-acme/pods/ws".to_string(), pod("10.42.0.9")),
            get("/api/v1/namespaces/wt-alice-acme/secrets/user-key".to_string(), secret(missing)),
        ]);
        let (st, body) = t.call("/v1/workspaces/w1/tools", &t.tok("alice")).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"address": "10.42.0.9:7788"}), "{missing:?}");
    }
    // And a Secret that is not there at all: the same answer, never a 500.
    let t = setup(vec![
        get(ws_path("w1"), ws_obj("w1", "alice", "acme", "ready", Some("wt-alice-acme/ws"))),
        get("/api/v1/namespaces/wt-alice-acme/pods/ws".to_string(), pod("10.42.0.9")),
        not_found("/api/v1/namespaces/wt-alice-acme/secrets/user-key"),
    ]);
    let (st, body) = t.call("/v1/workspaces/w1/tools", &t.tok("alice")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"address": "10.42.0.9:7788"}));
}

/// A Secret as the API SERVER answers one: `data`, base64. Built through the typed `Secret` so the
/// encoding is `k8s-openapi`'s own — a fixture that hand-wrote the plain string would let a
/// decoding bug in the route pass.
fn secret(token: Option<&str>) -> Value {
    use k8s_openapi::api::core::v1::Secret;
    use k8s_openapi::ByteString;
    let mut data = std::collections::BTreeMap::new();
    if let Some(t) = token {
        data.insert("workspace-token".to_string(), ByteString(t.as_bytes().to_vec()));
    }
    data.insert("registry-token".to_string(), ByteString(b"OTHER".to_vec()));
    let s = Secret {
        metadata: kube::api::ObjectMeta { name: Some("user-key".into()), ..Default::default() },
        data: Some(data),
        ..Default::default()
    };
    serde_json::to_value(s).unwrap()
}
