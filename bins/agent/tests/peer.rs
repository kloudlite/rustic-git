//! The agent's peer listener (`peer::router`), driven directly with `tower::ServiceExt::oneshot`
//! — no real socket, no real btrfs. The receive command is a fake script so the router's logic
//! (auth, path validation, before/after diffing, status widening) is testable on this Mac.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustic_git_agent::peer::{router, PeerState};
use rustic_git_workspaces::kube_test::{mock_client, Recorder, Route};
use std::os::unix::fs::PermissionsExt;
use tower::util::ServiceExt;

const WS_STATUS: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1/status";
const WS_GET: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1";

fn workspace_json(compatible: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1",
        "kind": "Workspace",
        "metadata": {"name": "ws-1", "uid": "uid-1", "generation": 1},
        "spec": {"owner": "alice", "name": "ws-1", "region": "r1", "image": "img", "desiredState": "running", "packages": []},
        "status": {"phase": "ready", "nodeName": "node-a", "compatibleNodes": compatible},
    })
}

/// Writes a fake `btrfs` to `dir`, understanding just the two subcommands `replicate` needs:
/// `receive <target>` (consumes stdin, creates `<target>/snap-recv`, exits `exit_code`) and
/// `subvolume delete <path>` (plain `rm -rf`, standing in for the real btrfs ioctl).
fn fake_btrfs(dir: &std::path::Path, exit_code: i32) -> String {
    let path = dir.join("btrfs");
    let script = format!(
        r#"#!/bin/sh
set -e
if [ "$1" = "receive" ]; then
    cat >/dev/null
    mkdir -p "$2/snap-recv"
    exit {exit_code}
elif [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    rm -rf "$3"
fi
"#
    );
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

fn state(pool: &std::path::Path, btrfs_bin: String, routes: Vec<Route>) -> (PeerState, Recorder) {
    let (client, rec) = mock_client(routes);
    (PeerState { client, pool: pool.to_string_lossy().into(), node: "node-b".into(), secret: "s3cret".into(), btrfs_bin }, rec)
}

/// A wrong or missing secret must be refused before the request body is read: the body is a
/// root-run `btrfs receive`, and auth is the only thing between the network and it.
#[tokio::test]
async fn every_peer_route_requires_the_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs(tmp.path(), 0);
    let (state, _rec) = state(tmp.path(), bin, vec![Route { method: "GET", path: WS_GET.into(), status: 404, body: serde_json::json!({}) }]);
    let app = router(state);

    for (method, path) in [("GET", "/peer/v1/snapshots/alice/ws-1"), ("POST", "/peer/v1/replicate/alice/ws-1")] {
        for header in [None, Some("wrong")] {
            let mut req = Request::builder().method(method).uri(path);
            if let Some(h) = header {
                req = req.header("x-peer-secret", h);
            }
            let resp = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {path} header={header:?}");
        }
    }

    let resp = app
        .clone()
        .oneshot(Request::builder().method("GET").uri("/peer/v1/snapshots/alice/ws-1").header("x-peer-secret", "s3cret").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A receive that dies mid-stream must leave nothing: a partial subvolume advertised in
/// compatibleNodes is a node that claims a workspace it cannot start.
#[tokio::test]
async fn a_failed_receive_deletes_its_partial_and_writes_no_status() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs(tmp.path(), 1);
    let (state, rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/peer/v1/replicate/alice/ws-1")
                .header("x-peer-secret", "s3cret")
                .body(Body::from(vec![0u8; 16]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!tmp.path().join("repl/ws-1/snap-recv").exists(), "the partial subvolume must be deleted");
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "a failed receive must write no status");
    assert!(rec.calls().is_empty(), "a failed receive must not touch the API server at all");
}

/// A clean receive widens the parent's `compatibleNodes` to include this node.
#[tokio::test]
async fn a_clean_receive_widens_the_parents_compatible_nodes() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs(tmp.path(), 0);
    let ws_get = Route { method: "GET", path: WS_GET.into(), status: 200, body: workspace_json(&["node-a"]) };
    let patch = Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: workspace_json(&["node-a", "node-b"]) };
    let (state, rec) = state(tmp.path(), bin, vec![ws_get, patch]);
    let app = router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/peer/v1/replicate/alice/ws-1")
                .header("x-peer-secret", "s3cret")
                .body(Body::from(vec![0u8; 16]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(tmp.path().join("repl/ws-1/snap-recv").exists(), "the received subvolume stays");

    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "exactly one status write");
    assert_eq!(sent[0]["status"]["compatibleNodes"], serde_json::json!(["node-a", "node-b"]));
}
