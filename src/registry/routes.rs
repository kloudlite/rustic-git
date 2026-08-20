use crate::http::Trusted;
use crate::App;
use axum::{extract::State, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::get, Extension, Router};
use std::sync::Arc;

/// `GET /v2/` — the version check every client makes before anything else. It carries no image, so
/// it is answered by whichever node receives it.
async fn v2_root(State(app): State<Arc<App>>, Extension(trusted): Extension<Trusted>, headers: HeaderMap) -> Response {
    match super::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(_)) => (
            StatusCode::OK,
            [("docker-distribution-api-version", "registry/2.0")],
            "{}",
        ).into_response(),
        Ok(None) => with_version(super::auth::challenge(None)),
        Err(r) => with_version(r),
    }
}

fn with_version(mut r: Response) -> Response {
    r.headers_mut().insert("docker-distribution-api-version", "registry/2.0".parse().unwrap());
    r
}

pub fn v2_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2", get(v2_root))
}

/// Verifies a token minted by `/v2/token`; `Some(owner)` when it is ours and unexpired.
/// Task 4 replaces this stub with the real verification.
pub fn verify_registry_token(_app: &App, _jwt: &str) -> Option<String> {
    None
}
