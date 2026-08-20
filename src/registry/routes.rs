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

/// How long a registry bearer lives. Long enough for a large push to finish on a slow link, short
/// enough that a leaked one is not a standing credential.
const TOKEN_TTL: u64 = 15 * 60;

#[derive(serde::Deserialize)]
struct TokenQuery {
    #[serde(default)]
    scope: String,
    #[serde(default)]
    service: String,
}

/// `GET /v2/token` — exchange a long-lived credential for a short-lived bearer.
async fn token(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<TokenQuery>,
) -> Response {
    let _ = q.service;
    let who = match super::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(o)) => o,
        // Anonymous is allowed to ask, and gets a token for nobody: it can still pull public
        // images. Refusing here would break anonymous pull for spec-following clients, which
        // always visit the token endpoint before the pull.
        Ok(None) => String::new(),
        Err(r) => return r,
    };
    let jwt = match app.jwt.mint_registry(&who, &q.scope, TOKEN_TTL) {
        Ok(t) => t,
        Err(e) => return crate::http::internal_pub(e),
    };
    let issued = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    axum::Json(serde_json::json!({
        "token": jwt,
        "access_token": jwt,
        "expires_in": TOKEN_TTL,
        "issued_at": issued,
    }))
    .into_response()
}

pub fn v2_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2", get(v2_root))
        .route("/v2/token", get(token))
}

/// Verifies a token minted by `/v2/token`; `Some(owner)` when it is ours, unexpired, and named
/// somebody — a token minted for the anonymous caller authenticates nobody.
pub fn verify_registry_token(app: &App, jwt: &str) -> Option<String> {
    let owner = app.jwt.verify_registry(jwt)?;
    (!owner.is_empty()).then_some(owner)
}
