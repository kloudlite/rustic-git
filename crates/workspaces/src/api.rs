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
        .with_state(state)
}

fn rid(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}-{}", rustic_git_core::hex(&b))
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
    let r = Region {
        id: body.id,
        name: body.name,
        storage_account: body.storage_account,
        blob_container: body.blob_container,
        status: "active".into(),
    };
    s.store.put_region(&r).await.map_err(store_err)?;
    Ok((StatusCode::CREATED, Json(r)).into_response())
}

async fn list_regions(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    caller(&s, &headers)?;
    let regions = s.store.regions().await.map_err(store_err)?;
    Ok(Json(regions).into_response())
}

// ── workspaces ───────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewWorkspace {
    name: String,
    region: String,
    quota_gb: u64,
}

async fn ws_job(store: &dyn MetaStore, region: &str, kind: JobKind, payload: serde_json::Value) -> Result<(), Response> {
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
    store.create_job(&j).await.map_err(store_err)
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
    ws_job(&*s.store, &w.region, JobKind::WsCreate, serde_json::json!({"workspace": w.id})).await?;
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
    ws_job(&*s.store, &w.region, JobKind::WsDelete, serde_json::json!({"workspace": w.id})).await?;
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

async fn env_job(store: &dyn MetaStore, region: &str, kind: JobKind, id: &str) -> Result<(), Response> {
    let j = Job {
        id: rid("job"),
        region: region.to_string(),
        agent: None,
        kind,
        payload: serde_json::json!({"environment": id}),
        state: JobState::Queued,
        lease_until: None,
        attempts: 0,
        error: None,
    };
    store.create_job(&j).await.map_err(store_err)
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
    env_job(&*s.store, &e.region, JobKind::EnvUp, &e.id).await?;
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
    env_job(&*s.store, &e.region, JobKind::EnvUp, &e.id).await?;
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
    env_job(&*s.store, &e.region, JobKind::EnvDown, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
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
    env_job(&*s.store, &e.region, JobKind::EnvDown, &e.id).await?;
    Ok((StatusCode::ACCEPTED, Json(e)).into_response())
}
