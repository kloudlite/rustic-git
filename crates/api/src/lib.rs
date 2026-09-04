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

pub mod gpg;

use kloudlite_git_core::Result;
use kloudlite_git_storage::cache::Cache;
use kloudlite_git_storage::events::Kind;
use kloudlite_git_storage::store::Store;
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
mod ratelimit;
mod repos;
mod signatures;
mod teams;

use browse::*;
use credentials::*;
/// Task 3's workspace `authorized_keys` writer reads this; nothing else here needs it public.
pub use credentials::{authorized_keys_for, git_identity_for};
use feed::*;
use forward::*;
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
// The hard ceiling on what is read from a git node is `httpx::MAX_REPLY` (`httpx::read_bounded`):
// `MAX_CACHED_BODY` gates only what is KEPT; a reply is buffered whole before it is answered, so
// without that one bad node is the same memory cliff the push path hit.
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

/// Called after an ssh key is added or removed, with the owner whose keys changed. Boxed and
/// dyn because the thing it does — rewriting Secrets in a Kubernetes namespace — lives two crates
/// away in `kloudlite-git-workspaces`, and this crate must not depend on kube to hand it a name.
pub type KeysChanged = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

pub struct Api {
    pub store: Arc<Store>,
    pub cache: Arc<Cache>,
    /// `None` when no database is configured: the browse routes still answer, and
    /// only the team routes report that they are unavailable. A missing database
    /// must not take down reads that never needed it.
    pub directory: Option<Arc<kloudlite_git_pulls::directory::Directory>>,
    /// Mints and verifies identity tokens. `None` leaves only the peer-header
    /// path, which is enough for internal calls but cannot issue a session.
    pub jwt: Option<Arc<kloudlite_git_core::jwt::Jwt>>,
    /// Base URL of the git peer Service, e.g. `http://kloudlite-git:8081`.
    pub upstream: String,
    pub secret: String,
    pub client: reqwest::Client,
    /// `None` outside the api binary (and in dev without a cluster): the key rows are still the
    /// record, and every workspace picks the change up the next time its Secret is written.
    pub on_keys_changed: Option<KeysChanged>,
    /// See `browse::Membership`: the browse path's answer to "may this person read under
    /// this owner", kept for a minute.
    pub membership: crate::browse::Membership,
    /// `stored ?? env ?? default`, refreshed every `SETTINGS_REFRESH_SECS` from `cluster/settings`
    /// — this tier's own copy, distinct from `App`'s on the git tier (this process has no `App`).
    /// `GET /v1/settings/central` reads the display fields off it for the web's clone menus;
    /// `/healthz` reports its version.
    pub central: kloudlite_git_core::settings::LiveSettings<kloudlite_git_core::settings::CentralSettings>,
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    store: Arc<Store>,
    cache: Arc<Cache>,
    directory: Option<Arc<kloudlite_git_pulls::directory::Directory>>,
    jwt: Option<Arc<kloudlite_git_core::jwt::Jwt>>,
    upstream: String,
    secret: String,
    listener: tokio::net::TcpListener,
    // Pre-built rather than an `ApiState`: which router (`api::router` vs `api::admin::router`)
    // is `bins/api`'s call via `KLOUDLITE_GIT_API_ROLE`, made once at startup — this crate only
    // merges whatever it is handed and never itself decides between a user and an admin surface.
    workspaces: Option<axum::Router>,
    on_keys_changed: Option<KeysChanged>,
    // Same `KLOUDLITE_GIT_API_ROLE` read that picks `workspaces`' router: the superadmin roster
    // routes are as admin-only as `/admin/*` is, so a user-role process must not compile them in
    // either, not just refuse them at auth time.
    admin_role: bool,
) -> Result<()> {
    // Refuse to boot rather than serve `caller`'s empty-secret guard as the only defense —
    // an empty secret is a misconfiguration, not a valid deployment.
    if secret.is_empty() {
        return Err(kloudlite_git_core::err("api peer secret must not be empty"));
    }
    let central = kloudlite_git_core::settings::LiveSettings::new(
        kloudlite_git_core::settings::CentralSettings::from_env(),
    );
    if let Some(bytes) = kloudlite_git_storage::config::get_central(&store.os).await {
        match serde_json::from_slice(&bytes) {
            Ok(doc) => central.store(
                kloudlite_git_core::settings::CentralSettings::from_env().merged_with(&doc),
            ),
            Err(e) => tracing::warn!(error = %e, "corrupt cluster/settings document at boot; using env defaults"),
        }
    }
    tokio::spawn(kloudlite_git_core::settings::refresh_central_beat(
        kloudlite_git_storage::config::central_fetch(store.os.clone()),
        central.clone(),
    ));
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
        on_keys_changed,
        membership: crate::browse::Membership::default(),
        central,
    });
    // The anonymous write surfaces, bounded per client address and per address-in-the-body.
    // `N/SECONDS`: a burst of N, refilling evenly. The cli-code bucket is sized to the code's
    // own TTL so it doubles as the cap on pending rows one address can hold at a time.
    let cli_code_limit = Arc::new(ratelimit::Limiter::from_env("KLOUDLITE_GIT_CLI_CODE_LIMIT", "20/600"));
    let signin_ip_limit = Arc::new(ratelimit::Limiter::from_env("KLOUDLITE_GIT_SIGNIN_IP_LIMIT", "10/60"));
    let signin_email_limit =
        Arc::new(ratelimit::Limiter::from_env("KLOUDLITE_GIT_SIGNIN_EMAIL_LIMIT", "1/60"));
    let app = Router::new()
        // Ahead of the fallback: `/healthz` is not a repo path and must never reach `handle`,
        // which would treat it as `/api/{owner}/{name}/...` and 404.
        .route(
            "/healthz",
            axum::routing::get(|State(api): State<Arc<Api>>| async move {
                (StatusCode::OK, format!("settings={}", api.central.version()))
            }),
        )
        // Display fields only — `clone_host`/`ssh_host`/`ssh_port`/`registry_host` — for the
        // web's clone menus (`lib/clone.ts`). Public, like `/healthz`: these are already shown
        // in a clone box to any visitor of a public repo's page, so there is nothing here worth
        // gating behind a bearer. The write half (`PUT /api/admin/settings`) lives on the git
        // tier's peer listener, not here.
        .route("/v1/settings/central", axum::routing::get(settings_central))
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
        // (superadmin roster routes are appended below, conditionally, before `.with_state`)
        // Anonymous on purpose: the public face of a team. The handler itself refuses a team
        // that has not opted in, so registering it without `caller` is not a hole.
        .route("/v1/teams/{slug}/profile", axum::routing::get(team_profile))
        .route(
            "/v1/teams/{slug}",
            axum::routing::get(get_team).patch(update_team).delete(delete_team),
        )
        .route(
            "/v1/teams/{slug}/members/{email}",
            axum::routing::patch(set_role).delete(remove_member),
        )
        // Joining is by invitation only. The raw token travels in the email and the accept
        // URL; the api stores its hash, so `/v1/invites/{token}` is the only place it is
        // ever presented back.
        .route("/v1/teams/{slug}/invites", axum::routing::post(create_invite))
        .route("/v1/teams/{slug}/invites/{id}", axum::routing::delete(revoke_invite))
        .route("/v1/invites/{token}", axum::routing::get(preview_invite))
        .route("/v1/invites/{token}/accept", axum::routing::post(accept_invite))
        // Magic-link sign-in: mint, then redeem. Peer-only, like /v1/users — no session
        // exists yet, and none may be used to mint one.
        .route(
            "/v1/signin/email",
            axum::routing::post(create_signin_link)
                .layer(axum::middleware::from_fn_with_state(signin_email_limit, ratelimit::per_email))
                .layer(axum::middleware::from_fn_with_state(signin_ip_limit, ratelimit::per_ip)),
        )
        .route("/v1/signin/email/{token}", axum::routing::post(redeem_signin_link))
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
        // The CLI login handshake. `code` is anonymous on purpose — it is what a machine with
        // no credentials asks for; nothing it returns is usable until a signed-in person
        // approves that code in the browser.
        .route(
            "/v1/cli/code",
            axum::routing::post(cli_code)
                .layer(axum::middleware::from_fn_with_state(cli_code_limit, ratelimit::per_ip)),
        )
        // Session-gated: the approval page reads it so it can name the DEVICE that is asking
        // before offering the button. Under `/code/` rather than beside it so the anonymous POST
        // and this stay one prefix apart from `/tokens`.
        .route("/v1/cli/code/{code}", axum::routing::get(cli_pending_code))
        .route("/v1/cli/approve", axum::routing::post(cli_approve))
        .route("/v1/cli/token", axum::routing::get(cli_token))
        .route("/v1/cli/tokens", axum::routing::get(list_cli_tokens))
        .route("/v1/cli/tokens/{id}", axum::routing::delete(revoke_cli_token))
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
        .layer(axum::middleware::from_fn_with_state("api", kloudlite_git_core::metrics::http_metrics));
    // Only the admin process compiles the superadmin roster routes in at all — same reasoning as
    // `workspaces_router` in `bins/api`'s main.rs: a user-role process must not be able to answer
    // them even if a future auth bug forgets to check the claim.
    let app = if admin_role {
        app.route("/api/admin/superadmins", axum::routing::get(list_superadmins)).route(
            "/api/admin/superadmins/{user}",
            axum::routing::post(add_superadmin).delete(remove_superadmin),
        )
    } else {
        app
    };
    let app = app.with_state(api);
    // Workspaces/environments/regions: a separate crate, a separate `MetaStore`, a separate
    // router state — merged in rather than folded into `Api` so that crate stays independent of
    // this one's git-repo machinery. Only mounted when a jwt signer is configured, same
    // precondition the routes' bearer-token auth already requires.
    let app = match workspaces {
        Some(ws) => app.merge(ws),
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
    /// The handle they picked, when the token carries one. Only a signed token has it; the
    /// peer path asserts an email and nothing more.
    pub username: Option<String>,
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
    if let Some(bearer) = kloudlite_git_core::httpx::bearer_token(headers) {
        let jwt = api
            .jwt
            .as_deref()
            .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "tokens not configured").into_response())?;
        return match jwt.verify(bearer.trim()) {
            Ok(c) => Ok(Identity { email: c.sub, name: Some(c.name), username: c.username }),
            // Never say which of signature, algorithm or expiry failed.
            Err(_) => Err((StatusCode::UNAUTHORIZED, "invalid or expired token").into_response()),
        };
    }
    peer_only(api, headers).map(|email| Identity { email, name: None, username: None })
}

/// `identify`, but a CLI login counts too.
///
/// Deliberately not folded into `identify`: a CLI token is revocable, and the ONLY thing that
/// makes that revocation real is the directory lookup below. A route that has not paid for that
/// lookup must keep refusing CLI tokens, or a revoked 30-day token would keep working there.
pub(crate) async fn user_identity(
    api: &Api,
    headers: &axum::http::HeaderMap,
) -> std::result::Result<Identity, Response> {
    let Some(bearer) = kloudlite_git_core::httpx::bearer_token(headers) else {
        return identify(api, headers);
    };
    let jwt = api
        .jwt
        .as_deref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "tokens not configured").into_response())?;
    let unauthorized = || (StatusCode::UNAUTHORIZED, "invalid or expired token").into_response();
    let (c, jti) = jwt.verify_any_user(bearer.trim()).map_err(|_| unauthorized())?;
    if let Some(jti) = jti {
        // The row IS the revocation list: `DELETE /v1/cli/tokens/{id}` removes it, and a `cli`
        // token whose row is gone authenticates nothing until it expires on its own.
        match directory(api)?.credential(&jti).await {
            Ok(Some(row)) if row.kind == kloudlite_git_pulls::directory::CredentialKind::CliToken => {}
            Ok(_) => return Err((StatusCode::UNAUTHORIZED, "this CLI login was revoked").into_response()),
            Err(e) => {
                tracing::error!(error = %e, "cli token lookup");
                return Err((StatusCode::BAD_GATEWAY, "could not check the login").into_response());
            }
        }
    }
    Ok(Identity { email: c.sub, name: Some(c.name), username: c.username })
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
        .get(kloudlite_git_core::peer::PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !kloudlite_git_core::peer::secret_eq(peer, &api.secret) {
        return Err((StatusCode::UNAUTHORIZED, "peer secret required").into_response());
    }
    match headers.get(kloudlite_git_core::peer::OWNER_HEADER).and_then(|v| v.to_str().ok()) {
        Some(u) if !u.trim().is_empty() => Ok(u.trim().to_string()),
        _ => Err((StatusCode::BAD_REQUEST, "caller identity required").into_response()),
    }
}

pub(crate) fn directory(api: &Api) -> std::result::Result<&kloudlite_git_pulls::directory::Directory, Response> {
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
            on_keys_changed: None,
            membership: crate::browse::Membership::default(),
            central: kloudlite_git_core::settings::LiveSettings::new(
                kloudlite_git_core::settings::CentralSettings::from_env(),
            ),
        }
    }

    pub(crate) fn test_marker(name: &str, public: bool) -> kloudlite_git_storage::index::Marker {
        kloudlite_git_storage::index::Marker {
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

/// `GET /v1/settings/central` — the display-only slice of the central document, for the web's
/// clone menus. `clone_host`/`ssh_host`/`registry_host` blank when unset (never written yet, or
/// no admin-set override), so the web falls back to its own `process.env` in that case exactly
/// as it did before this route existed.
async fn settings_central(State(api): State<Arc<Api>>) -> Response {
    let c = api.central.load();
    axum::Json(serde_json::json!({
        "cloneHost": c.clone_host,
        "sshHost": c.ssh_host,
        "sshPort": c.ssh_port,
        "registryHost": c.registry_host,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[tokio::test]
    async fn empty_peer_secret_never_authenticates() {
        let api = test_api_with_secret("").await;
        let mut h = axum::http::HeaderMap::new();
        h.insert(kloudlite_git_core::peer::PEER_HEADER, "".parse().unwrap());
        h.insert(kloudlite_git_core::peer::OWNER_HEADER, "alice".parse().unwrap());
        assert!(caller(&api, &h).is_err());
    }
}
