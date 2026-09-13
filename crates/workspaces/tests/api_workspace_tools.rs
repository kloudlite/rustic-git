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
