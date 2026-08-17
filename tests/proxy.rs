//! Forwarding and probing against a stub peer, so these cover the mechanics only.
use axum::{routing::{any, get}, Router};
use rustic_git::proxy::{Forwarder, HOPS_HEADER, OWNER_HEADER, PEER_HEADER};

const SECRET: &str = "s3cret";

/// A stub peer: /healthz and /probe behave like a real peer listener (secret required); anything
/// else echoes what crossed the wire.
async fn stub_peer(probe_answer: bool) -> String {
    let guard = |h: &axum::http::HeaderMap| h.get(PEER_HEADER).and_then(|v| v.to_str().ok()) == Some(SECRET);
    let app = Router::new()
        .route("/healthz", get(move |h: axum::http::HeaderMap| async move {
            if guard(&h) { (axum::http::StatusCode::OK, "ok") } else { (axum::http::StatusCode::FORBIDDEN, "") }
        }))
        .route("/probe", get(move |h: axum::http::HeaderMap| async move {
            if guard(&h) { (axum::http::StatusCode::OK, if probe_answer { "up" } else { "down" }) } else { (axum::http::StatusCode::FORBIDDEN, "") }
        }))
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
    let peer = stub_peer(true).await;
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

/// Reachability requires the application to answer 200 — with the secret, or a real peer listener
/// would refuse the probe and every peer would look down. A socket that accepts and closes is not
/// reachable: a pod mid-shutdown does exactly that.
#[tokio::test]
async fn reachable_requires_a_200_from_the_application() {
    let f = Forwarder::new(SECRET.into());
    assert!(f.reachable(&stub_peer(true).await).await);
    assert!(!Forwarder::new("wrong".into()).reachable(&stub_peer(true).await).await, "wrong secret must read as down, loudly");
    assert!(!f.reachable("127.0.0.1:1").await);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { loop { let (s, _) = l.accept().await.unwrap(); drop(s); } });
    // accept-then-close is a connection error, not a timeout, so this is fast despite the retry
    assert!(!f.reachable(&addr).await, "accept-then-close is not reachable");
}

/// A positive probe is cached briefly, a negative one is not: a hot owner is probed once per
/// second per node rather than once per request, but only fresh evidence may demote a peer.
/// The stub counts hits so the cache is actually observed.
#[tokio::test]
async fn positive_probes_are_cached_negative_ones_are_not() {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route("/healthz", get(move |hd: axum::http::HeaderMap| { let h = h.clone(); async move {
        h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if hd.get(PEER_HEADER).and_then(|v| v.to_str().ok()) == Some(SECRET) { axum::http::StatusCode::OK } else { axum::http::StatusCode::FORBIDDEN }
    }}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let f = Forwarder::new(SECRET.into());
    assert!(f.reachable(&addr).await);
    assert!(f.reachable(&addr).await);
    assert!(f.reachable(&addr).await);
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 1, "three reachable() calls within the cache window = one real probe");
    let f2 = Forwarder::new(SECRET.into());
    assert!(!f2.reachable("127.0.0.1:1").await);
    assert!(!f2.reachable("127.0.0.1:1").await, "negative is never cached");
}

/// Concurrent probes of one address share a single in-flight probe. Ten callers, one hit.
#[tokio::test]
async fn concurrent_probes_of_one_address_are_single_flight() {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route("/healthz", get(move || { let h = h.clone(); async move {
        h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await; // slow enough to overlap
        "ok"
    }}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let f = std::sync::Arc::new(Forwarder::new(SECRET.into()));
    let calls: Vec<_> = (0..10).map(|_| { let f = f.clone(); let a = addr.clone(); tokio::spawn(async move { f.reachable(&a).await }) }).collect();
    for c in calls { assert!(c.await.unwrap()); }
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 1, "ten concurrent callers, one probe");
}

/// The second vantage: ask a peer whether it can reach a third. Distinguishes "they said no" from
/// "we could not ask them", because only the former is evidence.
#[tokio::test]
async fn probe_via_distinguishes_no_from_could_not_ask() {
    let f = Forwarder::new(SECRET.into());
    assert_eq!(f.probe_via(&stub_peer(true).await, "rustic-git-0").await, Some(true));
    assert_eq!(f.probe_via(&stub_peer(false).await, "rustic-git-0").await, Some(false));
    assert_eq!(f.probe_via("127.0.0.1:1", "rustic-git-0").await, None);
}
