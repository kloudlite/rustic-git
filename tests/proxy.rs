//! Forwarding against a stub peer, so this covers the mechanics only.
use axum::{routing::any, Router};
use rustic_git::proxy::{Forwarder, HOPS_HEADER, OWNER_HEADER, PEER_HEADER};

const SECRET: &str = "s3cret";

/// A stub peer that echoes what crossed the wire.
async fn stub_peer() -> String {
    let app = Router::new()
        .route("/{*rest}", any(|h: axum::http::HeaderMap, body: axum::body::Bytes| async move {
            let g = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("none").to_string();
            format!("owner={} hops={} peer={} expect={} te={} len={}",
                g(OWNER_HEADER), g(HOPS_HEADER), g(PEER_HEADER), g("expect"), g("transfer-encoding"), body.len())
        }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// Forwarding carries the identity, the hop count, and the secret; strips hop-by-hop headers so
/// each hop frames its own body (git sends Expect: 100-continue on pushes over 1 MiB, and passing
/// it through to a peer that then frames differently is a mismatch a small test never hits).
#[tokio::test]
async fn forwarding_carries_identity_and_strips_hop_by_hop_headers() {
    let peer = stub_peer().await;
    let f = Forwarder::new(SECRET.into());
    let big = vec![b'x'; 2 * 1024 * 1024];
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/alice/web/git-receive-pack")
        .header("expect", "100-continue")
        .header("transfer-encoding", "chunked")
        .header("connection", "keep-alive")
        .body(axum::body::Body::from(big.clone()))
        .unwrap();
    let res = f.forward(&peer, "alice", 0, req).await.unwrap();
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&body),
        format!("owner=alice hops=1 peer={SECRET} expect=none te=none len={}", big.len()));
}
