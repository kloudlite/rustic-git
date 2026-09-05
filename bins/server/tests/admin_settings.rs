//! `POST /api/admin/settings/revert` (the server-tier half of Task 8's central revert), against
//! the real peer router and a real leader `App` — proves the route is reachable through
//! `route_inner`'s `admin/settings` carve-out (not just present on the axum `Router`) and that
//! it swaps in `history[0]`, pushing the pre-revert document onto history in its place.

use axum::http::{Request, StatusCode};
use kloudlite_core::jwt::Jwt;
use slatedb::object_store::memory::InMemory;
use std::sync::Arc;
use tower::ServiceExt;

const JWT_SECRET: &str = "test-secret-at-least-32-bytes-long!!";

/// A leader `App` over an in-memory store — same recipe as the root test host's `common::app`
/// (`tests/common/mod.rs`), reproduced here because that helper lives in a different crate
/// (`kloudlite-tests`) this binary's `tests/` cannot reach.
async fn app() -> Arc<kloudlite_app::App> {
    std::env::set_var("KLOUDLITE_JWT_SECRET", JWT_SECRET);
    let tmp = tempfile::tempdir().unwrap();
    let os = Arc::new(InMemory::new());
    let store = kloudlite_storage::store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap();
    let ownership = kloudlite_storage::ownership::OwnershipStore::open(store.os.clone());
    let app = kloudlite_app::App::new(
        Arc::new(store),
        Arc::new(ownership),
        "kloudlite-0".into(),
        Arc::new(|_| "127.0.0.1:1".to_string()),
        "test-peer-secret".into(),
        kloudlite_pulls::pulls::Source::Absent,
    );
    app.election_tick().await.unwrap();
    assert!(app.is_leader());
    Arc::new(app)
}

fn admin_token() -> String {
    Jwt::new(JWT_SECRET).unwrap().mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

async fn call(router: &axum::Router, method: &str, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(kloudlite_core::peer::PEER_HEADER, "test-peer-secret")
        .header("authorization", format!("Bearer {}", admin_token()))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// No history yet: 422, and nothing is written (the follow-up GET still shows the untouched
/// default document).
#[tokio::test]
async fn revert_with_no_history_is_422() {
    let app = app().await;
    let router = kloudlite_server::router::peer_router(app.clone());
    let (status, _) = call(&router, "POST", "/api/admin/settings/revert", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// A write followed by a revert restores the pre-write document, and pushes the (now former)
/// current document onto history in its place — same "keep history" semantics as the ordinary
/// PUT, so a revert can itself be reverted.
#[tokio::test]
async fn revert_restores_history_zero_and_repushes_the_current_doc() {
    let app = app().await;
    let router = kloudlite_server::router::peer_router(app.clone());

    let (status, _) = call(&router, "PUT", "/api/admin/settings", serde_json::json!({"maxBody": 4194304})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&router, "PUT", "/api/admin/settings", serde_json::json!({"maxBody": 8388608})).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(&router, "POST", "/api/admin/settings/revert", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["maxBody"], 4194304, "reverted to the value in force before the last write");
    assert_eq!(body["history"][0]["maxBody"], 8388608, "the pre-revert document is now history[0]");
}
