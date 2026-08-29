pub(crate) mod git;
pub(crate) mod limits;
pub(crate) mod route;

pub(crate) use git::{git_routes, open};
pub(crate) use limits::internal;
pub use limits::{max_body, Trusted};
use route::{own_claim, own_release, own_renew, route_peer, route_public, trust_nobody, trust_peer};

use crate::vol_agent::JobsState;
use crate::App;
use axum::{routing::get, routing::post, Router};
use std::sync::Arc;

/// Client-facing. Layers run outermost-first, and the LAST `.layer()` call is outermost — so
/// `trust_nobody` (added last) runs first, then `route`, then the handler.
///
/// `jobs` is the vol-agent token check's region lookup — the SAME `Arc` `peer_router` gets, so a
/// forwarded request is checked against the same regions it would have been checked against
/// had it landed on the owner directly. `None` store means "no Cosmos on this node", handled
/// inside the handlers (break-glass only), not by leaving the routes unmounted.
pub fn router(app: Arc<App>, jobs: Arc<JobsState>) -> Router {
    git_routes()
        .merge(crate::registry::routes::v2_routes())
        // The volume-registry agent surface: per-region-token gated inside the handlers, not in a
        // layer — routing must run before any auth check, per `route_inner`. Its handlers reach
        // `jobs` through `Extension`, wired in by the `.layer()` beneath.
        .merge(crate::vol_agent::vol_agent_routes())
        .route("/healthz", get(route::healthz))
        .route("/livez", get(route::livez))
        .layer(axum::middleware::from_fn_with_state(app.clone(), route_public))
        .layer(axum::middleware::from_fn(trust_nobody))
        .layer(axum::middleware::from_fn_with_state("public", rustic_git_core::metrics::http_metrics))
        .layer(axum::Extension(jobs))
        .with_state(app)
}

/// Peer-facing. `trust_peer` outermost (secret check first, on everything), then `route`, then
/// handlers. `/healthz` and the `/own/*` protocol are inside the secret check on purpose: a claim
/// without the secret must fail loudly (403), not silently succeed and hide a misconfiguration.
/// The `route` middleware ignores non-git paths, so `/own/*` passes straight through it.
pub fn peer_router(app: Arc<App>, jobs: Arc<JobsState>) -> Router {
    git_routes()
        .merge(crate::browse_api::browse_routes())
        .merge(crate::registry::routes::v2_routes())
        // The vol-agent RECORD routes must exist here too: a public request landing on a
        // non-owning node is forwarded to the owner's PEER listener, and a route that only
        // lives on the public router 404s every forwarded call — which is exactly how the
        // first multi-node deployment failed (single-node e2e never forwards, so it never
        // saw it). The peer secret is NOT a substitute for the agent token here: it proves a
        // node forwarded the request, not that whoever sent it to that node was an agent — a
        // marker that let the handlers skip the token check on this listener meant every
        // forwarded write was unauthenticated. The handlers run the same check on the
        // forwarded headers, against the same `jobs`.
        .merge(crate::vol_agent::vol_agent_routes())
        .layer(axum::Extension(jobs))
        .route("/healthz", get(route::healthz))
        .route("/livez", get(route::livez))
        .route("/own/claim", post(own_claim))
        .route("/own/renew", post(own_renew))
        .route("/own/release", post(own_release))
        // Scraped without the secret (see `trust_peer`); never mounted on the public router.
        .merge(rustic_git_core::metrics::routes())
        .layer(axum::middleware::from_fn_with_state(app.clone(), route_peer))
        .layer(axum::middleware::from_fn_with_state(app.clone(), trust_peer))
        .layer(axum::middleware::from_fn_with_state("peer", rustic_git_core::metrics::http_metrics))
        .with_state(app)
}
