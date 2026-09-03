//! The agent's peer listener (`peer::router`), driven directly with `tower::ServiceExt::oneshot`
//! — no real socket, no real btrfs. The send command is a fake script so the router's logic
//! (auth, path validation, streaming) is testable on this Mac.
//!
//! Drives the router with a fake `btrfs send` script — good coverage of auth, `valid_segment` and
//! streaming; zero coverage of the receive half (`pull_one`) against a real `btrfs receive`. That
//! half is only ever exercised by `tests/ws_e2e.sh`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustic_git_agent::peer::{peer_http_client, pull_one, receive_ceiling, router, PeerState};
use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
use rustic_git_workspaces::kube_test::{mock_client, Recorder, Route};
use std::os::unix::fs::PermissionsExt;
use tower::util::ServiceExt;

fn state(pool: &std::path::Path, btrfs_bin: String, routes: Vec<Route>) -> (PeerState, Recorder) {
    let (client, rec) = mock_client(routes);
    (PeerState::new(client, pool.to_string_lossy().into(), "node-b".into(), "s3cret".into(), btrfs_bin), rec)
}

// -------------------------------------------------------------------------------------------
// GET /peer/v1/snapshot/{volume}/{name} — the pull side's send.
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
        let resp = app.clone().oneshot(snapshot_req("/peer/v1/snapshot/vol-1/vol-1-abcd1234", header)).await.unwrap();
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

fn snapshot_req(path: &str, secret: Option<&str>) -> Request<Body> {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(s) = secret {
        req = req.header("x-peer-secret", s);
    }
    req.body(Body::empty()).unwrap()
}

/// Auth is checked before anything about the path — same order the module comment on `snapshot`
/// documents, and the same rule the receive side already proves.
#[tokio::test]
async fn snapshot_get_refuses_a_wrong_or_missing_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send(tmp.path());
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    for secret in [None, Some("wrong")] {
        let resp = app.clone().oneshot(snapshot_req("/peer/v1/snapshot/vol-1/vol-1-abcd1234", secret)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "secret={secret:?}");
    }
}

/// A path segment that fails `valid_segment` (here: a `..` traversal attempt) must be refused
/// before any filesystem path is built from it, whether it's the volume, the snapshot name, or the
/// `parent` query parameter.
#[tokio::test]
async fn snapshot_get_refuses_invalid_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send(tmp.path());
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    for path in ["/peer/v1/snapshot/..%2f..%2fetc/name", "/peer/v1/snapshot/vol-1/..%2f..%2fetc"] {
        let resp = app.clone().oneshot(snapshot_req(path, Some("s3cret"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{path}");
    }

    let resp = app.oneshot(snapshot_req("/peer/v1/snapshot/vol-1/vol-1-abcd1234?parent=..%2f..%2fetc", Some("s3cret"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "invalid parent segment");
}

/// The snapshot subvolume is simply absent — the ordinary "nothing here yet" case, not a server
/// error.
#[tokio::test]
async fn snapshot_get_404s_when_the_snapshot_subvolume_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send(tmp.path());
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    let resp = app.oneshot(snapshot_req("/peer/v1/snapshot/vol-1/vol-1-abcd1234", Some("s3cret"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A clean request against an existing snapshot streams `btrfs send`'s stdout back verbatim.
#[tokio::test]
async fn snapshot_get_streams_the_send_output() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send(tmp.path());
    std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-abcd1234")).unwrap();
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    let resp = app.oneshot(snapshot_req("/peer/v1/snapshot/vol-1/vol-1-abcd1234", Some("s3cret"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"snapshot-bytes");
}

/// A fake `btrfs send` that stalls after its first byte, to model a puller that opens the
/// connection and stops reading.
fn fake_btrfs_send_slow(dir: &std::path::Path) -> String {
    let path = dir.join("btrfs-send-slow");
    let script = r#"#!/bin/sh
if [ "$1" = "send" ]; then
    printf x
    sleep 60
    exit 0
fi
"#;
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

/// I3: the server bounds its own send. A puller that stops reading must not hold the volume's
/// send lock until its own hour-long client timeout — the next puller of the same volume waits
/// behind it, fleet-wide. With a one-second serve timeout, the second request must be served.
#[tokio::test]
async fn a_stalled_puller_does_not_hold_the_volume_send_lock() {
    std::env::set_var("WS_PEER_SERVE_TIMEOUT_SECS", "1");
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_btrfs_send_slow(tmp.path());
    std::fs::create_dir_all(tmp.path().join("vol/v1/snap/c1")).unwrap();
    let (state, _rec) = state(tmp.path(), bin, vec![]);
    let app = router(state);

    // Drives the body to completion (a stalled real puller keeps the connection's write side
    // polled the same way): `oneshot` alone only returns the response headers and drops the body
    // unread, which would free the lock immediately and prove nothing.
    let first = tokio::spawn({
        let app = app.clone();
        async move {
            let resp = app.oneshot(snapshot_req("/peer/v1/snapshot/v1/c1", Some("s3cret"))).await.unwrap();
            axum::body::to_bytes(resp.into_body(), usize::MAX).await
        }
    });
    // Long enough for the first request to have taken the lock, short enough to be inside the
    // fake script's 60 s sleep: the point is that the SECOND request is not blocked behind it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.oneshot(snapshot_req("/peer/v1/snapshot/v1/c1", Some("s3cret"))),
    )
    .await;

    assert!(second.is_ok(), "the second pull must not wait out the first puller's stall");
    let _ = first.await;
    std::env::remove_var("WS_PEER_SERVE_TIMEOUT_SECS");
}

/// Builds a router over a fake `btrfs send` (named by the caller's `label`, so two calls in the
/// same test don't collide on the script's filename) and returns the tempdir so the caller can
/// create snapshot subvolumes under it.
fn router_with_fake_btrfs(label: &str) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(format!("btrfs-send-{label}"));
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
    let (state, _rec) = state(tmp.path(), path.to_string_lossy().into_owned(), vec![]);
    (router(state), tmp)
}

async fn send_request(app: &axum::Router, path: &str) -> axum::http::Response<Body> {
    app.clone().oneshot(snapshot_req(path, Some("s3cret"))).await.unwrap()
}

/// I4: a puller declares the ceiling it will accept, and a source that cannot fit a full send
/// under it says so BEFORE streaming — a 413 is a fetchable answer, a truncated body after a 200
/// is a wasted transfer.
#[tokio::test]
async fn a_ceiling_below_the_volumes_quota_is_refused_with_413() {
    let (app, tmp) = router_with_fake_btrfs("ok");
    std::fs::create_dir_all(tmp.path().join("vol").join("v1").join("snap").join("c1")).unwrap();

    let resp = send_request(&app, "/peer/v1/snapshot/v1/c1?max=1").await;

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE, "a ceiling that cannot fit the volume is refused up front");
}

// -------------------------------------------------------------------------------------------
// POST /peer/v1/wake — the poke that makes a peer pull now.
// -------------------------------------------------------------------------------------------

/// The wake route is authenticated exactly like the snapshot route, and answers 204 with no body:
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

// -------------------------------------------------------------------------------------------
// The receive half — a fake `btrfs receive` this time, exercised directly through `pull_one`
// rather than the router (the send side above never touches this code at all).
// -------------------------------------------------------------------------------------------

/// A fake `btrfs` whose `receive` arm creates the destination subvolume and then exits non-zero
/// — exactly what a real `btrfs receive` does on a stream that dies mid-way — and whose
/// `subvolume delete` arm actually removes what it created, so `pull_one`'s own cleanup has
/// something real to act on.
fn write_fake_btrfs_receive_fails_after_creating(dir: &std::path::Path) -> String {
    let path = dir.join("btrfs-receive-fails");
    let script = r#"#!/bin/sh
if [ "$1" = "receive" ]; then
    mkdir -p "$2/c1"
    exit 1
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    rm -rf "$3"
    exit 0
fi
"#;
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

struct OneShotServer {
    addr: String,
}

/// The smallest HTTP/1.1 server that answers one GET with a fixed body — `pull_one` only needs a
/// 200 and bytes, never a real peer, so hand-rolling this is less than a dependency.
async fn serve_one_body(body: &'static [u8]) -> OneShotServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.write_all(body).await;
            let _ = socket.shutdown().await;
        }
    });
    OneShotServer { addr }
}

/// The receive half against a fake `btrfs receive`: a truncated body must delete the partial and
/// return an error, so the puller tries the next source rather than keeping a half-received
/// subvolume that `local_commits` would then advertise. The real `btrfs receive` is only ever
/// exercised by `tests/ws_e2e.sh`; this covers the code AROUND it, which is where the bugs were.
#[tokio::test]
async fn a_truncated_receive_deletes_the_partial_and_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = write_fake_btrfs_receive_fails_after_creating(tmp.path());
    let engine = Engine::new(EnginePool::new(tmp.path()));
    let server = serve_one_body(b"partial stream").await;

    let err = pull_one(&engine, &fake, &peer_http_client().unwrap(), &server.addr, "s3cret", "v1", "c1", None, receive_ceiling(0))
        .await
        .expect_err("a failed receive is an error");

    assert!(err.contains("btrfs receive failed"), "{err}");
    assert!(!engine.pool.snap("v1", "c1").exists(), "the partial must not survive a failed receive");
}
