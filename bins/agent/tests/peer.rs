//! The agent's peer listener (`peer::router`), driven directly with `tower::ServiceExt::oneshot`
//! — no real socket, no real btrfs. The send command is a fake script so the router's logic
//! (auth, path validation, streaming) is testable on this Mac.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustic_git_agent::peer::{router, PeerState};
use rustic_git_workspaces::kube_test::{mock_client, Recorder, Route};
use std::os::unix::fs::PermissionsExt;
use tower::util::ServiceExt;

fn state(pool: &std::path::Path, btrfs_bin: String, routes: Vec<Route>) -> (PeerState, Recorder) {
    let (client, rec) = mock_client(routes);
    (PeerState::new(client, pool.to_string_lossy().into(), "node-b".into(), "s3cret".into(), btrfs_bin), rec)
}

// -------------------------------------------------------------------------------------------
// GET /peer/v1/commit/{volume}/{name} — the pull side's send.
// -------------------------------------------------------------------------------------------

/// A misconfigured empty `WS_PEER_SECRET` must not authenticate a request with no header at all
/// — `secret_ok`'s `unwrap_or("")` would otherwise compare two empty strings and let it through.
/// Unreachable via `lib.rs` today (it only spawns this listener when the secret is non-empty),
/// but the guard belongs at the boundary that checks it, not at the one caller that happens to.
#[tokio::test]
async fn an_empty_configured_secret_authenticates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (client, _rec) = mock_client(vec![]);
    let state = PeerState::new(client, tmp.path().to_string_lossy().into(), "node-b".into(), String::new(), "btrfs".into());
    let app = router(state);

    for header in [None, Some(""), Some("anything")] {
        let resp = app.clone().oneshot(commit_req("/peer/v1/commit/vol-1/vol-1-abcd1234", header)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "header={header:?}");
    }
}

/// A fake `btrfs send`: understands `send -q [-p PARENT] PATH`, writes fixed bytes to stdout so a
/// test can assert the stream actually carried them.
fn fake_btrfs_send(dir: &std::path::Path) -> String {
    let path = dir.join("btrfs-send");
    let script = r#"#!/bin/sh
if [ "$1" = "send" ]; then
    printf 'snapshot-bytes'
    exit 0
fi
"#;
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

fn commit_req(path: &str, secret: Option<&str>) -> Request<Body> {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(s) = secret {
        req = req.header("x-peer-secret", s);
    }
    req.body(Body::empty()).unwrap()
}

/// Auth is checked before anything about the path — same order the module comment on `commit`
/// documents, and the same rule the receive side already proves.
#[tokio::test]
async fn commit_get_refuses_a_wrong_or_missing_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send(tmp.path());
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    for secret in [None, Some("wrong")] {
        let resp = app.clone().oneshot(commit_req("/peer/v1/commit/vol-1/vol-1-abcd1234", secret)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "secret={secret:?}");
    }
}

/// A path segment that fails `valid_segment` (here: a `..` traversal attempt) must be refused
/// before any filesystem path is built from it, whether it's the volume, the commit name, or the
/// `parent` query parameter.
#[tokio::test]
async fn commit_get_refuses_invalid_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send(tmp.path());
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    for path in ["/peer/v1/commit/..%2f..%2fetc/name", "/peer/v1/commit/vol-1/..%2f..%2fetc"] {
        let resp = app.clone().oneshot(commit_req(path, Some("s3cret"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{path}");
    }

    let resp = app.oneshot(commit_req("/peer/v1/commit/vol-1/vol-1-abcd1234?parent=..%2f..%2fetc", Some("s3cret"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "invalid parent segment");
}

/// The commit subvolume is simply absent — the ordinary "nothing here yet" case, not a server
/// error.
#[tokio::test]
async fn commit_get_404s_when_the_commit_subvolume_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send(tmp.path());
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    let resp = app.oneshot(commit_req("/peer/v1/commit/vol-1/vol-1-abcd1234", Some("s3cret"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A clean request against an existing commit streams `btrfs send`'s stdout back verbatim.
#[tokio::test]
async fn commit_get_streams_the_send_output() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send(tmp.path());
    std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-abcd1234")).unwrap();
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    let resp = app.oneshot(commit_req("/peer/v1/commit/vol-1/vol-1-abcd1234", Some("s3cret"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"snapshot-bytes");
}

// -------------------------------------------------------------------------------------------
// POST /peer/v1/wake — the poke that makes a peer pull now.
// -------------------------------------------------------------------------------------------

/// The wake route is authenticated exactly like the commit route, and answers 204 with no body:
/// it is a poke, not a transfer. An unauthenticated wake is a 401, or any pod on the cluster
/// could drive every agent's pull beat at will.
#[tokio::test]
async fn wake_requires_the_peer_secret_and_answers_204() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, _rec) = state(tmp.path(), "btrfs".into(), vec![]);
    let notify = state.pull_wake.clone();
    let app = router(state);

    let req = |secret: Option<&str>| {
        let mut req = Request::builder().method("POST").uri("/peer/v1/wake");
        if let Some(s) = secret {
            req = req.header("x-peer-secret", s);
        }
        req.body(Body::empty()).unwrap()
    };
    let bad = app.clone().oneshot(req(None)).await.unwrap();
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);

    let ok = app.oneshot(req(Some("s3cret"))).await.unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), notify.notified()).await.is_ok(),
        "an authenticated wake fires the pull notify"
    );
}
