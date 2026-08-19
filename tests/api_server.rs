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
    /// Path-and-query of the last request the fake node saw.
    saw_path: Arc<Mutex<String>>,
}

/// A fake git node that answers every path with `status` and counts what it is asked.
async fn upstream(status: axum::http::StatusCode) -> Upstream {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(HeaderMap::new()));
    let saw_path = Arc::new(Mutex::new(String::new()));
    let (h, s, sp) = (hits.clone(), seen.clone(), saw_path.clone());
    let router = axum::Router::new().fallback(axum::routing::any(
        move |uri: axum::http::Uri, hdrs: HeaderMap| {
        let (h, s, sp) = (h.clone(), s.clone(), sp.clone());
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            *sp.lock().unwrap() = uri.to_string();
            *s.lock().unwrap() = hdrs;
            (status, r#"[{"name":"refs/heads/master"}]"#)
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    Upstream { addr, hits, seen, saw_path }
}

/// The api process, pointed at `up`, with the cache disabled.
async fn api(e: &common::TestEnv, up: &Upstream) -> String {
    api_with(e, up, Arc::new(rustic_git::cache::Cache::connect(None).await)).await
}

/// The api process with a signing key but no database: enough to exercise the
/// identity path, which is where a forged or expired token has to be refused.
async fn api_with_jwt(e: &common::TestEnv, up: &Upstream, secret: &str) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (store, upstream) = (e.store.clone(), format!("http://{}", up.addr));
    let cache = Arc::new(rustic_git::cache::Cache::connect(None).await);
    let jwt = Arc::new(rustic_git::jwt::Jwt::new(secret).unwrap());
    tokio::spawn(async move {
        rustic_git::api::serve(store, cache, None, Some(jwt), upstream, "s".into(), l)
            .await
            .unwrap()
    });
    format!("http://{addr}")
}

/// The api process on a given cache.
async fn api_with(
    e: &common::TestEnv,
    up: &Upstream,
    cache: Arc<rustic_git::cache::Cache>,
) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (store, upstream) = (e.store.clone(), format!("http://{}", up.addr));
    tokio::spawn(async move {
        rustic_git::api::serve(store, cache, None, None, upstream, "s".into(), l)
            .await
            .unwrap()
    });
    format!("http://{addr}")
}

/// One GET with the path exactly as written — no client-side URL normalisation.
async fn raw_get(base: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.strip_prefix("http://").unwrap();
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out.lines().next().unwrap_or_default().to_string()
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

    // The client sets the owner header itself: it must gain nothing, since the api process builds
    // a fresh upstream request rather than copying the caller's. Otherwise this header is a total
    // authorization bypass — anyone could claim to be anyone.
    let r = reqwest::Client::new()
        .get(format!("{base}/api/alice/web/refs"))
        .header(rustic_git::proxy::OWNER_HEADER, "bob")
        .header(rustic_git::proxy::PEER_HEADER, "s")
        .send()
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

    for p in ["/api/alice/web", "/alice/web.git/info/refs"] {
        let r = reqwest::get(format!("{base}{p}")).await.unwrap();
        assert_eq!(r.status(), 404, "{p}");
    }
    // /healthz is the readiness probe target, not a browse route: 200, and never forwarded.
    let r = reqwest::get(format!("{base}/healthz")).await.unwrap();
    assert_eq!(r.status(), 200);
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
        saw_path: Arc::new(Mutex::new(String::new())),
    };
    drop(l);
    let base = api(&e, &dead).await;
    let r = reqwest::get(format!("{base}/api/alice/web/refs"))
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_cache_hit_never_touches_upstream() {
    // The component's whole purpose. Catches: forwarding on a hit, and losing the immutable
    // header on the hit path (the CDN would then re-ask for every id-addressed answer).
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let cache = Arc::new(rustic_git::cache::Cache::memory());
    cache.put("alice/web", rustic_git::api::META, b"1", 30).await;
    cache.put("alice/web", "tree:abc", br#"["cached"]"#, 60).await;
    let base = api_with(&e, &up, cache).await;

    let r = reqwest::get(format!("{base}/api/alice/web/tree/abc")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(r.text().await.unwrap(), r#"["cached"]"#);
    assert_eq!(up.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_private_body_is_never_served_to_a_stranger() {
    // Catches a cache read placed before the authorization decision: the body is in the cache
    // (an owner put it there), the visibility flag is not, so a stranger must still be sent
    // upstream — and get upstream's 404, not the cached bytes.
    let up = upstream(axum::http::StatusCode::NOT_FOUND).await;
    let e = common::env().await;
    let cache = Arc::new(rustic_git::cache::Cache::memory());
    cache.put("alice/web", "refs", br#"["secret"]"#, 60).await;
    let base = api_with(&e, &up, cache).await;

    let r = reqwest::get(format!("{base}/api/alice/web/refs")).await.unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(up.hits.load(Ordering::SeqCst), 1);
    assert!(!r.text().await.unwrap().contains("secret"));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_spelling_of_a_traversal_reaches_another_tenants_repo() {
    // Catches the authorize-one-repo/fetch-another split. reqwest's URL parsing strips `..` AND
    // its percent-encoded spellings, and turns `\` into `/`, so a guard that judges raw text
    // authorizes alice/web while upstream is asked for bob/private.
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let base = api(&e, &up).await;

    for path in [
        "/api/alice/web/tree/../../bob/private/tree/x",
        "/api/alice/web/tree/%2e%2e/%2e%2e/bob/private/tree/x",
        "/api/alice/web/tree/%2E%2E/%2E%2E/bob/private/tree/x",
        "/api/alice/web/tree/%2e./%2e./bob/private/tree/x",
        "/api/alice/web/tree/.%2E/.%2E/bob/private/tree/x",
        "/api/alice/web/tree/%2e/abc",
        "/api/alice/web/tree/%5C%5C/bob/private/tree/x",
        "/api/alice/web/tree//abc",
    ] {
        // Sent raw: a client library normalises these away before they ever reach the server,
        // which is precisely why the server may not assume they are gone.
        let status = raw_get(&base, path).await;
        assert!(status.starts_with("HTTP/1.1 404"), "{path} -> {status}");
        // The status alone proves little — a 404 arrives for several reasons. What must hold is
        // that no git node was ever asked anything.
        assert_eq!(up.hits.load(Ordering::SeqCst), 0, "{path} reached upstream");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fragment_in_the_query_cannot_redirect_the_upstream_request() {
    // `#` is a fragment to `Url::parse` and never travels; what must never happen is the cache key
    // naming one repo while the git node is asked for another.
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let base = api(&e, &up).await;

    let status = raw_get(&base, "/api/alice/web/log/abc?page=2#/api/bob/private/refs").await;
    assert!(status.starts_with("HTTP/1.1 2") || status.starts_with("HTTP/1.1 404"), "{status}");
    let saw = up.saw_path.lock().unwrap().clone();
    assert!(!saw.contains("bob"), "upstream saw {saw}");
    assert!(saw.is_empty() || saw.starts_with("/api/alice/web/log/abc"), "upstream saw {saw}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_method_is_refused_rather_than_forwarded_as_a_read() {
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let base = api(&e, &up).await;

    let r = reqwest::Client::new()
        .post(format!("{base}/api/alice/web/refs"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
    assert_eq!(up.hits.load(Ordering::SeqCst), 0);
}

/// Catches: a read-through miss writing its answer into the generation a mid-flight purge just
/// emptied — a private repo's body served to strangers for the whole TTL.
#[tokio::test(flavor = "multi_thread")]
async fn a_purge_during_a_miss_discards_the_answer() {
    let e = common::env().await;
    let cache = Arc::new(rustic_git::cache::Cache::memory());
    // The purge happens INSIDE the upstream handler: structurally after the api process read the
    // generation and before it writes the answer back. No sleeps, so nothing here can flake.
    let c = cache.clone();
    let router = axum::Router::new().fallback(axum::routing::any(move || {
        let c = c.clone();
        async move {
            c.bump_generation("alice/web").await.unwrap();
            (axum::http::StatusCode::OK, "body")
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });

    let base = api_with(
        &e,
        &Upstream {
            addr: up_addr,
            hits: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(HeaderMap::new())),
            saw_path: Arc::new(Mutex::new(String::new())),
        },
        cache.clone(),
    )
    .await;

    assert!(raw_get(&base, "/api/alice/web/blob/abc/x").await.contains("200"));
    assert_eq!(cache.get("alice/web", "blob:abc:x").await, None);
    assert_eq!(cache.get("alice/web", rustic_git::api::META).await, None);
}

// ── identity ────────────────────────────────────────────────────────────────

const KEY: &str = "0123456789012345678901234567890123456789";

/// A caller with no credentials at all is not a caller.
#[tokio::test(flavor = "multi_thread")]
async fn team_routes_refuse_an_anonymous_caller() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let r = reqwest::Client::new().get(format!("{base}/v1/teams")).send().await.unwrap();
    assert_eq!(r.status(), 401, "no token and no peer secret must not be a caller");
}

/// A token this server did not sign proves nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_signed_with_another_key_is_refused() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let forged = rustic_git::jwt::Jwt::new("abcdefghijabcdefghijabcdefghijabcdefghij")
        .unwrap()
        .mint("attacker@example.com", "A", None)
        .unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/v1/teams"))
        .header("authorization", format!("Bearer {forged}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "a token signed with a different key must be refused");
}

/// A token this server DID sign identifies its subject — the request gets past
/// identity and fails later, on the database it does not have.
#[tokio::test(flavor = "multi_thread")]
async fn a_valid_token_identifies_the_caller() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let token = rustic_git::jwt::Jwt::new(KEY).unwrap().mint("karthik@kloudlite.io", "K", Some("karthik")).unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/v1/teams"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        503,
        "identity should be accepted; only the missing database should stop it"
    );
}

/// The peer path still works, for calls made before a user has a token.
#[tokio::test(flavor = "multi_thread")]
async fn the_peer_secret_still_identifies_an_internal_caller() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let c = reqwest::Client::new();
    let wrong = c
        .get(format!("{base}/v1/teams"))
        .header("x-rustic-git-peer", "not-the-secret")
        .header("x-rustic-git-owner", "karthik@kloudlite.io")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401, "a wrong peer secret must be refused");

    let right = c
        .get(format!("{base}/v1/teams"))
        .header("x-rustic-git-peer", "s")
        .header("x-rustic-git-owner", "karthik@kloudlite.io")
        .send()
        .await
        .unwrap();
    assert_eq!(right.status(), 503, "the peer path should identify the caller");
}

/// Holding the peer secret must not let a caller mint an identity that is not the
/// one it asserted.
#[tokio::test(flavor = "multi_thread")]
async fn sign_in_refuses_a_body_that_disagrees_with_the_caller() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/users"))
        .header("x-rustic-git-peer", "s")
        .header("x-rustic-git-owner", "karthik@kloudlite.io")
        .header("content-type", "application/json")
        .body(r#"{"email":"someone@else.com","name":"X"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "the asserted caller and the body must agree");
}

/// Claiming a handle needs an identity like everything else.
#[tokio::test(flavor = "multi_thread")]
async fn claiming_a_username_refuses_an_anonymous_caller() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/users/username"))
        .header("content-type", "application/json")
        .body(r#"{"username":"someone"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

/// An identified caller gets past identity and stops at the missing database —
/// proving the route is wired to the directory and not to something else.
#[tokio::test(flavor = "multi_thread")]
async fn claiming_a_username_reaches_the_directory() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let token = rustic_git::jwt::Jwt::new(KEY).unwrap().mint("k@example.com", "K", None).unwrap();
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/users/username"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(r#"{"username":"someone"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "only the absent database should stop it");
}

// ── creating a repo ─────────────────────────────────────────────────────────

/// Creating is a write into someone's namespace, so the identity gate comes
/// first and the git fleet is never told about a caller who failed it.
#[tokio::test(flavor = "multi_thread")]
async fn creating_a_repo_refuses_an_anonymous_caller() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::CREATED).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/repos"))
        .header("content-type", "application/json")
        .body(r#"{"owner":"alice","name":"web"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(up.hits.load(Ordering::SeqCst), 0, "an unauthenticated create must not reach the fleet");
}

/// The name is validated before it is ever built into an upstream URL. Without
/// this, `../..` in a name addresses a route other than the one authorized —
/// the create is checked against `alice` and lands somewhere else entirely.
#[tokio::test(flavor = "multi_thread")]
async fn a_repo_name_that_could_address_another_route_never_reaches_the_fleet() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::CREATED).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let token = rustic_git::jwt::Jwt::new(KEY).unwrap().mint("k@example.com", "K", Some("k")).unwrap();
    for name in ["..", "../../bob/private", "a/b", "", "a\\b"] {
        let r = reqwest::Client::new()
            .post(format!("{base}/v1/repos"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(format!(r#"{{"owner":"alice","name":{}}}"#, serde_json_string(name)))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "name {name:?} must be refused");
    }
    assert_eq!(up.hits.load(Ordering::SeqCst), 0, "no invalid name may reach the fleet");
}

/// An identified caller with a valid name stops at the missing database: the
/// authorization question is asked BEFORE anything is forwarded, so a caller
/// whose membership cannot be established never creates a repo.
#[tokio::test(flavor = "multi_thread")]
async fn creating_a_repo_asks_the_directory_before_it_asks_the_fleet() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::CREATED).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let token = rustic_git::jwt::Jwt::new(KEY).unwrap().mint("k@example.com", "K", Some("k")).unwrap();
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/repos"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(r#"{"owner":"alice","name":"web"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "only the absent database should stop it");
    assert_eq!(up.hits.load(Ordering::SeqCst), 0, "authorization is not the fleet's to answer");
}

/// Minimal JSON string quoting, so the traversal cases above travel as data.
fn serde_json_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Listing is scoped to a namespace the caller belongs to, so it cannot be used
/// to read another owner's repos — or to find out that they have any.
#[tokio::test(flavor = "multi_thread")]
async fn listing_repos_refuses_an_anonymous_caller_and_requires_an_owner() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let c = reqwest::Client::new();

    let r = c.get(format!("{base}/v1/repos?owner=alice")).send().await.unwrap();
    assert_eq!(r.status(), 401);

    let token = rustic_git::jwt::Jwt::new(KEY).unwrap().mint("k@example.com", "K", Some("k")).unwrap();
    let r = c
        .get(format!("{base}/v1/repos"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "a listing with no namespace is not a listing");

    let r = c
        .get(format!("{base}/v1/repos?owner=alice"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "only the absent database should stop it");
    assert_eq!(up.hits.load(Ordering::SeqCst), 0, "listing never asks the fleet");
}
