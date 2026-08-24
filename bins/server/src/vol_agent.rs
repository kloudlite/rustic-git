//! The agent-facing volume registry surface: `/vol-agent/{owner}/{name}/{commits|ref|history}`.
//!
//! Public listener, gated by a per-region agent token — the same Bearer-style pattern
//! `crates/registry` already uses for the OCI registry — rather than the per-user bearer tokens
//! `git`/browse routes check. `RUSTIC_GIT_VOL_AGENT_TOKENS` (comma-separated) is a shared-secret
//! stand-in: Task 14's Cosmos client replaces this with a per-region token lookup, at which point
//! an agent for region X can no longer write another region's volumes with a token it happened to
//! know. Until then every configured token authorizes every volume.
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
use rustic_git_workspaces::registry::{CommitRecord, VolExt};
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

/// Constant-time check against every token in `RUSTIC_GIT_VOL_AGENT_TOKENS` (comma-separated). An
/// empty presented token never matches (`secret_eq` refuses it), and no candidate is ever logged
/// or formatted into a response — a rejected caller learns only that it was rejected.
fn authorized(headers: &axum::http::HeaderMap) -> bool {
    let presented = rustic_git_core::httpx::bearer_token(headers).unwrap_or("");
    let configured = std::env::var("RUSTIC_GIT_VOL_AGENT_TOKENS").unwrap_or_default();
    configured.split(',').map(str::trim).any(|t| rustic_git_core::peer::secret_eq(presented, t))
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "invalid or missing agent token").into_response()
}

pub(crate) async fn commits(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(records): Json<Vec<CommitRecord>>,
) -> Response {
    if !authorized(&headers) {
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
    Path((owner, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<MoveRef>,
) -> Response {
    if !authorized(&headers) {
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
    Path((owner, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !authorized(&headers) {
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

    #[test]
    fn token_check_rejects_empty_and_mismatched() {
        let mut h = axum::http::HeaderMap::new();
        std::env::set_var("RUSTIC_GIT_VOL_AGENT_TOKENS", "");
        assert!(!authorized(&h));
        std::env::set_var("RUSTIC_GIT_VOL_AGENT_TOKENS", "t1,t2");
        assert!(!authorized(&h));
        h.insert(axum::http::header::AUTHORIZATION, "Bearer t2".parse().unwrap());
        assert!(authorized(&h));
        h.insert(axum::http::header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!authorized(&h));
        std::env::remove_var("RUSTIC_GIT_VOL_AGENT_TOKENS");
    }
}
