//! `/v1/volumes` browse routes, against the mocked API server.
//!
//! History and refs no longer cross tiers: the INDEX of a volume's snapshots is a label list of
//! `done` SnapshotRequests, and the bytes those records name still live on the server tier. So
//! this file has no registry stub at all any more — only the cluster.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::kube_test::{get as kget, mock_client, Recorder, Route};
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

fn ws_obj(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name},
        "spec": {
            "owner": owner, "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": "running"
        },
        "status": {"phase": "ready", "nodeName": NODE, "volumeRef": name}
    })
}

fn vol_obj(name: &str, owner: &str, kind: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner, "rustic-git.io/kind": kind}},
        "spec": {"owner": owner, "nodeName": NODE, "region": "centralindia", "quotaGb": 20}
    })
}

fn vol_list(items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeList", "metadata": {}, "items": items})
}

/// A finished push, which is what a snapshot IS now — the object and its outcome in one place.
fn snap_obj(name: &str, volume: &str, id: &str, at: &str, message: Option<&str>) -> Value {
    let mut v = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": "karthik", "rustic-git.io/volume": volume}},
        "spec": {"volume": volume},
        "status": {"phase": "done", "snapshotId": id, "at": at}
    });
    if let Some(m) = message {
        v["spec"]["message"] = json!(m);
    }
    v
}

fn snap_list(items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequestList", "metadata": {}, "items": items})
}

async fn server(routes: Vec<Route>) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new()).with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

/// The wire shape the web reads has not moved — only where it comes from. `id`, `created_at` and
/// `message` are what the snapshots page renders, and `/history` still answers newest first.
#[tokio::test]
async fn history_lists_done_snapshot_requests_newest_first() {
    let list = snap_list(vec![
        snap_obj("snap-a", "ws-1", "c1", "2026-08-27T09:00:00Z", Some("first")),
        snap_obj("snap-b", "ws-1", "c2", "2026-08-27T10:00:00Z", None),
        // A request still running is a wish, not a snapshot; it must not appear.
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequest",
               "metadata": {"name": "snap-c", "labels": {"rustic-git.io/volume": "ws-1"}},
               "spec": {"volume": "ws-1"}, "status": {"phase": "working"}}),
    ]);
    let s = server(vec![
        kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik")),
        kget(format!("{API}/snapshotrequests"), list),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let records: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(records.len(), 2, "only `done` requests are snapshots: {records:?}");
    assert_eq!(records[0]["id"], "c2");
    assert_eq!(records[1]["id"], "c1");
    assert_eq!(records[1]["message"], "first");
    assert_eq!(records[0]["region"], "centralindia", "the region comes off the workspace");
    assert!(records[0]["created_at"].is_string(), "the web reads created_at: {}", records[0]);
}

#[tokio::test]
async fn refs_reports_the_newest_done_snapshot_as_main() {
    let list = snap_list(vec![snap_obj("snap-b", "ws-1", "c2", "2026-08-27T10:00:00Z", None)]);
    let s = server(vec![
        kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik")),
        kget(format!("{API}/snapshotrequests"), list),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/refs", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["main"], "c2");
}

/// Snapshot records carry no owner check of their own, so this is the only thing standing between a
/// caller and someone else's history.
#[tokio::test]
async fn cross_owner_history_read_is_not_found() {
    let s = server(vec![kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "alice"))]).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(!s.rec.calls().iter().any(|c| c.contains("snapshotrequests")), "refused before any read");
}

#[tokio::test]
async fn unauthorized_without_a_token() {
    let s = server(vec![]).await;
    let resp = reqwest::Client::new().get(format!("{}/v1/volumes/ws-1/history", s.base)).send().await.unwrap();
    assert_eq!(resp.status(), 401);
}

/// "Has this ever been pushed" is a query over `done` SnapshotRequests, not a field on the Volume.
/// A second controller writing the Volume's status would have its field pruned by the Volume
/// reconciler's next force-apply, so the answer lives where the writer is.
#[tokio::test]
async fn only_a_volume_with_a_done_snapshot_reports_a_registry_pointer() {
    let routes = vec![
        kget(
            format!("{API}/volumes"),
            vol_list(vec![vol_obj("ws-1", "karthik", "workspace"), vol_obj("env-1", "karthik", "environment")]),
        ),
        kget(
            format!("{API}/snapshotrequests"),
            snap_list(vec![snap_obj("snap-a", "ws-1", "c1", "2026-08-27T09:00:00Z", None)]),
        ),
    ];
    let s = server(routes).await;
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
    // ONE list, not one per row.
    assert_eq!(
        s.rec.calls().iter().filter(|c| c.contains("snapshotrequests")).count(),
        1,
        "the pushed-set is one label list: {:?}",
        s.rec.calls()
    );
}
