//! The running half of an environment reconcile: namespace, quotas, policies, one StatefulSet
//! per service, the restore gate that waits for the grafted snapshot, and the drain that empties
//! services before a stop or a decommission.

use super::*;


/// Converge the environment's namespace, storage and services against spec, and report what the
/// StatefulSets actually say about themselves.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_environment(
    e: &crd::Environment,
    vol: &crd::Volume,
    ns: &str,
    deployments: &Api<StatefulSet>,
    owner_ref: &OwnerReference,
    prev: crd::EnvironmentStatus,
    gen: i64,
    // F7: whether THIS node carries the decommission label, read off `apply_environment`'s one
    // Node GET. The running arm is where it has to be written — see `with_drain_notice`.
    decommissioning: bool,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr> {
    let id = vol.name_any();
    // The worktree is the environment's OWN name on whatever volume it resolved to — `id` for an
    // environment that owns its volume (the same string), the SOURCE's volume for a restored one,
    // which holds a SECOND worktree of it. Never `(id, id)`: that checked a restored environment
    // out on top of the source's live worktree, two environments writing one subvolume. It is also
    // the name `sync.rs`'s `live_worktrees` writes into `Snapshot.spec.worktree`, so every path
    // below — checkout, mount, mkdir, stop cut, drop — uses this one string.
    let wt = e.name_any();
    let mut prev = prev;
    if let Some(action) = materialise(e, vol, &wt, &mut prev, gen, owner_ref, ctx).await? {
        return Ok(action);
    }
    ensure_fabric(e, ns, owner_ref, prev.observed_generation != Some(gen), ctx).await?;
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &vol.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: ctx.runtime_class.as_deref(),
        default_image: &ctx.default_image,
        system: e.spec.system.as_deref(),
        registry_host: &ctx.registry_host,
    };
    ensure_mounts(&id, &wt, &e.spec.services, ctx).await?;
    if let Some(action) = capacity_gate(e, deployments, &prev, gen, ctx).await? {
        return Ok(action);
    }
    // Every intercept decided BEFORE anything is rendered: each service is in exactly one of the
    // three states below, and the rendering is a straight read of that decision.
    let (wishes, plan) = intercept_plan(e, &prev, ctx).await;
    apply_services(e, ns, &id, &pod_ctx, deployments, &prev, &wishes, &plan, owner_ref, ctx).await?;
    let service_status = read_services_back(e, deployments, &prev, &plan).await?;
    let (st, all_ready) = running_status(e, &prev, service_status, &id, &plan, decommissioning, gen);
    write_env_status(e, st, ctx).await?;
    // A held intercept has to be looked at again: nothing woke us for the grace running out.
    let holding = plan.values().any(|d| matches!(d, Intercepting::Keep { .. }));
    Ok(if all_ready && !holding { Action::await_change() } else { Action::requeue(TICK) })
}

/// Phase 1: the worktree — migration, head, checkout, quota — and the first graft's head write.
/// `Some(action)` means the environment is parked and the caller returns it.
async fn materialise(
    e: &crd::Environment,
    vol: &crd::Volume,
    wt: &str,
    prev: &mut crd::EnvironmentStatus,
    gen: i64,
    owner_ref: &OwnerReference,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    // Same worktree materialization a workspace does before any pod is built, and the same
    // HeadUnknown guard: an environment claimed onto this node for a volume with snapshots but no
    // recorded head yet must wait for Task 5/6 to write one rather than checking out empty next
    // to real history. Task 4 left this arm to this task — see `apply_workspace`'s twin.
    // Lazy per-volume migration, resolve the effective head, checkout and quota — identical for a
    // Workspace and an Environment down to the guard conditions; see `worktree_gate`.
    let gate = super::super::worktree_gate(
        wt,
        "Environment",
        vol,
        &e.spec.storage,
        prev.head.as_deref(),
        &e.spec.owner,
        owner_ref.clone(),
        crd::SnapshotState::of_environment(e),
        ctx,
    )
    .await?;
    match gate {
        super::super::WorktreeGate::Wait { reason: "NoSuchSnapshot", message, .. } => {
            // Permanent: only the caller can settle it, since `settle` needs the object itself.
            let prev = prev.clone();
            return Ok(Some(settle(
                Outcome::Permanent(message, "NoSuchSnapshot"),
                e,
                "Environment",
                gen,
                move |cond| {
                    serde_json::json!({
                        "phase": crd::Phase::Error,
                        "nodeName": prev.node_name,
                        "volumeRef": prev.volume_ref,
                        "conditions": kept_conditions(&prev.conditions, cond),
                    })
                },
                ctx,
            )
            .await?));
        }
        // `HeadUnknown` REPLACES conditions rather than keeping — matching the pre-extraction
        // code exactly; only `SnapshotPending` (and the settled `NoSuchSnapshot` above) keep them.
        super::super::WorktreeGate::Wait { reason: "HeadUnknown", message, action } => {
            let st = crd::EnvironmentStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                conditions: vec![crd::condition("Ready", false, "HeadUnknown", &message, gen)],
                ..prev.clone()
            };
            write_env_status(e, st, ctx).await?;
            return Ok(Some(action));
        }
        super::super::WorktreeGate::Wait { reason, message, action } => {
            let st = crd::EnvironmentStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                conditions: kept_conditions(&prev.conditions, crd::condition("Ready", false, reason, &message, gen)),
                ..prev.clone()
            };
            write_env_status(e, st, ctx).await?;
            return Ok(Some(action));
        }
        super::super::WorktreeGate::Ready => {}
    }
    // First graft: nothing else will ever write this head — a restored environment gets
    // `advance_head` only if it pushes itself. The preserve pattern, same as the workspace's.
    if prev.head.is_none() {
        if let Some(commit) = super::super::clone_commit(&e.spec.storage) {
            let prev2 = prev.clone();
            write_env_status(e, crd::EnvironmentStatus { head: Some(commit.to_string()), ..prev2 }, ctx).await?;
            prev.head = Some(commit.to_string());
        }
    }

    Ok(None)
}

/// Phase 2: everything the namespace needs before a pod: the namespace itself, the network
/// policies, the api's pull-credential grant, the per-container ceiling and the namespace total.
/// `changed` is "this pass is not a re-look at an already-observed generation". The namespace, the
/// policies, the grant and the LimitRange are rendered from spec alone, so on a converged
/// environment they were re-rendered and re-hashed on every child event for nothing (2026-09-12).
/// The ResourceQuota at the bottom is deliberately NOT gated: its input is the owner's `Quota` CR,
/// which changes without this environment's generation moving, and nothing else writes one for an
/// `env-` namespace (`binding.rs` covers only `ws-`).
async fn ensure_fabric(
    e: &crd::Environment,
    ns: &str,
    owner_ref: &OwnerReference,
    changed: bool,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    if changed {
        ensure(
            &Api::<Namespace>::all(ctx.client.clone()),
            &{
                let mut n = k8s::namespace(ns, &e.spec.owner, "environment", Some(owner_ref));
                // The pod fence (`deploy/k3s/workspace-admission.yaml`) admits `builder_hardened()`'s
                // wider capability list only in a namespace carrying this label. On the NAMESPACE, not
                // the pod: only this controller writes namespaces, while anything with pod create in a
                // tenant namespace could stamp a pod label and widen its own fence.
                if let Some(sys) = e.spec.system.as_deref() {
                    n.metadata.labels.get_or_insert_default().insert(crd::SYSTEM_LABEL.into(), sys.into());
                }
                n
            },
            ctx,
        )
        .await?;
        let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), ns);
        for p in k8s::default_policies(ns, &e.spec.owner, owner_ref) {
            ensure(&policies, &p, ctx).await?;
        }
        // Only the hidden per-owner builder environment gets this hole: the gate is the only thing
        // that may reach a builder's buildkit service, and an ordinary environment has no buildkit
        // service for it to reach.
        if e.spec.system.as_deref() == Some(crd::BUILDER_SYSTEM) {
            ensure(&policies, &k8s::builder_gate_ingress(ns, &e.spec.owner, owner_ref), ctx).await?;
        }
        // An environment's services are the likeliest place a private image appears, so this namespace
        // needs the same scoped grant a workspace namespace gets — the API writes the pull credential
        // here, and nowhere it has not been vouched for.
        ensure(
            &Api::<RoleBinding>::namespaced(ctx.client.clone(), ns),
            &k8s::api_secret_binding(ns, &e.spec.owner, API_SERVICE_ACCOUNT, API_NAMESPACE, None),
            ctx,
        )
        .await?;
        // The ceiling the services render under — the env unit, or the largest shape a service names
        // (the builder). Owned by the Environment — this namespace holds exactly one.
        ensure(
            &Api::<LimitRange>::namespaced(ctx.client.clone(), ns),
            &k8s::limit_range(ns, &e.spec.owner, "environment", &k8s::env_limit_resources(&e.spec.services), Some(owner_ref)),
            ctx,
        )
        .await?;
    }
    // The same ceiling, in the environment's own namespace: an environment's services are its
    // owner's capacity too, and the namespace is where Kubernetes can enforce it.
    //
    // `EnvironmentSpec` alone cannot say whether `owner` is a team or a person — it is one string,
    // "usually" a team slug but not always, and the agent has no directory to ask either way. The
    // binding reconciler already answers this for every owner it has ever seen (`is_team_owner`,
    // stamped onto `OwnerBinding.status.team`), so read THAT instead of guessing here. A binding
    // not yet reconciled (this owner's very first object) falls back to `false`: the smaller,
    // conservative table, logged once so a quietly-wrong ceiling is at least visible.
    let team = match crate::binding::get_binding(ctx, &e.spec.region, &e.spec.owner).await? {
        Some(b) => b.status.map(|s| s.team).unwrap_or(false),
        None => {
            tracing::info!(owner = %e.spec.owner, reason = "no-ownerbinding", "quota.defaulted");
            false
        }
    };
    let q = kloudlite_workspaces::quota::effective(&ctx.client, &e.spec.owner, team).await?;
    ensure(
        &Api::<ResourceQuota>::namespaced(ctx.client.clone(), ns),
        &k8s::resource_quota(ns, &e.spec.owner, "environment", &q),
        ctx,
    )
    .await?;
    Ok(())
}

/// Phase 3a: every declared mount folder, made inside the worktree on a blocking thread.
async fn ensure_mounts(id: &str, wt: &str, services: &[model::Service], ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    // Every declared folder must exist before a subPath binds it — and `validate_mount` here is a
    // security check, not a formality: `create_dir_all` on an unvalidated folder is itself the
    // escape, mkdir -p'ing outside the subvolume before a pod ever starts.
    // On a blocking thread: `create_dir_all` is sync IO, and the pool can be a network-backed or
    // busy disk. Same rule the module doc states for the btrfs work.
    // The worktree, not `live/` itself: the pod mounts the worktree and binds `volumes/{folder}`
    // as a subPath INSIDE it, so a folder made one level up is invisible to every service.
    let live = ctx.engine.pool.worktree(id, wt);
    let services = services.to_vec();
    tokio::task::spawn_blocking(move || mkdir_env_mounts(&live, &services))
        .await
        .map_err(|e| ReconcileErr(format!("mkdir panicked: {e}")))?
        .map_err(ReconcileErr)?;

    Ok(())
}

/// Phase 3b: the start-time capacity gate. `Some(action)` parks the environment at `NoCapacity`.
async fn capacity_gate(
    e: &crd::Environment,
    deployments: &Api<StatefulSet>,
    prev: &crd::EnvironmentStatus,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    // The same start-time capacity gate the workspace reconciler runs, for the same reason: the
    // claim checked capacity once, and an environment restarting onto its own node never reaches
    // that check at all. Only before the FIRST StatefulSet exists — once they are applied their
    // capacity is spent, and refusing here would strand a running environment at `Creating`.
    // An environment is all-or-nothing on purpose: its services share a namespace and a volume,
    // and half of them scheduled is not a working environment.
    // "Not started yet" is read off the FIRST service's StatefulSet rather than a list of the
    // namespace: the services are applied in order in the loop below, so the first one existing
    // means this environment has already been started here and its capacity is already spent.
    let first = e.spec.services.first().map(|s| s.name.clone());
    let started = match &first {
        Some(name) => deployments.get_opt(name).await?.is_some(),
        // No services at all: nothing to schedule, so nothing to gate.
        None => true,
    };
    if !started
        && !crate::claim::room_to_start(ctx, &e.name_any(), crate::claim::environment_want(e.spec.services.len())).await?
    {
        let st = crd::EnvironmentStatus {
            phase: crd::Phase::Creating,
            observed_generation: None,
            conditions: kept_conditions(
                &prev.conditions,
                crd::condition(
                    "Ready",
                    false,
                    "NoCapacity",
                    &format!("{} has no room for this environment right now; it starts as soon as room frees up", ctx.node),
                    gen,
                ),
            ),
            ..prev.clone()
        };
        write_env_status(e, st, ctx).await?;
        return Ok(Some(Action::requeue(TICK)));
    }
    Ok(None)
}

/// Phase 4: one StatefulSet, ClusterIP and (when intercepted) EndpointSlice per service, then the
/// intercept policies.
#[allow(clippy::too_many_arguments)]
async fn apply_services(
    e: &crd::Environment,
    ns: &str,
    id: &str,
    pod_ctx: &k8s::PodContext<'_>,
    deployments: &Api<StatefulSet>,
    prev: &crd::EnvironmentStatus,
    wishes: &std::collections::HashMap<&str, &crd::Intercept>,
    plan: &std::collections::HashMap<&str, Intercepting>,
    owner_ref: &OwnerReference,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    let services: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    let slices: Api<EndpointSlice> = Api::namespaced(ctx.client.clone(), ns);
    for svc in &e.spec.services {
        let decided = plan.get(svc.name.as_str());
        let intercepted = match decided {
            Some(Intercepting::Force { .. }) => true,
            // Nothing is known, or the grace has not run out: render what the LAST pass rendered,
            // which is exactly what `intercepted_by` records.
            Some(Intercepting::Keep { .. }) => was_intercepted(prev, &svc.name),
            _ => false,
        };
        let mut set = k8s::service_statefulset(svc, &e.name_any(), id, &e.spec.owner, pod_ctx).map_err(ReconcileErr)?;
        if intercepted {
            // The real service is STOPPED while its traffic goes elsewhere. Leaving it running is
            // wrong for anything that acts on its own rather than only answering — a queue consumer
            // would take messages the workspace never sees, a scheduler would fire twice.
            if let Some(spec) = set.spec.as_mut() {
                spec.replicas = Some(0);
            }
        }
        ensure(deployments, &set, ctx).await?;
        let slice = format!("{}-intercept", svc.name);
        // BEFORE the Service loses its selector, never after. A Service with no selector and no
        // slice has no endpoints at all — a total outage of that service — and the wish stays, so
        // every retry would repeat the same order; a slice write that fails because this region's
        // `agent-rbac.yaml` has not been applied yet would strand it there forever. Written first,
        // a failure leaves the real service serving and the next pass repairs it. The release
        // path is the mirror of this: the selector is back before the slice is deleted.
        // `get`, not the index: a `Force` decision always has its wish beside it today, but the
        // two maps are built in one loop and a panic in a reconciler takes the whole controller
        // down — an absent wish means there is nothing to render, not a crash (2026-09-12).
        if let (Some(Intercepting::Force { pod_ip, .. }), Some(ic)) = (decided, wishes.get(svc.name.as_str())) {
            ensure(&slices, &k8s::intercept_slice(svc, &e.name_any(), &e.spec.owner, owner_ref, ic, Some(pod_ip)), ctx).await?;
        }
        // A portless service (nothing declared to listen on) gets no ClusterIP — the API server
        // rejects a Service with an empty `ports` list outright. Clean up a stale one left behind
        // by an earlier definition that did have ports; `ensure` has no delete path of its own.
        match k8s::service_clusterip(svc, &e.name_any(), &e.spec.owner, owner_ref, intercepted) {
            Some(cs) => ensure(&services, &cs, ctx).await?,
            None => {
                delete_ignoring_404(&services, &svc.name).await?;
                forget_applied(ctx, "Service", ns, &svc.name);
            }
        }
        // Kubernetes ABANDONS what it built while the Service had a selector rather than deleting
        // it: the endpointslice controller's own slice, and the legacy `Endpoints` object, both
        // keep naming the stopped pod. kube-proxy unions every slice of a service, so the real
        // pod's address survives beside ours — measured on the fleet as two of six connections
        // going nowhere. Deleted AFTER the selector is gone, never before, or the controllers that
        // own them write them straight back; and the `Endpoints` object is the load-bearing half,
        // since the mirroring controller rebuilds a slice from it and deleting slices alone does
        // not hold.
        if intercepted {
            drop_abandoned_endpoints(&slices, ns, &svc.name, &slice, ctx).await?;
        }
        match decided {
            // Already written, above.
            Some(Intercepting::Force { .. }) => {}
            // Inside the grace, or with an unreadable answer, AND the last pass really did render
            // an intercept: the slice is left exactly as it is. Held without that second half it
            // would survive a pass that has just put the selector back — kube-proxy unions the
            // two, splitting the service's traffic at random between the real pod and the
            // workspace, which is worse than either end state.
            Some(Intercepting::Keep { .. }) if intercepted => {}
            // Deleted only when this service has a wish that is not in force, or had one in force
            // last pass — not on every reconcile of every environment that never intercepted
            // anything, which would be one wasted DELETE per service per tick.
            // ponytail: a slice whose wish AND whose status record are both gone is collected only
            // by the Environment's own delete (it is ownerReferenced); a list of the namespace's
            // slices per pass is the upgrade path if one is ever seen stranded.
            _ => {
                if decided.is_some() || was_intercepted(prev, &svc.name) {
                    forget_applied(ctx, "EndpointSlice", ns, &slice);
                    delete_ignoring_404(&slices, &slice).await?;
                }
            }
        }
    }
    intercept_policies(e, ns, prev, plan, owner_ref, ctx).await?;
    Ok(())
}

/// Phase 5a: what the StatefulSets actually say, per service, with the intercept that is in
/// force and the outage clock carried as the plan decided.
async fn read_services_back(
    e: &crd::Environment,
    deployments: &Api<StatefulSet>,
    prev: &crd::EnvironmentStatus,
    plan: &std::collections::HashMap<&str, Intercepting>,
) -> Result<Vec<crd::ServiceStatus>, ReconcileErr> {
    // Read each StatefulSet back rather than reporting `ready: true` from having applied it. A
    // service whose image will not pull, or whose pod cannot schedule, was previously reported
    // ready the instant its object existed — so `kubectl wait --for=condition=Ready
    // environment` returned before anything was listening, and the only thing that noticed was a
    // connectivity check failing two steps later.
    // ONE list of this namespace's StatefulSets for the whole pass, in place of a GET per service.
    let sets: std::collections::HashMap<String, StatefulSet> =
        deployments.list(&kube::api::ListParams::default()).await?.items.into_iter().map(|d| (d.name_any(), d)).collect();
    let mut service_status = Vec::with_capacity(e.spec.services.len());
    for svc in &e.spec.services {
        // What is actually IN FORCE, never the wish: a stopped workspace leaves its intercept in
        // spec and this reports `None`, which is what the web and the CLI show.
        let by = match plan.get(svc.name.as_str()) {
            Some(Intercepting::Force { ws, .. }) => Some(ws.name_any()),
            Some(Intercepting::Keep { .. }) => prev_intercepted_by(prev, &svc.name),
            _ => None,
        };
        // Reachable, gone, stopped or detached all CLEAR the clock — it dates one continuous
        // outage, and a workspace that came back and broke again is a new one. Only a `Keep` that
        // learned nothing carries the recorded value forward untouched.
        let unreachable_since = match plan.get(svc.name.as_str()) {
            Some(Intercepting::Keep { since: Some(t) }) => Some(*t),
            Some(Intercepting::Keep { since: None }) => {
                prev.service_status.iter().find(|st| st.name == svc.name).and_then(|st| st.unreachable_since)
            }
            _ => None,
        };
        service_status.push(deployment_status(sets.get(&svc.name), &svc.name, by, unreachable_since));
    }
    Ok(service_status)
}

/// Phase 5b, pure: the status a running environment writes, and whether every service is ready.
pub(crate) fn running_status(
    e: &crd::Environment,
    prev: &crd::EnvironmentStatus,
    service_status: Vec<crd::ServiceStatus>,
    id: &str,
    plan: &std::collections::HashMap<&str, Intercepting>,
    decommissioning: bool,
    gen: i64,
) -> (crd::EnvironmentStatus, bool) {
    let all_ready = service_status.iter().all(|s| s.ready);
    let st = crd::EnvironmentStatus {
        phase: crd::Phase::Running,
        // Not converged until every service is: leaving it unobserved is what makes the next pass
        // look again instead of declaring a half-up environment finished.
        observed_generation: all_ready.then_some(gen),
        service_status,
        conditions: {
            let mut c = vec![if all_ready {
                crd::condition("Ready", true, "Converged", "environment matches spec", gen)
            } else {
                crd::condition("Ready", false, "ServicesNotReady", "one or more services are not ready", gen)
            }];
            // Reaching here with a restore wish means the Volume already reports it materialized —
            // the gate above is what stops anything else getting this far — so the services being
            // ensured on this pass IS the scale back up, and this says the restore is over.
            if e.spec.restore.is_some() {
                c.push(crd::condition("Restoring", false, "Restored", "the snapshot is live", gen));
            }
            // Written in the SAME status write that records the running services: from here on no
            // other node is an option whatever the copies hold, and a stale `True` left over from
            // the last stop is exactly the answer placement must never read.
            c.push(running_condition(&prev.conditions, gen));
            // Why an intercept is, or is not, in force. The wish itself is never touched here —
            // a controller does not write spec, and a stopped workspace must not discard what
            // somebody asked for.
            if let Some(ic) = intercept_condition(plan, gen) {
                c.push(ic);
            }
            // A retirement in progress is told HERE, on the running environment, and nowhere else:
            // this write is the wholesale rewrite that used to erase the decommission beat's mark
            // every 15 s.
            super::super::with_drain_notice(&prev.conditions, c, decommissioning, gen)
        },
        volume_ref: Some(id.to_string()),
        ..prev.clone()
    };
    (st, all_ready)
}


/// Pods in `ns` that can still be WRITING. A Succeeded or Failed pod holds no file handles and is
/// never collected on its own, so counting every pod in the namespace waits for something that
/// will not happen — a restore would hang forever behind a job that finished days ago. A pod that
/// is already terminating still counts: it has not exited yet.
pub(crate) async fn writing_pods(ns: &str, ctx: &Arc<Ctx>) -> Result<usize, ReconcileErr> {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    Ok(pods
        .list(&kube::api::ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|p| {
            let phase = p.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or("Pending");
            matches!(phase, "Running" | "Pending")
        })
        .count())
}


/// `Some(action)` while an in-place restore is in flight; `None` when there is nothing to restore
/// or the Volume already reports the wished-for snapshot live.
///
/// The order is the whole point. A restore rewrites the bytes a running service has open, so every
/// StatefulSet is scaled to ZERO and its pods are gone from the API server before the wish is copied
/// down to the child Volume — "no replicas" is not "no processes", and a database still flushing
/// into a subvolume that is being swapped is corruption nobody can attribute later.
///
/// `spec.restore` is never cleared here: a controller does not edit the user's spec, and "done" is
/// expressible without it (`Volume.status.restoredTo == wish.snapshotId`). A second restore of the
/// same snapshot is a new `requestedAt`, which is a new generation, which the Volume's own guard
/// then sees as a new wish.
pub(crate) async fn restore_gate(
    e: &crd::Environment,
    vol: &crd::Volume,
    ns: &str,
    deployments: &Api<StatefulSet>,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    let Some(wish) = &e.spec.restore else { return Ok(None) };
    let st = vol.status.as_ref();
    if crd::wish_granted(
        wish,
        st.and_then(|s| s.restored_to.as_deref()),
        st.and_then(|s| s.restore_requested_at.as_deref()),
    ) {
        // Snapshot model: the wish IS a snapshot, so a freshly granted one INITIALIZES this
        // environment's head — once, against the recorded wish, never against `head` itself.
        // Comparing `head` to the wish is what shipped, and it silently undid every push: a push
        // advances `head` to the new snapshot, the next pass sees `head != wish` and stamps it back,
        // so an environment that was ever restored could never move past its restore point. The
        // wish stays in the spec forever (a controller does not edit desired state), so "have I
        // applied this one?" has to be a fact this environment records, exactly as the `Volume`
        // records it. A genuinely new restore is a new `requestedAt`, which fails this same
        // comparison and is applied in its turn.
        //
        // Preserve pattern: merge onto whatever this environment currently reports, never blank
        // `podRef`/`serviceStatus`/anything else already there.
        let applied = crd::wish_granted(
            wish,
            e.status.as_ref().and_then(|s| s.restored_to.as_deref()),
            e.status.as_ref().and_then(|s| s.restore_requested_at.as_deref()),
        );
        if !applied {
            let prev = e.status.clone().unwrap_or_default();
            write_env_status(
                e,
                crd::EnvironmentStatus {
                    head: Some(wish.snapshot_id.clone()),
                    restored_to: Some(wish.snapshot_id.clone()),
                    restore_requested_at: Some(wish.requested_at.clone()),
                    ..prev
                },
                ctx,
            )
            .await?;
        }
        return Ok(None);
    }

    let remaining = drain_services(e, ns, deployments, ctx).await?;
    let (reason, message) = match remaining {
        0 => ("Restoring", "materializing the snapshot"),
        _ => ("Draining", "waiting for the services to stop"),
    };
    if remaining == 0 && vol.spec.restore_to.as_ref() != Some(wish) {
        let api: Api<crd::Volume> = Api::all(ctx.client.clone());
        let patch = serde_json::json!({"spec": {"restoreTo": wish}});
        api.patch(&vol.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await?;
    }
    let st = crd::EnvironmentStatus {
        // Still `running`, exactly as the stop path is while it waits: `model::EnvState` has no
        // `Working`, and an unknown phase silently projects as `Creating` — both wrong and
        // alarming. The progress belongs in the condition below, which is where a reader looks.
        phase: crd::Phase::Running,
        // Deliberately unobserved: the restore is not finished, and the next pass has to look again.
        observed_generation: None,
        conditions: vec![crd::condition("Restoring", true, reason, message, gen)],
        // `service_status` carried over, not blanked: it is the last thing known about these
        // services, and replacing it with nothing reads as "this environment has no services".
        ..e.status.clone().unwrap_or_default()
    };
    write_env_status(e, st, ctx).await?;
    Ok(Some(Action::requeue(TICK)))
}


/// Scale every service to zero and wait, briefly, for its pods to be GONE. Returns how many are
/// still writing; zero means the subvolume has no open writers and may be snapshotted or swapped.
///
/// ONE short wait here — a database exits in about a second, and a restore or a stop is the one
/// moment a person is watching the clock — and then the answer goes back to the caller. Not the ten
/// second, twenty pod LIST loop it was (2026-09-12): this is one controller task, so an environment
/// draining slowly held every other environment on the node behind it. Anything still shutting down
/// after the second probe falls back to the namespace's own Pod watch, which wakes the pass that
/// finishes within milliseconds of the last pod going — the requeue is only the backstop.
pub(crate) async fn drain_services(
    e: &crd::Environment,
    ns: &str,
    deployments: &Api<StatefulSet>,
    ctx: &Arc<Ctx>,
) -> Result<usize, ReconcileErr> {
    for svc in &e.spec.services {
        // A merge patch on `replicas` alone: scaling is not a claim on the rest of a StatefulSet
        // spec the reconcile re-applies a few lines later.
        let patch = serde_json::json!({"spec": {"replicas": 0}});
        // The scale happens behind `ensure`'s back, so its memory of this set is wrong from here:
        // without this the re-apply that brings the replicas back is skipped as "unchanged".
        forget_applied(ctx, "StatefulSet", ns, &svc.name);
        match deployments.patch(&svc.name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            Ok(_) => {}
            // Nothing to scale down is the desired state already reached.
            Err(kube::Error::Api(s)) if s.code == 404 => {}
            Err(err) => return Err(err.into()),
        }
    }
    let mut remaining = writing_pods(ns, ctx).await?;
    if remaining > 0 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        remaining = writing_pods(ns, ctx).await?;
    }
    Ok(remaining)
}

#[cfg(test)]
mod status_tests {
    use super::*;

    fn env(restore: bool) -> crd::Environment {
        crd::Environment::new(
            "env-1",
            crd::EnvironmentSpec {
                owner: "acme".into(),
                name: "e".into(),
                region: "r".into(),
                services: vec![],
                storage: None,
                desired_state: DesiredState::Running,
                restore: restore.then(|| crd::RestoreWish { snapshot_id: "snap-1".into(), volume: "vol-1".into(), owner: None, region: None, requested_at: String::new() }),
                intercepts: vec![],
                system: None,
            },
        )
    }

    fn svc(name: &str, ready: bool) -> crd::ServiceStatus {
        crd::ServiceStatus { name: name.into(), ready, message: None, intercepted_by: None, unreachable_since: None }
    }

    /// Converged only when every service is: a half-up environment stays unobserved so the next
    /// pass looks again, and its Ready condition names the reason.
    #[test]
    fn a_running_status_is_converged_only_when_every_service_is_ready() {
        let plan = std::collections::HashMap::new();
        let prev = crd::EnvironmentStatus::default();
        let (st, all) = running_status(&env(false), &prev, vec![svc("db", true), svc("api", true)], "vol-1", &plan, false, 7);
        assert!(all);
        assert_eq!(st.observed_generation, Some(7));
        assert_eq!(st.volume_ref.as_deref(), Some("vol-1"));
        let ready = st.conditions.iter().find(|c| c.type_ == "Ready").unwrap();
        assert_eq!((ready.status.as_str(), ready.reason.as_str()), ("True", "Converged"));

        let (st, all) = running_status(&env(false), &prev, vec![svc("db", true), svc("api", false)], "vol-1", &plan, false, 7);
        assert!(!all);
        assert_eq!(st.observed_generation, None);
        let ready = st.conditions.iter().find(|c| c.type_ == "Ready").unwrap();
        assert_eq!((ready.status.as_str(), ready.reason.as_str()), ("False", "ServicesNotReady"));
    }

    /// Reaching the running status with a restore wish means the services are the scale back
    /// up, so the status says the restore is over — and nothing else does.
    #[test]
    fn a_restore_wish_is_reported_over_once_the_services_run() {
        let plan = std::collections::HashMap::new();
        let (st, _) = running_status(&env(true), &crd::EnvironmentStatus::default(), vec![], "vol-1", &plan, false, 1);
        let r = st.conditions.iter().find(|c| c.type_ == "Restoring").expect("Restoring");
        assert_eq!((r.status.as_str(), r.reason.as_str()), ("False", "Restored"));
        let (st, _) = running_status(&env(false), &crd::EnvironmentStatus::default(), vec![], "vol-1", &plan, false, 1);
        assert!(st.conditions.iter().all(|c| c.type_ != "Restoring"));
    }
}

