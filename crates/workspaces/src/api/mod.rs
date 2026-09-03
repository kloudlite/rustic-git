//! User-facing `/v1` routes for workspaces, environments and regions — spec §API
//! "User-facing (existing bearer token auth)".
//!
//! Every mutation writes a CUSTOM RESOURCE and answers 202 with a projection of it. The object is
//! the work item: there is no queue, no lease and no dispatch — the node named by `spec.nodeName`
//! reconciles what it owns. `Region` is a CRD too (`crd::Region`) — cross-cluster metadata by
//! nature, but registered rarely enough that the cluster this tier already talks to is the
//! cheapest correct home for it.
//!
//! Auth mirrors `crates/api`'s `caller()`: a Bearer JWT identifies the owner. There is no
//! existing "is this caller an admin" check anywhere in the codebase to reuse (grepped for one —
//! none exists), so region routes gate on a small static allowlist of emails passed in at
//! construction (`RUSTIC_GIT_WORKSPACES_ADMINS` in the api bin). Upgrade path: a real roles
//! table, if more than one admin-gated surface ever shows up.
//!
//! Split across `scope` (who the caller is, what they may act on), `workspaces`, `environments`,
//! `volumes` and `push` (I7) — one module per resource, this file keeps only what is shared by
//! all of them: `ApiState`, the router, auth, and the small set of error/lookup helpers every
//! handler in every submodule calls.

// Same idiom and same tradeoff as `crates/api`: `Result<T, Response>` is the handler style here,
// and boxing the Err to please the size lint would add an allocation per refusal for nothing.
#![allow(clippy::result_large_err)]

use crate::crd;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::ResourceExt;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rustic_git_core::httpx::bearer_token;
use rustic_git_core::jwt::Jwt;
use std::collections::HashSet;
use std::sync::Arc;

mod environments;
mod push;
mod scope;
mod volumes;
mod workspaces;

// The crate's public surface is unchanged by the split: `bins/api` and the tests name
// `api::{router, ApiState, Directory, …}` and must keep doing so.
pub use scope::{owner_set_selector, Owned};
pub use workspaces::refresh_user_keys;

use environments::{
    clone_env, create_env, delete_env, get_env, list_env, restore_env,
    restore_env_in_place, start_env, stop_env,
};
use push::{push_env, push_ws};
use volumes::{delete_snapshot, delete_volume, list_volumes, volume_history, volume_refs};
use workspaces::{
    attach_ws, clone_ws, create_ws, delete_ws, detach_ws, get_ws, list_ws, patch_ws_packages,
    restore_ws, ssh_session, start_ws, stop_ws,
};

/// What every workspace of an owner carries about them, from the directory the api tier owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerMaterial {
    /// The `authorized_keys` file sshd reads. Empty is a user with no keys.
    pub authorized_keys: String,
    /// What git commits as. Empty when the handle is nobody's, and git will ask.
    pub git_name: String,
    pub git_email: String,
}

/// The three lookups this api makes against the platform directory, kept behind a trait rather
/// than a direct dependency on `rustic_git_pulls::directory::Directory` (mongo-backed, heavy to
/// construct) so unit tests can supply a stub instead. Production wires `Directory` in via an
/// adapter in `bins/api`.
///
/// Every method is REQUIRED. A defaulted one made a partial test stub read as a live-but-empty
/// directory — `teams_for` returning an empty Vec is "asked and answered" to `resolve_new_owner`,
/// which is a 403 "not a member", where no directory at all is a 503. A stub must say which it
/// means.
#[async_trait::async_trait]
pub trait Directory: Send + Sync {
    /// Every team slug `user` belongs to. Called once per request, no cache —
    /// ponytail: an in-process cache would cut the N+1 here, add one if this ever shows up hot.
    async fn teams_for(&self, user: &str) -> Vec<String>;

    /// Is this CLI login still valid? A `cli` JWT carries a `jti` whose row in the directory IS
    /// the revocation list — the same rule `crates/api`'s `user_identity` enforces. `false`
    /// refuses the token, which is what an unwired directory must do: a 30-day token nobody can
    /// cancel is the worse failure.
    async fn is_live(&self, jti: &str) -> bool;

    /// The owner's ssh keys and git identity. `None` when the lookup FAILED — distinct from `Some`
    /// with an empty `authorized_keys`, which is a user with no keys and is written as an empty
    /// file.
    async fn for_owner(&self, owner: &str) -> Option<OwnerMaterial>;
}

pub struct ApiState {
    pub jwt: Arc<Jwt>,
    /// Emails allowed to hit the admin-gated region routes. See module docs.
    pub admins: HashSet<String>,
    /// Team membership, CLI-token revocation and the owner's ssh keys. `None` means no directory
    /// is wired (dev, or the directory tier is down): team envs answer 503 rather than silently
    /// behaving as if the caller has no teams, CLI tokens are refused outright, and a workspace
    /// comes up with the private key alone exactly as before ssh existed.
    pub directory: Option<Arc<dyn Directory>>,
    /// `None` when no kubeconfig/in-cluster config is available: every workspace, environment and
    /// volume route answers 503 rather than not existing.
    pub kube: Option<kube::Client>,
    /// The auth store, solely so workspace creation can copy the owner's platform-issued git key
    /// into their namespace. `None` in dev and in tests: workspaces still create, they just come
    /// up without a key.
    pub keys: Option<Arc<rustic_git_storage::store::Store>>,
}

impl ApiState {
    pub fn new(jwt: Arc<Jwt>, admins: HashSet<String>) -> Self {
        ApiState {
            jwt,
            admins,
            directory: None,
            kube: None,
            keys: None,
        }
    }

    pub fn with_directory(mut self, directory: Arc<dyn Directory>) -> Self {
        self.directory = Some(directory);
        self
    }

    pub fn with_kube(mut self, client: kube::Client) -> Self {
        self.kube = Some(client);
        self
    }

    pub fn with_keys(mut self, keys: Arc<rustic_git_storage::store::Store>) -> Self {
        self.keys = Some(keys);
        self
    }

}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/quota", get(get_quota))
        .route("/v1/regions", post(create_region).get(list_regions))
        .route("/v1/workspaces", post(create_ws).get(list_ws))
        .route("/v1/workspaces/restore", post(restore_ws))
        .route("/v1/workspaces/{id}", get(get_ws).delete(delete_ws).patch(patch_ws_packages))
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

#[derive(serde::Deserialize)]
struct QuotaQuery {
    /// Absent means the caller's own. A team slug they belong to is allowed; anything else is a
    /// 404, same as every other owner-scoped read.
    #[serde(default)]
    owner: Option<String>,
}

/// `GET /v1/quota?owner=` — the ceiling and what is against it, both for one owner.
///
/// Usage is computed here and nowhere else, on every request (see `quota::usage`'s module doc).
async fn get_quota(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<QuotaQuery>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let owner = q.owner.unwrap_or_else(|| c.clone());
    if !scope::may_act_on(&s, &c, &owner).await {
        return Err(not_found());
    }
    let client = kube(&s)?;
    let team = owner != c;
    let limit = crate::quota::effective(client, &owner, team).await.map_err(kube_err)?;
    let used = crate::quota::usage(client, &owner).await.map_err(kube_err)?;
    Ok(Json(serde_json::json!({"owner": owner, "limit": limit, "used": used})).into_response())
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
    let limit = crate::quota::effective(c, owner, team).await.map_err(kube_err)?;
    let used = crate::quota::usage(c, owner).await.map_err(kube_err)?;
    for (dim, adding) in want {
        if let Err(msg) = crate::quota::check(*dim, &limit, &used, *adding) {
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
    format!("{prefix}-{}", rustic_git_core::hex(&b))
}

/// The owner identity for everything workspace/environment/volume-shaped is the USERNAME,
/// not the email: volume paths (`vol/{owner}/{name}`) go through the same owner-name
/// validation as git repos, and an email's `@`/`.` can never route there. A token without a
/// chosen username cannot own workspaces yet — same rule the web app enforces for repos.
pub(crate) async fn caller(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<String, Response> {
    let tok = bearer_token(headers).ok_or_else(unauthorized)?;
    let (c, jti) = state.jwt.verify_any_user(tok.trim()).map_err(|_| unauthorized())?;
    // Only a CLI token carries a `jti`, and only a CLI token is revocable: a session's lifetime
    // IS its expiry. Without a directory to ask, a CLI token authenticates nothing here.
    // ponytail: one directory read per CLI request, no cache — same tradeoff `teams_for` takes;
    // add a short-TTL cache if it shows up hot, remembering it delays a revocation by its TTL.
    if let Some(jti) = jti {
        match &state.directory {
            Some(check) if check.is_live(&jti).await => {}
            _ => return Err(unauthorized()),
        }
    }
    c.username.filter(|u| !u.is_empty()).ok_or_else(|| {
        (StatusCode::FORBIDDEN, "pick a username before using workspaces").into_response()
    })
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response()
}

fn require_admin(state: &ApiState, email: &str) -> Result<(), Response> {
    if state.admins.contains(email) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "admin only").into_response())
    }
}

// ── regions ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewRegion {
    id: String,
    name: String,
    /// `active` or `inactive`. Re-registering a region is the only way to retire one — there is
    /// no delete — and a retired region must stop being offered to new workspaces while its
    /// existing records stay readable.
    #[serde(default = "active_status")]
    status: String,
}

fn active_status() -> String {
    "active".into()
}

/// What a caller sees: the three fields `check_region` and the web consume, and nothing about
/// where the region's infrastructure lives.
#[derive(serde::Serialize)]
struct RegionDoc {
    id: String,
    name: String,
    status: String,
}

fn region_doc(r: &crd::Region) -> RegionDoc {
    RegionDoc { id: r.name_any(), name: r.spec.name.clone(), status: r.spec.status.clone() }
}

async fn create_region(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewRegion>,
) -> Result<Response, Response> {
    // Admin gating keys on the EMAIL (the allowlist's identity), not the username `caller`
    // resolves — an admin needs no username to register regions.
    let tok = bearer_token(&headers).ok_or_else(unauthorized)?;
    let email = s.jwt.verify(tok.trim()).map(|c| c.sub).map_err(|_| unauthorized())?;
    require_admin(&s, &email)?;
    // The id becomes an object name and a gateway hostname label, so it goes through the same
    // segment check every other path segment here does.
    check_path_segment(&body.id)?;
    let status = if body.status == "inactive" { "inactive" } else { "active" };
    let r = crd::Region::new(&body.id, crd::RegionSpec { name: body.name, status: status.into() });
    // Apply, not create: re-registering IS how a region is retired or renamed, so a second POST of
    // the same id must not be a 409.
    let api: Api<crd::Region> = Api::all(kube(&s)?.clone());
    let saved = api
        .patch(&body.id, &PatchParams::apply("rustic-git-api").force(), &Patch::Apply(&r))
        .await
        .map_err(kube_err)?;
    Ok((StatusCode::CREATED, Json(region_doc(&saved))).into_response())
}

async fn list_regions(
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
            tracing::error!(error = %e, "kubernetes");
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
    match rustic_git_storage::store::valid_segment(s) {
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
        let missing = kube::Error::Api(Box::new(kube::core::Status::failure("workspaces.rustic-git.io \"ws-1\" not found", "NotFound").with_code(404)));
        assert!(super::is_missing(&missing));
        let other = kube::Error::Api(Box::new(kube::core::Status::failure("conflict", "Conflict").with_code(409)));
        assert!(!super::is_missing(&other));
    }
}
