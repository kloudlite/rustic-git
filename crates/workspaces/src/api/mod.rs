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
//! in `admin` (`/admin/*`, its own process under `RUSTIC_GIT_API_ROLE=admin`), which refuses a
//! token without the `superadmin` claim before routing. Here the claim is read only by
//! `may_act_on`'s third arm, and only for list/stop/delete/get — every ALLOCATING path (create,
//! clone, restore, push) decides its new object's owner through `scope::may_allocate_for`
//! instead, which never reads it: a superadmin is a claim, never an owner, and must not be able
//! to spend a team's quota without being a member. The static email allowlist this used to carry
//! is gone; `RUSTIC_GIT_WORKSPACES_ADMINS` is a bootstrap for the directory's list and nothing
//! reads it here.
//!
//! Split across `scope` (who the caller is, what they may act on), `workspaces`, `environments`,
//! `volumes` and `push` (I7) — one module per resource, this file keeps only what is shared by
//! all of them: `ApiState`, the router, auth, and the small set of error/lookup helpers every
//! handler in every submodule calls.

// Same idiom and same tradeoff as `crates/api`: `Result<T, Response>` is the handler style here,
// and boxing the Err to please the size lint would add an allocation per refusal for nothing.
#![allow(clippy::result_large_err)]

use crate::crd;
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
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
use std::sync::Arc;

pub mod admin;
mod environments;
mod push;
mod scope;
mod volumes;
mod workspaces;

// The crate's public surface is unchanged by the split: `bins/api` and the tests name
// `api::{router, ApiState, Directory, …}` and must keep doing so. `admin` is `pub` (unlike its
// siblings) because its handlers are reached from `bins/api/src/main.rs` choosing which router to
// mount, not only through `router()` here.
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

/// Who is calling, and whether they hold the platform-administrator claim.
///
/// A struct rather than the bare handle because two facts travel together everywhere: the owner
/// name every path is scoped by, and the claim `may_act_on` reads as its third arm. `Deref` and
/// `Display` are so the sites that only want the handle read unchanged.
#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    /// A CLAIM from the session token, minted at sign-in from the directory's list. Never an
    /// ownership: it decides who may act, never who owns anything, and it never widens a quota.
    pub superadmin: bool,
}

impl std::ops::Deref for Caller {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// What every workspace of an owner carries about them, from the directory the api tier owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerMaterial {
    /// The `authorized_keys` file sshd reads. Empty is a user with no keys.
    pub authorized_keys: String,
    /// What git commits as. Empty when the handle is nobody's, and git will ask.
    pub git_name: String,
    pub git_email: String,
}

/// A person's standing in a team, as the platform directory records it.
///
/// A local enum rather than `rustic_git_pulls::directory::Role` for the same reason the whole
/// `Directory` trait is local: this crate must not depend on the mongo-backed one just for a
/// lookup. `Ord` is declared by the variant ORDER — `Member < Admin < Owner` — so `>= Admin` is
/// the rank rule, and there is no second rank table to fall out of step with the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TeamRole {
    Member,
    Admin,
    Owner,
}

/// The four lookups this api makes against the platform directory, kept behind a trait rather
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

    /// The caller's role in `team`, or `None` when they are not a member — or when the lookup
    /// could not be made. Both answer "no" here, which is the safe direction for the one decision
    /// it feeds: who may raise a team's ceiling.
    ///
    /// `user` is whatever identity `teams_for` matches on, so the two can never disagree about who
    /// is in the team. Required (no default): unlike the other lookups, a stub that silently
    /// answered "not a member" would make the admin-only request check a no-op nobody tests.
    async fn team_role(&self, user: &str, team: &str) -> Option<TeamRole>;

    /// Does a team named `slug` exist? The one thing a slug's spelling cannot say by itself, and
    /// the quota routes need it to pick `default_quota(team)`'s right side rather than guess from
    /// who happens to be asking. Required (no default): a stub answering `false` here would make
    /// every team-owned quota request silently seed from the person defaults, and nothing would
    /// fail loudly enough to notice.
    async fn is_team(&self, slug: &str) -> bool;
}

pub struct ApiState {
    pub jwt: Arc<Jwt>,
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
    pub fn new(jwt: Arc<Jwt>) -> Self {
        ApiState {
            jwt,
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
        .route("/v1/quota-requests", post(create_quota_request).get(list_quota_requests))
        .route("/v1/regions", get(list_regions))
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
    let owner = q.owner.unwrap_or_else(|| c.name.clone());
    if !scope::may_act_on(&s, &c, &owner).await {
        return Err(not_found());
    }
    let client = kube(&s)?;
    let team = scope::is_team(&s, &owner).await;
    let limit = crate::quota::effective(client, &owner, team).await.map_err(kube_err)?;
    let used = crate::quota::usage(client, &owner).await.map_err(kube_err)?;
    Ok(Json(serde_json::json!({"owner": owner, "limit": limit, "used": used})).into_response())
}

#[derive(serde::Deserialize)]
struct NewQuotaRequest {
    /// Absent means the caller's own quota.
    #[serde(default)]
    owner: Option<String>,
    requested: crd::RequestedQuota,
    #[serde(default)]
    reason: String,
}

/// Who may ask, and for whom.
///
/// A person may always ask for their own. A team's ceiling is a team decision, so only a member
/// whose directory role is at least admin may ask on its behalf — checked against the DIRECTORY,
/// never against a label and never against who happens to have created something.
async fn may_request_for(s: &ApiState, caller: &str, owner: &str) -> Result<(), Response> {
    if owner == caller {
        return Ok(());
    }
    let Some(dir) = &s.directory else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured on this node").into_response());
    };
    match dir.team_role(caller, owner).await {
        Some(r) if r >= TeamRole::Admin => Ok(()),
        // A member gets the reason; a non-member learns nothing about the team at all.
        Some(_) => Err((StatusCode::FORBIDDEN, "only a team admin can request a team quota").into_response()),
        None => Err(not_found()),
    }
}

/// Every request of `owner`, label-selected — and re-checked against `spec.owner`, because the
/// label is a view.
async fn requests_of(c: &kube::Client, owner: &str) -> Result<Vec<crd::QuotaRequest>, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(c.clone());
    Ok(api
        .list(&scope::owned_by(owner))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.spec.owner == owner)
        .collect())
}

/// A request with no status yet is PENDING: `/v1` writes the object and stamps status in a second
/// call, and reading that window as "decided" would let two requests stand at once.
fn is_pending(r: &crd::QuotaRequest) -> bool {
    r.status.as_ref().map(|s| s.state).unwrap_or_default() == crd::RequestState::Pending
}

async fn create_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewQuotaRequest>,
) -> Result<Response, Response> {
    let caller = caller(&s, &headers).await?;
    let owner = body.owner.unwrap_or_else(|| caller.name.clone());
    may_request_for(&s, &caller, &owner).await?;
    let client = kube(&s)?;
    // One at a time, so the queue is a list of decisions rather than a list of the same ask.
    if requests_of(client, &owner).await?.iter().any(is_pending) {
        return Err((StatusCode::CONFLICT, "a request is already pending").into_response());
    }
    let id = rid("qr");
    let mut r = crd::QuotaRequest::new(
        &id,
        crd::QuotaRequestSpec { owner: owner.clone(), requested: body.requested, reason: body.reason },
    );
    // A view of `spec.owner`, so the queue and the owner's own list are indexed selectors — same
    // rule as every other label in this codebase.
    r.metadata.labels = Some(std::collections::BTreeMap::from([(OWNER_LABEL.to_string(), owner)]));
    let api: Api<crd::QuotaRequest> = Api::all(client.clone());
    let made = api.create(&kube::api::PostParams::default(), &r).await.map_err(kube_err)?;
    Ok((StatusCode::CREATED, Json(request_doc(&made))).into_response())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaRequestDoc {
    id: String,
    owner: String,
    requested: crd::RequestedQuota,
    reason: String,
    state: crd::RequestState,
    decided_by: Option<String>,
    decided_at: Option<String>,
    note: Option<String>,
    created_at: Option<String>,
}

fn request_doc(r: &crd::QuotaRequest) -> QuotaRequestDoc {
    let st = r.status.clone().unwrap_or_default();
    QuotaRequestDoc {
        id: r.name_any(),
        owner: r.spec.owner.clone(),
        requested: r.spec.requested.clone(),
        reason: r.spec.reason.clone(),
        state: st.state,
        decided_by: st.decided_by,
        decided_at: st.decided_at,
        note: st.note,
        created_at: r.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct RequestQuery {
    #[serde(default)]
    owner: Option<String>,
}

/// The caller's own requests and their teams'. `owner` narrows to one, and must be something the
/// caller may act on — same rule as every other owner-scoped read.
async fn list_quota_requests(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<RequestQuery>,
) -> Result<Response, Response> {
    let caller = caller(&s, &headers).await?;
    let client = kube(&s)?;
    let mut rows = Vec::new();
    match q.owner {
        Some(owner) => {
            if !scope::may_act_on(&s, &caller, &owner).await {
                return Err(not_found());
            }
            rows.extend(requests_of(client, &owner).await?);
        }
        None => {
            for owner in scope::caller_owners(&s, &caller).await {
                rows.extend(requests_of(client, &owner).await?);
            }
        }
    }
    rows.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    Ok(Json(rows.iter().map(request_doc).collect::<Vec<_>>()).into_response())
}

#[derive(serde::Deserialize, Default)]
struct Decision {
    #[serde(default)]
    note: Option<String>,
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
pub(crate) async fn caller(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<Caller, Response> {
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
    let superadmin = c.superadmin;
    let name = c.username.filter(|u| !u.is_empty()).ok_or_else(|| {
        (StatusCode::FORBIDDEN, "pick a username before using workspaces").into_response()
    })?;
    Ok(Caller { name, superadmin })
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response()
}

// ── regions ──────────────────────────────────────────────────────────────
//
// The write half (`create_region`) lives in `api::admin` now — a region is a platform decision.
// `list_regions` stays here: reading which regions exist is not superadmin-gated.

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
