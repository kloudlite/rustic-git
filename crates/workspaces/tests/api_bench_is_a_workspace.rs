//! The two edges of "a bench is a Workspace": the name `bench` belongs to the bench alone, and no
//! admin surface ever shows one.

mod common;
use common::{admin_token, token};

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{ApiState, router};
use kloudlite_workspaces::kube_test::{Route, get, mock_client, not_found};
use serde_json::{Value, json};
use std::sync::Arc;

const API: &str = "/apis/kloudlite.io/v1alpha1";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
}

async fn serve(admin: bool, routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, _rec) = mock_client(routes);
    let state = Arc::new(ApiState::new(jwt.clone()).with_kube(client));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = if admin { kloudlite_workspaces::api::admin::router(state) } else { router(state) };
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt }
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn ws_obj(name: &str, owner: &str, bench: bool) -> Value {
    let mut o = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"kloudlite.io/owner": owner}},
        "spec": {"owner": owner, "team": "", "name": name, "region": "centralindia",
                 "image": "img:1", "desiredState": "running", "packages": [],
                 "resources": {"cpuRequest": "2", "cpuLimit": "4", "memoryRequest": "4Gi", "memoryLimit": "8Gi"},
                 "storage": {"quotaGb": 20}}
    });
    if bench {
        o["spec"]["bench"] = json!({"model": "sonnet"});
    }
    o
}

/// `bench` is the bench workspace's own name in every (owner, team), and the folder
/// `~/workspaces/bench` is its own. `refuse_taken_name` only catches this once a bench exists, so
/// the reservation is its own refusal — and it is 422 with the exact sentence the web shows.
#[tokio::test]
async fn an_ordinary_create_may_not_take_the_name_bench() {
    let s = serve(false, vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "bench", "region": "centralindia", "quota_gb": 5}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 422);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "the name bench is the bench's");
}

/// The admin Owner detail page reads `workspaces::ws_for_owner`, which filters through
/// `visible()`: an owner's bench is not a workspace anyone made and must never be listed — or
/// stopped and deleted — from the console.
#[tokio::test]
async fn an_owners_admin_detail_lists_the_workspace_and_not_the_bench() {
    let routes = vec![
        not_found(format!("{API}/quotas/karthik")),
        get(
            format!("{API}/workspaces"),
            list_of("Workspace", vec![ws_obj("ws-1", "karthik", false), ws_obj("bench-karthik", "karthik", true)]),
        ),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        get(format!("{API}/quotarequests"), list_of("QuotaRequest", vec![])),
    ];
    let s = serve(true, routes).await;
    let body: Value = reqwest::Client::new()
        .get(format!("{}/admin/owners/karthik", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap()
        .json().await.unwrap();
    let ws = body["workspaces"].as_array().expect("a list");
    assert_eq!(ws.len(), 1, "{body}");
    assert_eq!(ws[0]["id"], "ws-1");
}
