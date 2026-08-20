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

/// The three outcomes of presenting a Bearer token, which `Option<String>` cannot tell apart:
/// a forged/expired/foreign token must be refused, but our own anonymous token must NOT be —
/// it is the token a spec-following client gets from `/v2/token` before an anonymous public pull.
pub enum RegistryToken {
    /// Ours, and names an owner.
    Owner(String),
    /// Ours, minted for the anonymous caller: verified, but authenticates nobody.
    Anonymous,
    /// Not ours, expired, or malformed — a refusal, not anonymity.
    Invalid,
}

/// Verifies a token minted by `/v2/token`. See `RegistryToken` for why this can't be `Option`.
pub fn verify_registry_token(jwt_keys: &crate::jwt::Jwt, jwt: &str) -> RegistryToken {
    match jwt_keys.verify_registry(jwt) {
        Some(owner) if !owner.is_empty() => RegistryToken::Owner(owner),
        Some(_) => RegistryToken::Anonymous,
        None => RegistryToken::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt() -> crate::jwt::Jwt {
        crate::jwt::Jwt::new("0123456789012345678901234567890123456789").unwrap()
    }

    /// The defect this fixes: an anonymous-issued token must NOT collapse into the same outcome
    /// as a forged one, or a spec-following client's anonymous pull gets refused instead of
    /// allowed through as anonymous.
    #[test]
    fn an_anonymous_token_and_a_forged_token_produce_different_outcomes() {
        let j = jwt();
        let anon = j.mint_registry("", "repository:acme/nginx:pull", 900).unwrap();
        let owned = j.mint_registry("acme", "repository:acme/nginx:pull,push", 900).unwrap();

        assert!(matches!(verify_registry_token(&j, &anon), RegistryToken::Anonymous));
        assert!(matches!(verify_registry_token(&j, "not.a.jwt"), RegistryToken::Invalid));
        match verify_registry_token(&j, &owned) {
            RegistryToken::Owner(o) => assert_eq!(o, "acme"),
            _ => panic!("expected Owner"),
        }
    }
}
