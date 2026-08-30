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

/// A fake `btrfs receive` whose outcome depends on call order rather than a fixed exit code: the
/// Nth invocation (tracked via `seqfile`, since two requests share one process) creates a
/// uniquely-named subvolume `snap-N` and succeeds only for N == 1. Used to prove the per-id lock
/// serializes two overlapping receives rather than to test the receive logic itself.
fn fake_btrfs_seq(dir: &std::path::Path, seqfile: &std::path::Path) -> String {
    let path = dir.join("btrfs-seq");
    let script = format!(
        r#"#!/bin/sh
set -e
if [ "$1" = "receive" ]; then
    cat >/dev/null
    n=$(( $(cat "{seqfile}" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "{seqfile}"
    mkdir -p "$2/snap-$n"
    [ "$n" = "1" ]
elif [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    rm -rf "$3"
fi
"#,
        seqfile = seqfile.display()
    );
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

fn state(pool: &std::path::Path, btrfs_bin: String, routes: Vec<Route>) -> (PeerState, Recorder) {
    let (client, rec) = mock_client(routes);
    (PeerState::new(client, pool.to_string_lossy().into(), "node-b".into(), "s3cret".into(), btrfs_bin), rec)
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

/// A misconfigured empty `WS_PEER_SECRET` must not authenticate a request with no header at all
/// — `secret_ok`'s `unwrap_or("")` would otherwise compare two empty strings and let it through.
/// Unreachable via `lib.rs` today (it only spawns this listener when the secret is non-empty),
/// but the guard belongs at the boundary that checks it, not at the one caller that happens to.
#[tokio::test]
async fn an_empty_configured_secret_authenticates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs(tmp.path(), 0);
    let (client, _rec) = mock_client(vec![]);
    let state = PeerState::new(client, tmp.path().to_string_lossy().into(), "node-b".into(), String::new(), bin);
    let app = router(state);

    for header in [None, Some(""), Some("anything")] {
        let mut req = Request::builder().method("GET").uri("/peer/v1/snapshots/alice/ws-1");
        if let Some(h) = header {
            req = req.header("x-peer-secret", h);
        }
        let resp = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "header={header:?}");
    }
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

/// A retried sender can overlap its retry with the receive it is retrying. Without the per-id
/// lock, the loser's before/after diff races the winner's write to the same `repl/ws-1/` and can
/// `btrfs subvolume delete` the snapshot the winner just landed and advertised in
/// `compatibleNodes`. With the lock, the two receives serialize: exactly one subvolume survives
/// and exactly one status write happens, regardless of which request's task the scheduler runs
/// first.
#[tokio::test]
async fn overlapping_receives_for_one_id_serialize_and_the_losers_cleanup_spares_the_winner() {
    let tmp = tempfile::tempdir().unwrap();
    let seqfile = tmp.path().join("seq");
    let bin = fake_btrfs_seq(tmp.path(), &seqfile);
    let ws_get = Route { method: "GET", path: WS_GET.into(), status: 200, body: workspace_json(&["node-a"]) };
    let patch = Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: workspace_json(&["node-a", "node-b"]) };
    let (state, rec) = state(tmp.path(), bin, vec![ws_get, patch]);
    let app = router(state);
    let app2 = app.clone();

    let mk_req = || {
        Request::builder()
            .method("POST")
            .uri("/peer/v1/replicate/alice/ws-1")
            .header("x-peer-secret", "s3cret")
            .body(Body::from(vec![0u8; 16]))
            .unwrap()
    };
    let (resp_a, resp_b) = tokio::join!(app.oneshot(mk_req()), app2.oneshot(mk_req()));
    let statuses = [resp_a.unwrap().status(), resp_b.unwrap().status()];
    assert!(statuses.contains(&StatusCode::OK), "{statuses:?}");
    assert!(statuses.contains(&StatusCode::INTERNAL_SERVER_ERROR), "{statuses:?}");

    let repl_dir = tmp.path().join("repl/ws-1");
    let remaining: Vec<String> =
        std::fs::read_dir(&repl_dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(remaining, vec!["snap-1"], "the loser's cleanup must not remove the winner's snapshot");
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "only the winning receive widens compatibleNodes");
}
