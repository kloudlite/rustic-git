//! `/v1` push/history/refs served from the new commit model (`WS_COMMIT_MODEL=1`).
//!
//! A separate test BINARY on purpose: `ApiState::commit_model` is read once from the process
//! environment at construction (mirrors `Ctx.commit_model` on the agent), and `cargo test` gives
//! every `tests/*.rs` file its own process — so setting `WS_COMMIT_MODEL=1` here can never race
//! `api_user.rs`'s flag-off assertions running in parallel in a shared process.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::kube_test::{get, mock_client, Recorder, Route};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";
const NODE: &str = "node-a";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

fn placed_ws(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {
            "owner": owner, "team": "", "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": "running"
        },
        "status": {"phase": "ready", "nodeName": NODE, "compatibleNodes": [NODE], "volumeRef": name}
    })
}

fn placed_ws_with_head(name: &str, owner: &str, head: &str) -> Value {
    let mut w = placed_ws(name, owner);
    w["status"]["head"] = json!(head);
    w
}

fn snapshot(name: &str, volume: &str, owner: &str, worktree: &str, parent: &str, phase: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name},
        "spec": {"volume": volume, "owner": owner, "worktree": worktree, "parent": parent, "pinned": false},
        "status": {"phase": phase},
    })
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

async fn server(routes: Vec<Route>) -> Server {
    std::env::set_var("WS_COMMIT_MODEL", "1");
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new()).with_kube(client);
    assert!(state.commit_model, "WS_COMMIT_MODEL=1 must be read at construction");
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

/// A push under the flag creates a `Working` `Snapshot` naming the workspace as `worktree` and
/// the workspace's current `head` as `parent` — never a `SnapshotRequest`.
#[tokio::test]
async fn push_creates_a_working_snapshot_with_worktree_and_parent() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws_with_head("ws-1", "karthik", "ws-1-aaaaaaaa")),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201, body: snapshot("ws-1-cccccccc", "ws-1", "karthik", "ws-1", "", "working") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .json(&json!({"message": "checkpoint"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());

    let req = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert_eq!(req["spec"]["volume"], "ws-1");
    assert_eq!(req["spec"]["worktree"], "ws-1");
    assert_eq!(req["spec"]["parent"], "ws-1-aaaaaaaa");
    assert_eq!(req["spec"]["message"], "checkpoint");
    assert_eq!(req["spec"]["owner"], "karthik");
    assert_eq!(req["status"]["phase"], "working");
    assert!(!s.rec.calls().iter().any(|c| c.contains("snapshotrequests")), "no SnapshotRequest under the flag");
}

/// A workspace with no recorded head yet (its first push) writes an EMPTY parent — the root of a
/// new chain.
#[tokio::test]
async fn first_push_of_a_workspace_has_no_parent() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201, body: snapshot("ws-1-cccccccc", "ws-1", "karthik", "ws-1", "", "working") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let req = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert_eq!(req["spec"]["parent"], "");
}

/// `/history` walks `Snapshot` CRs, oldest first, parent-linked — no registry round trip.
#[tokio::test]
async fn history_lists_snapshot_crs_in_parent_order() {
    let mut root = snapshot("ws-1-aaaaaaaa", "ws-1", "karthik", "ws-1", "", "ready");
    root["metadata"]["creationTimestamp"] = json!("2026-01-01T00:00:00Z");
    let mut tip = snapshot("ws-1-bbbbbbbb", "ws-1", "karthik", "ws-1", "ws-1-aaaaaaaa", "working");
    tip["metadata"]["creationTimestamp"] = json!("2026-01-02T00:00:00Z");
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [tip, root]}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let rows: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "ws-1-aaaaaaaa");
    assert_eq!(rows[0]["parent"], Value::Null);
    assert_eq!(rows[1]["id"], "ws-1-bbbbbbbb");
    assert_eq!(rows[1]["parent"], "ws-1-aaaaaaaa");
}

/// `/refs` names the newest commit as `main` — same "first = tip" convention the registry path
/// keeps, computed here from creation order instead.
#[tokio::test]
async fn refs_names_the_newest_commit_as_main() {
    let mut root = snapshot("ws-1-aaaaaaaa", "ws-1", "karthik", "ws-1", "", "ready");
    root["metadata"]["creationTimestamp"] = json!("2026-01-01T00:00:00Z");
    let mut tip = snapshot("ws-1-bbbbbbbb", "ws-1", "karthik", "ws-1", "ws-1-aaaaaaaa", "ready");
    tip["metadata"]["creationTimestamp"] = json!("2026-01-02T00:00:00Z");
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [root, tip]}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/refs", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["main"], "ws-1-bbbbbbbb");
}

/// A caller reading a volume that has no `Snapshot` under any owner label they may read gets a
/// 404 — same "not found" the registry path returns for a volume that is not theirs.
#[tokio::test]
async fn history_of_an_unknown_volume_is_not_found() {
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
