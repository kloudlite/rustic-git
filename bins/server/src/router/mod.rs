pub(crate) mod admin_settings;
pub(crate) mod git;
pub(crate) mod limits;
pub mod route;

pub(crate) use git::{git_routes, open};
pub(crate) use limits::internal;
use route::{own_claim, own_owner, own_release, own_renew, route_peer, route_public, trust_nobody, trust_peer};

use crate::App;
use axum::{routing::get, routing::post, Router};
use std::sync::Arc;

/// Client-facing. Layers run outermost-first, and the LAST `.layer()` call is outermost — so
/// `trust_nobody` (added last) runs first, then `route`, then the handler.
pub fn router(app: Arc<App>) -> Router {
    git_routes()
        .merge(crate::registry::routes::v2_routes())
        .route("/healthz", get(route::healthz))
        .route("/livez", get(route::livez))
        .layer(axum::middleware::from_fn_with_state(app.clone(), route_public))
        .layer(axum::middleware::from_fn(trust_nobody))
        .layer(axum::middleware::from_fn_with_state("public", rustic_git_core::metrics::http_metrics))
        .with_state(app)
}

/// Peer-facing. `trust_peer` outermost (secret check first, on everything), then `route`, then
/// handlers. `/healthz` and the `/own/*` protocol are inside the secret check on purpose: a claim
/// without the secret must fail loudly (403), not silently succeed and hide a misconfiguration.
/// The `route` middleware ignores non-git paths, so `/own/*` passes straight through it.
/// `/metrics` is NOT here: it is a listener of its own (`RUSTIC_GIT_METRICS_ADDR`), because a
/// scrape route inside the secret check cannot be scraped, and one outside it is an enumeration
/// oracle for any pod on a cluster running `networkPolicy: none`.
pub fn peer_router(app: Arc<App>) -> Router {
    git_routes()
        .merge(crate::browse_api::browse_routes())
        .merge(crate::registry::routes::v2_routes())
        .route("/healthz", get(route::healthz))
        .route("/livez", get(route::livez))
        .route("/own/claim", post(own_claim))
        .route("/own/renew", post(own_renew))
        .route("/own/owner", post(own_owner))
        .route("/own/release", post(own_release))
        // Shared object-store document, not a per-repo database — servable on any node, so it
        // carries no `BROWSE_TAILS` entry (`route_inner`'s `admin/settings` exemption is what lets
        // it through the "unrecognised /api/ path" 404). Peer-only like everything else here;
        // `admin_settings::put_settings` adds the second gate, a `superadmin` bearer claim.
        .route(
            "/api/admin/settings",
            get(admin_settings::get_settings).put(admin_settings::put_settings),
        )
        .layer(axum::middleware::from_fn_with_state(app.clone(), route_peer))
        .layer(axum::middleware::from_fn_with_state(app.clone(), trust_peer))
        .layer(axum::middleware::from_fn_with_state("peer", rustic_git_core::metrics::http_metrics))
        .with_state(app)
}
