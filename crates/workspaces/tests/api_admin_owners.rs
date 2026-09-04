//! `GET /admin/owners`, `GET /admin/owners/{slug}` and the required-note gate on
//! `PUT /admin/quota/{owner}` — `api::admin::router` against a mocked kube API, same harness
//! shape `api_admin.rs`/`api_admin_audit.rs` use.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{admin::router, ApiState};
use rustic_git_workspaces::kube_test::{get, mock_client, not_found, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
}

async fn admin_server(routes: Vec<Route>) -> Server {
    admin_server_with_keys(routes, None).await
}

async fn admin_server_with_keys(routes: Vec<Route>, keys: Option<Arc<rustic_git_storage::store::Store>>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(jwt.clone());
    let (client, _rec) = mock_client(routes);
    state = state.with_kube(client);
    if let Some(k) = keys {
        state = state.with_keys(k);
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt }
}

async fn keys_store() -> Arc<rustic_git_storage::store::Store> {
    let tmp = tempfile::tempdir().unwrap();
    Arc::new(
        rustic_git_storage::store::Store::open(Arc::new(object_store::memory::InMemory::new()), tmp.path().join("cache"), false)
            .await
            .unwrap(),
    )
}

fn admin_token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn ws_obj(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"owner": owner, "team": "", "name": name, "region": "centralindia",
                 "image": "img:1", "desiredState": "running", "packages": [],
                 "resources": {"cpuRequest": "2", "cpuLimit": "4", "memoryRequest": "4Gi", "memoryLimit": "8Gi"},
                 "storage": {"quotaGb": 20}}
    })
}

fn req_obj(name: &str, owner: &str, state: Option<&str>) -> Value {
    let mut o = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "QuotaRequest",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"owner": owner, "requested": {"workspaces": 10}, "reason": "more room"}
    });
    if let Some(st) = state {
        o["status"] = json!({"state": st, "decidedBy": "root", "decidedAt": "2026-09-04T00:00:00Z"});
    }
    o
}

/// `usage_all`'s replacement is wider: an owner with a live `Workspace` and neither a `Quota` nor
/// a `QuotaRequest` still shows up, riding the default column.
#[tokio::test]
async fn owners_list_includes_an_owner_with_only_live_objects_and_no_quota_or_request() {
    let routes = vec![
        get(format!("{API}/quotas"), list_of("Quota", vec![])),
        get(format!("{API}/quotarequests"), list_of("QuotaRequest", vec![])),
        get(format!("{API}/workspaces"), list_of("Workspace", vec![ws_obj("ws-1", "carol")])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
    ];
    let s = admin_server(routes).await;
    let body: Value = reqwest::Client::new()
        .get(format!("{}/admin/owners", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap()
        .json().await.unwrap();
    let rows = body.as_array().expect("a list");
    let carol = rows.iter().find(|r| r["owner"] == "carol").expect("carol listed");
    assert_eq!(carol["source"], "default");
    assert_eq!(carol["pending"], false);
}

/// The detail composes usage/limit, the owner's own workspaces/environments/volumes, its last
/// requests and its audit rows — each scoped to that owner only, a second owner's rows never
/// leaking in.
#[tokio::test]
async fn owner_detail_composes_usage_limit_objects_requests_and_audit() {
    let routes = vec![
        not_found(format!("{API}/quotas/acme")),
        get(
            format!("{API}/workspaces"),
            list_of("Workspace", vec![ws_obj("ws-1", "acme"), ws_obj("ws-2", "someone-else")]),
        ),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        get(
            format!("{API}/quotarequests"),
            list_of("QuotaRequest", vec![
                req_obj("qr-acme", "acme", Some("approved")),
                req_obj("qr-other", "someone-else", Some("approved")),
            ]),
        ),
    ];
    let keys = keys_store().await;
    let entry = rustic_git_workspaces::audit::AuditEntry {
        ts: "2026-09-04T00:00:00Z".into(),
        actor: "root".into(),
        action: "set-quota".into(),
        target: "acme".into(),
        reason: Some("initial grant".into()),
        result: "ok".into(),
    };
    rustic_git_workspaces::audit::record(&keys.os, &entry).await.unwrap();
    let s = admin_server_with_keys(routes, Some(keys)).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{}/admin/owners/acme", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap()
        .json().await.unwrap();

    assert_eq!(body["owner"], "acme");
    assert_eq!(body["source"], "default");
    let workspaces = body["workspaces"].as_array().unwrap();
    assert_eq!(workspaces.len(), 1, "{body}");
    assert_eq!(workspaces[0]["id"], "ws-1");

    let requests = body["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 1, "{body}");
    assert_eq!(requests[0]["owner"], "acme");

    let audit = body["audit"].as_array().unwrap();
    assert_eq!(audit.len(), 1, "{body}");
    assert_eq!(audit[0]["target"], "acme");
}

/// A missing note on a quota write is a 422, not a silent no-reason write — no kube call is even
/// reachable, so this needs no routes.
#[tokio::test]
async fn write_quota_without_a_note_is_422() {
    let s = admin_server(vec![]).await;
    let resp = reqwest::Client::new()
        .put(format!("{}/admin/quota/acme", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"spec": {"workspaces": 5, "environments": 5, "snapshots": 20, "diskGb": 100, "cpu": 8, "memoryGb": 32}, "note": ""}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 422);
}
