pub(crate) mod git;
pub(crate) mod limits;
pub(crate) mod route;

pub(crate) use git::{git_routes, open};
pub(crate) use limits::internal;
pub use limits::{max_body, Trusted};
use route::{own_claim, own_draining, own_release, own_renew, route_peer, route_public, trust_nobody, trust_peer};

use crate::vol_agent::JobsState;
use crate::App;
use axum::{routing::get, routing::post, Router};
use std::sync::Arc;

/// Client-facing. Layers run outermost-first, and the LAST `.layer()` call is outermost — so
/// `trust_nobody` (added last) runs first, then `route`, then the handler.
///
/// `jobs` is the agent work surface's state (Task 14) — `None` store means "not configured on
/// this node", handled inside the handlers (503), not by leaving the routes unmounted.
pub fn router(app: Arc<App>, jobs: Arc<JobsState>) -> Router {
    git_routes()
        .merge(crate::registry::routes::v2_routes())
        // The volume-registry agent surface: public, per-region-token gated (inside the
        // handlers, not this layer — routing must run before any auth check, per `route_inner`),
        // never the peer listener, agents have no peer secret.
        .merge(crate::vol_agent::vol_agent_routes())
        // The agent WORK surface (register/work/jobs/*) — same `Router<Arc<App>>` type, so it
        // merges in cleanly and inherits `route_public`/`trust_nobody` below. Its handlers reach
        // `jobs` through `Extension`, not `State<Arc<App>>`, wired in by the `.layer()` beneath.
        .merge(crate::vol_agent::vol_agent_job_routes())
        .route("/healthz", get(route::healthz))
        .layer(axum::middleware::from_fn_with_state(app.clone(), route_public))
        .layer(axum::middleware::from_fn(trust_nobody))
        .layer(axum::Extension(jobs))
        .with_state(app)
}

/// Peer-facing. `trust_peer` outermost (secret check first, on everything), then `route`, then
/// handlers. `/healthz` and the `/own/*` protocol are inside the secret check on purpose: a claim
/// without the secret must fail loudly (403), not silently succeed and hide a misconfiguration.
/// The `route` middleware ignores non-git paths, so `/own/*` passes straight through it.
pub fn peer_router(app: Arc<App>) -> Router {
    git_routes()
        .merge(crate::browse_api::browse_routes())
        .merge(crate::registry::routes::v2_routes())
        .route("/healthz", get(route::healthz))
        .route("/own/claim", post(own_claim))
        .route("/own/renew", post(own_renew))
        .route("/own/release", post(own_release))
        .route("/own/draining", post(own_draining))
        .layer(axum::middleware::from_fn_with_state(app.clone(), route_peer))
        .layer(axum::middleware::from_fn_with_state(app.clone(), trust_peer))
        .with_state(app)
}
