//! Reading the services back: StatefulSet readiness into `status.services[]`, the Endpoints and
//! EndpointSlices Kubernetes abandons when a Service loses its selector, and the status write.

use super::*;


/// Every endpoint object for `service` that this controller did not write, gone.
///
/// Bounded by the service's own label and by `ours`, so it can only ever remove what Kubernetes
/// abandoned for the one service being intercepted, in a namespace this controller reconciles.
pub(crate) async fn drop_abandoned_endpoints(
    slices: &Api<EndpointSlice>,
    ns: &str,
    service: &str,
    ours: &str,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    let lp = kube::api::ListParams::default().labels(&format!("kubernetes.io/service-name={service}"));
    for s in slices.list(&lp).await?.items {
        let name = s.name_any();
        if name == ours {
            continue;
        }
        forget_applied(ctx, "EndpointSlice", ns, &name);
        delete_ignoring_404(slices, &name).await?;
    }
    // Named exactly for the Service, which is what makes this safe to delete by name.
    delete_ignoring_404(&Api::<Endpoints>::namespaced(ctx.client.clone(), ns), service).await
}


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
    unreachable_since: Option<i64>,
) -> crd::ServiceStatus {
    let Some(d) = set else {
        return crd::ServiceStatus {
            name: name.into(),
            ready: false,
            message: Some("statefulset not created yet".into()),
            intercepted_by,
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
            unreachable_since,
        };
    }
    crd::ServiceStatus {
        name: name.into(),
        ready: ready >= 1,
        message: (ready < 1).then(|| "no ready replicas".to_string()),
        intercepted_by,
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
