//! The read API's own process. It holds no repository state: it authenticates, consults the
//! cache, and on a miss asks the git fleet, whose routing already knows which node owns what.
//!
//! Browse routes live on the git nodes' PEER listener only, so `upstream` is the peer Service and
//! every forwarded request carries the peer secret plus, when the caller is authenticated, the
//! owner header — exactly the identity a forwarding node presents, so upstream authorizes this
//! the way it authorizes a peer.

// `Result<T, axum::Response>` is the handler idiom here: the Err is an early-return response,
// unwrapped exactly once per request by `?`. Boxing it to please the size lint would add an
// allocation per refusal for no measurable gain.
#![allow(clippy::result_large_err)]

pub(crate) use rustic_git_core::{err, hex, jwt, Result};
pub(crate) use rustic_git_storage::{cache, events, index, ownership, store};
pub(crate) use rustic_git_pulls::directory;
// The pure header helpers (`scheme`, `user_names`, `authorize`) live in `storage::auth`; the
// `axum`-dependent ones (`bearer_token`, `basic_token`, `basic_user_names`, `unauthorized`) moved
// to `core::httpx` because both this crate and `registry` need them and neither may depend on the
// other. One local module keeps every `crate::auth::…` call site unchanged.
pub(crate) mod auth {
    pub use rustic_git_core::httpx::{basic_token, basic_user_names, bearer_token, unauthorized};
    pub use rustic_git_storage::auth::*;
}
// `proxy::{PEER_HEADER, OWNER_HEADER, secret_eq}` — the peer-forwarding header names and
// constant-time compare live in `rustic_git_core::peer` (the axum/reqwest-heavy forwarder itself
// stays in the `git` crate, which this crate does not depend on). Aliased to keep every call
// site (`crate::proxy::...`) unchanged.
pub(crate) use rustic_git_core::peer as proxy;

pub mod gpg;

use crate::cache::Cache;
use crate::events::Kind;
use crate::store::Store;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use std::sync::Arc;

mod browse;
mod credentials;
mod feed;
mod forward;
mod images;
mod passkeys;
mod pulls;
mod repos;
mod signatures;
mod teams;

use browse::*;
use credentials::*;
use feed::*;
use forward::*;
pub use forward::read_bounded;
use images::*;
use passkeys::*;
use pulls::*;
use repos::*;
use signatures::*;
use teams::*;


/// How long each kind of answer is kept. Only `refs` can go stale; the rest are keyed by an
/// object id and are true forever, so their TTL is an eviction hint rather than a correctness one.
const TTL_REFS: u64 = 5;
const TTL_IMMUTABLE: u64 = 7 * 24 * 3600;
const TTL_META: u64 = 30;
const MAX_CACHED_BODY: usize = 1 << 20;
/// Hard ceiling on what is read from a git node. `MAX_CACHED_BODY` gates only what is KEPT; a
/// reply is buffered whole before it is answered, so without this one bad node is the same memory
/// cliff the push path hit. Comfortably above the largest browse answer (a 1 MiB inline blob).
/// Hand-synced twin in `bins/server/src/boot.rs` (`post_to_owner`) — mirror any change there.
const MAX_BODY: usize = 8 << 20;
/// A hanging git node must not hang every api request. `proxy::LEADER_TIMEOUT` is the precedent;
/// browse answers come off an already-open odb, so they are not slower than a lease call.
/// Hand-synced twin in `bins/server/src/boot.rs` — mirror any change there.
pub const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The visibility flag, cached apart from the answers it guards: it is what lets a hit be served
/// without asking a git node who may read this repo.
/// `%` is always escaped in a suffix, so `%00` is a byte sequence no path can produce: the
/// visibility flag can never collide with a cached answer (`/api/o/n/meta` would otherwise key on
/// exactly this).
pub const META: &str = "%00meta";

pub struct Api {
    pub store: Arc<Store>,
    pub cache: Arc<Cache>,
    /// `None` when no database is configured: the browse routes still answer, and
    /// only the team routes report that they are unavailable. A missing database
    /// must not take down reads that never needed it.
    pub directory: Option<Arc<crate::directory::Directory>>,
    /// Mints and verifies identity tokens. `None` leaves only the peer-header
    /// path, which is enough for internal calls but cannot issue a session.
    pub jwt: Option<Arc<crate::jwt::Jwt>>,
    /// Base URL of the git peer Service, e.g. `http://rustic-git:8081`.
    pub upstream: String,
    pub secret: String,
    pub client: reqwest::Client,
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    store: Arc<Store>,
    cache: Arc<Cache>,
    directory: Option<Arc<crate::directory::Directory>>,
    jwt: Option<Arc<crate::jwt::Jwt>>,
    upstream: String,
    secret: String,
    listener: tokio::net::TcpListener,
    workspaces: Option<Arc<rustic_git_workspaces::api::ApiState>>,
) -> Result<()> {
    // Refuse to boot rather than serve `caller`'s empty-secret guard as the only defense —
    // an empty secret is a misconfiguration, not a valid deployment.
    if secret.is_empty() {
        return Err(crate::err("api peer secret must not be empty"));
    }
    let api = Arc::new(Api {
        store,
        cache,
        directory,
        jwt,
        upstream: upstream.trim_end_matches('/').to_string(),
        secret,
        client: reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            // A default client has NO timeout, which silently undid `UPSTREAM_TIMEOUT`.
            .expect("building an HTTP client cannot fail with these options"),
    });
    let app = Router::new()
        // Ahead of the fallback: `/healthz` is not a repo path and must never reach `handle`,
        // which would treat it as `/api/{owner}/{name}/...` and 404.
        .route("/healthz", axum::routing::get(|| async { StatusCode::OK }))
        // Owner-scoped, not repo-scoped: two segments, not the three `handle`'s
        // `split_api_path` requires. Registered ahead of the fallback so it is matched
        // before `handle` ever sees it and refuses it as too short.
        .route("/api/{owner}/images", axum::routing::get(images_proxy))
        // Both writes on an image, same shape as `images_proxy`: registered ahead of the GET-only
        // fallback because they are POSTs, and hand-written because the caller check is "is this
        // exact owner", not the read-side `authorize` a repo's visibility flag drives.
        .route(
            "/api/{owner}/{image}/imagetagdelete",
            axum::routing::post(imagetagdelete_proxy),
        )
        .route("/api/{owner}/{image}/imagedelete", axum::routing::post(imagedelete_proxy))
        .route("/api/{owner}/{image}/imagevisibility", axum::routing::post(imagevisibility_proxy))
        // Team routes sit under /v1/, which `api_route` never parses, so they can
        // never collide with a repo path. Registered before the fallback because
        // the fallback is GET-only and would swallow the POST as a 405.
        .route("/v1/teams", axum::routing::post(create_team).get(list_teams))
        // Sign-in calls this. It is an upsert, not a create: the web app cannot
        // know whether this is someone's first visit, and should not have to.
        .route("/v1/users", axum::routing::post(upsert_user))
        // Picking a handle. Separate from sign-in because it happens once, later,
        // and can fail in a way sign-in must not: the handle may be taken.
        .route("/v1/users/username", axum::routing::post(claim_username))
        // Creating a repo. The api tier owns the question the git fleet cannot
        // answer — whether this person may create under this owner — and then
        // forwards to the node that will serve the repo.
        .route("/v1/repos", axum::routing::post(create_repo).get(list_repos))
        // A repo's own settings. `{owner}/{name}` rather than a flat id so the
        // path reads as the repo it is about.
        .route(
            "/v1/repos/{owner}/{name}",
            axum::routing::patch(update_repo).delete(delete_repo).get(get_repo),
        )
        .route(
            "/v1/repos/{owner}/{name}/protection",
            axum::routing::get(list_protection).post(set_protection),
        )
        // Pull requests. The metadata is this tier's; the commits, the diff and
        // the merge itself are the fleet's.
        .route(
            "/v1/repos/{owner}/{name}/pulls",
            axum::routing::get(list_pulls).post(open_pull),
        )
        .route("/v1/repos/{owner}/{name}/pulls/{number}", axum::routing::get(get_pull))
        .route(
            "/v1/repos/{owner}/{name}/pulls/{number}/comments",
            axum::routing::post(comment_on_pull),
        )
        .route("/v1/repos/{owner}/{name}/pulls/{number}/merge", axum::routing::post(merge_pull))
        .route("/v1/repos/{owner}/{name}/commits", axum::routing::post(commit_patch))
        .route("/v1/activity", axum::routing::get(activity))
        .route("/v1/repos/{owner}/{name}/pulls/{number}/close", axum::routing::post(close_pull))
        .route("/v1/repos/{owner}/{name}/compare", axum::routing::get(compare_branches))
        .route(
            "/v1/repos/{owner}/{name}/commits/{sha}/signature",
            axum::routing::get(verify_commit),
        )
        // Credentials: what a person uses to clone and push. The secret is written
        // to the object store the fleet authenticates against; only its metadata
        // is recorded here, so this is the one place that can list or revoke.
        .route("/v1/tokens", axum::routing::post(create_token).get(list_tokens))
        .route("/v1/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/v1/keys", axum::routing::post(add_key).get(list_keys))
        .route("/v1/keys/{id}", axum::routing::delete(remove_key))
        // The platform-issued key, distinct from /v1/keys (which the user supplies). POST is a
        // rotation, not a create: there is at most one, and regenerating revokes the old.
        .route(
            "/v1/platform-key",
            axum::routing::get(platform_key).post(regenerate_platform_key),
        )
        // Passkeys. Registration and listing are the signed-in person's; the
        // lookup is not — a sign-in has no session yet, which is the point.
        .route("/v1/passkeys", axum::routing::post(add_passkey).get(list_passkeys))
        .route("/v1/passkeys/{id}", axum::routing::delete(remove_passkey))
        .route("/v1/passkeys/lookup", axum::routing::post(lookup_passkey))
        .route("/v1/passkeys/{id}/used", axum::routing::post(passkey_used))
        // GET only. These are read-only views, and forwarding a POST as a GET (which is what
        // `any` did) would let a method the fleet never sees drive the cache.
        .fallback(axum::routing::get(handle))
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(api);
    // Workspaces/environments/regions: a separate crate, a separate `MetaStore`, a separate
    // router state — merged in rather than folded into `Api` so that crate stays independent of
    // this one's git-repo machinery. Only mounted when a jwt signer is configured, same
    // precondition the routes' bearer-token auth already requires.
    let app = match workspaces {
        Some(ws) => app.merge(rustic_git_workspaces::api::router(ws)),
        None => app,
    };
    axum::serve(listener, app).await?;
    Ok(())
}

/// Who is asking, resolved once. `name` is `Some` only for a session token — the peer path
/// asserts an email and nothing more.
pub(crate) struct Identity {
    pub email: String,
    pub name: Option<String>,
}

/// Who is asking.
///
/// A signed token first: it proves the identity by itself, so no trust in the
/// caller is required. The peer secret plus an asserted identity is the fallback
/// for service-to-service calls that have no user token yet — notably sign-in,
/// which is where a token comes FROM.
///
/// Verified ONCE per request: a handler that needs the display name as well as the email takes
/// the whole `Identity` rather than paying for a second HMAC over the same token.
pub(crate) fn identify(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<Identity, Response> {
    if let Some(bearer) = crate::auth::bearer_token(headers) {
        let jwt = api
            .jwt
            .as_deref()
            .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "tokens not configured").into_response())?;
        return match jwt.verify(bearer.trim()) {
            Ok(c) => Ok(Identity { email: c.sub, name: Some(c.name) }),
            // Never say which of signature, algorithm or expiry failed.
            Err(_) => Err((StatusCode::UNAUTHORIZED, "invalid or expired token").into_response()),
        };
    }
    peer_only(api, headers).map(|email| Identity { email, name: None })
}

/// `identify`, for the many callers that only need the email.
pub(crate) fn caller(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<String, Response> {
    identify(api, headers).map(|i| i.email)
}

/// The peer half of `caller`, on its own: the peer secret plus the identity the peer asserts,
/// and NO Bearer path. For the routes that mint or precede a session — sign-in, passkey lookup,
/// the passkey counter — a session must not be enough, or a leaked token renews itself forever
/// and any signed-in person can read or corrupt another's passkey. A Bearer header is simply not
/// looked at here: a caller that also presents the peer secret is the web app, and it is the
/// secret that admits it.
pub(crate) fn peer_only(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<String, Response> {
    let peer = headers
        .get(crate::proxy::PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !crate::proxy::secret_eq(peer, &api.secret) {
        return Err((StatusCode::UNAUTHORIZED, "peer secret required").into_response());
    }
    match headers.get(crate::proxy::OWNER_HEADER).and_then(|v| v.to_str().ok()) {
        Some(u) if !u.trim().is_empty() => Ok(u.trim().to_string()),
        _ => Err((StatusCode::BAD_REQUEST, "caller identity required").into_response()),
    }
}

pub(crate) fn directory(api: &Api) -> std::result::Result<&crate::directory::Directory, Response> {
    api.directory
        .as_deref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "teams database not configured").into_response())
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    /// Minimal `Api` for tests that only exercise header/secret logic — an in-memory
    /// store and cache so no real infra is needed to build the struct.
    pub(crate) async fn test_api_with_secret(secret: &str) -> Api {
        let os: Arc<dyn slatedb::object_store::ObjectStore> =
            Arc::new(slatedb::object_store::memory::InMemory::new());
        let store = Store::open(os, std::env::temp_dir(), false).await.unwrap();
        Api {
            store: Arc::new(store),
            cache: Arc::new(Cache::memory()),
            directory: None,
            jwt: None,
            upstream: String::new(),
            secret: secret.to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub(crate) fn test_marker(name: &str, public: bool) -> crate::index::Marker {
        crate::index::Marker {
            name: name.into(),
            public,
            created_by: "alice@example.com".into(),
            created_ms: 1_700_000_000_000,
            description: format!("the {name} repo"),
            manifests: 0,
            updated_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[tokio::test]
    async fn empty_peer_secret_never_authenticates() {
        let api = test_api_with_secret("").await;
        let mut h = axum::http::HeaderMap::new();
        h.insert(crate::proxy::PEER_HEADER, "".parse().unwrap());
        h.insert(crate::proxy::OWNER_HEADER, "alice".parse().unwrap());
        assert!(caller(&api, &h).is_err());
    }
}
