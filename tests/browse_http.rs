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
