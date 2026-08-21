mod common;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn get_as(
    router: &axum::Router,
    as_owner: &str,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(path)
        .header(rustic_git::proxy::PEER_HEADER, "test-peer-secret")
        .header(rustic_git::proxy::OWNER_HEADER, as_owner)
        .body(axum::body::Body::empty())
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default(),
    )
}

async fn post_as(router: &axum::Router, as_owner: &str, path: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header(rustic_git::proxy::PEER_HEADER, "test-peer-secret")
        .header(rustic_git::proxy::OWNER_HEADER, as_owner)
        .body(axum::body::Body::empty())
        .unwrap();
    router.clone().oneshot(req).await.unwrap().status()
}

/// Create/flip/delete must keep the listing-index markers in sync with the real state: a
/// listing answers from these markers without opening the repo's own database, so a marker left
/// stale after an admin op would show a name that no longer exists, or hide one that does.
#[tokio::test(flavor = "multi_thread")]
async fn repo_lifecycle_maintains_markers() {
    use rustic_git::index::{self, Kind};
    use slatedb::object_store::ObjectStoreExt;

    let e = common::env().await;
    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);

    let priv_path = index::path(false, Kind::Repo, "alice", "widget");
    let pub_path = index::path(true, Kind::Repo, "alice", "widget");

    // create private -> private marker exists, public absent
    let s = post_as(&router, "alice", "/api/alice/widget/create").await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(e.store.os.get(&priv_path).await.is_ok(), "private marker missing after create");
    assert!(e.store.os.get(&pub_path).await.is_err(), "public marker present after private create");

    // flip public -> public marker exists, private absent
    let s = post_as(&router, "alice", "/api/alice/widget/visibility?visibility=public").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(e.store.os.get(&pub_path).await.is_ok(), "public marker missing after flip");
    assert!(e.store.os.get(&priv_path).await.is_err(), "private marker left behind after flip");

    // delete -> both absent
    let s = post_as(&router, "alice", "/api/alice/widget/delete").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(e.store.os.get(&pub_path).await.is_err(), "public marker survived delete");
    assert!(e.store.os.get(&priv_path).await.is_err(), "private marker survived delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn refs_then_tree_then_blob_then_log_and_commit() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let app = common::app(e.store.clone()).await;
    let router = rustic_git::http::peer_router(app);

    let (s, refs) = get_as(&router, "alice", "/api/alice/web/refs").await;
    assert_eq!(s, StatusCode::OK);
    let oid = refs[0]["oid"].as_str().unwrap().to_string();
    assert_eq!(refs[0]["kind"], "branch");

    let (s, tree) = get_as(&router, "alice", &format!("/api/alice/web/tree/{oid}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(tree.as_array().unwrap().iter().any(|e| e["name"] == "src"));

    let (s, sub) = get_as(&router, "alice", &format!("/api/alice/web/tree/{oid}/src")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(sub.as_array().unwrap().iter().any(|e| e["name"] == "main.rs"));

    let (s, blob) = get_as(&router, "alice", &format!("/api/alice/web/blob/{oid}/src/main.rs")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(blob["truncated"], false);
    assert!(blob["bytes_base64"].as_str().unwrap().len() > 0);

    // Two commits from the fixture, and `n` clamps rather than errors.
    let (s, log) = get_as(&router, "alice", &format!("/api/alice/web/log/{oid}?n=999")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(log.as_array().unwrap().len(), 2);

    let (s, c) = get_as(&router, "alice", &format!("/api/alice/web/commit/{oid}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(c["oid"], oid);
    assert!(c["diff"].as_str().unwrap().contains("main.rs"));

    let (s, _) = get_as(&router, "alice", &format!("/api/alice/web/tree/{oid}/nope")).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unknown path is 404, never 500");

    let (s, _) = get_as(&router, "alice", "/api/alice/web/tree/not-an-oid").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a malformed oid is 404, never 400/500");

    let (s, _) = get_as(&router, "alice", "/api/alice/ghost/refs").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn private_repo_is_404_to_a_stranger() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let app = common::app(e.store.clone()).await;
    let (s, _) = get_as(&rustic_git::http::peer_router(app), "bob", "/api/alice/web/refs").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "existence must not leak");
}

/// Catches the production behaviour the unit test alone would have missed: on a PUBLIC repo an
/// authenticated non-owner got 404 from the browse routes while an anonymous caller got 200.
#[tokio::test(flavor = "multi_thread")]
async fn public_repo_browses_for_a_non_owner_token() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    e.store.set_public("alice", "web", true).await.unwrap();
    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);
    let (s, refs) = get_as(&router, "bob", "/api/alice/web/refs").await;
    assert_eq!(s, StatusCode::OK, "public grants read to any authenticated caller");
    assert!(!refs.as_array().unwrap().is_empty());
    // and the owner is unaffected
    let (s, _) = get_as(&router, "alice", "/api/alice/web/refs").await;
    assert_eq!(s, StatusCode::OK);
}

/// The browse API is peer-only. On the public listener `/api/...` must 404 here, never be forwarded
/// to the owner's peer port with the shared secret.
#[tokio::test(flavor = "multi_thread")]
async fn public_router_does_not_serve_the_browse_api() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let router = rustic_git::http::router(common::app(e.store.clone()).await);
    for path in ["/api/alice/web/refs", "/api/alice/web/tree/HEAD", "/api/alice/web/log/x"] {
        let req = Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let r = router.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path} must not be public");
    }
}

/// The path both routers used to disagree about. `alice/info` is a real repo, so
/// `/api/alice/info/refs` is its browse route on the peer listener — and the handler that actually
/// runs must operate on `alice/info`, not on a repo `api/alice` the middleware picked instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_repo_named_info_browses_as_itself() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "info").await;
    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);

    // 200 with real refs proves the handler opened `alice/info`: no other repo exists here, and a
    // handler that had been given owner=api name=alice would 404.
    let (s, refs) = get_as(&router, "alice", "/api/alice/info/refs").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(refs[0]["kind"], "branch");
    let oid = refs[0]["oid"].as_str().unwrap().to_string();
    let (s, tree) = get_as(&router, "alice", &format!("/api/alice/info/tree/{oid}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(tree.as_array().unwrap().iter().any(|e| e["name"] == "src"));

    // The same path is alice's repo, so bob may not see it.
    let (s, _) = get_as(&router, "bob", "/api/alice/info/refs").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "existence must not leak");
}

/// ...and on the public listener that same path is a flat 404. Forwarding is what must not happen:
/// this node's peer address is 127.0.0.1:1, so a forward would surface as 502/503, not 404.
#[tokio::test(flavor = "multi_thread")]
async fn public_router_404s_a_repo_named_info() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "info").await;
    let router = rustic_git::http::router(common::app(e.store.clone()).await);
    for path in ["/api/alice/info/refs", "/api/alice/info", "/api/alice/git-upload-pack"] {
        let req = Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let r = router.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path} must not be public");
    }
    // The public git route of the same repo is untouched: `/alice/info/info/refs`.
    let req = Request::builder()
        .uri("/alice/info/info/refs?service=git-upload-pack")
        .body(axum::body::Body::empty())
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "private repo, but routed and reached");
}

/// `GET .../protect` used to skip the visibility gate entirely: any caller, owner or
/// stranger, got the branch-protection list. It must now behave exactly like every
/// other browse route.
#[tokio::test(flavor = "multi_thread")]
async fn protections_require_visibility() {
    if !common::have_git() {
        eprintln!("skipping: no git"); // ponytail: eprintln
        return;
    }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);

    let (s, _) = get_as(&router, "bob", "/api/alice/web/protect").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "existence must not leak");

    let (s, list) = get_as(&router, "alice", "/api/alice/web/protect").await;
    assert_eq!(s, StatusCode::OK);
    assert!(list.as_array().is_some());
}
