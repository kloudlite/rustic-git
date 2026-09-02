use crate::Trusted;
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

/// An owner's images for listing: the index markers, unioned with the object-store directory
/// listing for any image pushed before the backfill ran (no marker yet). Both sides are plain
/// object-store reads — same any-node safety as `image_names` — so this stays callable from a
/// handler that cannot route to a specific image's database.
///
/// `include_private` is passed straight through to `index::list`: callers must never pass `true`
/// for an unauthenticated caller, exactly as that function documents.
///
/// `q` is the caller's `n`/`last` query: only that page's marker bodies are read (`index::list_page`).
/// Callers still run `paginate` over the result — the unmarked fallback below is unpaged, and the
/// second pass is what keeps the two halves on one contract.
pub async fn image_listing(
    app: &App,
    owner: &str,
    include_private: bool,
    q: &std::collections::HashMap<String, String>,
) -> crate::Result<Vec<crate::index::Marker>> {
    let n = q.get("n").and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(usize::MAX);
    let mut markers =
        crate::index::list_page(&app.store, crate::index::Kind::Img, owner, include_private, q.get("last").map(String::as_str), n)
            .await?;
    let marked: std::collections::HashSet<String> = markers.iter().map(|m| m.name.clone()).collect();
    // An unmarked (pre-backfill) image has no visibility record, so it defaults private just like
    // a freshly-pushed one — an unauthenticated caller must never see it, exactly as `index::list`
    // already withholds a marked-private name from that caller.
    let unmarked: Vec<String> = if include_private {
        // ponytail: fallback dies with the backfill
        image_names(app, owner).await?.into_iter().filter(|n| !marked.contains(n)).collect()
    } else {
        Vec::new()
    };
    // One listing per image, fanned out — a serial loop here put the whole catalog page behind
    // N sequential round trips.
    let stats = futures::future::join_all(unmarked.iter().map(|n| super::store::manifest_stat(&app.store, owner, n))).await;
    for (name, stat) in unmarked.into_iter().zip(stats) {
        let (count, newest) = stat.unwrap_or((0, None));
        markers.push(crate::index::Marker {
            name,
            public: false,
            created_by: String::new(),
            created_ms: 0,
            description: String::new(),
            manifests: count as u64,
            updated_ms: newest.unwrap_or(0),
        });
    }
    markers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(markers)
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
        Err(e) => return crate::oci_internal(e),
    };
    // RFC 3339, not a Unix integer: the field is a `time.Time` in docker's token response, so a
    // number here fails its JSON decode with "input is not a JSON string" AFTER the token was
    // successfully minted — an error that reads like an auth failure but is a formatting one.
    let issued = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
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
    // Owner-scoped and already authenticated as `who` above, so `include_private: true` is safe —
    // same source `images` uses (`image_listing`), just re-shaped into repository names.
    // `last` on the wire is `{who}/{name}`; the index knows the name alone. A marker that does
    // not carry this owner's prefix names nothing in the index, so it must not be handed down as
    // one — the page below re-filters against the full `{who}/{name}` strings and is the honest
    // answer for a foreign or malformed marker. `n` goes with it: an index page truncated
    // against a marker the index never saw would hide the rows the page then wants.
    let mut page_q = q.clone();
    // `final_q` drives the second `paginate` below over the full `{who}/{name}` strings, so a
    // foreign `last` has to be scrubbed there too — leaving it in place would truncate `all`
    // against a marker the index page above never saw either.
    let mut final_q = q.clone();
    if let Some(last) = q.get("last") {
        match last.strip_prefix(&format!("{who}/")) {
            Some(name) => {
                page_q.insert("last".into(), name.to_string());
            }
            None => {
                page_q.remove("last");
                page_q.remove("n");
                final_q.remove("last");
                final_q.remove("n");
            }
        }
    }
    let markers = match image_listing(&app, &who, true, &page_q).await {
        Ok(m) => m,
        Err(e) => return crate::oci_internal(e),
    };
    let all: Vec<String> = markers.into_iter().map(|m| format!("{who}/{}", m.name)).collect();
    let (page, truncated) = super::paginate(&all, &final_q);
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
    // `bins/server/src/router/route.rs`: a layer push and a git push are different sizes of thing and must not share one
    // knob. The handlers take the raw `Body` (they stream), and axum's `DefaultBodyLimit` does
    // NOT apply to that extractor — so the cap that actually holds is `uploads::pour`'s own
    // count. This layer stays for the day a `Bytes` extractor sneaks back onto one of these
    // routes: it would then be capped at the right number instead of axum's 2 MB default.
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
        .merge(
            // Same reasoning as `blob_routes` above: axum's `DefaultBodyLimit` enforces BEFORE
            // the handler runs, so without an explicit cap here the 2 MB default would 413 a
            // legal ~3.9 MB manifest before `put_manifest`'s own `MAX_MANIFEST` check ever sees
            // it. Sized off `manifests::MAX_MANIFEST` so the two limits can't drift apart — the
            // layer is the enforcement, the handler check is the second line of defence.
            Router::new()
                .route(
                    "/v2/{owner}/{name}/manifests/{reference}",
                    get(manifests::get_manifest)
                        .head(manifests::head_manifest)
                        .put(manifests::put_manifest)
                        .delete(manifests::delete_manifest),
                )
                .layer(axum::extract::DefaultBodyLimit::max(manifests::MAX_MANIFEST)),
        )
        .route("/v2/{owner}/{name}/tags/list", get(manifests::tags_list))
        .route("/v2/{owner}/{name}/referrers/{digest}", get(referrers::list))
        .layer(axum::middleware::map_response(oci_envelope))
}

/// axum's own refusals — the `DefaultBodyLimit` 413, a 405 for a method no route takes — are
/// plain text, and a registry client parses every `/v2` error as the OCI envelope. Re-wrapped
/// here so the rule "every `/v2` error is `oci_err`" has no exceptions. Headers are kept: a
/// refusal's `Range`/`Location`/`WWW-Authenticate` are what the client acts on.
async fn oci_envelope(r: Response) -> Response {
    let json = r
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    if !r.status().is_client_error() || json {
        return r;
    }
    let code = match r.status() {
        StatusCode::PAYLOAD_TOO_LARGE => "SIZE_INVALID",
        StatusCode::NOT_FOUND => "NAME_UNKNOWN",
        _ => "UNSUPPORTED",
    };
    let (mut parts, _) = r.into_parts();
    let envelope = super::oci_err(parts.status, code, parts.status.canonical_reason().unwrap_or("refused"));
    let (env_parts, body) = envelope.into_parts();
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.insert(axum::http::header::CONTENT_TYPE, env_parts.headers[axum::http::header::CONTENT_TYPE].clone());
    Response::from_parts(parts, body)
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
