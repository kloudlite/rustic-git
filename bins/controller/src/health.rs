//! One route. The probe answers as soon as the process is up — NOT only while it is the leader:
//! a follower is healthy, it is simply not writing, and failing its probe would restart the one
//! pod that is about to take over.

use std::sync::Arc;

pub fn app(ctx: Arc<crate::ctx::Ctx>) -> axum::Router {
    axum::Router::new().route(
        "/healthz",
        axum::routing::get(move || {
            let ctx = ctx.clone();
            async move {
                // The role is in the BODY, not the status: a 503 for a follower would flap the
                // Deployment's readiness on every ordinary handover.
                if ctx.leading() {
                    format!("leader epoch={}\n", ctx.epoch())
                } else {
                    "follower\n".to_string()
                }
            }
        }),
    )
}
