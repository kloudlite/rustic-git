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
    // The service's `{svc}-0` pod, from the pass's ONE pod listing: the StatefulSet only says "no
    // ready replicas", the pod says why (ImagePullBackOff, CrashLoopBackOff, ...).
    pod: Option<&Pod>,
) -> crd::ServiceStatus {
    let Some(d) = set else {
        return crd::ServiceStatus {
            name: name.into(),
            ready: false,
            message: Some("statefulset not created yet".into()),
            intercepted_by,
            proxy,
            unreachable_since,
            failing: false,
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
            failing: false,
        };
    }
    let (message, failing) = match (ready < 1).then(|| not_ready_reason(pod)) {
        None => (None, false),
        Some(Some((m, f))) => (Some(m), f),
        Some(None) => (Some("no ready replicas".to_string()), false),
    };
    crd::ServiceStatus { name: name.into(), ready: ready >= 1, message, intercepted_by, proxy, unreachable_since, failing }
}


/// Waiting reasons that never clear on their own: the owner has to change the image, the code or
/// the config. Anything else (`ContainerCreating`, `PodInitializing`) is still on its way up.
const FAILING: &[&str] = &[
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
];

/// Why the pod is not ready, in the kubelet's own words, and whether it is [`FAILING`]. A waiting
/// container's `reason: message`, except a crash loop, whose back-off text says nothing: that one
/// reads the last termination instead, `crashing (exit N, R restarts): <its last log lines>` (the
/// container's `FallbackToLogsOnError` puts them there). Else a terminated last state's
/// `reason (exit N)`. `None` leaves the caller's generic message. Cut at 300 chars (on a char
/// boundary) because pull errors can run to pages and this lands in a CR status; a crash keeps the
/// LAST 600 chars of its log, where the error is.
fn not_ready_reason(pod: Option<&Pod>) -> Option<(String, bool)> {
    let cs = pod?.status.as_ref()?.container_statuses.as_ref()?;
    let cut = |t: String| t.chars().take(300).collect::<String>();
    cs.iter()
        .find_map(|c| {
            let w = c.state.as_ref()?.waiting.as_ref()?;
            let reason = w.reason.as_deref().unwrap_or("Waiting");
            let failing = FAILING.contains(&reason);
            if reason == "CrashLoopBackOff" {
                if let Some(t) = c.last_state.as_ref().and_then(|l| l.terminated.as_ref()) {
                    let head = format!("crashing (exit {}, {} restarts)", t.exit_code, c.restart_count);
                    let log = t.message.as_deref().map(str::trim).unwrap_or_default();
                    let n = log.chars().count();
                    let tail: String = log.chars().skip(n.saturating_sub(600)).collect();
                    return Some((if tail.is_empty() { head } else { format!("{head}: {tail}") }, true));
                }
            }
            Some((
                cut(match w.message.as_deref().filter(|m| !m.is_empty()) {
                    Some(m) => format!("{reason}: {m}"),
                    None => reason.to_string(),
                }),
                failing,
            ))
        })
        .or_else(|| {
            cs.iter().find_map(|c| {
                let t = c.last_state.as_ref()?.terminated.as_ref()?;
                Some((cut(format!("{} (exit {})", t.reason.as_deref().unwrap_or("Terminated"), t.exit_code)), false))
            })
        })
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
