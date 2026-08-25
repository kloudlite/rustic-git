//! User-facing `/v1` routes for workspaces, environments and regions — spec §API
//! "User-facing (existing bearer token auth)".
//!
//! Every mutation follows the same shape: write the doc (`state: creating/...`), then enqueue a
//! `Job{state: Queued}` in that region, then answer 202 with the doc. The agent (not this crate)
//! does the real work; the doc's fields the job needs to see (region, ref, src ids) are set here
//! so the agent never has to re-derive them.
//!
//! Auth mirrors `crates/api`'s `caller()`: a Bearer JWT identifies the owner. There is no
//! existing "is this caller an admin" check anywhere in the codebase to reuse (grepped for one —
//! none exists), so region routes gate on a small static allowlist of emails passed in at
//! construction (`RUSTIC_GIT_WORKSPACES_ADMINS` in the api bin). Upgrade path: a real roles
//! table, if more than one admin-gated surface ever shows up.

// Same idiom and same tradeoff as `crates/api`: `Result<T, Response>` is the handler style here,
// and boxing the Err to please the size lint would add an allocation per refusal for nothing.
#![allow(clippy::result_large_err)]

use crate::model::*;
use crate::registry_client::{MAIN_REF, RegistryClient};
use crate::store::MetaStore;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rustic_git_core::httpx::bearer_token;
use rustic_git_core::jwt::Jwt;
use std::collections::HashSet;
use std::sync::Arc;

/// Team-membership lookup, kept behind a trait rather than a direct dependency on
/// `rustic_git_pulls::directory::Directory` (mongo-backed, heavy to construct) so unit tests can
/// supply a closure/stub instead. Production wires `Directory` in via an adapter in `bins/api`.
/// One method only: "is this caller in this team" reduces to "which teams is the caller in", and
/// list_env needs the full list anyway.
#[async_trait::async_trait]
pub trait MembershipCheck: Send + Sync {
    /// Every team slug `user` belongs to. Called once per request, no cache —
    /// ponytail: an in-process cache would cut the N+1 here, add one if this ever shows up hot.
    async fn teams_for(&self, user: &str) -> Vec<String>;
}

pub struct ApiState {
    pub store: Arc<dyn MetaStore>,
    pub jwt: Arc<Jwt>,
    /// Emails allowed to hit the admin-gated region routes. See module docs.
    pub admins: HashSet<String>,
    /// Reads volume history/refs by calling the server tier's `/vol-agent/{owner}/{name}/*`
    /// surface (`bins/server/src/vol_agent.rs`) with a shared agent token — the same
    /// `RegistryClient` the agent binary already uses to WRITE that surface, reused here to
    /// READ it. Picked over building a peer-listener forward (the `crates/api` browse pattern):
    /// this process has no peer secret or ownership-routing plumbing at all today, while
    /// `RegistryClient` already exists, is tested, and the vol-agent routes are public-listener
    /// and token-gated by design (agents are never on the peer network either). `None` when
    /// unconfigured — volume routes answer 503 rather than not existing.
    pub registry: Option<RegistryClient>,
    /// Team lookups for team-owned environments (see module docs' Part 2). `None` means no
    /// directory is wired (dev, or the directory tier is down) — team envs answer 503 rather than
    /// silently behaving as if the caller has no teams.
    pub membership: Option<Arc<dyn MembershipCheck>>,
}

impl ApiState {
    pub fn new(store: Arc<dyn MetaStore>, jwt: Arc<Jwt>, admins: HashSet<String>) -> Self {
        ApiState { store, jwt, admins, registry: None, membership: None }
    }

    pub fn with_registry(mut self, registry: RegistryClient) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn with_membership(mut self, membership: Arc<dyn MembershipCheck>) -> Self {
        self.membership = Some(membership);
        self
    }
}

async fn teams_for(s: &ApiState, caller: &str) -> Vec<String> {
    match &s.membership {
        Some(m) => m.teams_for(caller).await,
        None => Vec::new(),
    }
}

/// `owner` is the environment's actual owner field (a username or a team slug). Personal envs
/// (`owner == caller`) always pass; a team env passes when the caller is a member.
async fn may_act_on(s: &ApiState, caller: &str, owner: &str) -> bool {
    caller == owner || teams_for(s, caller).await.iter().any(|t| t == owner)
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/regions", post(create_region).get(list_regions))
        .route("/v1/workspaces", post(create_ws).get(list_ws))
        .route("/v1/workspaces/restore", post(restore_ws))
        .route("/v1/workspaces/{id}", get(get_ws).delete(delete_ws))
        .route("/v1/workspaces/{id}/clone", post(clone_ws))
        .route("/v1/workspaces/{id}/commit", post(commit_ws))
        .route("/v1/workspaces/{id}/push", post(push_ws))
        .route("/v1/workspaces/{id}/start", post(start_ws))
        .route("/v1/workspaces/{id}/stop", post(stop_ws))
        .route("/v1/environments", post(create_env).get(list_env))
        .route("/v1/environments/{id}", get(get_env).delete(delete_env))
        .route("/v1/environments/{id}/start", post(start_env))
        .route("/v1/environments/{id}/stop", post(stop_env))
        .route("/v1/environments/{id}/commit", post(commit_env))
        .route("/v1/environments/{id}/push", post(push_env))
        .route("/v1/volumes", get(list_volumes))
        .route("/v1/volumes/{name}/history", get(volume_history))
        .route("/v1/volumes/{name}/refs", get(volume_refs))
        .with_state(state)
}

fn rid(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}-{}", rustic_git_core::hex(&b))
}

/// Header carrying the per-region agent token, mirroring `rustic_git_core::peer::PEER_HEADER`'s
/// naming and constant-time-compare style.
pub const WS_AGENT_HEADER: &str = "x-rustic-git-ws-agent-token";

fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut b);
    rustic_git_core::hex(&b)
}

/// The owner identity for everything workspace/environment/volume-shaped is the USERNAME,
/// not the email: volume paths (`vol/{owner}/{name}`) go through the same owner-name
/// validation as git repos, and an email's `@`/`.` can never route there. A token without a
/// chosen username cannot own workspaces yet — same rule the web app enforces for repos.
fn caller(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<String, Response> {
    let tok = bearer_token(headers).ok_or_else(unauthorized)?;
    let c = state.jwt.verify(tok.trim()).map_err(|_| unauthorized())?;
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

fn store_err(e: crate::store::StoreErr) -> Response {
    use crate::store::StoreErr::*;
    match e {
        NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
        Conflict | CasFailed => (StatusCode::CONFLICT, "conflict, retry").into_response(),
        Other(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
    }
}

// ── regions ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewRegion {
    id: String,
    name: String,
    storage_account: String,
    blob_container: String,
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
    // Existing region re-registered without a token yet: generate and persist one now rather
    // than leaving agents unable to authenticate. Returned once, here, on the create response —
    // callers must save it (same shape as any bearer secret minted on creation).
    let agent_token =
        s.store.regions().await.map_err(store_err)?.into_iter().find(|r| r.id == body.id).and_then(|r| {
            if r.agent_token.is_empty() {
                None
            } else {
                Some(r.agent_token)
            }
        });
    let r = Region {
        id: body.id,
        name: body.name,
        storage_account: body.storage_account,
        blob_container: body.blob_container,
        status: "active".into(),
        agent_token: agent_token.unwrap_or_else(random_token),
    };
    s.store.put_region(&r).await.map_err(store_err)?;
    Ok((StatusCode::CREATED, Json(r)).into_response())
}

async fn list_regions(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    caller(&s, &headers)?;
    // The token is a secret, returned once on creation only — never echoed back on list.
    let mut regions = s.store.regions().await.map_err(store_err)?;
    for r in &mut regions {
        r.agent_token.clear();
    }
    Ok(Json(regions).into_response())
}

// ── workspaces ───────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewWorkspace {
    name: String,
    region: String,
    quota_gb: u64,
    #[serde(default = "default_ws_image")]
    image: String,
}

/// Creates the job and immediately tries to place it — most jobs land on an agent before the
/// caller ever sees the 202. `owner` is folded into the payload so the scheduler can look the
/// workspace back up for its warm-placement hint without re-deriving it.
async fn ws_job(
    store: &dyn MetaStore,
    owner: &str,
    region: &str,
    kind: JobKind,
    mut payload: serde_json::Value,
) -> Result<(), Response> {
    payload["owner"] = serde_json::json!(owner);
    let j = Job {
        id: rid("job"),
        region: region.to_string(),
        agent: None,
        kind,
        payload,
        state: JobState::Queued,
        lease_until: None,
        attempts: 0,
        error: None,
    };
    store.create_job(&j).await.map_err(store_err)?;
    let _ = crate::scheduler::schedule(store, &j).await.map_err(store_err)?;
    Ok(())
}

async fn create_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewWorkspace>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let w = Workspace {
        id: rid("ws"),
        owner,
        name: body.name,
        region: body.region,
        state: WsState::Creating,
        image: body.image,
        placement: None,
        volume: None,
        quota_gb: body.quota_gb,
        live_state: serde_json::Value::Null,
    };
    s.store.create_ws(&w).await.map_err(store_err)?;
    ws_job(&*s.store, &w.owner, &w.region, JobKind::WsCreate, serde_json::json!({"workspace": w.id})).await?;
    Ok((StatusCode::ACCEPTED, Json(w)).into_response())
}

async fn list_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    // Deleted docs stay in the store (blobs and history are immutable, the doc is the tombstone)
    // but a list is the living view — the first production listing showed five deleted test
    // workspaces beside the one real one.
    let list: Vec<_> = s
        .store
        .list_ws(&owner)
        .await
        .map_err(store_err)?
        .into_iter()
        .filter(|w| w.state != WsState::Deleted)
        .collect();
    Ok(Json(list).into_response())
}

async fn get_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (w, _) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    Ok(Json(w).into_response())
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

async fn delete_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (mut w, etag) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    w.state = WsState::Deleted;
    s.store.replace_ws(&w, &etag).await.map_err(store_err)?;
    ws_job(&*s.store, &w.owner, &w.region, JobKind::WsDelete, serde_json::json!({"workspace": w.id})).await?;
    Ok((StatusCode::ACCEPTED, Json(w)).into_response())
}

/// Mirrors `start_env`/`stop_env`: the doc state flips optimistically, the agent's job does the
/// actual `docker start`/`docker stop`.
async fn start_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (w, _) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    ws_job(&*s.store, &w.owner, &w.region, JobKind::WsStart, serde_json::json!({"workspace": w.id})).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

async fn stop_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (w, _) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    ws_job(&*s.store, &w.owner, &w.region, JobKind::WsStop, serde_json::json!({"workspace": w.id})).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

#[derive(serde::Deserialize)]
struct CloneBody {
    name: String,
}

/// The one local-copy route. Payload keys are `workspace` (the new copy), `src` (the source's
/// id), `owner`, `stop_container` — the agent (`bins/agent/src/lib.rs`'s `WsClone` arm) decides
/// at run time whether `src`'s container is running and picks `Engine::clone_running` (pause
/// around a live copy) or `Engine::clone_local` (no container to pause) accordingly; this
/// handler doesn't need to know which.
async fn clone_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CloneBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (src, _) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    let w = Workspace {
        id: rid("ws"),
        owner,
        name: body.name,
        region: src.region.clone(),
        state: WsState::Creating,
        image: src.image.clone(),
        placement: None,
        volume: src.volume.clone(),
        quota_gb: src.quota_gb,
        live_state: src.live_state.clone(),
    };
    s.store.create_ws(&w).await.map_err(store_err)?;
    // `Mount` names a VOLUME (a folder inside an env's own subvolume), never a workspace, so an
    // env can no longer mount a standalone workspace and there is nothing here to stop before
    // cloning it. `stop_projects` stays on the wire (the payload key + the agent's consumer in
    // `bins/agent/src/lib.rs`) for a future env-clone, which will have its own envs to stop.
    let stop_projects: Vec<String> = Vec::new();
    // The source's own container (`ws-{src.id}`) IS running state now — the clone hook must
    // pause it around the block-layer clone just like it would a compose project, so a clone
    // never races a write happening inside the container.
    let stop_container = format!("ws-{}", src.id);
    ws_job(
        &*s.store,
        &w.owner,
        &w.region,
        JobKind::WsClone,
        serde_json::json!({
            "workspace": w.id, "src": src.id, "stop_projects": stop_projects,
            "stop_container": stop_container,
        }),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(w)).into_response())
}

#[derive(serde::Deserialize)]
struct RestoreBody {
    name: String,
    snapshot_id: String,
    src_workspace: String,
}

/// New workspace grafted onto an explicit, possibly-older snapshot: lineage and live_state come
/// from the snapshot record, not from the source workspace's current head (that is what makes
/// this different from `clone`, which always clones off the current state).
async fn restore_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (src, _) =
        s.store.get_ws(&owner, &body.src_workspace).await.map_err(store_err)?.ok_or_else(not_found)?;
    let snap = s
        .store
        .get_snapshot(&body.src_workspace, &body.snapshot_id)
        .await
        .map_err(store_err)?
        .ok_or_else(not_found)?;
    let w = Workspace {
        id: rid("ws"),
        owner,
        name: body.name,
        region: src.region.clone(),
        state: WsState::Creating,
        image: src.image.clone(),
        placement: None,
        volume: src.volume.clone(),
        quota_gb: src.quota_gb,
        // The snapshot's own `state` snapshot of live_state, falling back to the source
        // workspace's current live_state if the snapshot never recorded one.
        live_state: if snap.state.is_null() { src.live_state.clone() } else { snap.state.clone() },
    };
    s.store.create_ws(&w).await.map_err(store_err)?;
    ws_job(
        &*s.store,
        &w.owner,
        &w.region,
        JobKind::WsRestore,
        serde_json::json!({
            "workspace": w.id,
            "src_workspace": src.id,
            "snapshot_id": snap.id,
        }),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(w)).into_response())
}

// ── environments ─────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewEnvironment {
    name: String,
    region: String,
    #[serde(default)]
    services: Vec<Service>,
    /// A team slug — makes this a team-owned environment, run on the team's bound node.
    /// `None`/equal to the caller means an ordinary personal environment.
    #[serde(default)]
    owner: Option<String>,
}

/// Resolve `NewEnvironment.owner` against the caller: personal (`None` or `caller`) always
/// passes; a different owner must be a team the caller belongs to, which needs a directory —
/// 503 rather than silently creating an environment nobody but this caller can ever see again.
async fn resolve_new_owner(s: &ApiState, caller: &str, owner: Option<String>) -> Result<String, Response> {
    let Some(owner) = owner else { return Ok(caller.to_string()) };
    if owner == caller {
        return Ok(owner);
    }
    match &s.membership {
        None => Err((StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured on this node").into_response()),
        Some(_) if may_act_on(s, caller, &owner).await => Ok(owner),
        Some(_) => Err((StatusCode::FORBIDDEN, "not a member of that team").into_response()),
    }
}

/// Finds an environment by id, trying the caller's own namespace first, then each team they
/// belong to — the store partitions by owner, so there is no other way to look one up by id
/// alone. ponytail: N+1 across the caller's teams, acceptable at current scale.
async fn find_env(s: &ApiState, caller: &str, id: &str) -> Result<(Environment, crate::store::Etag, String), Response> {
    if let Some((e, etag)) = s.store.get_env(caller, id).await.map_err(store_err)? {
        return Ok((e, etag, caller.to_string()));
    }
    for team in teams_for(s, caller).await {
        if let Some((e, etag)) = s.store.get_env(&team, id).await.map_err(store_err)? {
            return Ok((e, etag, team));
        }
    }
    Err(not_found())
}

async fn env_job(store: &dyn MetaStore, owner: &str, region: &str, kind: JobKind, id: &str) -> Result<(), Response> {
    let j = Job {
        id: rid("job"),
        region: region.to_string(),
        agent: None,
        kind,
        payload: serde_json::json!({"environment": id, "owner": owner}),
        state: JobState::Queued,
        lease_until: None,
        attempts: 0,
        error: None,
    };
    store.create_job(&j).await.map_err(store_err)?;
    let _ = crate::scheduler::schedule(store, &j).await.map_err(store_err)?;
    Ok(())
}

async fn create_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewEnvironment>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    // Mounts name volumes (folders inside the env's own subvolume), not workspaces — there is
    // no doc to look up any more, just a non-empty name for `EnvUp` to mkdir.
    if body.services.iter().any(|svc| svc.mounts.iter().any(|m| m.folder.is_empty())) {
        return Err((StatusCode::BAD_REQUEST, "mount folder name must not be empty").into_response());
    }
    let owner = resolve_new_owner(&s, &caller_id, body.owner).await?;
    let e = Environment {
        id: rid("env"),
        owner,
        name: body.name,
        region: body.region,
        state: EnvState::Creating,
        placement: None,
        volume: None,
        services: body.services,
    };
    s.store.create_env(&e).await.map_err(store_err)?;
    env_job(&*s.store, &e.owner, &e.region, JobKind::EnvUp, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}

#[derive(serde::Deserialize)]
struct ListEnvQuery {
    /// Filter to one owner (a username or a team slug) — what the web app's `/{owner}/environments`
    /// page passes so a team page shows only that team's environments, not the caller's personal
    /// ones mixed in. Validated the same way `create_env`'s team owner is: caller must be that
    /// owner, or a member of it.
    #[serde(default)]
    owner: Option<String>,
}

async fn list_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ListEnvQuery>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let owners: Vec<String> = match q.owner {
        Some(o) if may_act_on(&s, &caller_id, &o).await => vec![o],
        Some(_) => return Err(not_found()),
        None => {
            let mut owners = vec![caller_id.clone()];
            owners.extend(teams_for(&s, &caller_id).await);
            owners
        }
    };
    let mut list = vec![];
    for owner in owners {
        list.extend(s.store.list_env(&owner).await.map_err(store_err)?);
    }
    list.retain(|e| e.state != EnvState::Deleted);
    Ok(Json(list).into_response())
}

async fn get_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (e, _, _) = find_env(&s, &caller_id, &id).await?;
    Ok(Json(e).into_response())
}

async fn start_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (mut e, etag, _) = find_env(&s, &caller_id, &id).await?;
    e.state = EnvState::Creating;
    s.store.replace_env(&e, &etag).await.map_err(store_err)?;
    env_job(&*s.store, &e.owner, &e.region, JobKind::EnvUp, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}

async fn stop_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (mut e, etag, _) = find_env(&s, &caller_id, &id).await?;
    e.state = EnvState::Stopped;
    s.store.replace_env(&e, &etag).await.map_err(store_err)?;
    env_job(&*s.store, &e.owner, &e.region, JobKind::EnvDown, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}

async fn delete_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (mut e, etag, _) = find_env(&s, &caller_id, &id).await?;
    e.state = EnvState::Deleted;
    s.store.replace_env(&e, &etag).await.map_err(store_err)?;
    env_job(&*s.store, &e.owner, &e.region, JobKind::EnvDelete, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}

// ── commit / push ────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
struct CommitBody {
    message: Option<String>,
}

/// The commit body is optional (`{message?}`), and axum's `Json<T>` extractor 415s a request
/// with no body/content-type at all rather than treating it as absent — so the message is read
/// as raw bytes and parsed only when present, same forgiving shape a curl with no `-d` expects.
async fn optional_commit_message(body: axum::body::Bytes) -> Result<Option<String>, Response> {
    if body.is_empty() {
        return Ok(None);
    }
    let parsed: CommitBody = serde_json::from_slice(&body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid commit body").into_response())?;
    Ok(parsed.message)
}

/// Shared by every commit/push handler: builds the `Commit`/`Push` job payload the agent
/// consumes (`crates/workspaces/src/engine/ops.rs`'s `commit_core`/`push_core` read
/// `workspace`/`environment`, `owner`, `message`), region-scoped like every other job.
async fn commit_or_push_job(
    store: &dyn MetaStore,
    owner: &str,
    region: &str,
    kind: JobKind,
    id_key: &str,
    id: &str,
    message: Option<String>,
) -> Result<Response, Response> {
    let mut payload = serde_json::json!({id_key: id, "owner": owner});
    if let Some(m) = message {
        payload["message"] = serde_json::json!(m);
    }
    ws_job(store, owner, region, kind, payload).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

async fn commit_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (w, _) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    let msg = optional_commit_message(body).await?;
    commit_or_push_job(&*s.store, &w.owner, &w.region, JobKind::Commit, "workspace", &w.id, msg).await
}

async fn push_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (w, _) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    commit_or_push_job(&*s.store, &w.owner, &w.region, JobKind::Push, "workspace", &w.id, None).await
}

async fn commit_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (e, _, _) = find_env(&s, &caller_id, &id).await?;
    let msg = optional_commit_message(body).await?;
    commit_or_push_job(&*s.store, &e.owner, &e.region, JobKind::Commit, "environment", &e.id, msg).await
}

async fn push_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (e, _, _) = find_env(&s, &caller_id, &id).await?;
    commit_or_push_job(&*s.store, &e.owner, &e.region, JobKind::Push, "environment", &e.id, None).await
}

// ── volumes ──────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct VolumeSummary {
    /// Registry name — the ws/env id, matching the `{owner}/{name}` the vol-agent surface and
    /// `RegistryClient` already key on.
    name: String,
    kind: &'static str,
    /// `None` until the workspace/environment's first push writes a volume pointer.
    volume: Option<String>,
}

async fn list_volumes(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let mut out = vec![];
    // Workspaces stay strictly personal (no team ownership) — only the caller's own.
    for w in s.store.list_ws(&owner).await.map_err(store_err)? {
        if w.state != WsState::Deleted {
            out.push(VolumeSummary { name: w.id, kind: "workspace", volume: w.volume });
        }
    }
    // Environments can be team-owned: include the caller's own plus every team they belong to.
    let mut env_owners = vec![owner.clone()];
    env_owners.extend(teams_for(&s, &owner).await);
    for env_owner in env_owners {
        for e in s.store.list_env(&env_owner).await.map_err(store_err)? {
            if e.state != EnvState::Deleted {
                out.push(VolumeSummary { name: e.id, kind: "environment", volume: e.volume });
            }
        }
    }
    Ok(Json(out).into_response())
}

fn registry(s: &ApiState) -> Result<&RegistryClient, Response> {
    s.registry.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "volume registry not configured on this node").into_response()
    })
}

/// A volume `name` is only readable by the caller who owns the workspace or environment it
/// belongs to — the registry itself has no owner check (it trusts the agent token, not a JWT),
/// so this crate enforces it before ever asking the registry for anything.
async fn owns_volume(s: &ApiState, owner: &str, name: &str) -> Result<(), Response> {
    // Workspaces stay strictly personal.
    if s.store.get_ws(owner, name).await.map_err(store_err)?.is_some() {
        return Ok(());
    }
    // Environments can be team-owned: the caller's own namespace, then each team they belong to.
    if s.store.get_env(owner, name).await.map_err(store_err)?.is_some() {
        return Ok(());
    }
    for team in teams_for(s, owner).await {
        if s.store.get_env(&team, name).await.map_err(store_err)?.is_some() {
            return Ok(());
        }
    }
    Err(not_found())
}

async fn volume_history(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    owns_volume(&s, &owner, &name).await?;
    let reg = registry(&s)?;
    let history = reg
        .get_history(&owner, &name)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e).into_response())?;
    Ok(Json(history).into_response())
}

/// Derived from `history` rather than a raw registry ref-read: the vol-agent surface exposes no
/// `GET .../ref` (only the agent-only `POST .../ref` that moves it — see
/// `bins/server/src/vol_agent.rs`'s `VOL_AGENT_TAILS`), and there is exactly one ref per volume
/// (`MAIN_REF`, "main") whose value is always the newest commit in `history` — the same
/// "first = tip" convention `engine::ops` already relies on. Cheaper than adding a new
/// agent-token-gated route for one more read of data `history` already carries.
async fn volume_refs(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    owns_volume(&s, &owner, &name).await?;
    let reg = registry(&s)?;
    let history = reg
        .get_history(&owner, &name)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e).into_response())?;
    let tip = history.first().map(|r| r.id.clone());
    Ok(Json(serde_json::json!({MAIN_REF: tip})).into_response())
}
