//! The agent-facing volume registry surface: `/vol-agent/{owner}/{name}/{commits|ref|history}`.
//!
//! Public listener, gated by a per-region agent token — the same Bearer-style pattern
//! `crates/registry` already uses for the OCI registry — rather than the per-user bearer tokens
//! `git`/browse routes check. `RUSTIC_GIT_VOL_AGENT_TOKENS` (comma-separated) is a shared-secret
//! break-glass stand-in.
//!
//! A token authorizes writes to volumes of ITS OWN region only (`authorized_for`). It used to
//! authorize writes to any volume in the fleet, which meant one leaked agent token could rewrite
//! another region's commit history and move its `main` refs. The volume's owning region is stamped
//! into its own database by the first record ever written to it.
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
/// The region whose agent token this request presents, if any.
async fn presented_region(jobs: &JobsState, headers: &axum::http::HeaderMap) -> Option<String> {
    let presented = rustic_git_core::httpx::bearer_token(headers)
        .or_else(|| headers.get(WS_AGENT_HEADER).and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    let store = jobs.store.as_ref()?;
    let regions = store.regions().await.ok()?;
    regions
        .iter()
        .find(|r| !r.agent_token.is_empty() && rustic_git_core::peer::secret_eq(presented, &r.agent_token))
        .map(|r| r.id.clone())
}

fn presents_break_glass(headers: &axum::http::HeaderMap) -> bool {
    let presented = rustic_git_core::httpx::bearer_token(headers)
        .or_else(|| headers.get(WS_AGENT_HEADER).and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    break_glass_matches(presented)
}

/// Whether this request may touch THIS volume's records.
///
/// Scoped to the volume's own region, not merely to "some registered region". Before this, any
/// region's agent token authorized writes to every volume in the fleet, so one leaked token could
/// rewrite another region's commit history and move its `main` refs — a data-integrity blast
/// radius, not just a confidentiality one.
///
/// A volume with no region stamped yet is claimed by its first writer (`append_commits` records
/// it). That is trust-on-first-use, and it is the honest limit of this check: it prevents a leaked
/// token from touching volumes that already belong to another region, which is what the audit
/// found, but it cannot stop one from claiming a volume nothing has written to.
/// ponytail: trust-on-first-use for an unstamped volume; the stronger form is the /v1 admission
/// path stamping the region at create time, before any agent writes.
async fn authorized_for(app: &App, jobs: &JobsState, headers: &axum::http::HeaderMap, owner: &str, name: &str) -> bool {
    // Break-glass stays deliberately fleet-wide: it exists for the case where the region records
    // themselves are unreachable or wrong, which is exactly when scoping would lock you out.
    if presents_break_glass(headers) {
        return true;
    }
    let Some(region) = presented_region(jobs, headers).await else {
        return false;
    };
    // A token is a string; a string leaks. Binding each region's token to the addresses its nodes
    // actually send from means a copy of it is useless from anywhere else — the same posture as
    // the operator's NSG rules, applied to the one credential that can rewrite volume history.
    // The client address is what the ingress resolved (`X-Real-IP` from `CF-Connecting-IP`, trusted
    // only from Cloudflare's ranges — see deploy/ingress-nginx-config.yaml); with no binding
    // configured for the region, the token alone still suffices, so an unlisted region is not
    // locked out by this.
    if !source_allowed(&region, client_ip(headers), &std::env::var("RUSTIC_GIT_AGENT_SOURCES").unwrap_or_default()) {
        tracing::warn!(%region, "agent token presented from an address outside the region's sources");
        return false;
    }
    match app.store.region(owner, name).await {
        Ok(Some(owning)) => owning == region,
        // Never written to: the first writer claims it.
        Ok(None) => true,
        // A database we cannot read is not an authorization decision we can make.
        Err(_) => false,
    }
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
    if vouched.is_none() && !authorized_for(&app, &jobs, &headers, &owner, &name).await {
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
    if vouched.is_none() && !authorized_for(&app, &jobs, &headers, &owner, &name).await {
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
    if vouched.is_none() && !authorized_for(&app, &jobs, &headers, &owner, &name).await {
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









/// The address the ingress attributed the request to. `X-Real-IP` is set by ingress-nginx from
/// the real client address, never copied from the client, so it cannot be forged from outside.
fn client_ip(headers: &axum::http::HeaderMap) -> Option<std::net::Ipv4Addr> {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()))
        .and_then(|v| v.trim().parse().ok())
}

/// `RUSTIC_GIT_AGENT_SOURCES` is `region=cidr[,cidr];region2=...`. A region with no entry is
/// unbound; a region with an entry must present from one of its CIDRs. IPv4 only: the nodes are
/// Azure VMs with v4 public addresses, and a v6 client with a bound region's token is refused
/// (`None` address never matches a bound region), which is the safe direction.
fn source_allowed(region: &str, ip: Option<std::net::Ipv4Addr>, bindings: &str) -> bool {
    let Some(cidrs) = bindings
        .split(';')
        .filter_map(|e| e.split_once('='))
        .find(|(r, _)| r.trim() == region)
        .map(|(_, c)| c)
    else {
        return true;
    };
    let Some(ip) = ip else { return false };
    cidrs.split(',').filter_map(|c| parse_cidr(c.trim())).any(|(net, bits)| {
        let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
        (u32::from(ip) & mask) == (u32::from(net) & mask)
    })
}

fn parse_cidr(c: &str) -> Option<(std::net::Ipv4Addr, u32)> {
    let (addr, bits) = c.split_once('/').unwrap_or((c, "32"));
    Some((addr.parse().ok()?, bits.parse::<u32>().ok().filter(|b| *b <= 32)?))
}

fn break_glass_matches(tok: &str) -> bool {
    let configured = std::env::var("RUSTIC_GIT_VOL_AGENT_TOKENS").unwrap_or_default();
    configured.split(',').map(str::trim).any(|t| rustic_git_core::peer::secret_eq(tok, t))
}



















#[cfg(test)]
mod tests {
    #[test]
    fn a_bound_region_accepts_only_its_own_addresses() {
        use super::source_allowed;
        let ip = |s: &str| Some(s.parse().unwrap());
        let b = "centralindia-k3s=40.80.82.158/32,20.219.22.61/32;other=10.0.0.0/8";
        assert!(source_allowed("centralindia-k3s", ip("40.80.82.158"), b));
        assert!(source_allowed("centralindia-k3s", ip("20.219.22.61"), b));
        assert!(!source_allowed("centralindia-k3s", ip("20.219.22.62"), b));
        assert!(!source_allowed("centralindia-k3s", None, b), "no address never matches a bound region");
        assert!(source_allowed("other", ip("10.42.1.9"), b));
        assert!(source_allowed("unbound", ip("1.2.3.4"), b), "an unlisted region is not locked out");
        assert!(source_allowed("centralindia-k3s", ip("9.9.9.9"), ""), "no config at all binds nothing");
        assert!(!source_allowed("centralindia-k3s", ip("1.2.3.4"), "centralindia-k3s=not-a-cidr"), "garbage binds to nothing");
    }

    #[test]
    fn the_client_address_is_the_ingress_s_word_not_the_client_s() {
        use super::client_ip;
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forwarded-for", "8.8.8.8, 1.1.1.1".parse().unwrap());
        assert_eq!(client_ip(&h), Some("8.8.8.8".parse().unwrap()));
        h.insert("x-real-ip", "40.80.82.158".parse().unwrap());
        assert_eq!(client_ip(&h), Some("40.80.82.158".parse().unwrap()), "X-Real-IP wins");
    }

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

    /// The break-glass half of the check, which is the half that stays fleet-wide. Region scoping
    /// is exercised over HTTP in `tests/vol_agent.rs`, where a volume can actually be written and
    /// so can actually have an owning region.
    #[test]
    fn break_glass_rejects_empty_and_mismatched() {
        let mut h = axum::http::HeaderMap::new();

        // No env configured at all: empty presented token, refused.
        std::env::remove_var("RUSTIC_GIT_VOL_AGENT_TOKENS");
        assert!(!presents_break_glass(&h));

        // Configured list, still no header presented: refused. An empty presented token must never
        // match, however the list is configured.
        std::env::set_var("RUSTIC_GIT_VOL_AGENT_TOKENS", "t1,t2");
        assert!(!presents_break_glass(&h));

        // Mismatched Bearer token: refused.
        h.insert(axum::http::header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!presents_break_glass(&h));

        // Matching break-glass token via Bearer: accepted.
        h.insert(axum::http::header::AUTHORIZATION, "Bearer t2".parse().unwrap());
        assert!(presents_break_glass(&h));

        // Matching break-glass token via the WS agent header instead of Bearer: accepted.
        h.remove(axum::http::header::AUTHORIZATION);
        h.insert(WS_AGENT_HEADER, "t1".parse().unwrap());
        assert!(presents_break_glass(&h));

        std::env::remove_var("RUSTIC_GIT_VOL_AGENT_TOKENS");
    }
}
