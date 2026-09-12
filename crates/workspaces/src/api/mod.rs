//! User-facing `/v1` routes for workspaces, environments and regions — spec §API
//! "User-facing (existing bearer token auth)".
//!
//! Every mutation writes a CUSTOM RESOURCE and answers 202 with a projection of it. The object is
//! the work item: there is no queue, no lease and no dispatch — the node named by `spec.nodeName`
//! reconciles what it owns. `Region` is a CRD too (`crd::Region`) — cross-cluster metadata by
//! nature, but registered rarely enough that the cluster this tier already talks to is the
//! cheapest correct home for it.
//!
//! Auth mirrors `crates/api`'s `caller()`: a Bearer JWT identifies the owner. Nothing superadmin-
//! only lives on this router: region creation, quota decisions and every cross-owner surface are
//! in `admin` (`/admin/*`, its own process under `KLOUDLITE_API_ROLE=admin`), which refuses a
//! token without the `superadmin` claim before routing. Here the claim is read only by
//! `may_act_on`'s third arm, and only for list/stop/delete/get — every ALLOCATING path (create,
//! clone, restore, push) decides its new object's owner through `scope::may_allocate_for`
//! instead, which never reads it: a superadmin is a claim, never an owner, and must not be able
//! to spend a team's quota without being a member. The static email allowlist this used to carry
//! is gone; `KLOUDLITE_WORKSPACES_ADMINS` is a bootstrap for the directory's list and nothing
//! reads it here.
//!
//! Split across `scope` (who the caller is, what they may act on), `workspaces`, `environments`,
//! `volumes` and `push` (I7) — one module per resource, this file keeps only what is shared by
//! all of them: `ApiState`, the router, auth, and the small set of error/lookup helpers every
//! handler in every submodule calls.

// A panicking request path is a dead pod (`panic = "abort"` in the release profile), so a
// `.unwrap()`/`.expect()` here is a decision, taken per site with an `allow` and its reason.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

// Same idiom and same tradeoff as `crates/api`: `Result<T, Response>` is the handler style here,
// and boxing the Err to please the size lint would add an allocation per refusal for nothing.
#![allow(clippy::result_large_err)]

use crate::crd;
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use kube::ResourceExt;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use kloudlite_core::httpx::bearer_token;
use kloudlite_core::jwt::Jwt;
use kloudlite_core::settings::LiveSettings;
use crate::settings::AgentSettings;
use std::sync::Arc;

mod state;
pub use state::*;
mod requests;
pub(crate) use requests::*;


pub mod admin;

mod environments;

pub mod keys;

mod push;

pub(crate) mod scope;

mod volumes;
// `pub`: the SLO probe reads `KNOWN_CENTRAL` so its rollout yield asks about exactly the
// workloads a roll moves — one list, not a second copy that drifts.

pub mod workloads;
mod workspaces;

// The crate's public surface is unchanged by the split: `bins/api` and the tests name
// `api::{router, ApiState, Directory, …}` and must keep doing so. `admin` is `pub` (unlike its
// siblings) because its handlers are reached from `bins/api/src/main.rs` choosing which router to
// mount, not only through `router()` here.
pub use scope::{owner_set_selector, Owned};
pub use workspaces::keys_changed;

use environments::{
    get_builder, get_my_builder, list_builders, start_builder, stop_builder,
    clear_intercept, clone_env, create_env, delete_env, get_env, list_env, restore_env,
    restore_env_in_place, set_intercept, start_env, stop_env,
};
use push::{push_env, push_ws};
use volumes::{delete_snapshot, delete_volume, list_volumes, volume_history, volume_refs};
use workspaces::{
    attach_ws, clone_ws, create_ws, delete_ws, detach_ws, get_ws, list_ws, patch_ws_packages, restore_ws,
    ssh_session, start_ws, stop_ws, update_ws_packages,
};


/// Every route that names an environment by id, and therefore every route a caller could name the
/// hidden builder on. `api_builders.rs` iterates this to prove each one 404s; the test below holds
/// it equal to the router itself, so a route added above and forgotten here fails the build rather
/// than quietly gaining a hole. (An axum `Router` cannot be walked for its paths, which is why
/// this is a list checked against the source rather than derived from the router value.)
pub const ENVIRONMENT_ID_ROUTES: &[&str] = &[
    "/v1/environments/{id}",
    "/v1/environments/{id}/start",
    "/v1/environments/{id}/stop",
    "/v1/environments/{id}/clone",
    "/v1/environments/{id}/push",
    "/v1/environments/{id}/restore-in-place",
    "/v1/environments/{id}/intercepts",
    "/v1/environments/{id}/intercepts/{service}",
];


#[cfg(test)]
mod route_tests {
    /// The router is the truth; this is the copy the builder's 404 test can iterate. Reading this
    /// file's own source is the only way to compare them — and it is a real check: adding
    /// `.route("/v1/environments/{id}/anything", ...)` above fails here until it is listed.
    #[test]
    fn every_environment_id_route_is_listed() {
        let mut found: Vec<String> = include_str!("mod.rs")
            .lines()
            .filter_map(|l| l.trim().strip_prefix(".route(\""))
            .filter_map(|l| l.split_once('"').map(|(p, _)| p.to_string()))
            .filter(|p| p.starts_with("/v1/environments/{id}"))
            .collect();
        found.sort();
        found.dedup();
        let mut listed: Vec<String> = super::ENVIRONMENT_ID_ROUTES.iter().map(|p| (*p).to_string()).collect();
        listed.sort();
        assert_eq!(found, listed, "the router and ENVIRONMENT_ID_ROUTES have drifted");
    }
}


/// Constant-time bytes compare. Neither `subtle` nor `ring` is in this crate's tree and this is
/// five lines: an early-exit `==` on a shared secret leaks it one byte at a time to anything that
/// can time a request.
pub(super) fn secret_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u32;
    for (x, y) in a.iter().zip(b) {
        diff |= u32::from(x ^ y);
    }
    diff == 0
}


/// The build gate's own routes: no person behind them, so no `caller`, no team membership and no
/// `visible_env` — one shared secret instead, checked here so no handler can forget it.
pub(super) async fn require_builder_secret(
    State(s): State<Arc<ApiState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let ok = s
        .builder_secret
        .as_deref()
        .zip(bearer_token(req.headers()))
        .is_some_and(|(want, got)| secret_eq(want.as_bytes(), got.trim().as_bytes()));
    if !ok {
        return unauthorized();
    }
    next.run(req).await
}


pub(super) fn internal_router(state: Arc<ApiState>) -> Router<Arc<ApiState>> {
    Router::new()
        .route("/v1/internal/builders", get(list_builders))
        .route("/v1/internal/builders/{slug}", get(get_builder))
        .route("/v1/internal/builders/{slug}/start", post(start_builder))
        .route("/v1/internal/builders/{slug}/stop", post(stop_builder))
        .route_layer(axum::middleware::from_fn_with_state(state, require_builder_secret))
}


pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/quota", get(get_quota))
        .route("/v1/quota-requests", post(create_quota_request).get(list_quota_requests))
        .route("/v1/requests", post(create_request).get(list_requests))
        .route("/v1/requests/{id}", get(get_request))
        .route("/v1/regions", get(list_regions))
        .route("/v1/workspaces", post(create_ws).get(list_ws))
        .route("/v1/workspaces/restore", post(restore_ws))
        .route("/v1/workspaces/{id}", get(get_ws).delete(delete_ws).patch(patch_ws_packages))
        .route("/v1/workspaces/{id}/packages/update", post(update_ws_packages))
        .route("/v1/workspaces/{id}/clone", post(clone_ws))
        .route("/v1/workspaces/{id}/push", post(push_ws))
        .route("/v1/workspaces/{id}/start", post(start_ws))
        .route("/v1/workspaces/{id}/stop", post(stop_ws))
        .route("/v1/workspaces/{id}/attach", post(attach_ws))
        .route("/v1/workspaces/{id}/detach", post(detach_ws))
        .route("/v1/workspaces/{id}/ssh-session", post(ssh_session))
        .route("/v1/environments", post(create_env).get(list_env))
        // Before `/{id}`: `restore` is a verb, not an environment id.
        .route("/v1/environments/restore", post(restore_env))
        .route("/v1/environments/{id}", get(get_env).delete(delete_env))
        .route("/v1/environments/{id}/start", post(start_env))
        .route("/v1/environments/{id}/stop", post(stop_env))
        .route("/v1/environments/{id}/clone", post(clone_env))
        .route("/v1/environments/{id}/push", post(push_env))
        .route("/v1/environments/{id}/restore-in-place", post(restore_env_in_place))
        .route("/v1/environments/{id}/intercepts", post(set_intercept))
        .route("/v1/environments/{id}/intercepts/{service}", axum::routing::delete(clear_intercept))
        .route("/v1/builders/me", get(get_my_builder))
        .merge(internal_router(state.clone()))
        .route("/v1/volumes", get(list_volumes))
        .route("/v1/volumes/{name}/history", get(volume_history))
        .route("/v1/volumes/{name}", axum::routing::delete(delete_volume))
        .route(
            "/v1/volumes/{name}/snapshots/{snapshot}",
            axum::routing::delete(delete_snapshot),
        )
        .route("/v1/volumes/{name}/refs", get(volume_refs))
        .with_state(state)
}

// ── quota ───────────────────────────────────────────────────────────────


/// Every request of `owner`, label-selected — and re-checked against `spec.owner`, because the
/// label is a view.
pub(crate) async fn requests_of_generic(c: &kube::Client, owner: &str) -> Result<Vec<crd::Request>, Response> {
    let api: Api<crd::Request> = Api::all(c.clone());
    Ok(api
        .list(&scope::owned_by(owner))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.spec.owner == owner)
        .collect())
}


/// The ONE place `/v1` refuses an allocation.
///
/// Every route that brings a new working copy, a new disk or a new snapshot into existence goes
/// through here, so the sentence, the status and the read-then-write window are decided once (see
/// `quota::check`'s doc for why read-then-write is accepted rather than locked).
///
/// `owner` is the OBJECT's owner, never the caller: a team's working copies count against the
/// team and nobody else. A superadmin gets no exemption — the claim says who may act, never how
/// much may exist.
pub(crate) async fn guard_alloc(
    s: &ApiState,
    owner: &str,
    team: bool,
    want: &[(crate::quota::Dim, u64)],
) -> Result<(), Response> {
    let c = kube(s)?;
    // The limit and the usage are independent reads of the same cluster; awaiting them in turn
    // made every create/clone/restore/push pay both round trips end to end (2026-09-12). Still
    // computed from the CRDs on every request — concurrency is not a cache.
    let (limit, used) = futures::try_join!(crate::quota::effective(c, owner, team), crate::quota::usage(c, owner))
        .map_err(kube_err)?;
    for (dim, adding) in want {
        if let Err(msg) = crate::quota::check(*dim, &limit, &used, *adding) {
            // The single gate every create/restore/clone/push passes through, so one counter here
            // covers every refusal without a second one per handler. `dim.word()` is the same word
            // the 409 sentence uses, so a spike and the message a user saw name the same thing.
            metrics::counter!("quota_refusals_total", "dimension" => dim.word()).increment(1);
            return Err((StatusCode::CONFLICT, msg).into_response());
        }
    }
    Ok(())
}

// The design doc also lists "changing a volume's quota" and "changing resources". Neither has a
// route today (`/v1` has no resize and no resources patch — `patch_ws_packages` is packages only),
// so there is nothing to gate. A future resize route calls `guard_alloc` with the DELTA, never a
// check of its own: the sentence and the read-then-write window are decided here.


/// What a new workspace costs, from the values the handler has already resolved and clamped.
pub(crate) fn workspace_cost(quota_gb: u64, res: &crd::PodResources) -> Vec<(crate::quota::Dim, u64)> {
    use crate::quota::{mebibytes, millicores, Dim};
    vec![
        (Dim::Workspaces, 1),
        (Dim::DiskGb, quota_gb),
        (Dim::Cpu, millicores(&res.cpu_limit).div_ceil(1000)),
        (Dim::MemoryGb, mebibytes(&res.memory_limit).div_ceil(1024)),
    ]
}


/// The same for an environment: every service gets the env unit, one definition in `k8s`.
pub(crate) fn environment_cost(quota_gb: u64, services: usize) -> Vec<(crate::quota::Dim, u64)> {
    use crate::quota::{mebibytes, millicores, Dim};
    let unit = crate::k8s::env_unit_resources();
    let n = services as u64;
    vec![
        (Dim::Environments, 1),
        (Dim::DiskGb, quota_gb),
        (Dim::Cpu, (n * millicores(&unit.cpu_limit)).div_ceil(1000)),
        (Dim::MemoryGb, (n * mebibytes(&unit.memory_limit)).div_ceil(1024)),
    ]
}


pub(crate) fn rid(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}-{}", kloudlite_core::hex(&b))
}


/// The owner identity for everything workspace/environment/volume-shaped is the USERNAME,
/// not the email: volume paths (`vol/{owner}/{name}`) go through the same owner-name
/// validation as git repos, and an email's `@`/`.` can never route there. A token without a
/// chosen username cannot own workspaces yet — same rule the web app enforces for repos.
pub(crate) async fn caller(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<Caller, Response> {
    let tok = bearer_token(headers).ok_or_else(unauthorized)?;
    let (c, jti) = state.jwt.verify_any_user(tok.trim()).map_err(|_| unauthorized())?;
    // Only a CLI token carries a `jti`, and only a CLI token is revocable: a session's lifetime
    // IS its expiry. Without a directory to ask, a CLI token authenticates nothing here.
    if let Some(jti) = jti {
        if !cli_token_live(state, &jti).await {
            return Err(unauthorized());
        }
    }
    let superadmin = c.superadmin;
    let name = c.username.filter(|u| !u.is_empty()).ok_or_else(|| {
        (StatusCode::FORBIDDEN, "pick a username before using workspaces").into_response()
    })?;
    Ok(Caller { name, superadmin })
}


/// How long a CLI `jti` the directory called live is trusted without asking again. Short on
/// purpose: this is exactly how late a revocation can take effect, and `kl-connect` makes several
/// `/v1` calls per command, each of which was its own directory round trip before (2026-09-12).
const CLI_LIVE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Positive answers only, and only for `CLI_LIVE_TTL`. A "no" is never remembered, so a token the
/// directory refuses stays refused on every request, and an unwired directory still authenticates
/// nothing.
async fn cli_token_live(state: &ApiState, jti: &str) -> bool {
    let now = std::time::Instant::now();
    if let Ok(seen) = state.cli_live.lock() {
        if seen.get(jti).is_some_and(|at| now.duration_since(*at) < CLI_LIVE_TTL) {
            return true;
        }
    }
    let Some(dir) = &state.directory else { return false };
    if !dir.is_live(jti).await {
        return false;
    }
    if let Ok(mut seen) = state.cli_live.lock() {
        // Swept here rather than on a beat: the map only ever holds the jtis this process has
        // actually seen, and a process nobody calls has nothing to sweep.
        seen.retain(|_, at| now.duration_since(*at) < CLI_LIVE_TTL);
        seen.insert(jti.to_string(), now);
    }
    true
}

pub(super) fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response()
}

// ── regions ──────────────────────────────────────────────────────────────
//
// The write half (`create_region`) lives in `api::admin` now — a region is a platform decision.
// `list_regions` stays here: reading which regions exist is not superadmin-gated.


/// What a caller sees: the three fields `check_region` and the web consume, and nothing about
/// where the region's infrastructure lives.
#[derive(serde::Serialize)]
pub(super) struct RegionDoc {
    id: String,
    name: String,
    status: String,
}


pub(super) fn region_doc(r: &crd::Region) -> RegionDoc {
    RegionDoc { id: r.name_any(), name: r.spec.name.clone(), status: r.spec.status.clone() }
}


pub(super) async fn list_regions(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    caller(&s, &headers).await?;
    let api: Api<crd::Region> = Api::all(kube(&s)?.clone());
    let rows: Vec<RegionDoc> =
        api.list(&ListParams::default()).await.map_err(kube_err)?.items.iter().map(region_doc).collect();
    Ok(Json(rows).into_response())
}

// ── the cluster ──────────────────────────────────────────────────────────


pub(crate) fn kube(s: &ApiState) -> Result<&kube::Client, Response> {
    s.kube.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "kubernetes not configured on this node").into_response()
    })
}


pub(crate) fn aks(s: &ApiState) -> Result<&kube::Client, Response> {
    s.aks.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "this node's own cluster is not configured").into_response()
    })
}


/// An API-server error keeps its own status where the caller can act on it (404 is "no such
/// workspace", 409 is "retry"); anything else is ours, not the caller's.
pub(crate) fn is_missing(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(ae) if ae.code == 404)
}


pub(crate) fn kube_err(e: kube::Error) -> Response {
    match &e {
        kube::Error::Api(ae) if ae.code == 404 => not_found(),
        kube::Error::Api(ae) if ae.code == 409 => (StatusCode::CONFLICT, "conflict, retry").into_response(),
        _ => {
            tracing::error!(reason = "kubernetes", error = %e, "request.failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "kubernetes error").into_response()
        }
    }
}

pub use crate::k8s::{ATTACHED_ENV_LABEL, KIND_LABEL, OWNER_LABEL, TEAM_LABEL};


/// `status.phase` is the state, and an object the controller has not seen yet has no status at
/// all — `creating` rather than a `null` the web app's enum cannot parse.
pub(crate) fn phase<T: serde::de::DeserializeOwned>(p: Option<&str>, default: T) -> T {
    p.and_then(|p| serde_json::from_value(serde_json::json!(p)).ok()).unwrap_or(default)
}


/// A region is an id the caller typed, and it becomes the OwnerBinding's name and the gateway
/// hostname. Unknown: a workspace no controller ever claims. Chosen: a binding name squatted in
/// someone else's region. Only what an admin registered and left active gets through.
pub(crate) async fn check_region(s: &ApiState, region: &str) -> Result<(), Response> {
    check_path_segment(region)?;
    let api: Api<crd::Region> = Api::all(kube(s)?.clone());
    let active = api
        .get_opt(region)
        .await
        .map_err(kube_err)?
        .is_some_and(|r| r.spec.status == "active");
    if active {
        return Ok(());
    }
    Err((StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error": "unknown region"}))).into_response())
}


/// The single `kube::Client` this tier holds today, but only after proving `region` names an
/// EXISTING active `crd::Region` — `admin::workloads`'s `Scope::Region(seg)` and the settings
/// routes both take `seg`/`region` straight off a URL path segment, so without this check a typo
/// or a probe would resolve to `kube(s)` anyway (there is only one client wired) and PATCH the
/// real cluster under a name nothing registered. `client_for` upgrades to a real per-region map
/// the day one exists; this is the one place both callers go through so that upgrade is a single
/// change (review finding on Task 5).
pub(crate) async fn client_for_region<'a>(s: &'a ApiState, region: &str) -> Result<&'a kube::Client, Response> {
    let client = kube(s)?;
    let api: Api<crd::Region> = Api::all(client.clone());
    match api.get_opt(region).await.map_err(kube_err)? {
        Some(r) if r.spec.status == "active" => Ok(client),
        _ => Err((StatusCode::NOT_FOUND, "no such region").into_response()),
    }
}


pub(crate) fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}


/// A workspace whose `Volume` the controller has not reported yet: 409, not a 500 and not a
/// silently dropped request. The caller can retry in a second.
pub(crate) fn not_ready() -> Response {
    (StatusCode::CONFLICT, "not ready yet: no volume for this workspace").into_response()
}


/// A volume name or snapshot id from the URL is spliced into a PEER url by `Upstream`, so a
/// `..` or an encoded slash would re-route the request to any browse route under the caller's
/// own owner. The same rule the create path applies to the names it mints.
pub(crate) fn check_path_segment(s: &str) -> Result<(), Response> {
    match kloudlite_storage::store::valid_segment(s) {
        true => Ok(()),
        false => Err((StatusCode::BAD_REQUEST, "invalid name").into_response()),
    }
}


pub(crate) fn kube_unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "the cluster could not be reached").into_response()
}


#[cfg(test)]
mod tests {
    /// Kube error text names endpoints, keys and query shapes; the caller gets a fixed body and
    /// the log gets the detail.
    #[tokio::test]
    async fn backend_error_text_never_reaches_the_caller() {
        let body = |r: axum::response::Response| async move {
            String::from_utf8_lossy(&axum::body::to_bytes(r.into_body(), 4096).await.unwrap()).into_owned()
        };
        let e = kube::Error::Api(Box::new(kube::core::Status::failure("AccountEndpoint=https://secret", "InternalError").with_code(500)));
        let r = super::kube_err(e);
        assert_eq!(r.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body(r).await.contains("secret"));
    }

    /// `delete_ws` must not stop at a 404 from the Workspace delete — that's the race the
    /// reorder was meant to cover, another caller already deleted it — and still has to fall
    /// through to collect the environment-side policy.
    #[test]
    fn a_404_from_the_workspace_delete_is_not_an_error() {
        let missing = kube::Error::Api(Box::new(kube::core::Status::failure("workspaces.kloudlite.io \"ws-1\" not found", "NotFound").with_code(404)));
        assert!(super::is_missing(&missing));
        let other = kube::Error::Api(Box::new(kube::core::Status::failure("conflict", "Conflict").with_code(409)));
        assert!(!super::is_missing(&other));
    }

    /// A directory that has not implemented granting answers `Unsupported`, and the approve arm
    /// turns that into a refusal — never a silent success on a membership nothing wrote.
    #[tokio::test]
    async fn a_directory_without_granting_refuses_rather_than_pretending() {
        use super::Directory as _;
        struct Bare;
        #[async_trait::async_trait]
        impl super::Directory for Bare {
            async fn teams_for(&self, _u: &str) -> Vec<String> {
                Vec::new()
            }
            async fn is_live(&self, _j: &str) -> bool {
                false
            }
            async fn for_owner(&self, _o: &str) -> Option<super::OwnerMaterial> {
                None
            }
            async fn authorized_keys_for_owner(&self, _o: &str) -> Option<String> {
                None
            }
            async fn owners_of(&self, _e: &str) -> Vec<String> {
                Vec::new()
            }
            async fn team_role(&self, _u: &str, _t: &str) -> Option<super::TeamRole> {
                None
            }
            async fn is_team(&self, _s: &str) -> bool {
                false
            }
            async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
                Err("no directory".into())
            }
            async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
                Err("no directory".into())
            }
        }
        assert_eq!(
            Bare.grant_access("acme", "meera", super::TeamRole::Admin).await,
            super::GrantAccess::Unsupported
        );
    }
}
