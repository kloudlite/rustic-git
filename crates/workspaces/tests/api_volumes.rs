//! `/v1/volumes` browse routes, against an in-process stub of the vol-agent history surface
//! (`bins/server/src/vol_agent.rs`'s `GET /vol-agent/{owner}/{name}/history`) — this crate can't
//! see `bins/server`'s router, so the stub reimplements just enough of that one route's contract
//! (bearer-token check, JSON array of `CommitRecord`) to exercise `RegistryClient::get_history`
//! for real over HTTP, the same way `ApiState::registry` calls it in production.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::registry::CommitRecord;
use rustic_git_workspaces::registry_client::RegistryClient;
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const AGENT_TOKEN: &str = "vol-agent-test-token";

struct StubRegistry {
    // owner/name -> records, newest first.
    history: std::collections::HashMap<(String, String), Vec<CommitRecord>>,
}

async fn stub_history(
    State(s): State<Arc<StubRegistry>>,
    Path((owner, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let presented = rustic_git_core::httpx::bearer_token(&headers).unwrap_or("");
    if presented != AGENT_TOKEN {
        return (StatusCode::UNAUTHORIZED, "bad agent token").into_response();
    }
    let records = s.history.get(&(owner, name)).cloned().unwrap_or_default();
    Json(records).into_response()
}

async fn spawn_stub_registry(history: std::collections::HashMap<(String, String), Vec<CommitRecord>>) -> String {
    let state = Arc::new(StubRegistry { history });
    let app: Router = Router::new()
        .route("/vol-agent/{owner}/{name}/history", get(stub_history))
        .with_state(state);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

struct Server {
    base: String,
    store: Arc<MemStore>,
    jwt: Arc<Jwt>,
}

async fn server(registry_base: Option<String>) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(store.clone() as Arc<dyn MetaStore>, jwt.clone(), HashSet::new());
    if let Some(base) = registry_base {
        state = state.with_registry(RegistryClient::new(base, AGENT_TOKEN));
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), store, jwt }
}

fn token(jwt: &Jwt, email: &str) -> String {
    jwt.mint(email, "Test User", None).unwrap()
}

async fn ws(store: &MemStore, id: &str, owner: &str) {
    store
        .create_ws(&rustic_git_workspaces::model::Workspace {
            id: id.into(),
            owner: owner.into(),
            name: id.into(),
            region: "centralindia".into(),
            state: rustic_git_workspaces::model::WsState::Ready,
            placement: None,
            volume: Some(format!("vol/{owner}/{id}")),
            quota_gb: 20,
            live_state: json!({}),
        })
        .await
        .unwrap();
}

fn record(id: &str, created_at: chrono::DateTime<chrono::Utc>) -> CommitRecord {
    CommitRecord { id: id.into(), state: json!({}), lineage: vec![], region: "centralindia".into(), message: None, created_at }
}

#[tokio::test]
async fn history_round_trips_newest_first() {
    let now = chrono::Utc::now();
    let mut history = std::collections::HashMap::new();
    history.insert(
        ("karthik@example.com".to_string(), "ws-1".to_string()),
        vec![
            record("c2", now),
            record("c1", now - chrono::Duration::minutes(5)),
        ],
    );
    let reg_base = spawn_stub_registry(history).await;
    let s = server(Some(reg_base)).await;
    ws(&s.store, "ws-1", "karthik@example.com").await;
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let records: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["id"], "c2");
    assert_eq!(records[1]["id"], "c1");
}

#[tokio::test]
async fn refs_reports_the_newest_commit_as_main() {
    let now = chrono::Utc::now();
    let mut history = std::collections::HashMap::new();
    history.insert(("karthik@example.com".to_string(), "ws-1".to_string()), vec![record("c2", now)]);
    let reg_base = spawn_stub_registry(history).await;
    let s = server(Some(reg_base)).await;
    ws(&s.store, "ws-1", "karthik@example.com").await;
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/refs", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let refs: Value = resp.json().await.unwrap();
    assert_eq!(refs["main"], "c2");
}

#[tokio::test]
async fn cross_owner_history_read_is_not_found() {
    let reg_base = spawn_stub_registry(Default::default()).await;
    let s = server(Some(reg_base)).await;
    ws(&s.store, "ws-1", "alice@example.com").await;
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn unauthorized_without_a_token() {
    let reg_base = spawn_stub_registry(Default::default()).await;
    let s = server(Some(reg_base)).await;
    let resp = reqwest::Client::new().get(format!("{}/v1/volumes/ws-1/history", s.base)).send().await.unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn volumes_list_includes_workspaces_and_environments() {
    let s = server(None).await;
    ws(&s.store, "ws-1", "karthik@example.com").await;
    s.store
        .create_env(&rustic_git_workspaces::model::Environment {
            id: "env-1".into(),
            owner: "karthik@example.com".into(),
            name: "env-1".into(),
            region: "centralindia".into(),
            state: rustic_git_workspaces::model::EnvState::Running,
            placement: None,
            volume: None,
            services: vec![],
        })
        .await
        .unwrap();
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new().get(format!("{}/v1/volumes", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().any(|v| v["name"] == "ws-1" && v["kind"] == "workspace"));
    assert!(list.iter().any(|v| v["name"] == "env-1" && v["kind"] == "environment"));
}

/// No registry configured (dev/local) — the two per-volume browse routes answer 503, not a 404
/// that would read as "this feature doesn't exist". `/v1/volumes` itself needs no registry (it
/// only reads Cosmos), so it still works.
#[tokio::test]
async fn volume_history_without_a_configured_registry_is_503() {
    let s = server(None).await;
    ws(&s.store, "ws-1", "karthik@example.com").await;
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}
