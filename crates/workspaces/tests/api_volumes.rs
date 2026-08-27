//! `/v1/volumes` browse routes, against an in-process stub of the vol-agent history surface
//! (`bins/server/src/vol_agent.rs`'s `GET /vol-agent/{owner}/{name}/history`) — this crate can't
//! see `bins/server`'s router, so the stub reimplements just enough of that one route's contract
//! (bearer-token check, JSON array of `CommitRecord`) to exercise `RegistryClient::get_history`
//! for real over HTTP, the same way `ApiState::registry` calls it in production.
//!
//! The ownership check in front of it (`owns_volume`) reads the cluster, so that half runs against
//! the mocked API server.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::kube_test::{get as kget, mock_client, Route};
use rustic_git_workspaces::registry::CommitRecord;
use rustic_git_workspaces::registry_client::RegistryClient;
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const AGENT_TOKEN: &str = "vol-agent-test-token";
const API: &str = "/apis/rustic-git.io/v1alpha1";
const NODE: &str = "node-a";

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
    jwt: Arc<Jwt>,
}

fn ws_obj(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name},
        "spec": {
            "owner": owner, "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "volumeRef": name, "nodeName": NODE, "desiredState": "running"
        }
    })
}

/// `pushed` is what makes the projection report a `volume` pointer: on the CRD side "has this ever
/// been pushed" lives in the Volume's status, not in a doc field.
fn vol_obj(name: &str, owner: &str, kind: &str, pushed: bool) -> Value {
    let mut v = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner, "rustic-git.io/kind": kind}},
        "spec": {"owner": owner, "nodeName": NODE, "region": "centralindia", "quotaGb": 20}
    });
    if pushed {
        v["status"] = json!({"phase": "ready", "lineageTip": "c2"});
    }
    v
}

fn vol_list(items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeList", "metadata": {}, "items": items})
}

async fn server(registry_base: Option<String>, routes: Vec<Route>) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new());
    if let Some(base) = registry_base {
        state = state.with_registry(RegistryClient::new(base, AGENT_TOKEN));
    }
    let (client, _rec) = mock_client(routes);
    state = state.with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt }
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

fn record(id: &str, created_at: chrono::DateTime<chrono::Utc>) -> CommitRecord {
    CommitRecord { id: id.into(), state: json!({}), lineage: vec![], region: "centralindia".into(), message: None, created_at }
}

#[tokio::test]
async fn history_round_trips_newest_first() {
    let now = chrono::Utc::now();
    let mut history = std::collections::HashMap::new();
    history.insert(
        ("karthik".to_string(), "ws-1".to_string()),
        vec![record("c2", now), record("c1", now - chrono::Duration::minutes(5))],
    );
    let reg_base = spawn_stub_registry(history).await;
    let s = server(Some(reg_base), vec![kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik"))]).await;
    let tok = token(&s.jwt, "karthik");

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
    history.insert(("karthik".to_string(), "ws-1".to_string()), vec![record("c2", now)]);
    let reg_base = spawn_stub_registry(history).await;
    let s = server(Some(reg_base), vec![kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik"))]).await;
    let tok = token(&s.jwt, "karthik");

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

/// The registry has no owner check of its own (it trusts an agent token, not a JWT), so this is
/// the only thing standing between a caller and someone else's history.
#[tokio::test]
async fn cross_owner_history_read_is_not_found() {
    let reg_base = spawn_stub_registry(Default::default()).await;
    let s = server(Some(reg_base), vec![kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "alice"))]).await;
    let tok = token(&s.jwt, "karthik");

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
    let s = server(Some(reg_base), vec![]).await;
    let resp = reqwest::Client::new().get(format!("{}/v1/volumes/ws-1/history", s.base)).send().await.unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn volumes_list_reports_kind_and_only_pushed_volumes_have_a_pointer() {
    let routes = vec![kget(
        format!("{API}/volumes"),
        vol_list(vec![vol_obj("ws-1", "karthik", "workspace", true), vol_obj("env-1", "karthik", "environment", false)]),
    )];
    let s = server(None, routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new().get(format!("{}/v1/volumes", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 2);
    let ws = list.iter().find(|v| v["name"] == "ws-1").unwrap();
    assert_eq!(ws["kind"], "workspace");
    assert_eq!(ws["volume"], "vol/karthik/ws-1");
    let env = list.iter().find(|v| v["name"] == "env-1").unwrap();
    assert_eq!(env["kind"], "environment");
    assert!(env["volume"].is_null(), "never pushed means no registry pointer yet");
}

/// No registry configured (dev/local) — the two per-volume browse routes answer 503, not a 404
/// that would read as "this feature doesn't exist".
#[tokio::test]
async fn volume_history_without_a_configured_registry_is_503() {
    let s = server(None, vec![kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik"))]).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}
