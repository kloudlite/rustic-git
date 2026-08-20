use crate::http::Trusted;
use crate::App;
use axum::{extract::State, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::{get, post, put}, Extension, Router};
use super::{blobs, manifests, referrers, uploads};
use std::sync::Arc;

/// The images an owner has pushed: the sub-prefixes under their `repo/img/{owner}/` object-store
/// prefix. A listing rather than a maintained index, because a maintained index is state that can
/// disagree with what was actually pushed — this cannot. Shared by `_catalog` and (Task 11) the
/// web page, so there is exactly one place that knows the layout of that prefix.
pub async fn image_names(app: &App, owner: &str) -> crate::Result<Vec<String>> {
    super::list_dir_names(&app.store.os, &format!("repo/img/{owner}/")).await
}

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

/// Every `scope` in the query, joined by spaces.
///
/// A client may send `scope` MORE THAN ONCE — docker asks for `pull` and `pull,push` as two
/// parameters — and a struct with one `scope: String` makes serde reject the whole query as a
/// duplicate field, which axum turns into a 400 before the handler ever runs. The token records
/// scope without enforcing it (authorization is re-checked per request), so collecting them is
/// enough; the space-separated form is what the spec's token response carries back.
fn scopes(raw: Option<&str>) -> String {
    let Some(raw) = raw else { return String::new() };
    let mut out: Vec<String> = vec![];
    for (k, v) in form_urlencoded::parse(raw.as_bytes()) {
        if k == "scope" && !v.is_empty() && !out.iter().any(|s| s == v.as_ref()) {
            out.push(v.into_owned());
        }
    }
    out.join(" ")
}

/// `GET /v2/token` — exchange a long-lived credential for a short-lived bearer.
async fn token(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let scope = scopes(raw.as_deref());
    let who = match super::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(o)) => o,
        // Anonymous is allowed to ask, and gets a token for nobody: it can still pull public
        // images. Refusing here would break anonymous pull for spec-following clients, which
        // always visit the token endpoint before the pull.
        Ok(None) => String::new(),
        Err(r) => return r,
    };
    let jwt = match app.jwt.mint_registry(&who, &scope, TOKEN_TTL) {
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
        // Echoed so a client can see WHICH scopes were granted when it asked for several. It is
        // recorded, not enforced: authorization is re-checked per request against the image.
        "scope": scope,
    }))
    .into_response()
}

/// `GET /v2/_catalog?n=&last=` — the caller's own images. Scoped to the caller's owner: there is
/// no cross-team catalog, because there is no cross-team read.
async fn catalog(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let who = match super::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(w)) => w,
        Ok(None) => return super::auth::challenge(None),
        Err(r) => return r,
    };
    let names = match image_names(&app, &who).await {
        Ok(n) => n,
        Err(e) => return crate::http::internal_pub(e),
    };
    let all: Vec<String> = names.into_iter().map(|n| format!("{who}/{n}")).collect();
    let (page, truncated) = super::paginate(&all, &q);
    let mut r = axum::Json(serde_json::json!({"repositories": page})).into_response();
    if let Some(last) = truncated {
        let n = q.get("n").cloned().unwrap_or_default();
        r.headers_mut().insert(
            axum::http::header::LINK,
            format!("</v2/_catalog?n={n}&last={last}>; rel=\"next\"").parse().unwrap(),
        );
    }
    r
}

pub fn v2_routes() -> Router<Arc<App>> {
    // Blob routes get their own body cap, `max_layer()`, not the git-sized `max_body()` from
    // `http.rs`: a layer push and a git push are different sizes of thing and must not share one
    // knob — `max_body`'s 2 GiB default would otherwise make `max_layer`'s 10 GiB default
    // unreachable. Axum enforces this BEFORE the handler runs, so `finish_blob`'s own
    // `body.len()` check is the second line of defence, not the first: this layer stops an
    // oversized body during the read, the handler check catches anything that still reaches it.
    let blob_routes = Router::new()
        .route(
            "/v2/{owner}/{name}/blobs/{digest}",
            get(blobs::get_blob).head(blobs::head_blob).delete(blobs::delete_blob),
        )
        .route("/v2/{owner}/{name}/blobs/uploads/", post(blobs::start_upload))
        // Real clients send both forms, and without a trailing slash the path has the same
        // segment count as `.../blobs/{digest}` — matchit would otherwise route it there and
        // answer a confusing DIGEST_INVALID for a "digest" of literally "uploads". Registered
        // explicitly rather than relying on route-registration order to break the tie.
        .route("/v2/{owner}/{name}/blobs/uploads", post(blobs::start_upload))
        .route(
            "/v2/{owner}/{name}/blobs/uploads/{uuid}",
            put(blobs::finish_upload).patch(uploads::patch).get(uploads::status).delete(uploads::cancel),
        )
        .layer(axum::extract::DefaultBodyLimit::max(blobs::max_layer() as usize));

    Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2", get(v2_root))
        .route("/v2/token", get(token))
        .route("/v2/_catalog", get(catalog))
        .merge(blob_routes)
        .route(
            "/v2/{owner}/{name}/manifests/{reference}",
            get(manifests::get_manifest)
                .head(manifests::head_manifest)
                .put(manifests::put_manifest)
                .delete(manifests::delete_manifest),
        )
        .route("/v2/{owner}/{name}/tags/list", get(manifests::tags_list))
        .route("/v2/{owner}/{name}/referrers/{digest}", get(referrers::list))
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
