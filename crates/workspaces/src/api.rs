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
use crate::store::MetaStore;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rustic_git_core::httpx::bearer_token;
use rustic_git_core::jwt::Jwt;
use std::collections::HashSet;
use std::sync::Arc;

pub struct ApiState {
    pub store: Arc<dyn MetaStore>,
    pub jwt: Arc<Jwt>,
    /// Emails allowed to hit the admin-gated region routes. See module docs.
    pub admins: HashSet<String>,
    /// Long-poll hold and inner retry interval for `GET /v1/agent/work`. Real 30s/1s; tests
    /// shrink these so the leasing/204-timeout tests don't take real minutes.
    pub agent_poll_window: std::time::Duration,
    pub agent_poll_interval: std::time::Duration,
}

impl ApiState {
    /// Real deployment defaults (30s hold, 1s inner interval) — see spec §API agent-facing.
    pub fn new(store: Arc<dyn MetaStore>, jwt: Arc<Jwt>, admins: HashSet<String>) -> Self {
        ApiState {
            store,
            jwt,
            admins,
            agent_poll_window: std::time::Duration::from_secs(30),
            agent_poll_interval: std::time::Duration::from_secs(1),
        }
    }
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/regions", post(create_region).get(list_regions))
        .route("/v1/workspaces", post(create_ws).get(list_ws))
        .route("/v1/workspaces/from-snapshot", post(from_snapshot))
        .route("/v1/workspaces/{id}", get(get_ws).delete(delete_ws))
        .route("/v1/workspaces/{id}/fork", post(fork_ws))
        .route("/v1/workspaces/{id}/clone", post(clone_ws))
        .route("/v1/environments", post(create_env).get(list_env))
        .route("/v1/environments/{id}", get(get_env).delete(delete_env))
        .route("/v1/environments/{id}/start", post(start_env))
        .route("/v1/environments/{id}/stop", post(stop_env))
        .route("/v1/agent/register", post(agent_register))
        .route("/v1/agent/work", get(agent_work))
        .route("/v1/agent/jobs/{id}/done", post(agent_job_done))
        .route("/v1/agent/jobs/{id}/failed", post(agent_job_failed))
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

fn caller(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<String, Response> {
    let tok = bearer_token(headers).ok_or_else(unauthorized)?;
    state.jwt.verify(tok.trim()).map(|c| c.sub).map_err(|_| unauthorized())
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
    let email = caller(&s, &headers)?;
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
        placement: None,
        ref_: None,
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
    let list = s.store.list_ws(&owner).await.map_err(store_err)?;
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

#[derive(serde::Deserialize)]
struct ForkBody {
    name: String,
}

async fn fork_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ForkBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (src, _) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    let w = Workspace {
        id: rid("ws"),
        owner,
        name: body.name,
        region: src.region.clone(),
        state: WsState::Creating,
        placement: None,
        ref_: src.ref_.clone(),
        quota_gb: src.quota_gb,
        live_state: src.live_state.clone(),
    };
    s.store.create_ws(&w).await.map_err(store_err)?;
    ws_job(
        &*s.store,
        &w.owner,
        &w.region,
        JobKind::WsFork,
        serde_json::json!({"workspace": w.id, "src_workspace": src.id}),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(w)).into_response())
}

async fn clone_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ForkBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (src, _) = s.store.get_ws(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    let w = Workspace {
        id: rid("ws"),
        owner,
        name: body.name,
        region: src.region.clone(),
        state: WsState::Creating,
        placement: None,
        ref_: src.ref_.clone(),
        quota_gb: src.quota_gb,
        live_state: src.live_state.clone(),
    };
    s.store.create_ws(&w).await.map_err(store_err)?;
    ws_job(
        &*s.store,
        &w.owner,
        &w.region,
        JobKind::WsClone,
        serde_json::json!({"workspace": w.id, "src": src.id}),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(w)).into_response())
}

#[derive(serde::Deserialize)]
struct FromSnapshotBody {
    name: String,
    snapshot_id: String,
    src_workspace: String,
}

/// New workspace grafted onto an explicit, possibly-older snapshot: lineage and live_state come
/// from the snapshot record, not from the source workspace's current head (that is what makes
/// this different from `fork`, which always forks off the current state).
async fn from_snapshot(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<FromSnapshotBody>,
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
        placement: None,
        ref_: src.ref_.clone(),
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
        JobKind::WsFork,
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
    let owner = caller(&s, &headers)?;
    let e = Environment {
        id: rid("env"),
        owner,
        name: body.name,
        region: body.region,
        state: EnvState::Creating,
        placement: None,
        services: body.services,
    };
    s.store.create_env(&e).await.map_err(store_err)?;
    env_job(&*s.store, &e.owner, &e.region, JobKind::EnvUp, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}

async fn list_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let list = s.store.list_env(&owner).await.map_err(store_err)?;
    Ok(Json(list).into_response())
}

async fn get_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (e, _) = s.store.get_env(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    Ok(Json(e).into_response())
}

async fn start_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (mut e, etag) = s.store.get_env(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
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
    let owner = caller(&s, &headers)?;
    let (mut e, etag) = s.store.get_env(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    e.state = EnvState::Stopped;
    s.store.replace_env(&e, &etag).await.map_err(store_err)?;
    env_job(&*s.store, &e.owner, &e.region, JobKind::EnvDown, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}

// ── agents ───────────────────────────────────────────────────────────────
// Auth here is a per-region shared secret (`Region.agent_token`), not the user JWT — agents
// are unattended fleet processes, not logged-in humans. `WS_AGENT_HEADER` mirrors
// `rustic_git_core::peer::PEER_HEADER`'s header-secret pattern, `secret_eq` its constant-time
// compare so a mismatched token can't be timed out byte-by-byte.

fn agent_header(headers: &axum::http::HeaderMap) -> Result<&str, Response> {
    headers.get(WS_AGENT_HEADER).and_then(|v| v.to_str().ok()).ok_or_else(unauthorized)
}

/// Register: the region is named explicitly in the payload, so the token is checked against
/// that one region's doc directly.
async fn region_by_id(state: &ApiState, id: &str) -> Result<Region, Response> {
    state.store.regions().await.map_err(store_err)?.into_iter().find(|r| r.id == id).ok_or_else(not_found)
}

/// Work/done/failed carry no region in the URL, only the token — so the region is recovered by
/// scanning for the region whose token matches (small, fixed set; a per-token index would be
/// premature for this cardinality).
async fn region_by_token(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<Region, Response> {
    let tok = agent_header(headers)?;
    state
        .store
        .regions()
        .await
        .map_err(store_err)?
        .into_iter()
        .find(|r| rustic_git_core::peer::secret_eq(tok, &r.agent_token))
        .ok_or_else(unauthorized)
}

#[derive(serde::Deserialize)]
struct RegisterAgent {
    region: String,
    hostname: String,
    pool: String,
    capacity: Capacity,
}

#[derive(serde::Serialize)]
struct RegisterAgentResp {
    id: String,
}

async fn agent_register(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RegisterAgent>,
) -> Result<Response, Response> {
    let tok = agent_header(&headers)?;
    let region = region_by_id(&s, &body.region).await?;
    if !rustic_git_core::peer::secret_eq(tok, &region.agent_token) {
        return Err(unauthorized());
    }
    let a = AgentDoc {
        id: rid("agent"),
        region: body.region,
        hostname: body.hostname,
        pool: body.pool,
        capacity: body.capacity,
        used: Capacity { cpu: 0, mem_mb: 0, disk_gb: 0 },
        heartbeat_at: chrono::Utc::now(),
        status: "alive".into(),
    };
    s.store.upsert_agent(&a).await.map_err(store_err)?;
    Ok(Json(RegisterAgentResp { id: a.id }).into_response())
}

#[derive(serde::Deserialize)]
struct AgentWorkQuery {
    agent: String,
    // Current load self-report (closes the ponytail on `AgentDoc::used` — see model.rs):
    // absent/defaulted to 0 so older agent builds and the existing tests, which don't send
    // these, keep working unchanged.
    #[serde(default)]
    used_cpu: u32,
    #[serde(default)]
    used_mem_mb: u64,
    #[serde(default)]
    used_disk_gb: u64,
}

/// Long-poll ≤`agent_poll_window`, checking every `agent_poll_interval`: bump the agent's
/// heartbeat each iteration (that's the "doubles as heartbeat" in the spec), look for a queued
/// job addressed to this agent or unclaimed, and CAS-lease the first one found. A CAS loss just
/// means another poller (or this one, next tick) got it first — retry within the same window
/// rather than erroring, since the job is likely still gettable.
async fn agent_work(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<AgentWorkQuery>,
) -> Result<Response, Response> {
    let region = region_by_token(&s, &headers).await?;
    let deadline = tokio::time::Instant::now() + s.agent_poll_window;
    loop {
        let agents = s.store.agents_in(&region.id).await.map_err(store_err)?;
        let mut me = agents.into_iter().find(|a| a.id == q.agent).ok_or_else(not_found)?;
        me.heartbeat_at = chrono::Utc::now();
        me.status = "alive".into();
        me.used = Capacity { cpu: q.used_cpu, mem_mb: q.used_mem_mb, disk_gb: q.used_disk_gb };
        s.store.upsert_agent(&me).await.map_err(store_err)?;

        let queued = s.store.queued_jobs(&region.id).await.map_err(store_err)?;
        let mine = queued.into_iter().find(|(j, _)| j.agent.as_deref().is_none_or(|a| a == q.agent));
        if let Some((mut job, etag)) = mine {
            job.agent = Some(q.agent.clone());
            job.state = JobState::Leased;
            job.lease_until = Some(chrono::Utc::now() + chrono::Duration::seconds(120));
            match s.store.replace_job(&job, &etag).await {
                Ok(()) => return Ok(Json(job).into_response()),
                // Someone else leased it first — loop around and look again.
                Err(crate::store::StoreErr::CasFailed) => {}
                Err(e) => return Err(store_err(e)),
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        tokio::time::sleep(s.agent_poll_interval).await;
    }
}

#[derive(serde::Deserialize)]
struct JobDone {
    #[allow(dead_code)] // carried through for the caller's own record-keeping, not used here
    result: Option<serde_json::Value>,
}

/// Marks the workspace named by `job.payload["workspace"]`/`["owner"]` `Ready` — CAS retried
/// once, same shape as `Engine::set_ref`. Only `WsCreate`/`WsFork`/`WsClone` carry a workspace
/// that just became live; `WsDelete`'s payload does too, but that job already set `Deleted` at
/// request time, and `WsPush`'s workspace was already `Ready`. A missing workspace (already
/// deleted, or an env job) is a no-op, not an error — the job still succeeded.
async fn mark_ws_ready(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value) {
    if !matches!(kind, JobKind::WsCreate | JobKind::WsFork | JobKind::WsClone) {
        return;
    }
    let (Some(owner), Some(id)) = (payload["owner"].as_str(), payload["workspace"].as_str()) else {
        return;
    };
    for _ in 0..2 {
        let Ok(Some((mut w, etag))) = store.get_ws(owner, id).await else { return };
        w.state = WsState::Ready;
        use crate::store::StoreErr::*;
        match store.replace_ws(&w, &etag).await {
            Ok(()) | Err(NotFound) => return,
            Err(CasFailed) => continue,
            Err(_) => return,
        }
    }
}

/// Marks the workspace `Error` once its job's retry budget is exhausted — same target/no-op
/// rules as `mark_ws_ready`.
async fn mark_ws_error(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value) {
    if !matches!(kind, JobKind::WsCreate | JobKind::WsFork | JobKind::WsClone) {
        return;
    }
    let (Some(owner), Some(id)) = (payload["owner"].as_str(), payload["workspace"].as_str()) else {
        return;
    };
    for _ in 0..2 {
        let Ok(Some((mut w, etag))) = store.get_ws(owner, id).await else { return };
        w.state = WsState::Error;
        use crate::store::StoreErr::*;
        match store.replace_ws(&w, &etag).await {
            Ok(()) | Err(NotFound) => return,
            Err(CasFailed) => continue,
            Err(_) => return,
        }
    }
}

/// Moves the environment named by `job.payload["environment"]`/`["owner"]` to `target` on
/// `EnvUp`/`EnvDown` completion — same CAS-retry-once shape as `mark_ws_ready`. An `EnvDown`
/// that lands after `delete_env` already marked the doc `Deleted` must not resurrect it to
/// `Stopped` (delete wins).
async fn mark_env_state(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value) {
    let target = match kind {
        JobKind::EnvUp => EnvState::Running,
        JobKind::EnvDown => EnvState::Stopped,
        _ => return,
    };
    let (Some(owner), Some(id)) = (payload["owner"].as_str(), payload["environment"].as_str()) else {
        return;
    };
    for _ in 0..2 {
        let Ok(Some((mut e, etag))) = store.get_env(owner, id).await else { return };
        if kind == JobKind::EnvDown && e.state == EnvState::Deleted {
            return;
        }
        e.state = target;
        use crate::store::StoreErr::*;
        match store.replace_env(&e, &etag).await {
            Ok(()) | Err(NotFound) => return,
            Err(CasFailed) => continue,
            Err(_) => return,
        }
    }
}

/// Same no-resurrect-a-delete rule as `mark_env_state`, for the retry-exhausted path.
async fn mark_env_error(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value) {
    if !matches!(kind, JobKind::EnvUp | JobKind::EnvDown) {
        return;
    }
    let (Some(owner), Some(id)) = (payload["owner"].as_str(), payload["environment"].as_str()) else {
        return;
    };
    for _ in 0..2 {
        let Ok(Some((mut e, etag))) = store.get_env(owner, id).await else { return };
        if e.state == EnvState::Deleted {
            return;
        }
        e.state = EnvState::Error;
        use crate::store::StoreErr::*;
        match store.replace_env(&e, &etag).await {
            Ok(()) | Err(NotFound) => return,
            Err(CasFailed) => continue,
            Err(_) => return,
        }
    }
}

async fn agent_job_done(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(_body): Json<JobDone>,
) -> Result<Response, Response> {
    let region = region_by_token(&s, &headers).await?;
    let (mut job, etag) = s.store.get_job(&region.id, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    job.state = JobState::Done;
    job.lease_until = None;
    s.store.replace_job(&job, &etag).await.map_err(store_err)?;
    mark_ws_ready(&*s.store, job.kind, &job.payload).await;
    mark_env_state(&*s.store, job.kind, &job.payload).await;
    Ok(Json(job).into_response())
}

#[derive(serde::Deserialize)]
struct JobFailed {
    error: String,
}

async fn agent_job_failed(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<JobFailed>,
) -> Result<Response, Response> {
    let region = region_by_token(&s, &headers).await?;
    let (mut job, etag) = s.store.get_job(&region.id, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    job.attempts += 1;
    job.error = Some(body.error);
    job.lease_until = None;
    let exhausted = job.attempts > 3;
    if exhausted {
        job.state = JobState::Failed;
    } else {
        job.state = JobState::Queued;
        job.agent = None;
    }
    s.store.replace_job(&job, &etag).await.map_err(store_err)?;
    if exhausted {
        mark_ws_error(&*s.store, job.kind, &job.payload).await;
        mark_env_error(&*s.store, job.kind, &job.payload).await;
    }
    Ok(Json(job).into_response())
}

async fn delete_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let (mut e, etag) = s.store.get_env(&owner, &id).await.map_err(store_err)?.ok_or_else(not_found)?;
    e.state = EnvState::Deleted;
    s.store.replace_env(&e, &etag).await.map_err(store_err)?;
    env_job(&*s.store, &e.owner, &e.region, JobKind::EnvDown, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}
