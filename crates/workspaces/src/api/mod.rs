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

mod bench;
mod me;
pub mod membership;
pub mod removals;
pub mod spaces;
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
use bench::{bench_session, bench_teams, create_bench, get_bench, mint_tool_token, revoke_tool_token, start_bench, stop_bench};
use me::{attach_gone, clear_my_environment, list_my_environments, set_my_environment};
use push::{push_env, push_ws};
use volumes::{delete_snapshot, delete_volume, list_volumes, volume_history, volume_refs};
use workspaces::{
    clone_ws, create_ws, delete_ws, get_ws, list_ws, patch_ws_packages, restore_ws,
    ssh_session, start_ws, stop_ws, update_ws_packages, ws_tools,
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

/// (method, axum path pattern). The pod's audience; everything else refuses a bench-tool caller.
/// Exactly what the bench's pi tools call — the test below holds it, with its complement, to the
/// router, so a new route is refused to a bench tool until somebody decides otherwise.
pub(crate) const BENCH_TOOL_ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/quota"),
    ("GET", "/v1/regions"),
    ("GET", "/v1/workspaces"),
    ("POST", "/v1/workspaces"),
    ("POST", "/v1/workspaces/restore"),
    ("GET", "/v1/workspaces/{id}"),
    ("PATCH", "/v1/workspaces/{id}"),
    ("DELETE", "/v1/workspaces/{id}"),
    ("GET", "/v1/workspaces/{id}/tools"),
    ("POST", "/v1/workspaces/{id}/packages/update"),
    ("POST", "/v1/workspaces/{id}/clone"),
    ("POST", "/v1/workspaces/{id}/push"),
    ("POST", "/v1/workspaces/{id}/start"),
    ("POST", "/v1/workspaces/{id}/stop"),
    ("POST", "/v1/workspaces/{id}/attach"),
    ("POST", "/v1/workspaces/{id}/detach"),
    ("GET", "/v1/me/environments"),
    ("PUT", "/v1/me/environments/{team}"),
    ("DELETE", "/v1/me/environments/{team}"),
    ("GET", "/v1/environments"),
    ("POST", "/v1/environments"),
    ("POST", "/v1/environments/restore"),
    ("GET", "/v1/environments/{id}"),
    ("DELETE", "/v1/environments/{id}"),
    ("POST", "/v1/environments/{id}/start"),
    ("POST", "/v1/environments/{id}/stop"),
    ("POST", "/v1/environments/{id}/clone"),
    ("POST", "/v1/environments/{id}/push"),
    ("POST", "/v1/environments/{id}/restore-in-place"),
    ("POST", "/v1/environments/{id}/intercepts"),
    ("DELETE", "/v1/environments/{id}/intercepts/{service}"),
    ("GET", "/v1/builders/me"),
    ("GET", "/v1/volumes"),
    ("GET", "/v1/volumes/{name}/history"),
    ("GET", "/v1/volumes/{name}/refs"),
    ("DELETE", "/v1/volumes/{name}"),
    ("DELETE", "/v1/volumes/{name}/snapshots/{snapshot}"),
];

/// What `kl` calls from inside a workspace pod, and nothing else. Much narrower than a bench's
/// audience on purpose: this credential is a FILE in every workspace of the owner, so its blast
/// radius is exactly this list — the package set and the space environment, never a lifecycle
/// verb and never a credential route.
pub(crate) const WORKSPACE_TOOL_ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/workspaces/{id}"),
    // The spec spells this `PATCH /v1/workspaces/{id}/packages`; the router has no such route —
    // `patch_ws_packages` IS the PATCH on the workspace and takes packages only. The router is
    // the truth, and the complement test below is what keeps this table honest about it.
    ("PATCH", "/v1/workspaces/{id}"),
    ("POST", "/v1/workspaces/{id}/packages/update"),
    ("GET", "/v1/environments"),
    ("GET", "/v1/me/environments"),
    ("PUT", "/v1/me/environments/{team}"),
    ("DELETE", "/v1/me/environments/{team}"),
];

/// Segment match against `BENCH_TOOL_ROUTES`: a `{x}` matches one non-empty segment, anything
/// else matches itself. `/v1/workspaces/restore` also matching `{id}` is harmless: the router
/// still sends it to the literal route, and both are bench-tool routes.
pub(crate) fn bench_tool_route(method: &axum::http::Method, path: &str) -> bool {
    route_in(BENCH_TOOL_ROUTES, method, path)
}

/// The same for `kl`'s audience.
pub(crate) fn workspace_tool_route(method: &axum::http::Method, path: &str) -> bool {
    route_in(WORKSPACE_TOOL_ROUTES, method, path)
}

fn route_in(table: &[(&str, &str)], method: &axum::http::Method, path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').collect();
    table.iter().any(|(m, pat)| {
        *m == method.as_str() && {
            let pats: Vec<&str> = pat.split('/').collect();
            pats.len() == segs.len()
                && pats.iter().zip(&segs).all(|(p, s)| {
                    if p.starts_with('{') { !s.is_empty() } else { p == s }
                })
        }
    })
}

#[cfg(test)]
mod route_tests {
    /// The router is the truth; this is the copy the builder's 404 test can iterate. Reading this
    /// file's own source is the only way to compare them — and it is a real check: adding
    /// `.route("/v1/environments/{id}/anything", ...)` above fails here until it is listed.
    /// Every path this file hands to `.route(`, including one whose path sits on the next line.
    fn registered_routes() -> Vec<String> {
        let lines: Vec<&str> = include_str!("mod.rs").lines().map(str::trim).collect();
        lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| match l.strip_prefix(".route(") {
                Some("") => lines.get(i + 1).copied(),
                other => other,
            })
            .filter_map(|l| l.strip_prefix('"')?.split_once('"').map(|(p, _)| p.to_string()))
            .filter(|p| p.starts_with("/v1"))
            .collect()
    }

    /// Every `/v1` route a bench-tool token is refused on. Held with `BENCH_TOOL_ROUTES` to the
    /// router, so a new route is a deliberate choice of audience rather than a silent default.
    const NOT_BENCH_TOOL_ROUTES: &[&str] = &[
        "/v1/quota-requests",
        "/v1/requests",
        "/v1/requests/{id}",
        "/v1/workspaces/{id}/ssh-session",
        "/v1/internal/builders",
        "/v1/internal/builders/{slug}",
        "/v1/internal/builders/{slug}/start",
        "/v1/internal/builders/{slug}/stop",
        "/v1/teams/{slug}/members/{email}/delete-now",
        "/v1/teams/{slug}/removals",
        "/v1/bench",
        "/v1/bench/teams",
        "/v1/bench/start",
        "/v1/bench/stop",
        "/v1/bench/session",
        "/v1/bench/tool-token",
        "/v1/bench/attach",
        "/v1/bench/detach",
    ];

    #[test]
    fn every_v1_route_is_classified_for_bench_tools() {
        let mut found = registered_routes();
        found.sort();
        found.dedup();
        let mut classified: Vec<String> = super::BENCH_TOOL_ROUTES
            .iter()
            .map(|(_, p)| (*p).to_string())
            .chain(NOT_BENCH_TOOL_ROUTES.iter().map(|p| (*p).to_string()))
            .collect();
        classified.sort();
        classified.dedup();
        assert_eq!(found, classified, "a /v1 route is unclassified (or a table names a gone route)");
        for p in NOT_BENCH_TOOL_ROUTES {
            assert!(!super::BENCH_TOOL_ROUTES.iter().any(|(_, b)| b == p), "{p} is in both tables");
        }
    }

    /// `WORKSPACE_TOOL_ROUTES` has no complement list of its own: it is a strict subset of the
    /// routes that exist, and `workspace_tool` refuses everything off it with `audience` by
    /// construction. What this holds is that every entry still NAMES a route — a rename would
    /// otherwise leave `kl` with an audience for a path the router has not got — and that no
    /// other route's SHAPE is admitted under any method.
    ///
    /// A literal path that also matches a `{id}` entry is skipped for the reason
    /// `bench_tool_route` documents: axum sends it to the literal route, or answers 405.
    #[test]
    fn every_workspace_tool_route_exists_and_no_other_shape_is_admitted() {
        use axum::http::Method;
        let found = registered_routes();
        for (_, p) in super::WORKSPACE_TOOL_ROUTES {
            assert!(found.iter().any(|r| r == p), "{p} is not a route");
        }
        for p in found.iter().filter(|p| p.contains('{')) {
            if super::WORKSPACE_TOOL_ROUTES.iter().any(|(_, tp)| tp == p) {
                continue;
            }
            let concrete: String =
                p.split('/').map(|s| if s.starts_with('{') { "x" } else { s }).collect::<Vec<_>>().join("/");
            for m in [Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
                assert!(!super::workspace_tool_route(&m, &concrete), "{m} {p} admits a workspace token");
            }
        }
    }

    #[test]
    fn workspace_tool_route_is_the_seven_and_the_team_segment_is_read() {
        use axum::http::Method;
        assert!(super::workspace_tool_route(&Method::GET, "/v1/workspaces/w1"));
        assert!(super::workspace_tool_route(&Method::PUT, "/v1/me/environments/acme"));
        assert!(!super::workspace_tool_route(&Method::POST, "/v1/workspaces/w1/start"));
        assert!(!super::workspace_tool_route(&Method::DELETE, "/v1/workspaces/w1"));
        assert!(!super::workspace_tool_route(&Method::GET, "/v1/workspaces/{id}/tools".replace("{id}", "w1").as_str()));
        assert_eq!(super::team_segment("/v1/me/environments/acme"), Some("acme"));
        assert_eq!(super::team_segment("/v1/me/environments"), None);
        assert_eq!(super::team_segment("/v1/me/environments/a/b"), None);
        assert_eq!(super::team_segment("/v1/workspaces/w1"), None);
    }

    #[test]
    fn bench_tool_route_matches_patterns() {
        use axum::http::Method;
        assert!(super::bench_tool_route(&Method::GET, "/v1/workspaces/w1"));
        assert!(!super::bench_tool_route(&Method::POST, "/v1/workspaces/w1/ssh-session"));
        assert!(!super::bench_tool_route(&Method::GET, "/v1/cli/tokens"));
        assert!(!super::bench_tool_route(&Method::POST, "/v1/bench/tool-token"));
        assert!(!super::bench_tool_route(&Method::GET, "/v1/workspaces/a/b/c"));
        assert!(!super::bench_tool_route(&Method::PUT, "/v1/workspaces/w1"));
        assert!(!super::bench_tool_route(&Method::GET, "/v1/workspaces//tools"));
        assert!(!super::BENCH_TOOL_ROUTES.iter().any(|(_, p)| *p == "/v1/bench/teams"), "a desktop picker");
    }

    #[test]
    fn bench_admits_tool_checks_owner_team_state_access() {
        let bench = |owner: &str, team: &str, desired: &str, access: &str| -> crate::crd::Workspace {
            serde_json::from_value(serde_json::json!({
                "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace", "metadata": {"name": "b"},
                "spec": {"owner": owner, "team": team, "name": "bench", "region": "r1", "image": "i",
                         "desiredState": desired, "access": access, "bench": {"model": "m"}}
            }))
            .unwrap()
        };
        assert!(super::bench_admits_tool(&bench("alice", "acme", "running", "full"), "alice", "acme"));
        assert!(!super::bench_admits_tool(&bench("bob", "acme", "running", "full"), "alice", "acme"));
        assert!(!super::bench_admits_tool(&bench("alice", "t2", "running", "full"), "alice", "acme"));
        assert!(!super::bench_admits_tool(&bench("alice", "acme", "stopped", "full"), "alice", "acme"));
        assert!(!super::bench_admits_tool(&bench("alice", "acme", "running", "readOnly"), "alice", "acme"));
        assert!(!super::bench_admits_tool(&bench("alice", "acme", "running", "paused"), "alice", "acme"));
        // An ordinary workspace of the same owner is not this token's audience.
        let mut plain = bench("alice", "acme", "running", "full");
        plain.spec.bench = None;
        assert!(!super::bench_admits_tool(&plain, "alice", "acme"));
    }

    #[test]
    fn every_environment_id_route_is_listed() {
        let mut found: Vec<String> = registered_routes()
            .into_iter()
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
        .route("/v1/workspaces/{id}/tools", get(ws_tools))
        .route("/v1/workspaces/{id}/packages/update", post(update_ws_packages))
        .route("/v1/workspaces/{id}/clone", post(clone_ws))
        .route("/v1/workspaces/{id}/push", post(push_ws))
        .route("/v1/workspaces/{id}/start", post(start_ws))
        .route("/v1/workspaces/{id}/stop", post(stop_ws))
        .route("/v1/workspaces/{id}/attach", post(attach_gone))
        .route("/v1/workspaces/{id}/detach", post(attach_gone))
        .route("/v1/me/environments", get(list_my_environments))
        .route("/v1/me/environments/{team}", axum::routing::put(set_my_environment).delete(clear_my_environment))
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
        // Param names match the directory's `/v1/teams/{slug}/members/{email}/*`: both routers merge into one app.
        .route("/v1/teams/{slug}/members/{email}/delete-now", post(removals::delete_now_route))
        .route("/v1/teams/{slug}/removals", get(removals::team_removals))
        .merge(internal_router(state.clone()))
        .route("/v1/volumes", get(list_volumes))
        .route("/v1/volumes/{name}/history", get(volume_history))
        .route("/v1/volumes/{name}", axum::routing::delete(delete_volume))
        .route(
            "/v1/volumes/{name}/snapshots/{snapshot}",
            axum::routing::delete(delete_snapshot),
        )
        .route("/v1/volumes/{name}/refs", get(volume_refs))
        .route("/v1/bench", get(get_bench).post(create_bench))
        .route("/v1/bench/teams", get(bench_teams))
        .route("/v1/bench/start", post(start_bench))
        .route("/v1/bench/stop", post(stop_bench))
        .route("/v1/bench/session", post(bench_session))
        .route("/v1/bench/tool-token", post(mint_tool_token).delete(revoke_tool_token))
        .route("/v1/bench/attach", post(attach_gone))
        .route("/v1/bench/detach", post(attach_gone))
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
    if let Some(jti) = &jti {
        if !admin::timing::step("directory.token_live", cli_token_live(state, jti)).await {
            return Err(unauthorized());
        }
    }
    let superadmin = c.superadmin;
    let name = c.username.filter(|u| !u.is_empty()).ok_or_else(|| {
        (StatusCode::FORBIDDEN, "pick a username before using workspaces").into_response()
    })?;
    Ok(Caller { name, superadmin, parent: jti, scope: None, jti8: None })
}


/// `caller`, for a handler on a `BENCH_TOOL_ROUTES` route: a bench-tool token is admitted here
/// and nowhere else, so a handler that still calls `caller` stays unreachable from a bench pod.
/// `path` is the query-free `uri.path()` the table is matched against.
pub(crate) async fn caller_for(
    state: &ApiState,
    headers: &axum::http::HeaderMap,
    method: &axum::http::Method,
    path: &str,
) -> Result<Caller, Response> {
    let Some(tok) = bearer_token(headers) else { return caller(state, headers).await };
    let tok = tok.trim();
    let claims = match state.jwt.verify_bench_tool(tok) {
        Ok(c) => c,
        Err(_) => {
            if let Some(c) = state.jwt.expired_bench_tool(tok) {
                return Err(bench_tool_refused(&c, "expired"));
            }
            if let Some(r) = workspace_tool(state, tok, method, path) {
                return r;
            }
            return caller(state, headers).await;
        }
    };
    match bench_tool_check(state, &claims, method, path).await? {
        Ok(c) => {
            kloudlite_core::metrics::mark_via("bench-tool");
            Ok(c)
        }
        Err(reason) => Err(bench_tool_refused(&claims, reason)),
    }
}

/// The bench-tool gate: the caller, or the refusal reason. The outer `Err` is a kube outage (500),
/// never a refusal, so an unreadable Bench is not logged as a bad token.
pub(crate) async fn bench_tool_check(
    state: &ApiState,
    claims: &kloudlite_core::jwt::BenchToolClaims,
    method: &axum::http::Method,
    path: &str,
) -> Result<Result<Caller, &'static str>, Response> {
    if !bench_tool_route(method, path) {
        return Ok(Err("audience"));
    }
    // The pod's credential dies with the login it was minted from, on the same 30 s cache.
    if !cli_token_live(state, &claims.parent).await {
        return Ok(Err("parent"));
    }
    let benches: Api<crd::Workspace> = Api::all(kube(state)?.clone());
    let bench = benches.get_opt(&claims.bench).await.map_err(kube_err)?;
    if !bench.is_some_and(|b| bench_admits_tool(&b, &claims.sub, &claims.team)) {
        return Ok(Err("bench"));
    }
    Ok(Ok(Caller {
        name: claims.sub.clone(),
        superadmin: false,
        parent: None,
        scope: Some(claims.team.clone()),
        jti8: Some(claims.jti.chars().take(8).collect()),
    }))
}

/// Whether a bench's tools may act now. One predicate so a new way to suspend a bench (pause) is
/// one more arm here, never a second check somewhere else.
///
/// `is_bench` leads deliberately: the claim names a workspace id, and an ORDINARY workspace of the
/// same owner must not answer a bench-tool token — the audience is the bench, not the person.
pub(crate) fn bench_admits_tool(w: &crd::Workspace, sub: &str, team: &str) -> bool {
    crd::is_bench(w)
        && w.spec.owner == sub
        && w.spec.team == team
        && w.spec.desired_state != crd::DesiredState::Stopped
        && w.spec.access == crd::Access::Full
}

/// Eight hex characters of the jti: enough to join log lines, useless as a credential.
fn bench_tool_refused(c: &kloudlite_core::jwt::BenchToolClaims, reason: &'static str) -> Response {
    let jti8 = c.jti.get(..8).unwrap_or_default();
    tracing::info!(owner = %c.sub, jti8, reason, "bench.tool.refused");
    unauthorized()
}


/// The workspace-token gate. `None` means "not a workspace token at all", so the caller falls
/// through to an ordinary login.
///
/// No directory call and no CR read, unlike the bench-tool gate: the pod's identity is the
/// owner's key projection, and the keys beat rewriting `user-key` without this item — which is
/// what a revoked key or a dropped membership already does — IS the revocation path.
fn workspace_tool(
    state: &ApiState,
    tok: &str,
    method: &axum::http::Method,
    path: &str,
) -> Option<Result<Caller, Response>> {
    let claims = match state.jwt.verify_workspace_tool(tok) {
        Ok(c) => c,
        Err(_) => return Some(Err(workspace_tool_refused(&state.jwt.expired_workspace_tool(tok)?, "expired"))),
    };
    let outcome = if !workspace_tool_route(method, path) {
        Err("audience")
    } else if team_segment(path).is_some_and(|t| !t.eq_ignore_ascii_case(&claims.space)) {
        Err("space")
    } else {
        // `scope` is the ceiling everything else reads: the existing `my_ws` and `in_scope` checks
        // then refuse a workspace or a team this caller does not own, exactly as for a person.
        Ok(Caller {
            name: claims.sub.clone(),
            superadmin: false,
            parent: None,
            scope: Some(claims.space.clone()),
            jti8: Some(claims.jti.chars().take(8).collect()),
        })
    };
    Some(match outcome {
        Ok(c) => {
            kloudlite_core::metrics::mark_via("workspace-tool");
            Ok(c)
        }
        Err(reason) => Err(workspace_tool_refused(&claims, reason)),
    })
}

/// The `{team}` segment of the two space-environment routes, the only `WORKSPACE_TOOL_ROUTES`
/// entries that name a space in the path.
fn team_segment(path: &str) -> Option<&str> {
    path.strip_prefix("/v1/me/environments/").filter(|t| !t.is_empty() && !t.contains('/'))
}

fn workspace_tool_refused(c: &kloudlite_core::jwt::WorkspaceToolClaims, reason: &'static str) -> Response {
    let jti8 = c.jti.get(..8).unwrap_or_default();
    tracing::info!(owner = %c.sub, jti8, reason, "workspace.tool.refused");
    unauthorized()
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
    method: axum::http::Method,
    uri: axum::extract::OriginalUri,
) -> Result<Response, Response> {
    caller_for(&s, &headers, &method, uri.path()).await?;
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
/// `check_region`'s question for a caller outside this crate (team creation on the directory tier):
/// `Ok(false)` for a name that is not an active region, `Err` when the cluster could not say.
pub async fn region_active(s: &ApiState, region: &str) -> Result<bool, String> {
    if check_path_segment(region).is_err() {
        return Ok(false);
    }
    let Some(client) = s.kube.as_ref() else { return Err("kubernetes not configured".into()) };
    let api: Api<crd::Region> = Api::all(client.clone());
    Ok(api.get_opt(region).await.map_err(|e| e.to_string())?.is_some_and(|r| r.spec.status == "active"))
}

/// Every ACTIVE region's name, for the directory tier's "place a new person in the only region".
pub async fn active_regions(s: &ApiState) -> Result<Vec<String>, String> {
    let Some(client) = s.kube.as_ref() else { return Err("kubernetes not configured".into()) };
    let api: Api<crd::Region> = Api::all(client.clone());
    let list = api.list(&Default::default()).await.map_err(|e| e.to_string())?;
    Ok(list.items.iter().filter(|r| r.spec.status == "active").map(kube::ResourceExt::name_any).collect())
}

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

#[cfg(test)]
mod bench_tool_check_tests {
    use super::*;
    use axum::http::Method;
    use kloudlite_core::jwt::{BenchToolClaims, Jwt};
    use std::sync::Arc;

    /// Only `revoked` is a dead login.
    struct Live;
    #[async_trait::async_trait]
    impl Directory for Live {
        async fn teams_for(&self, _u: &str) -> Vec<String> {
            Vec::new()
        }
        async fn is_live(&self, j: &str) -> bool {
            j != "revoked"
        }
        async fn for_owner(&self, _o: &str) -> Option<OwnerMaterial> {
            None
        }
        async fn authorized_keys_for_owner(&self, _o: &str) -> Option<String> {
            None
        }
        async fn owners_of(&self, _e: &str) -> Vec<String> {
            Vec::new()
        }
        async fn team_role(&self, _u: &str, _t: &str) -> Option<TeamRole> {
            None
        }
        async fn is_team(&self, _s: &str) -> bool {
            true
        }
        async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
            Err("no".into())
        }
        async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
            Err("no".into())
        }
    }

    fn claims(parent: &str) -> BenchToolClaims {
        BenchToolClaims {
            sub: "alice".into(),
            team: "acme".into(),
            bench: "b1".into(),
            parent: parent.into(),
            jti: "0123456789abcdef".into(),
            iat: 0,
            exp: u64::MAX,
            typ: "bench-tool".into(),
        }
    }

    /// The reason for one check, against a bench Workspace with this spec (None = no object).
    async fn reason(c: &BenchToolClaims, method: Method, path: &str, spec: Option<serde_json::Value>) -> Result<Caller, &'static str> {
        let routes = spec
            .map(|sp| {
                vec![crate::kube_test::get(
                    "/apis/kloudlite.io/v1alpha1/workspaces/b1",
                    serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace", "metadata": {"name": "b1"}, "spec": sp}),
                )]
            })
            .unwrap_or_default();
        let (client, _) = crate::kube_test::mock_client(routes);
        let jwt = Arc::new(Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap());
        let s = ApiState::new(jwt).with_kube(client).with_directory(Arc::new(Live));
        bench_tool_check(&s, c, &method, path).await.unwrap_or_else(|_| panic!("kube outage"))
    }

    fn spec(owner: &str, team: &str, desired: &str, access: &str) -> Option<serde_json::Value> {
        Some(serde_json::json!({"owner": owner, "team": team, "name": "bench", "region": "r1", "image": "i",
                                "desiredState": desired, "access": access, "bench": {"model": "m"}}))
    }

    #[tokio::test]
    async fn each_gate_refuses_with_its_own_reason() {
        let good = || spec("alice", "acme", "running", "full");
        let ok = reason(&claims("live"), Method::GET, "/v1/workspaces", good()).await.unwrap();
        assert_eq!((ok.name.as_str(), ok.scope.as_deref(), ok.jti8.as_deref()), ("alice", Some("acme"), Some("01234567")));
        assert!(ok.parent.is_none());

        assert_eq!(reason(&claims("live"), Method::POST, "/v1/workspaces/w1/ssh-session", good()).await.err(), Some("audience"));
        assert_eq!(reason(&claims("revoked"), Method::GET, "/v1/workspaces", good()).await.err(), Some("parent"));
        for (sp, why) in [
            (spec("alice", "acme", "stopped", "full"), "stopped"),
            (spec("alice", "acme", "running", "readOnly"), "stored read-only"),
            (spec("alice", "acme", "running", "paused"), "paused"),
            (spec("bob", "acme", "running", "full"), "wrong owner"),
            (spec("alice", "t2", "running", "full"), "wrong team"),
            // An ordinary workspace under this id is not a bench, whatever else matches.
            (spec("alice", "acme", "running", "full").map(|mut v| { v.as_object_mut().unwrap().remove("bench"); v }), "not a bench"),
            (None, "missing"),
        ] {
            assert_eq!(reason(&claims("live"), Method::GET, "/v1/workspaces", sp).await.err(), Some("bench"), "{why}");
        }
    }
}


#[cfg(test)]
mod workspace_tool_tests {
    use super::*;
    use axum::http::Method;
    use kloudlite_core::jwt::Jwt;
    use std::sync::Arc;

    fn state() -> ApiState {
        ApiState::new(Arc::new(Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap()))
    }

    /// `Some(Ok)` admitted, `Some(Err)` refused, `None` "not a workspace token" — the third is
    /// what lets an ordinary login keep working on the same routes.
    fn gate(s: &ApiState, tok: &str, m: Method, path: &str) -> Option<Result<Caller, axum::http::StatusCode>> {
        super::workspace_tool(s, tok, &m, path).map(|r| r.map_err(|e| e.status()))
    }

    #[test]
    fn the_audience_is_the_seven_routes_and_the_team_segment_must_be_the_space() {
        let s = state();
        let (tok, _) = s.jwt.mint_workspace_tool("alice", "acme").unwrap();

        let ok = gate(&s, &tok, Method::GET, "/v1/workspaces/w1").unwrap().unwrap();
        // The scope is the ceiling `in_scope`/`my_ws` then apply — no route of its own says who
        // owns what.
        assert_eq!((ok.name.as_str(), ok.scope.as_deref()), ("alice", Some("acme")));
        assert!(ok.parent.is_none() && !ok.superadmin && ok.jti8.is_some());

        assert!(gate(&s, &tok, Method::PUT, "/v1/me/environments/acme").unwrap().is_ok());
        // Another space's environment, with a token that is otherwise perfectly good.
        assert_eq!(gate(&s, &tok, Method::PUT, "/v1/me/environments/globex").unwrap().unwrap_err(), axum::http::StatusCode::UNAUTHORIZED);
        // Off the audience: a lifecycle verb and a credential route are both refused here rather
        // than by the handler, so a new route is never admitted by default.
        assert!(gate(&s, &tok, Method::POST, "/v1/workspaces/w1/start").unwrap().is_err());
        assert!(gate(&s, &tok, Method::POST, "/v1/workspaces/w1/ssh-session").unwrap().is_err());
        assert!(gate(&s, &tok, Method::DELETE, "/v1/workspaces/w1").unwrap().is_err());
    }

    /// Anything that is not a workspace token falls through to `caller`, and an EXPIRED one does
    /// not — it is refused where the reason can still be logged.
    #[test]
    fn only_a_workspace_token_is_gated_here() {
        let s = state();
        let (cli, _) = s.jwt.mint_cli("a@b.c", "A", Some("a")).unwrap();
        assert!(gate(&s, &cli, Method::GET, "/v1/workspaces/w1").is_none());
        assert!(gate(&s, "not-a-token", Method::GET, "/v1/workspaces/w1").is_none());
        let (bench, _) = s.jwt.mint_bench_tool("alice", "acme", "b1", "p").unwrap();
        assert!(gate(&s, &bench, Method::GET, "/v1/workspaces/w1").is_none());
    }
}
