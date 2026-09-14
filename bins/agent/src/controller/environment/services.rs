//! Reading the services back: StatefulSet readiness into `status.services[]`, the intercept's own
//! proxy state beside it, and the status write.

use super::*;


/// One service's observed readiness, from the StatefulSet's own status.
///
/// `readyReplicas >= 1`, not `replicas`: `replicas` is what was asked for, `readyReplicas` is what
/// is actually serving. A missing StatefulSet reports not-ready rather than erroring — it is the
/// ordinary gap between applying it and the API server materializing it.
pub(crate) fn deployment_status(
    // The set as the pass's ONE listing found it, not a GET of its own (2026-09-12): this ran once
    // per service per reconcile, so a ten-service environment made ten GETs of a collection one
    // LIST already answers.
    set: Option<&StatefulSet>,
    name: &str,
    // Decided by `intercept_plan`, threaded in rather than recomputed: this function reconstructs
    // the whole `ServiceStatus` every pass, so anything it defaults here is stomped every pass.
    intercepted_by: Option<String>,
    // What `apply_intercept` answered this pass: `starting`, `ready` or `failed`, and `None` for a
    // service no intercept wrote anything about.
    proxy: Option<String>,
    unreachable_since: Option<i64>,
) -> crd::ServiceStatus {
    let Some(d) = set else {
        return crd::ServiceStatus {
            name: name.into(),
            ready: false,
            message: Some("statefulset not created yet".into()),
            intercepted_by,
            proxy,
            unreachable_since,
        };
    };
    let ready = d.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0);
    // An intercepted service is scaled to zero BY US, so zero ready replicas is the converged
    // state, not a fault — reporting it not-ready would park the environment at `ServicesNotReady`
    // and requeue it forever for as long as somebody is debugging.
    if let Some(ws) = &intercepted_by {
        return crd::ServiceStatus {
            name: name.into(),
            ready: true,
            message: Some(format!("intercepted by {ws}")),
            intercepted_by: intercepted_by.clone(),
            proxy,
            unreachable_since,
        };
    }
    crd::ServiceStatus {
        name: name.into(),
        ready: ready >= 1,
        message: (ready < 1).then(|| "no ready replicas".to_string()),
        intercepted_by,
        proxy,
        unreachable_since,
    }
}


pub(crate) async fn write_env_status(e: &crd::Environment, st: crd::EnvironmentStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    write_status(e, "Environment", e.status.as_ref(), &st, ctx, |a, b| {
        a.phase == b.phase
            && a.observed_generation == b.observed_generation
            && a.node_name == b.node_name
            && a.volume_ref == b.volume_ref
            && a.service_status == b.service_status
            // See `write_ws_status`'s twin comment: without this, a head-only advance is a no-op.
            && a.head == b.head
            // Same rule: the pass that re-applies a restore of the snapshot `head` already names
            // changes only these two, and without them here it would never be recorded — leaving
            // `restore_gate` re-applying that wish forever.
            && a.restored_to == b.restored_to
            && a.restore_requested_at == b.restore_requested_at
            && conditions_eq(&a.conditions, &b.conditions)
    })
    .await
}
