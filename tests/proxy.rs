//! Forwarding against a stub peer, so this covers the mechanics only.
use axum::{routing::any, Router};
use rustic_git_core::peer::{Forwarder, HOPS_HEADER, OWNER_HEADER, PEER_HEADER};

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

/// A stub peer answering a HEAD the way the registry does: headers describing the entity, no body.
async fn head_peer() -> String {
    let app = Router::new().route(
        "/{*rest}",
        any(|| async {
            (
                axum::http::StatusCode::OK,
                [
                    (axum::http::header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json"),
                    (axum::http::header::CONTENT_LENGTH, "423"),
                ],
            )
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// A forwarded HEAD must keep its `Content-Length`.
///
/// `content-length` is hop-by-hop on the way OUT because each hop frames its own body. On the way
/// BACK that reasoning only holds when there IS a body to re-frame: a HEAD has none, so stripping
/// the header does not defer to the next hop's framing, it destroys the one number the client
/// asked for. Real clients then log "HEAD request failed, falling back on GET" and pay a second
/// round trip on every manifest check — which is exactly what a registry probe against the fleet
/// did, while a single-node test saw nothing because nothing forwarded.
#[tokio::test]
async fn a_forwarded_head_keeps_its_content_length() {
    let peer = head_peer().await;
    let f = Forwarder::new(SECRET.into());
    let req = axum::http::Request::builder()
        .method("HEAD")
        .uri("/v2/acme/nginx/manifests/latest")
        .body(axum::body::Body::empty())
        .unwrap();
    let res = f.forward(&peer, "acme", 0, req).await.unwrap();
    assert_eq!(
        res.headers().get("content-length").map(|v| v.to_str().unwrap().to_string()),
        Some("423".to_string()),
        "a forwarded HEAD must report the length the owner gave"
    );
    assert!(res.headers().get("content-type").is_some(), "the media type must survive too");
}
