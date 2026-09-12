//! Intercepts as the controller sees them: the wish in `spec.intercepts`, what is in force in
//! `status.services[].intercepted_by`, the grace a blip is given before the real service comes
//! back, and the clocks that grace is measured from. The wiring itself (scale to 0, selector
//! off, EndpointSlice) is in `k8s`; this file decides WHEN.

use super::*;


/// How long the intercepting workspace's pod may be unreachable before the real service comes
/// back up.
///
/// A pod restarting is unreachable for a few seconds, and bouncing the real StatefulSet up and
/// down around every restart would be worse than the gap. Stopped, deleted and detached are NOT
/// graced: each is a deliberate act, observed as itself rather than as an absence.
pub(crate) const INTERCEPT_GRACE_SECS: i64 = 30;


/// What this pass decided about one intercept — the spec's three states, plus the one thing an
/// unreadable API answer is allowed to do, which is nothing.
pub(crate) enum Intercepting {
    /// In force: the real service off, the slice pointing at this pod.
    Force { ws: Box<crd::Workspace>, pod_ip: String },
    /// Either nothing is known (an API error) or the pod has not been unreachable long enough.
    /// Render what the last pass rendered and look again — a blip in the API server must never
    /// flap a service, and an unreadable answer is not evidence of anything.
    ///
    /// `since` is when the outage began, carried out so the status write keeps it: `None` means
    /// this pass learned nothing (an API error), so whatever was recorded stands unchanged.
    Keep { since: Option<i64> },
    /// Not in force. The wish STAYS in spec; the rendering goes back to the ordinary one and the
    /// condition says why.
    Off { reason: &'static str, message: String, ws: Option<Box<crd::Workspace>> },
}


/// A pod's `Ready` truth and the instant it last changed, in one read.
pub(crate) fn pod_ready(p: &Pod) -> (bool, Option<i64>) {
    p.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .find(|c| c.type_ == "Ready")
        .map_or((false, None), |c| (c.status == "True", c.last_transition_time.as_ref().map(|t| t.0.as_second())))
}


/// When the workspace itself last said it was NOT ready — never when it said it was. A `Ready=True`
/// workspace whose pod has merely gone dates nothing about the outage; see `decide_intercept`.
/// The moment the outage began, in the order the clocks are worth trusting: the pod's own
/// `Ready=False`, then the Workspace's, then what a previous pass of ours recorded, then now.
///
/// Split out because the three-way fallback is the whole correctness of the grace and the caller
/// around it needs a live cluster to reach.
pub(crate) fn outage_since(pod: Option<i64>, workspace: Option<i64>, recorded: Option<i64>, now: i64) -> i64 {
    pod.or(workspace).or(recorded).unwrap_or(now)
}


pub(crate) fn not_ready_since(conds: &[Condition]) -> Option<i64> {
    let c = conds.iter().find(|c| c.type_ == "Ready")?;
    (c.status == "False").then(|| c.last_transition_time.0.as_second())
}


/// What the LAST pass rendered for this service, read off the one record of it.
pub(crate) fn prev_intercepted_by(prev: &crd::EnvironmentStatus, svc: &str) -> Option<String> {
    prev.service_status.iter().find(|s| s.name == svc)?.intercepted_by.clone()
}


pub(crate) fn was_intercepted(prev: &crd::EnvironmentStatus, svc: &str) -> bool {
    prev_intercepted_by(prev, svc).is_some()
}


/// One `Intercepted` condition for the whole environment: the first intercept that is NOT in
/// force, since that is the one somebody has to act on, and otherwise that they are.
///
/// ponytail: one condition for every intercept, so a second not-in-force intercept is invisible
/// until the first is dealt with; a per-service condition type is the upgrade path.
pub(crate) fn intercept_condition(plan: &std::collections::HashMap<&str, Intercepting>, gen: i64) -> Option<Condition> {
    if plan.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = plan.keys().copied().collect();
    names.sort_unstable();
    for n in &names {
        if let Some(Intercepting::Off { reason, message, .. }) = plan.get(n) {
            return Some(crd::condition("Intercepted", false, reason, &format!("{n}: {message}"), gen));
        }
    }
    // `Keep` is the absence of a decision — an unreadable answer, or a grace still running — so it
    // states nothing. Claiming `InForce` there would be an affirmative falsehood on the very first
    // pass over a fresh wish that hit an API error, when nothing has been rendered at all.
    // `status.services[].intercepted_by` is the per-service truth and is correct either way.
    if plan.values().any(|d| matches!(d, Intercepting::Keep { .. })) {
        return None;
    }
    Some(crd::condition("Intercepted", true, "InForce", &format!("intercepted: {}", names.join(", ")), gen))
}


/// The wish per service (first entry wins) and what this pass decided about each.
///
/// Keyed by service name, and a wish naming a service this environment does not declare is
/// dropped: `/v1` refuses one, and a hand-edited object must not make the controller flap.
#[allow(clippy::type_complexity)]
pub(crate) async fn intercept_plan<'a>(
    e: &'a crd::Environment,
    prev: &crd::EnvironmentStatus,
    ctx: &Arc<Ctx>,
) -> (std::collections::HashMap<&'a str, &'a crd::Intercept>, std::collections::HashMap<&'a str, Intercepting>) {
    let mut wishes: std::collections::HashMap<&str, &crd::Intercept> = std::collections::HashMap::new();
    let mut plan: std::collections::HashMap<&str, Intercepting> = std::collections::HashMap::new();
    for ic in &e.spec.intercepts {
        let Some(svc) = e.spec.services.iter().find(|s| s.name == ic.service) else { continue };
        if wishes.contains_key(ic.service.as_str()) {
            continue;
        }
        wishes.insert(&ic.service, ic);
        // A port rewrite is checked HERE, before anything renders it (2026-09-12). `k8s`'s slice
        // matches the service's port to the workspace's by the port name `p{port}`, so a rewrite
        // naming a port the service does not declare silently produces a slice that matches
        // nothing — the real service scaled to 0 and the traffic delivered nowhere. Settled `Off`
        // with the reason instead, which leaves the real service up and says why.
        if let Some(bad) = invalid_port_map(svc, ic) {
            plan.insert(&ic.service, Intercepting::Off { reason: "PortsInvalid", message: bad, ws: None });
            continue;
        }
        plan.insert(&ic.service, decide_intercept(ic, &e.name_any(), prev, ctx).await);
    }
    (wishes, plan)
}


/// The first `ports` entry this service cannot honour, as the message to put in the condition.
/// `0` is not a port on either side, and a `service` port the service does not declare has nothing
/// to rewrite.
pub(crate) fn invalid_port_map(svc: &model::Service, ic: &crd::Intercept) -> Option<String> {
    for p in &ic.ports {
        if p.service == 0 || p.workspace == 0 {
            return Some(format!("port rewrite {}->{} is not a port", p.service, p.workspace));
        }
        if !svc.ports.contains(&p.service) {
            return Some(format!("{} does not listen on {}", svc.name, p.service));
        }
    }
    None
}


/// One intercept's fate, from the Workspace and its pod. Never errors: an unreadable answer is
/// `Keep`, which changes nothing at all.
pub(crate) async fn decide_intercept(ic: &crd::Intercept, env_name: &str, prev: &crd::EnvironmentStatus, ctx: &Arc<Ctx>) -> Intercepting {
    let off = |reason, message: String, w: Option<crd::Workspace>| Intercepting::Off { reason, message, ws: w.map(Box::new) };
    let w = match Api::<crd::Workspace>::all(ctx.client.clone()).get_opt(&ic.workspace).await {
        Ok(Some(w)) => w,
        Ok(None) => return off("WorkspaceGone", format!("{} no longer exists", ic.workspace), None),
        Err(_) => return Intercepting::Keep { since: None },
    };
    if w.spec.desired_state == DesiredState::Stopped {
        return off("WorkspaceStopped", format!("{} is stopped", ic.workspace), Some(w));
    }
    // `spec` only, never `crd::attached_environment`'s condition fallback: that reads back a
    // DETACHED workspace's last attachment, which is the one answer this must not accept.
    if w.spec.attached_environment.as_deref() != Some(env_name) {
        return off("WorkspaceDetached", format!("{} is not attached to this environment", ic.workspace), Some(w));
    }
    // `podRef` is `{namespace}/{name}`, written that way by the workspace's own controller — the
    // whole string is not a pod name, and passing it as one is a request the API server rejects
    // outright, which reads here as an unreadable answer and holds forever. `bins/gateway`'s
    // `resolve.rs` splits it the same way; it is the one shape this field ever has.
    let pod = match w.status.as_ref().and_then(|s| s.pod_ref.clone()) {
        Some(pod_ref) => {
            let Some((pod_ns, name)) = pod_ref.split_once('/') else {
                return off("PodRefMalformed", format!("{}'s podRef is not namespace/name", ic.workspace), Some(w));
            };
            match Api::<Pod>::namespaced(ctx.client.clone(), pod_ns).get_opt(name).await {
                Ok(p) => p,
                Err(_) => return Intercepting::Keep { since: None },
            }
        }
        None => None,
    };
    let ready = pod.as_ref().map(pod_ready);
    let ip = pod.as_ref().and_then(|p| p.status.as_ref()?.pod_ip.clone());
    // Read live and never stored: a pod IP changes on every recreate, and a stale one in status is
    // a wrong answer that looks right — the same rule `bins/gateway/src/resolve.rs` already states.
    if let (Some((true, _)), Some(ip)) = (ready, ip) {
        return Intercepting::Force { ws: Box::new(w), pod_ip: ip };
    }
    // The clock is the POD's own `Ready` condition, or — with no pod at all — the Workspace's, and
    // the workspace's only while it SAYS it is not ready. Neither is a field this controller
    // invented, and both are stamped by whoever observed the transition, so the grace measures the
    // real outage rather than this pass's first sight of it.
    //
    // The `Ready == "False"` guard is the whole grace. A workspace that has been `Ready=True` for
    // ten minutes and has just lost its pod would otherwise date the outage from when it CAME UP,
    // yielding `waited = 600` and an immediate fallback — in exactly the ordinary pod restart the
    // grace exists to ride out.
    //
    // With NO clock anywhere this pass stamps one ITSELF and keeps it in `unreachable_since`. That
    // shape is a workspace whose node died: no pod object, and no controller of its own left to
    // stamp `Ready=False`, so no event will ever arrive and no other object dates the outage.
    // Borrowing one that does exist is worse than having none — the `Intercepted` condition dates
    // the intercept's last state CHANGE, so an intercept in force since morning reads as an outage
    // hours old and skips the grace on the very first pass.
    let now = k8s_openapi::jiff::Timestamp::now().as_second();
    let since = outage_since(
        // Only a `Ready=False` pod dates anything — the same rule `not_ready_since` applies to
        // the workspace. A `Ready=True` pod reaches here only with no IP yet, and its transition
        // time is when it came UP, which would expire the grace on the spot.
        ready.and_then(|(ok, t)| if ok { None } else { t }),
        w.status.as_ref().and_then(|st| not_ready_since(&st.conditions)),
        prev.service_status.iter().find(|s| s.name == ic.service).and_then(|s| s.unreachable_since),
        now,
    );
    let waited = now - since;
    // A NEGATIVE wait is a node whose clock runs ahead of whoever stamped the condition. Held, it
    // would hold forever; expired, the real service comes back. Expired is the safe direction.
    if (0..INTERCEPT_GRACE_SECS).contains(&waited) {
        return Intercepting::Keep { since: Some(since) };
    }
    off("PodUnreachable", format!("{}'s pod has been unreachable for {waited}s", ic.workspace), Some(w))
}


/// The environment → workspace direction, which `allow_internet_egress` denies by default: without
/// this pair an in-force intercept renders perfectly and delivers nothing.
///
/// Written from HERE and not from the workspace's own pass because the wish is this object's, and
/// this pass already holds the Workspace the workspace-side half needs.
pub(crate) async fn intercept_policies(
    e: &crd::Environment,
    ns: &str,
    prev: &crd::EnvironmentStatus,
    plan: &std::collections::HashMap<&str, Intercepting>,
    owner_ref: &OwnerReference,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    let here: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), ns);
    let mut in_force: std::collections::HashSet<String> = Default::default();
    // A `Keep` renders what the last pass rendered — and that includes its grants: the workspace
    // the status names is still being served, so its policies are in force, not stale. Deleting
    // them on an API blip cut the intercepted traffic the rendering was keeping (2026-09-12).
    for (svc, d) in plan {
        if matches!(d, Intercepting::Keep { .. }) {
            if let Some(by) = prev.service_status.iter().find(|s| s.name == *svc).and_then(|s| s.intercepted_by.clone()) {
                in_force.insert(by);
            }
        }
    }
    for d in plan.values() {
        let Intercepting::Force { ws, .. } = d else { continue };
        let ws_ns = crd::ws_namespace(&ws.spec.owner, &ws.spec.team);
        in_force.insert(ws.name_any());
        ensure(&here, &k8s::intercept_egress(ns, &ws_ns, &ws.name_any(), &e.spec.owner, owner_ref), ctx).await?;
        // The workspace-side half cannot be owned by this Environment: an ownerReference may not
        // cross namespaces. Owned by the Workspace instead, exactly as the attach pair splits.
        let in_ws: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ws_ns);
        let ws_ref = owner_ref_of_kind(&**ws)?;
        ensure(&in_ws, &k8s::intercept_ingress(&ws_ns, ns, &ws.name_any(), &e.spec.owner, &ws_ref), ctx).await?;
    }
    // Every workspace this environment could still be holding a grant open for: one it wishes for
    // and is not serving, and one the LAST pass recorded as in force — which is the ordinary
    // release, where `/v1` has taken the wish out of spec and status is the only record left.
    // ponytail: a grant whose wish AND whose status record are both gone (a release that raced a
    // lost status write) is left until the Environment is deleted, which collects it; a label
    // selector over the namespace's policies is the upgrade path.
    let mut stale: Vec<(String, Option<crd::Workspace>)> = Vec::new();
    for d in plan.values() {
        if let Intercepting::Off { ws: Some(w), .. } = d {
            stale.push((w.name_any(), Some((**w).clone())));
        }
    }
    for s in &prev.service_status {
        if let Some(by) = &s.intercepted_by {
            stale.push((by.clone(), None));
        }
    }
    for (id, ws) in stale {
        if in_force.contains(&id) {
            continue;
        }
        delete_ignoring_404(&here, &k8s::intercept_policy_name(&id)).await?;
        forget_applied(ctx, "NetworkPolicy", ns, &k8s::intercept_policy_name(&id));
        // The workspace-side half lives in a namespace only the Workspace itself can name, so a
        // release recorded in status alone costs one GET to find it. Worth it: the ingress rule
        // opens this environment's whole namespace to that pod, and it would otherwise sit there
        // until somebody deleted the workspace. One pass only — the next has no record to clean.
        let ws = match ws {
            Some(w) => Some(w),
            // An API error is not "gone": read as gone, the workspace-side policy that opens this
            // environment's namespace to that pod would never be deleted, and this pass is the
            // one that had a record to clean (2026-09-12). The error keeps the record for a retry.
            None => Api::<crd::Workspace>::all(ctx.client.clone()).get_opt(&id).await.map_err(|e| ReconcileErr(e.to_string()))?,
        };
        // A workspace that is GONE takes its half with it: the policy is ownerReferenced.
        if let Some(w) = ws {
            let ws_ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
            let in_ws: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ws_ns);
            delete_ignoring_404(&in_ws, &k8s::intercept_policy_name(&id)).await?;
            forget_applied(ctx, "NetworkPolicy", &ws_ns, &k8s::intercept_policy_name(&id));
        }
    }
    Ok(())
}
