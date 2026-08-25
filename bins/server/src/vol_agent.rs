//! The agent-facing volume registry surface: `/vol-agent/{owner}/{name}/{commits|ref|history}`.
//!
//! Public listener, gated by a per-region agent token — the same Bearer-style pattern
//! `crates/registry` already uses for the OCI registry — rather than the per-user bearer tokens
//! `git`/browse routes check. `RUSTIC_GIT_VOL_AGENT_TOKENS` (comma-separated) is a shared-secret
//! stand-in. v1 contract: any registered region's agent token (or a break-glass token from this
//! env var) authorizes writes to ANY volume's records, not just that region's own — a trusted-
//! operator-fleet model, not per-region isolation. `authorized` deliberately checks the presented
//! token against every registered region, unscoped by the volume's own region.
//! // ponytail: no region scoping yet — a leaked agent token from region X can write region Y's
//! // volume records too. Upgrade path: look up the volume's owning region (workspace/env doc)
//! // and require the presented token to match that region specifically, the way `region_by_id`
//! // already scopes register's token check to one named region.
//!
//! Per-volume, so it is routed exactly like a repo or an image path — `repo_of` in
//! `router/route.rs` sends it through the ownership middleware before this handler ever runs,
//! because only the node holding `repo/vol/{owner}/{name}` may open that database.

use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use rustic_git_storage::store::valid_owner;
use rustic_git_workspaces::api::WS_AGENT_HEADER;
use rustic_git_workspaces::model::*;
use rustic_git_workspaces::registry::{CommitRecord, VolExt};
use rustic_git_workspaces::store::{MetaStore, StoreErr};
use std::sync::Arc;

/// The final path segment of a `/vol-agent/{owner}/{name}/{tail}` route — the volume-registry
/// analogue of `registry::IMAGE_TAILS` and `route::BROWSE_TAILS`. A route missing from this list
/// is unreachable: `vol_agent_route` refuses it, and `route_inner`'s vol-agent block never falls
/// through to a handler that was never routed.
pub(crate) const VOL_AGENT_TAILS: [&str; 3] = ["commits", "ref", "history"];

/// Whether `path` starts with the `/vol-agent/` prefix, regardless of whether the rest parses.
pub(crate) fn vol_agent_prefixed(path: &str) -> bool {
    let p = path.trim_start_matches('/');
    p == "vol-agent" || p.starts_with("vol-agent/")
}

/// `Some((owner, name))` when the path names a volume's agent route. Strict like
/// `registry::image_route`: exactly `/vol-agent/{owner}/{name}/{tail}`, `tail` one of
/// `VOL_AGENT_TAILS`, `owner`/`name` valid segments (and `owner` not itself reserved).
pub(crate) fn vol_agent_route(path: &str) -> Option<(&str, &str)> {
    let mut it = path.trim_start_matches('/').strip_prefix("vol-agent/")?.split('/');
    let (owner, name, tail) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || !VOL_AGENT_TAILS.contains(&tail) {
        return None;
    }
    (valid_owner(owner) && rustic_git_storage::store::valid_segment(name)).then_some((owner, name))
}

/// Whether `path` is one of the agent-work-surface routes (`register`, `work`,
/// `jobs/{id}/{done|failed}`) — the shape `route_inner` serves locally regardless of ownership,
/// since these touch Cosmos, not a per-repo database. Distinct from `vol_agent_route`, which is
/// the per-volume `commits`/`ref`/`history` shape.
pub(crate) fn vol_agent_job_shape(path: &str) -> bool {
    let Some(rest) = path.trim_start_matches('/').strip_prefix("vol-agent/") else {
        return false;
    };
    if rest == "register" || rest == "work" {
        return true;
    }
    let Some(jobs_rest) = rest.strip_prefix("jobs/") else {
        return false;
    };
    let mut it = jobs_rest.split('/');
    matches!(
        (it.next(), it.next(), it.next()),
        (Some(_id), Some("done" | "failed"), None)
    )
}

/// Record-route auth accepts the same identities the job routes do: any region doc's minted
/// agent_token (the normal path — agents present the token their region registration handed
/// out) or the `RUSTIC_GIT_VOL_AGENT_TOKENS` break-glass list. The presented token may arrive
/// as a Bearer (the registry clients) or the WS agent header (the agent's job calls) — both
/// name the same secret. Constant-time compares throughout; empty never matches; nothing is
/// ever logged or echoed.
async fn authorized(jobs: &JobsState, headers: &axum::http::HeaderMap) -> bool {
    let presented = rustic_git_core::httpx::bearer_token(headers)
        .or_else(|| headers.get(WS_AGENT_HEADER).and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    if break_glass_matches(presented) {
        return true;
    }
    if let Some(store) = jobs.store.as_ref() {
        if let Ok(regions) = store.regions().await {
            return regions
                .iter()
                .any(|r| !r.agent_token.is_empty() && rustic_git_core::peer::secret_eq(presented, &r.agent_token));
        }
    }
    false
}

/// Marker the PEER router layers in: `trust_peer` has already validated the shared peer
/// secret on that listener, which vouches strictly harder than any agent token — a forwarded
/// request re-presenting its region token cannot be re-validated there without Cosmos, and
/// does not need to be.
#[derive(Clone, Copy)]
pub struct PeerVouched;

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "invalid or missing agent token").into_response()
}

pub(crate) async fn commits(
    State(app): State<Arc<App>>,
    axum::Extension(jobs): axum::Extension<Arc<JobsState>>,
    vouched: Option<axum::Extension<PeerVouched>>,
    Path((owner, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(records): Json<Vec<CommitRecord>>,
) -> Response {
    if vouched.is_none() && !authorized(&jobs, &headers).await {
        return unauthorized();
    }
    match app.store.append_commits(&owner, &name, &records).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"appended": records.len()}))).into_response(),
        Err(e) => crate::router::internal(e),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct MoveRef {
    name: String,
    commit: String,
}

pub(crate) async fn move_ref(
    State(app): State<Arc<App>>,
    axum::Extension(jobs): axum::Extension<Arc<JobsState>>,
    vouched: Option<axum::Extension<PeerVouched>>,
    Path((owner, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<MoveRef>,
) -> Response {
    if vouched.is_none() && !authorized(&jobs, &headers).await {
        return unauthorized();
    }
    match app.store.move_ref(&owner, &name, &body.name, &body.commit).await {
        // Ref moved to unknown commit: 404, not 409 — there is no conflicting write to lose to,
        // just a commit id that was never appended (a push that named the wrong id, or arrived out
        // of order). A caller that gets an unrelated conflict has nothing useful to retry.
        Ok(false) => (StatusCode::NOT_FOUND, "unknown commit").into_response(),
        Ok(true) => StatusCode::OK.into_response(),
        Err(e) => crate::router::internal(e),
    }
}

pub(crate) async fn history(
    State(app): State<Arc<App>>,
    axum::Extension(jobs): axum::Extension<Arc<JobsState>>,
    vouched: Option<axum::Extension<PeerVouched>>,
    Path((owner, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if vouched.is_none() && !authorized(&jobs, &headers).await {
        return unauthorized();
    }
    match app.store.history(&owner, &name).await {
        Ok(records) => Json(records).into_response(),
        Err(e) => crate::router::internal(e),
    }
}

/// Mounted on the PUBLIC router only — agents have no reason to reach the peer listener, and the
/// peer listener's `trust_peer` layer would reject them anyway (they carry an agent token, not
/// the peer secret).
pub fn vol_agent_routes() -> axum::Router<Arc<App>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/vol-agent/{owner}/{name}/commits", post(commits))
        .route("/vol-agent/{owner}/{name}/ref", post(move_ref))
        .route("/vol-agent/{owner}/{name}/history", get(history))
}

// ── agent work surface: register / work / jobs/{id}/done / jobs/{id}/failed ────────────────────
//
// Moved here verbatim from `crates/workspaces/src/api.rs`'s old `/v1/agent/*` routes (Task 7/8):
// this process runs on every server node already, so an agent fleet reaches it the same way it
// reaches the volume-commit routes above, instead of a separate `bins/api` process that exists
// for a completely different reason (browse reads) and has no natural relationship to the
// workspaces feature. `bins/api` keeps the USER-facing `/v1/workspaces|environments|regions`
// routes — those still need a JWT-verifying, admin-gated process, which this one is not.
//
// Not routed through the per-repo ownership middleware (`route::vol_agent_job_shape` carves an
// exception): the metadata these handlers touch lives in Cosmos, shared by every node, not in a
// per-repo SlateDB — so any node can answer, exactly like `/v2/token` and `/v2/_catalog`.

/// Server-tier state for the agent work surface. `store` is `None` when no `COSMOS_ENDPOINT` is
/// configured — the routes are always mounted (so a request gets a clear 503, not a 404 that
/// reads as "this feature doesn't exist"), but every handler refuses immediately.
pub struct JobsState {
    pub store: Option<Arc<dyn MetaStore>>,
    /// Long-poll hold and inner retry interval for `GET /vol-agent/work`. Real 30s/1s; tests
    /// shrink these so the leasing/204-timeout tests don't take real minutes.
    pub poll_window: std::time::Duration,
    pub poll_interval: std::time::Duration,
}

impl JobsState {
    pub fn new(store: Option<Arc<dyn MetaStore>>) -> Self {
        JobsState {
            store,
            poll_window: std::time::Duration::from_secs(30),
            poll_interval: std::time::Duration::from_secs(1),
        }
    }
}

fn rid(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}-{}", rustic_git_core::hex(&b))
}

fn job_unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or invalid agent token").into_response()
}

fn job_not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn job_store_err(e: StoreErr) -> Response {
    match e {
        StoreErr::NotFound => job_not_found(),
        StoreErr::Conflict | StoreErr::CasFailed => (StatusCode::CONFLICT, "conflict, retry").into_response(),
        StoreErr::Other(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
    }
}

/// `None` when the workspaces feature is not configured on this node — every handler answers 503
/// rather than 404, so an operator sees "not configured" instead of "route doesn't exist".
fn require_store(s: &JobsState) -> Result<&Arc<dyn MetaStore>, Response> {
    s.store.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "workspaces metadata store not configured on this node").into_response()
    })
}

fn agent_header(headers: &HeaderMap) -> Result<&str, Response> {
    headers.get(WS_AGENT_HEADER).and_then(|v| v.to_str().ok()).ok_or_else(job_unauthorized)
}

/// Register: the region is named explicitly in the payload, so the token is checked against
/// that one region's doc directly. Falls back to `RUSTIC_GIT_VOL_AGENT_TOKENS` (see
/// `region_by_token`'s doc) when no Cosmos region doc matches.
async fn region_by_id(store: &dyn MetaStore, tok: &str, id: &str) -> Result<Region, Response> {
    if let Some(r) = store.regions().await.map_err(job_store_err)?.into_iter().find(|r| r.id == id) {
        // agent_token is serde(default): a legacy region doc can deserialize with an empty
        // token, and an empty presented header must never match that.
        if !r.agent_token.is_empty() && rustic_git_core::peer::secret_eq(tok, &r.agent_token) {
            return Ok(r);
        }
    }
    if break_glass_matches(tok) {
        return Ok(synthetic_region(id));
    }
    Err(job_unauthorized())
}

/// Work/done/failed carry no region in the URL, only the token — so the region is recovered by
/// scanning for the region whose token matches (small, fixed set; a per-token index would be
/// premature for this cardinality).
///
/// `RUSTIC_GIT_VOL_AGENT_TOKENS` (comma-separated) is a break-glass override for standing an
/// agent up when Cosmos is unreachable or a region's `agent_token` was never provisioned — same
/// shared-secret shape Task 13 used for every volume before a per-region Cosmos doc existed. It
/// only ever authorizes the region the CALLER names in `region_hint` (register's body, or
/// work/done/failed's `?region=` query param): unlike the pre-Cosmos placeholder, a leaked
/// override token cannot reach every region's queue, only the one it was told to use.
async fn region_by_token(
    store: &dyn MetaStore,
    headers: &HeaderMap,
    region_hint: Option<&str>,
) -> Result<Region, Response> {
    let tok = agent_header(headers)?;
    if let Some(r) = store
        .regions()
        .await
        .map_err(job_store_err)?
        .into_iter()
        .find(|r| !r.agent_token.is_empty() && rustic_git_core::peer::secret_eq(tok, &r.agent_token))
    {
        return Ok(r);
    }
    if let Some(id) = region_hint {
        if break_glass_matches(tok) {
            return Ok(synthetic_region(id));
        }
    }
    Err(job_unauthorized())
}

fn break_glass_matches(tok: &str) -> bool {
    let configured = std::env::var("RUSTIC_GIT_VOL_AGENT_TOKENS").unwrap_or_default();
    configured.split(',').map(str::trim).any(|t| rustic_git_core::peer::secret_eq(tok, t))
}

/// Only `id` matters downstream (job/agent queries are scoped by region id); the other fields are
/// never read off this break-glass path.
fn synthetic_region(id: &str) -> Region {
    Region {
        id: id.to_string(),
        name: id.to_string(),
        storage_account: String::new(),
        blob_container: String::new(),
        status: "active".into(),
        agent_token: String::new(),
    }
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

async fn register(
    Extension(s): Extension<Arc<JobsState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterAgent>,
) -> Result<Response, Response> {
    let store = require_store(&s)?;
    let tok = agent_header(&headers)?;
    region_by_id(&**store, tok, &body.region).await?;
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
    store.upsert_agent(&a).await.map_err(job_store_err)?;
    Ok(Json(RegisterAgentResp { id: a.id }).into_response())
}

#[derive(serde::Deserialize)]
struct AgentWorkQuery {
    agent: String,
    // Current load self-report (closes the ponytail on `AgentDoc::used` — see workspaces'
    // model.rs): absent/defaulted to 0 so older agent builds keep working unchanged.
    #[serde(default)]
    used_cpu: u32,
    #[serde(default)]
    used_mem_mb: u64,
    #[serde(default)]
    used_disk_gb: u64,
    /// Only consulted on the `RUSTIC_GIT_VOL_AGENT_TOKENS` break-glass path — see
    /// `region_by_token`.
    #[serde(default)]
    region: Option<String>,
}

/// Long-poll ≤`poll_window`, checking every `poll_interval`: bump the agent's heartbeat each
/// iteration (that's the "doubles as heartbeat" in the spec), look for a queued job addressed to
/// this agent or unclaimed, and CAS-lease the first one found. A CAS loss just means another
/// poller (or this one, next tick) got it first — retry within the same window rather than
/// erroring, since the job is likely still gettable.
async fn work(
    Extension(s): Extension<Arc<JobsState>>,
    headers: HeaderMap,
    Query(q): Query<AgentWorkQuery>,
) -> Result<Response, Response> {
    let store = require_store(&s)?;
    let region = region_by_token(&**store, &headers, q.region.as_deref()).await?;
    let deadline = tokio::time::Instant::now() + s.poll_window;
    loop {
        let agents = store.agents_in(&region.id).await.map_err(job_store_err)?;
        // `me` is the stored doc, not one the agent presents on the wire — the poll query only
        // carries heartbeat/used.
        let mut me = agents.into_iter().find(|a| a.id == q.agent).ok_or_else(job_not_found)?;
        me.heartbeat_at = chrono::Utc::now();
        me.status = "alive".into();
        me.used = Capacity { cpu: q.used_cpu, mem_mb: q.used_mem_mb, disk_gb: q.used_disk_gb };
        store.upsert_agent(&me).await.map_err(job_store_err)?;

        let queued = store.queued_jobs(&region.id).await.map_err(job_store_err)?;
        // `agent: None` means "not placed", NOT "free for anyone": the scheduler clears it when
        // the owner's bound agent is dead and the sweep clears it on every expiry, so handing it
        // to whoever polls first runs the job on a node that does not hold the subvolumes. An
        // unplaced job waits for `lease::sweep`'s re-`schedule` pass to bind it (≤30s).
        let mine = queued.into_iter().find(|(j, _)| j.agent.as_deref() == Some(q.agent.as_str()));
        if let Some((mut job, etag)) = mine {
            job.agent = Some(q.agent.clone());
            job.state = JobState::Leased;
            job.lease_until = Some(chrono::Utc::now() + chrono::Duration::seconds(120));
            match store.replace_job(&job, &etag).await {
                Ok(()) => return Ok(Json(job).into_response()),
                // Someone else leased it first — loop around and look again.
                Err(StoreErr::CasFailed) => {}
                Err(e) => return Err(job_store_err(e)),
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        tokio::time::sleep(s.poll_interval).await;
    }
}

#[derive(serde::Deserialize)]
struct JobDone {
    #[allow(dead_code)] // carried through for the caller's own record-keeping, not used here
    result: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct RegionHint {
    #[serde(default)]
    region: Option<String>,
}

/// Marks the workspace named by `job.payload["workspace"]`/`["owner"]` `Ready` — CAS retried
/// once, same shape as `Engine::set_ref`. Only `WsCreate`/`WsClone`/`WsRestore` carry a workspace
/// that just became live; `WsDelete`'s payload does too, but that job already set `Deleted` at
/// request time, and `WsPush`'s workspace was already `Ready`. A missing workspace (already
/// deleted, or an env job) is a no-op, not an error — the job still succeeded.
async fn mark_ws_ready(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value) {
    // `Ready` means "running now" (the container lifecycle rides along with the volume
    // lifecycle) — `WsStart` lands here too, `WsStop` gets its own `mark_ws_stopped` below.
    if !matches!(kind, JobKind::WsCreate | JobKind::WsClone | JobKind::WsRestore | JobKind::WsStart) {
        return;
    }
    let (Some(owner), Some(id)) = (payload["owner"].as_str(), payload["workspace"].as_str()) else {
        return;
    };
    for _ in 0..2 {
        let Ok(Some((mut w, etag))) = store.get_ws(owner, id).await else { return };
        w.state = WsState::Ready;
        use StoreErr::*;
        match store.replace_ws(&w, &etag).await {
            Ok(()) | Err(NotFound) => return,
            Err(CasFailed) => continue,
            Err(_) => return,
        }
    }
}

/// `WsStop`'s done handler — the only job kind that lands a workspace in `Stopped`.
async fn mark_ws_stopped(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value) {
    if kind != JobKind::WsStop {
        return;
    }
    let (Some(owner), Some(id)) = (payload["owner"].as_str(), payload["workspace"].as_str()) else {
        return;
    };
    for _ in 0..2 {
        let Ok(Some((mut w, etag))) = store.get_ws(owner, id).await else { return };
        w.state = WsState::Stopped;
        use StoreErr::*;
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
    if !matches!(kind, JobKind::WsCreate | JobKind::WsClone | JobKind::WsRestore) {
        return;
    }
    let (Some(owner), Some(id)) = (payload["owner"].as_str(), payload["workspace"].as_str()) else {
        return;
    };
    for _ in 0..2 {
        let Ok(Some((mut w, etag))) = store.get_ws(owner, id).await else { return };
        w.state = WsState::Error;
        use StoreErr::*;
        match store.replace_ws(&w, &etag).await {
            Ok(()) | Err(NotFound) => return,
            Err(CasFailed) => continue,
            Err(_) => return,
        }
    }
}

/// Moves the environment named by `job.payload["environment"]`/`["owner"]` to `target` on
/// `EnvUp`/`EnvDown`/env-`WsClone` completion — same CAS-retry-once shape as `mark_ws_ready`. An
/// `EnvDown` that lands after `delete_env` already marked the doc `Deleted` must not resurrect it
/// to `Stopped` (delete wins). `WsClone` also drives workspace clones (`mark_ws_ready` above);
/// the `environment` payload key (absent on a workspace clone) is what tells the two apart.
async fn mark_env_state(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value) {
    let target = match kind {
        JobKind::EnvUp => EnvState::Running,
        JobKind::EnvDown => EnvState::Stopped,
        JobKind::WsClone => EnvState::Running,
        _ => return,
    };
    let (Some(owner), Some(id)) = (payload["owner"].as_str(), payload["environment"].as_str()) else {
        return;
    };
    for _ in 0..2 {
        let Ok(Some((mut e, etag))) = store.get_env(owner, id).await else { return };
        if matches!(kind, JobKind::EnvDown | JobKind::WsClone) && e.state == EnvState::Deleted {
            return;
        }
        e.state = target;
        use StoreErr::*;
        match store.replace_env(&e, &etag).await {
            Ok(()) | Err(NotFound) => return,
            Err(CasFailed) => continue,
            Err(_) => return,
        }
    }
}

/// Same no-resurrect-a-delete rule as `mark_env_state`, for the retry-exhausted path.
async fn mark_env_error(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value) {
    if !matches!(kind, JobKind::EnvUp | JobKind::EnvDown | JobKind::WsClone) {
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
        use StoreErr::*;
        match store.replace_env(&e, &etag).await {
            Ok(()) | Err(NotFound) => return,
            Err(CasFailed) => continue,
            Err(_) => return,
        }
    }
}

async fn job_done(
    Extension(s): Extension<Arc<JobsState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(hint): Query<RegionHint>,
    Json(_body): Json<JobDone>,
) -> Result<Response, Response> {
    let store = require_store(&s)?;
    let region = region_by_token(&**store, &headers, hint.region.as_deref()).await?;
    let (mut job, etag) = store.get_job(&region.id, &id).await.map_err(job_store_err)?.ok_or_else(job_not_found)?;
    job.state = JobState::Done;
    job.lease_until = None;
    store.replace_job(&job, &etag).await.map_err(job_store_err)?;
    mark_ws_ready(&**store, job.kind, &job.payload).await;
    mark_ws_stopped(&**store, job.kind, &job.payload).await;
    mark_env_state(&**store, job.kind, &job.payload).await;
    Ok(Json(job).into_response())
}

#[derive(serde::Deserialize)]
struct JobFailed {
    error: String,
}

async fn job_failed(
    Extension(s): Extension<Arc<JobsState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(hint): Query<RegionHint>,
    Json(body): Json<JobFailed>,
) -> Result<Response, Response> {
    let store = require_store(&s)?;
    let region = region_by_token(&**store, &headers, hint.region.as_deref()).await?;
    let (mut job, etag) = store.get_job(&region.id, &id).await.map_err(job_store_err)?.ok_or_else(job_not_found)?;
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
    store.replace_job(&job, &etag).await.map_err(job_store_err)?;
    if exhausted {
        mark_ws_error(&**store, job.kind, &job.payload).await;
        mark_env_error(&**store, job.kind, &job.payload).await;
    }
    Ok(Json(job).into_response())
}

/// Requeue sweep (spec §Scheduler), moved here from `bins/api`: 30s beat per known region, so a
/// leased job whose agent died or overran its lease gets back in the queue without a human
/// noticing. Only spawned when `store` is configured — see `boot::spawn_vol_agent_sweep`.
pub fn spawn_sweep(store: Arc<dyn MetaStore>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if let Ok(regions) = store.regions().await {
                for r in regions {
                    if let Err(e) = rustic_git_workspaces::lease::sweep(&*store, &r.id).await {
                        eprintln!("requeue sweep failed for region {}: {e:?}", r.id); // ponytail: eprintln
                    }
                }
            }
        }
    });
}

/// Same `Router<Arc<App>>` type as `vol_agent_routes()` so it merges straight into `router()`'s
/// chain and inherits its `route_public`/`trust_nobody` layers — the handlers reach `JobsState`
/// through `Extension` (wired in by `router()`'s `.layer(Extension(jobs))`), not `State`, because
/// `Router::merge` requires every merged piece to share one state TYPE, and this crate's `App` is
/// that type everywhere else. Always mounted, whether or not `state.store` is `Some`: see
/// `JobsState`'s doc for why a 503 beats a 404 here.
pub fn vol_agent_job_routes() -> axum::Router<Arc<App>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/vol-agent/register", post(register))
        .route("/vol-agent/work", get(work))
        .route("/vol-agent/jobs/{id}/done", post(job_done))
        .route("/vol-agent/jobs/{id}/failed", post(job_failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_shape_matches_the_tails_list() {
        assert_eq!(vol_agent_route("/vol-agent/alice/web/commits"), Some(("alice", "web")));
        assert_eq!(vol_agent_route("/vol-agent/alice/web/ref"), Some(("alice", "web")));
        assert_eq!(vol_agent_route("/vol-agent/alice/web/history"), Some(("alice", "web")));
        assert_eq!(vol_agent_route("/vol-agent/alice/web/frobnicate"), None);
        assert_eq!(vol_agent_route("/vol-agent/alice/web"), None);
        assert_eq!(vol_agent_route("/vol-agent/vol/web/commits"), None, "owner `vol` is reserved");
        assert!(vol_agent_prefixed("/vol-agent/alice/web/commits"));
        assert!(!vol_agent_prefixed("/vol-agentxyz"));
    }

    #[tokio::test]
    async fn token_check_rejects_empty_and_mismatched() {
        let jobs = JobsState::new(None);
        let mut h = axum::http::HeaderMap::new();

        // No env configured at all: empty presented token, refused.
        std::env::remove_var("RUSTIC_GIT_VOL_AGENT_TOKENS");
        assert!(!authorized(&jobs, &h).await);

        // Configured break-glass list, still no header presented: refused.
        std::env::set_var("RUSTIC_GIT_VOL_AGENT_TOKENS", "t1,t2");
        assert!(!authorized(&jobs, &h).await);

        // Mismatched Bearer token: refused.
        h.insert(axum::http::header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!authorized(&jobs, &h).await);

        // Matching break-glass token via Bearer: accepted.
        h.insert(axum::http::header::AUTHORIZATION, "Bearer t2".parse().unwrap());
        assert!(authorized(&jobs, &h).await);

        // Matching break-glass token via the WS agent header instead of Bearer: accepted.
        h.remove(axum::http::header::AUTHORIZATION);
        h.insert(WS_AGENT_HEADER, "t1".parse().unwrap());
        assert!(authorized(&jobs, &h).await);

        std::env::remove_var("RUSTIC_GIT_VOL_AGENT_TOKENS");
    }
}
