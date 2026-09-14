//! Intercepts as the controller sees them: the wish in `spec.intercepts`, what is in force in
//! `status.services[].intercepted_by`, the grace a blip is given before the real service comes
//! back, and the clocks that grace is measured from. What an intercept RENDERS (the proxy pod,
//! the workspace-side target Service, both halves of the grant) is in `k8s::intercept`; this file
//! decides WHEN, applies it in `apply_intercept`, and takes it back in `converge_intercepts`.

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
pub enum Intercepting {
    /// Wished and decidable: the workspace is up and serving. Whether the switch actually
    /// COMPLETES this pass is `apply_intercept`'s answer — the proxy has to be Ready first.
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


/// The proxy state the LAST pass recorded for this service.
pub(crate) fn prev_proxy(prev: &crd::EnvironmentStatus, svc: &str) -> Option<String> {
    prev.service_status.iter().find(|s| s.name == svc)?.proxy.clone()
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
        // The two things `intercept_render` refuses, decided HERE for the same reason the port map
        // is: a bare reconcile error would retry forever with the real service already handed over
        // and nothing saying what to fix, where an `Off` leaves the service serving and the
        // condition names it. Their messages are the render's own.
        if ctx.intercept_proxy_image.is_empty() {
            let message = "no intercept proxy image is configured on this region's agent".to_string();
            plan.insert(&ic.service, Intercepting::Off { reason: "ProxyImageUnset", message, ws: None });
            continue;
        }
        if svc.ports.is_empty() {
            let message = format!("{} declares no ports, so there is nothing to intercept", svc.name);
            plan.insert(&ic.service, Intercepting::Off { reason: "NoPorts", message, ws: None });
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
    ic.ide_port_collision(&svc.ports)
        .map(|p| format!("port {p} would land on workspace port {}, the tool server's; map it elsewhere", k8s::IDE_PORT))
}


/// One intercept's fate, from the Workspace and its pod. Never errors: an unreadable answer is
/// `Keep`, which changes nothing at all.
pub async fn decide_intercept(ic: &crd::Intercept, env_name: &str, prev: &crd::EnvironmentStatus, ctx: &Arc<Ctx>) -> Intercepting {
    let off = |reason, message: String, w: Option<crd::Workspace>| Intercepting::Off { reason, message, ws: w.map(Box::new) };
    // From the cluster-wide cache, not a GET per pass (2026-09-12): the intercepting workspace may
    // be claimed by any node, so the controller's own node-scoped store cannot answer, and the
    // environment reconciles on every one of that workspace's transitions.
    //
    // A store that has NOT finished its first list is `Keep`, never `WorkspaceGone`: an empty cache
    // is "not known yet", and reading it as "gone" would scale the real service back up and drop
    // an intercept that is perfectly healthy.
    let Some(store) = ctx.workspaces() else {
        tracing::debug!(workspace = %ic.workspace, "store.not_ready");
        return Intercepting::Keep { since: None };
    };
    let w = match store.get(&kube::runtime::reflector::ObjectRef::new(&ic.workspace)) {
        Some(w) => (*w).clone(),
        None => return off("WorkspaceGone", format!("{} no longer exists", ic.workspace), None),
    };
    tracing::debug!(workspace = %ic.workspace, source = "store", "intercept.decided");
    if w.spec.desired_state == DesiredState::Stopped {
        return off("WorkspaceStopped", format!("{} is stopped", ic.workspace), Some(w));
    }
    // Attached means the workspace's SPACE uses this environment. An unlisted space cache is
    // `Keep`, for the same reason the workspace cache above is.
    match crate::controller::space::space_environment(ctx, &w.spec.owner, &w.spec.team, crd::retired_attach(&w.metadata, w.spec.attached_environment.as_deref())) {
        crate::controller::space::SpaceEnv::Unknown => return Intercepting::Keep { since: None },
        crate::controller::space::SpaceEnv::Known(Some(c)) if c.environment == env_name => {}
        crate::controller::space::SpaceEnv::Known(_) => {
            return off("WorkspaceDetached", format!("{} is in a space that does not use this environment", ic.workspace), Some(w));
        }
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


/// The workspace-side ports of every service `ws_id` is serving in force this pass — the same
/// `workspace_port` over the service's ports that each proxy pod forwards to, deduplicated.
pub(crate) fn intercepted_ports(e: &crd::Environment, plan: &std::collections::HashMap<&str, Intercepting>, ws_id: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = e
        .spec
        .services
        .iter()
        .filter(|s| matches!(plan.get(s.name.as_str()), Some(Intercepting::Force { ws, .. }) if ws.name_any() == ws_id))
        .filter_map(|s| e.spec.intercepts.iter().find(|ic| ic.service == s.name).map(|ic| (s, ic)))
        .flat_map(|(s, ic)| s.ports.iter().map(|p| ic.workspace_port(*p)))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}


/// Everything an intercept in force renders, and how far along it is: the workspace-side target
/// Service, the proxy Pod that stands in for the real service, and both halves of the grant —
/// which `allow_internet_egress` denies by default, so without them an intercept renders perfectly
/// and delivers nothing. Written from HERE and not from the workspace's own pass because the wish
/// is this object's, and this pass already holds the Workspace the workspace-side halves need.
///
/// The answer is what to record in `status.services[].proxy` and whether the switch may STAND:
/// the caller moves the selector onto the proxy only on `ready`, but leaves one already there
/// alone while `hold` says the proxy is merely coming back.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_intercept(
    e: &crd::Environment,
    svc: &model::Service,
    ic: &crd::Intercept,
    ws: &crd::Workspace,
    plan: &std::collections::HashMap<&str, Intercepting>,
    ns: &str,
    owner_ref: &OwnerReference,
    ctx: &Arc<Ctx>,
) -> Result<Proxy, ReconcileErr> {
    let ws_id = ws.name_any();
    let ws_ns = crd::ws_namespace(&ws.spec.owner, &ws.spec.team);
    let ws_ref = owner_ref_of_kind(ws)?;
    // The union over every service this workspace serves here: there is ONE target Service per
    // workspace and one ingress policy per workspace, and a sibling proxy dials the same object.
    let ports = intercepted_ports(e, plan, &ws_id);
    let render = k8s::intercept_render(k8s::RenderArgs {
        svc,
        ic,
        env_id: &e.name_any(),
        owner: &e.spec.owner,
        env_ref: owner_ref,
        ws_id: &ws_id,
        ws_ns: &ws_ns,
        ws_ref: &ws_ref,
        ws_ports: &ports,
        image: &ctx.intercept_proxy_image,
        runtime_class: ctx.runtime_class.as_deref(),
    })
    // Both refusals are decided as `Off` in `intercept_plan`, so reaching here with one is a bug,
    // not a configuration mistake.
    .map_err(ReconcileErr)?;

    // What the proxy dials, before the proxy: its readiness probe is the listener, not the dial,
    // but a proxy whose target does not resolve is a pass spent for nothing.
    ensure(&Api::<Service>::namespaced(ctx.client.clone(), &ws_ns), &render.target, ctx).await?;
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let name = k8s::proxy_pod_name(&svc.name);
    let existing = pods.get_opt(&name).await?;
    let Some(p) = existing else {
        // Created, never applied: a Pod is immutable, so a server-side apply of one that already
        // exists is a permanent error on every later pass. A 409 is a race with our own earlier
        // pass or with the kubelet, which is the desired state already reached.
        match pods.create(&kube::api::PostParams::default(), &render.pod).await {
            Ok(_) => {}
            Err(kube::Error::Api(st)) if st.code == 409 => {}
            Err(err) => return Err(err.into()),
        }
        ensure(&Api::<NetworkPolicy>::namespaced(ctx.client.clone(), ns), &render.egress, ctx).await?;
        ensure(&Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &ws_ns), &k8s::intercept_ingress(&ws_ns, ns, &ws_id, &ports, &e.spec.owner, &ws_ref), ctx).await?;
        // A pod created THIS pass has not had a chance to be anything yet, so it holds: the next
        // pass reads its own `Ready=False` and the grace runs from there.
        // ponytail: a create that keeps succeeding while the object keeps vanishing holds forever;
        // the bound is the same clock the grace uses, once there is a pod to read one off.
        return Ok(Proxy { state: "starting".into(), hold: true });
    };
    // The spec of an intercept is immutable while it runs; a change to ports or workspace is a new
    // pod, and this is where that becomes true. The IMAGE is compared too, because it is a Boot
    // setting: a rolled `WS_INTERCEPT_PROXY_IMAGE` otherwise reaches new intercepts only, and the
    // running ones keep forwarding through the old binary until somebody releases them by hand.
    let spec = |p: &Pod| {
        let c = p.spec.as_ref().and_then(|s| s.containers.first());
        (c.and_then(|c| c.args.clone()).unwrap_or_default(), c.and_then(|c| c.image.clone()).unwrap_or_default())
    };
    if spec(&p) != spec(&render.pod) {
        delete_ignoring_404(&pods, &name).await?;
        forget_applied(ctx, "Pod", ns, &name);
        tracing::info!(environment = %e.name_any(), service = %svc.name, "intercept.proxy.respawned");
        // A recreate this controller CHOSE is not an outage to hand the service back over: the
        // replacement is one pass away, and handing back would cost a scale-up and a re-take.
        return Ok(Proxy { state: "starting".into(), hold: true });
    }
    ensure(&Api::<NetworkPolicy>::namespaced(ctx.client.clone(), ns), &render.egress, ctx).await?;
    ensure(&Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &ws_ns), &k8s::intercept_ingress(&ws_ns, ns, &ws_id, &ports, &e.spec.owner, &ws_ref), ctx).await?;
    Ok(proxy_state(&p, &e.name_any(), &svc.name))
}


/// How far along one proxy pod is, and whether a service already behind it should stay there.
pub(crate) struct Proxy {
    /// `status.services[].proxy`: `starting`, `ready` or `failed`.
    pub state: String,
    /// The switch may STAND — this is not an outage worth handing the service back for. `false`
    /// only where waiting is pointless (the pod failed, or cannot start) or has gone on too long.
    pub hold: bool,
}


/// The one thing that cannot start on its own: a pod whose image will not pull sits in
/// `ImagePullBackOff` forever, and reported as `starting` it reads as "any second now" while the
/// real service stays up and nobody knows why the intercept never takes.
///
/// A pod that is merely NotReady holds for `INTERCEPT_GRACE_SECS`, dated from its own
/// `Ready=False` transition exactly as the workspace's grace is: an ordinary proxy restart would
/// otherwise cost a StatefulSet scale-up and a re-take — two real gaps — for one pod that is back
/// in seconds.
fn proxy_state(p: &Pod, env: &str, service: &str) -> Proxy {
    let (ready, since) = pod_ready(p);
    if ready {
        return Proxy { state: "ready".into(), hold: true };
    }
    let status = p.status.as_ref();
    let stuck = status
        .and_then(|s| s.container_statuses.as_ref())
        .into_iter()
        .flatten()
        .filter_map(|c| c.state.as_ref()?.waiting.as_ref())
        .find(|w| matches!(w.reason.as_deref(), Some("ImagePullBackOff" | "ErrImagePull" | "CreateContainerError")));
    if status.and_then(|s| s.phase.as_deref()) == Some("Failed") || stuck.is_some() {
        // The pod's own message, which is the only place the real reason (a private registry, a
        // typo'd tag) is written down.
        let why = stuck
            .and_then(|w| w.message.clone().or_else(|| w.reason.clone()))
            .or_else(|| status.and_then(|s| s.message.clone()))
            .unwrap_or_else(|| "the proxy pod failed".into());
        tracing::warn!(environment = %env, service = %service, reason = %why, "intercept.proxy.failed");
        return Proxy { state: "failed".into(), hold: false };
    }
    // Same shape as the workspace grace, including its rule about a clock that runs backwards:
    // expired is the safe direction, because a hold that never expires is a service that never
    // comes back. No clock at all is a pod too young to have stamped one — it holds.
    let waited = since.map(|t| k8s_openapi::jiff::Timestamp::now().as_second() - t);
    Proxy { state: "starting".into(), hold: waited.is_none_or(|w| (0..INTERCEPT_GRACE_SECS).contains(&w)) }
}


/// Everything an intercept that is no longer in force leaves behind, taken back in one sweep: the
/// proxy Pod and its egress grant per (workspace, service), and — only once NO service of that
/// workspace is still in force — the workspace-side target Service and ingress grant, which are
/// per workspace.
///
/// Called AFTER every service's selector and replicas are back, never before: the mirror of the
/// order `apply_services` takes an intercept in, and for the same reason — the service must never
/// be without a ready endpoint in either direction.
pub(crate) async fn converge_intercepts(
    ns: &str,
    prev: &crd::EnvironmentStatus,
    plan: &std::collections::HashMap<&str, Intercepting>,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    let here: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), ns);
    // Keyed by (workspace, SERVICE), because the environment-side grant is per service now: a
    // workspace serving `a` and releasing `b` is in force for one pair and stale for the other, and
    // a workspace-keyed set would have skipped the release and leaked `intercept-{ws}-b` forever.
    let mut in_force: std::collections::HashSet<(String, String)> = Default::default();
    // A `Keep` renders what the last pass rendered — and that includes its grants: the workspace
    // the status names is still being served, so its policies are in force, not stale. Deleting
    // them on an API blip cut the intercepted traffic the rendering was keeping (2026-09-12).
    for (svc, d) in plan {
        if matches!(d, Intercepting::Keep { .. }) {
            if let Some(by) = prev.service_status.iter().find(|s| s.name == *svc).and_then(|s| s.intercepted_by.clone()) {
                in_force.insert((by, (*svc).to_string()));
            }
        }
    }
    // A `Force` holds its grants whatever `apply_intercept` answered: a proxy still starting is
    // an intercept being taken, not one being released, and sweeping it would delete the pod this
    // same pass created.
    for (svc, d) in plan {
        let Intercepting::Force { ws, .. } = d else { continue };
        in_force.insert((ws.name_any(), (*svc).to_string()));
    }
    // Every workspace this environment could still be holding a grant open for: one it wishes for
    // and is not serving, and one the LAST pass recorded as in force — which is the ordinary
    // release, where `/v1` has taken the wish out of spec and status is the only record left.
    // ponytail: a grant whose wish AND whose status record are both gone (a release that raced a
    // lost status write) is left until the Environment is deleted, which collects it; a label
    // selector over the namespace's policies is the upgrade path.
    let mut stale: Vec<(String, String, Option<crd::Workspace>)> = Vec::new();
    for (svc, d) in plan {
        if let Intercepting::Off { ws: Some(w), .. } = d {
            stale.push((w.name_any(), (*svc).to_string(), Some((**w).clone())));
        }
    }
    for s in &prev.service_status {
        if let Some(by) = &s.intercepted_by {
            stale.push((by.clone(), s.name.clone(), None));
        }
    }
    // The workspace-side half is still ONE ingress per workspace (the union of its ports), so it
    // may only be deleted once NO service of that workspace is in force.
    let ws_in_force: std::collections::HashSet<&String> = in_force.iter().map(|(w, _)| w).collect();
    for (id, svc, ws) in stale {
        if in_force.contains(&(id.clone(), svc.clone())) {
            continue;
        }
        delete_ignoring_404(&here, &k8s::intercept_egress_name(&id, &svc)).await?;
        forget_applied(ctx, "NetworkPolicy", ns, &k8s::intercept_egress_name(&id, &svc));
        // The proxy is per SERVICE, like the egress grant beside it: the pod that stood in for
        // this one service has nothing left to forward.
        let proxy = k8s::proxy_pod_name(&svc);
        delete_ignoring_404(&Api::<Pod>::namespaced(ctx.client.clone(), ns), &proxy).await?;
        forget_applied(ctx, "Pod", ns, &proxy);
        // The workspace-side half lives in a namespace only the Workspace itself can name, so a
        // release recorded in status alone costs one GET to find it. Worth it: the ingress rule
        // opens this environment's whole namespace to that pod, and it would otherwise sit there
        // until somebody deleted the workspace. One pass only — the next has no record to clean.
        let ws = match ws {
            Some(w) => Some(w),
            // An API error is not "gone": read as gone, the workspace-side policy that opens this
            // environment's namespace to that pod would never be deleted, and this pass is the
            // one that had a record to clean (2026-09-12). The error keeps the record for a retry.
            // The cache when it is ready; the GET only while it is not (2026-09-12). Falling back
            // rather than skipping keeps the one pass that has a record to clean from being the
            // pass that ran during a relist.
            None => match ctx.workspaces() {
                Some(store) => store.get(&kube::runtime::reflector::ObjectRef::new(&id)).map(|w| (*w).clone()),
                None => Api::<crd::Workspace>::all(ctx.client.clone()).get_opt(&id).await.map_err(|e| ReconcileErr(e.to_string()))?,
            },
        };
        // A workspace that is GONE takes its half with it: both objects are ownerReferenced.
        if let Some(w) = ws.filter(|_| !ws_in_force.contains(&id)) {
            let ws_ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
            let in_ws: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ws_ns);
            delete_ignoring_404(&in_ws, &k8s::intercept_policy_name(&id)).await?;
            forget_applied(ctx, "NetworkPolicy", &ws_ns, &k8s::intercept_policy_name(&id));
            // Per workspace for the same reason the ingress is: one Service carries the union of
            // the ports every proxy of this workspace dials.
            let target = k8s::target_service_name(&id);
            delete_ignoring_404(&Api::<Service>::namespaced(ctx.client.clone(), &ws_ns), &target).await?;
            forget_applied(ctx, "Service", &ws_ns, &target);
        }
    }
    Ok(())
}
