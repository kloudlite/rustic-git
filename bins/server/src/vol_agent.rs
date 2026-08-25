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
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rustic_git_storage::store::valid_owner;
use rustic_git_workspaces::api::WS_AGENT_HEADER;
use rustic_git_workspaces::registry::{CommitRecord, VolExt};
use rustic_git_workspaces::store::MetaStore;
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
    /// Regions, for `authorized` only. The job queue this struct was named for is gone, but the
    /// record routes still authenticate agents against every region's minted `agent_token`, and
    /// Cosmos is where regions live — so this is the region lookup, not a work queue.
    /// ponytail: the name outlived the queue; rename to `AgentAuth` when something else touches
    /// this file.
    pub store: Option<Arc<dyn MetaStore>>,
}

impl JobsState {
    pub fn new(store: Option<Arc<dyn MetaStore>>) -> Self {
        JobsState { store }
    }
}









fn break_glass_matches(tok: &str) -> bool {
    let configured = std::env::var("RUSTIC_GIT_VOL_AGENT_TOKENS").unwrap_or_default();
    configured.split(',').map(str::trim).any(|t| rustic_git_core::peer::secret_eq(tok, t))
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
