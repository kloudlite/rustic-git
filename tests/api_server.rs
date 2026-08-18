//! The api process in front of a fake git fleet. No Redis and no git nodes: the cache is disabled
//! (`Cache::connect(None)`), so every path here is the miss path — which is where forwarding,
//! local authentication and the downstream cache headers live.
mod common;

use axum::http::HeaderMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Upstream {
    addr: std::net::SocketAddr,
    hits: Arc<AtomicUsize>,
    /// Headers of the last request the fake node saw.
    seen: Arc<Mutex<HeaderMap>>,
}

/// A fake git node that answers every path with `status` and counts what it is asked.
async fn upstream(status: axum::http::StatusCode) -> Upstream {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(HeaderMap::new()));
    let (h, s) = (hits.clone(), seen.clone());
    let router = axum::Router::new().fallback(axum::routing::any(move |hdrs: HeaderMap| {
        let (h, s) = (h.clone(), s.clone());
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            *s.lock().unwrap() = hdrs;
            (status, r#"[{"name":"refs/heads/master"}]"#)
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    Upstream { addr, hits, seen }
}

/// The api process, pointed at `up`, with the cache disabled.
async fn api(e: &common::TestEnv, up: &Upstream) -> String {
    let cache = Arc::new(rustic_git::cache::Cache::connect(None).await);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (store, upstream) = (e.store.clone(), format!("http://{}", up.addr));
    tokio::spawn(async move {
        rustic_git::api::serve(store, cache, upstream, "s".into(), l)
            .await
            .unwrap()
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forwarded_request_presents_the_peer_identity() {
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let base = api(&e, &up).await;

    let r = reqwest::Client::new()
        .get(format!("{base}/api/alice/web/tree/abc123/src"))
        .basic_auth("x", Some(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // A private answer must never be cacheable downstream: the caller is authenticated, so the
    // api process cannot know this repo is public.
    assert_eq!(r.headers()["cache-control"], "private, no-store");
    assert_eq!(r.text().await.unwrap(), r#"[{"name":"refs/heads/master"}]"#);

    // The git nodes serve browse routes on the secret-guarded peer listener only, so the request
    // has to arrive wearing both halves of a forwarding node's identity, or upstream refuses it.
    assert_eq!(up.hits.load(Ordering::SeqCst), 1);
    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(seen[rustic_git::proxy::PEER_HEADER], "s");
    assert_eq!(seen[rustic_git::proxy::OWNER_HEADER], "alice");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_anonymous_hit_is_public_and_carries_no_owner() {
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let base = api(&e, &up).await;

    let r = reqwest::get(format!("{base}/api/alice/web/refs"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // Upstream served an anonymous caller, so the repo is public — and `refs` is the one answer
    // that can go stale.
    assert_eq!(r.headers()["cache-control"], "public, max-age=5");
    assert!(!up
        .seen
        .lock()
        .unwrap()
        .contains_key(rustic_git::proxy::OWNER_HEADER));

    let r = reqwest::get(format!("{base}/api/alice/web/tree/abc123"))
        .await
        .unwrap();
    assert_eq!(
        r.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn upstreams_401_passes_through_so_a_client_knows_to_present_a_token() {
    let up = upstream(axum::http::StatusCode::UNAUTHORIZED).await;
    let e = common::env().await;
    let base = api(&e, &up).await;

    let r = reqwest::get(format!("{base}/api/alice/web/refs"))
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(up.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_token_is_refused_locally_without_asking_a_git_node() {
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let base = api(&e, &up).await;

    let r = reqwest::Client::new()
        .get(format!("{base}/api/alice/web/refs"))
        .basic_auth("x", Some("not-a-token"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // The whole point of authenticating here: a bogus credential never reaches the fleet.
    assert_eq!(up.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_path_that_is_not_a_browse_route_never_reaches_upstream() {
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let base = api(&e, &up).await;

    for p in ["/api/alice/web", "/alice/web.git/info/refs", "/healthz"] {
        let r = reqwest::get(format!("{base}{p}")).await.unwrap();
        assert_eq!(r.status(), 404, "{p}");
    }
    assert_eq!(up.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_fleet_is_a_502_not_a_hang() {
    let e = common::env().await;
    // A port nothing listens on: the api process holds no repo state, so a dead fleet is the only
    // thing it can report.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = Upstream {
        addr: l.local_addr().unwrap(),
        hits: Arc::new(AtomicUsize::new(0)),
        seen: Arc::new(Mutex::new(HeaderMap::new())),
    };
    drop(l);
    let base = api(&e, &dead).await;
    let r = reqwest::get(format!("{base}/api/alice/web/refs"))
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
}
