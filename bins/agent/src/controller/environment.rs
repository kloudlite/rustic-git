//! The `Environment` reconciler: one volume, a namespace of StatefulSets, and the restore gate.
//! Split out of `controller.rs` unchanged.

use super::stop::{replicated_condition, running_condition, stop_name, stop_push, StopPush};
use super::workspace::{cleared_node_dead, replaced};
use super::{my_node, delete_ignoring_404, ensure, forget_applied, heal_labels, kept_conditions, owner_ref_of_kind, resolve_volume, settle, write_status, conditions_eq, Ctx, Outcome, ReconcileErr, Resolved, API_NAMESPACE, API_SERVICE_ACCOUNT, TICK};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{LimitRange, Namespace, Pod, ResourceQuota, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::RoleBinding;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Api, Resource, ResourceExt};
use kloudlite_workspaces::crd::{self, DesiredState};
use kloudlite_workspaces::k8s;
use kloudlite_workspaces::model;
use std::sync::Arc;
use std::time::Duration;

pub async fn apply_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // Above every write, exactly as `apply_workspace` does — see `my_node`.
    let me = my_node(ctx).await;
    if me.dead {
        return Ok(Action::requeue(TICK));
    }
    let gen = e.meta().generation.unwrap_or(0);
    // `spec.owner` reaches `ensure_homecache`'s `{pool}/homecache/{owner}` here too. Only the
    // owner: `EnvironmentSpec.name` is display text that reaches no path and no argv — the
    // namespace and every pool path are built from `vol.name_any()`, not from it.
    if let Err(why) = model::validate_owner(&e.spec.owner) {
        let prev = e.status.clone().unwrap_or_default();
        return settle(
            Outcome::Permanent(why, "InvalidSpec"),
            e,
            "Environment",
            gen,
            // Pruned-on-omit, as above: keep the placement fields and the prior conditions.
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
        .await;
    }
    heal_labels(&Api::<crd::Environment>::all(ctx.client.clone()), e, &e.spec.owner, "", "environment").await?;
    let prev = e.status.clone().unwrap_or_default();
    let owner_ref = owner_ref_of_kind(e)?;
    // Same resolution as a workspace, including the release-1 adoption — an environment is
    // team-owned, so it has no team of its own.
    let vol = match resolve_volume(
        e,
        &e.spec.owner,
        "",
        &e.spec.region,
        &e.spec.storage,
        &prev.node_name.clone(),
        &prev.conditions.clone(),
        gen,
        ctx,
    )
    .await?
    {
        Resolved::Ready(v) => *v,
        Resolved::Settled(a) => return Ok(a),
        // No StatefulSet may exist before the disk does: a pod bound to an unmaterialized subvolume
        // wedges forever on `path … does not exist`.
        Resolved::Wait { volume_ref, phase, cond, action } => {
            let st = crd::EnvironmentStatus {
                // An environment whose disk is being swapped is not being CREATED, and saying so
                // is alarming in the one moment a person is already nervous: an in-flight restore
                // keeps whatever phase this environment had. `Creating` is right only for a volume
                // that has never been materialized.
                phase: if e.spec.restore.is_some() && prev.phase != crd::Phase::Pending { prev.phase } else { phase },
                observed_generation: None,
                volume_ref: volume_ref.or(prev.volume_ref.clone()),
                conditions: vec![cond],
                ..prev
            };
            write_env_status(e, st, ctx).await?;
            return Ok(action);
        }
    };
    let id = vol.name_any();

    // The environment's OWN id, never the volume's: a restored environment resolves to the
    // SOURCE's volume (`resolve_volume`'s `shared` arm), and running it in the source's namespace
    // would collide every StatefulSet name with the source's.
    let ns = crd::env_namespace(&e.name_any());
    let deployments: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &ns);

    // Before anything else, including the stop path: an environment that is being restored has no
    // business converging its services against a disk that is about to be swapped underneath them.
    if let Some(action) = restore_gate(e, &vol, &ns, &deployments, gen, ctx).await? {
        return Ok(action);
    }

    if e.spec.desired_state == DesiredState::Stopped {
        return stop_environment(e, &vol, &ns, &deployments, prev, gen, ctx).await;
    }
    // Starts spread, exactly as a workspace's does — same decision, same one-caller rule: only the
    // owner, only when nothing on the volume is running. An environment has no `podRef`, so
    // `is_live_worktree` reads any non-`Stopped` phase as live: the decision belongs on the START
    // pass, which is the one moment its status still says `Stopped`, and that is what this gate is.
    if super::start_spread("Environment", &e.name_any(), &id, &vol, prev.phase, ctx).await?.is_some() {
        return Ok(Action::await_change());
    }
    run_environment(e, &vol, &ns, &deployments, &owner_ref, prev, gen, me.decommissioning, ctx).await
}

/// Tear the environment down, fail-closed: the services drain, the environment's own subvolume is
/// pushed, and only a push that has LANDED lets the StatefulSets go.
async fn stop_environment(
    e: &crd::Environment,
    vol: &crd::Volume,
    ns: &str,
    deployments: &Api<StatefulSet>,
    prev: crd::EnvironmentStatus,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr> {
    let id = vol.name_any();

    // Already stopped at this generation: nothing to do. Cheap rather than load-bearing now —
    // the `stop-{env}` request is kept after teardown, so a later event would find it `Ready` at
    // this same generation and re-run a teardown that is already done, rather than cutting
    // anything. The guard saves the round trips.
    // Already stopped at this generation: the teardown is done, but `Replicated` is not a
    // one-shot fact — a peer catches up minutes later, and the condition is what tells the UI
    // (and the placement rule) that this may now start elsewhere. Recomputed each pass, written
    // only when it actually changed, so a converged environment is idle.
    if e.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Stopped && s.observed_generation == Some(gen)) {
        let replicated = replicated_condition(ctx, &id, &e.name_any(), vol.spec.replicas, &prev.conditions, gen).await?;
        let conditions = replaced(&cleared_node_dead(&prev.conditions), replicated);
        if !conditions_eq(&prev.conditions, &conditions) {
            let st = crd::EnvironmentStatus { conditions, ..prev };
            write_env_status(e, st, ctx).await?;
        }
        return Ok(Action::requeue(TICK));
    }
    // Stopped at an OLDER generation: the services were torn down after a push that landed,
    // and nothing has run since, so there is nothing new on disk to push. A restore is the
    // common way here (`restore_gate` above bumps the generation), and pushing the freshly
    // restored subvolume as a new snapshot is a snapshot nobody asked for. Observe and stop.
    if prev.phase == crd::Phase::Stopped {
        let st = crd::EnvironmentStatus { observed_generation: Some(gen), volume_ref: Some(id), ..prev };
        write_env_status(e, st, ctx).await?;
        return Ok(Action::await_change());
    }
    // Scaled to zero and DRAINED before the push, not after: the pushed record is what a
    // restore on another node reads back as this environment's last state, and a snapshot
    // taken under a running database is crash-consistent at best. Same shape as the restore
    // gate, same reason. The StatefulSets themselves are not deleted here — that still waits
    // for the push to land, below.
    if drain_services(e, ns, deployments, ctx).await? > 0 {
        let st = crd::EnvironmentStatus {
            phase: crd::Phase::Running,
            observed_generation: None,
            conditions: vec![crd::condition("Progressing", true, "Draining", "waiting for the services to stop", gen)],
            ..prev
        };
        write_env_status(e, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    // An environment that stops must push first. One push of the env's own subvolume covers
    // every mounted volume atomically; an env torn down without it loses its last state for
    // good, which is why the deletes below are gated on the push having landed, not merely
    // requested.
    // The worktree is the environment's own name — the same string the sync beat cuts under, so
    // the stop's sync point extends that chain rather than starting a second one.
    match stop_push(&stop_name(e), &e.spec.owner, &vol.name_any(), &e.name_any(), e, crd::SnapshotState::of_environment(e), ctx).await? {
        StopPush::Landed => {}
        StopPush::Waiting => {
            let st = crd::EnvironmentStatus {
                // Still `running`: the StatefulSets exist (at zero) until the push lands, and
                // `model::EnvState` has no `Stopping` — an unknown phase silently becomes
                // `Creating`, which is both wrong and alarming. Progress belongs in the condition
                // below, which is where a reader looks for it.
                phase: crd::Phase::Running,
                observed_generation: None,
                service_status: vec![],
                conditions: vec![crd::condition("Progressing", true, "FlushBeforeStop", "waiting for the final sync point", gen)],
                ..e.status.clone().unwrap_or_default()
            };
            write_env_status(e, st, ctx).await?;
            return Ok(Action::requeue(TICK));
        }
    };
    for svc in &e.spec.services {
        forget_applied(ctx, "StatefulSet", ns, &svc.name);
        delete_ignoring_404(deployments, &svc.name).await?;
    }
    // Poke every placeable peer: the cut exists NOW, and waiting out the pull beat is what used to
    // make a cross-node start take minutes. Best-effort by construction — the ticker still comes.
    let live = crate::peer::placeable_nodes(ctx).await;
    crate::peer::wake_peers(ctx, &live, &ctx.peer_secret).await;
    let replicated = replicated_condition(ctx, &id, &e.name_any(), vol.spec.replicas, &prev.conditions, gen).await?;
    let st = crd::EnvironmentStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        service_status: vec![],
        conditions: vec![stopped_condition(gen), replicated],
        ..prev
    };
    write_env_status(e, st, ctx).await?;
    Ok(Action::requeue(TICK))
}

/// The stop's own Ready condition. No `FlushUnreplicated` arm: whether the last sync point has
/// reached another node is the `Replicated` condition's job, written on every reconcile of a
/// stopped parent and true for as long as it is true — not a one-shot record of one bad moment.
pub(crate) fn stopped_condition(gen: i64) -> Condition {
    crd::condition("Ready", true, "Stopped", "pushed and stopped", gen)
}

/// Converge the environment's namespace, storage and services against spec, and report what the
/// StatefulSets actually say about themselves.
#[allow(clippy::too_many_arguments)]
async fn run_environment(
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

    // Same worktree materialization a workspace does before any pod is built, and the same
    // HeadUnknown guard: an environment claimed onto this node for a volume with snapshots but no
    // recorded head yet must wait for Task 5/6 to write one rather than checking out empty next
    // to real history. Task 4 left this arm to this task — see `apply_workspace`'s twin.
    // The worktree is the environment's OWN name on whatever volume it resolved to — `id` for an
    // environment that owns its volume (the same string), the SOURCE's volume for a restored one,
    // which holds a SECOND worktree of it. Never `(id, id)`: that checked a restored environment
    // out on top of the source's live worktree, two environments writing one subvolume. It is also
    // the name `sync.rs`'s `live_worktrees` writes into `Snapshot.spec.worktree`, so every path
    // below — checkout, mount, mkdir, stop cut, drop — uses this one string.
    let wt = e.name_any();
    // Lazy per-volume migration, resolve the effective head, checkout and quota — identical for a
    // Workspace and an Environment down to the guard conditions; see `worktree_gate`.
    let gate = super::worktree_gate(
        &wt,
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
    let mut prev = prev;
    match gate {
        super::WorktreeGate::Wait { reason: "NoSuchSnapshot", message, .. } => {
            // Permanent: only the caller can settle it, since `settle` needs the object itself.
            let prev = prev.clone();
            return settle(
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
            .await;
        }
        // `HeadUnknown` REPLACES conditions rather than keeping — matching the pre-extraction
        // code exactly; only `SnapshotPending` (and the settled `NoSuchSnapshot` above) keep them.
        super::WorktreeGate::Wait { reason: "HeadUnknown", message, action } => {
            let st = crd::EnvironmentStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                conditions: vec![crd::condition("Ready", false, "HeadUnknown", &message, gen)],
                ..prev.clone()
            };
            write_env_status(e, st, ctx).await?;
            return Ok(action);
        }
        super::WorktreeGate::Wait { reason, message, action } => {
            let st = crd::EnvironmentStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                conditions: kept_conditions(&prev.conditions, crd::condition("Ready", false, reason, &message, gen)),
                ..prev.clone()
            };
            write_env_status(e, st, ctx).await?;
            return Ok(action);
        }
        super::WorktreeGate::Ready => {}
    }
    // First graft: nothing else will ever write this head — a restored environment gets
    // `advance_head` only if it pushes itself. The preserve pattern, same as the workspace's.
    if prev.head.is_none() {
        if let Some(commit) = super::clone_commit(&e.spec.storage) {
            let prev2 = prev.clone();
            write_env_status(e, crd::EnvironmentStatus { head: Some(commit.to_string()), ..prev2 }, ctx).await?;
            prev.head = Some(commit.to_string());
        }
    }

    ensure(
        &Api::<Namespace>::all(ctx.client.clone()),
        &k8s::namespace(ns, &e.spec.owner, "environment", Some(owner_ref)),
        ctx,
    )
    .await?;
    let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), ns);
    for p in k8s::default_policies(ns, &e.spec.owner, owner_ref) {
        ensure(&policies, &p, ctx).await?;
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
    // The env unit's ceiling, matching `service_deployment`'s resources: 4 GB limit, packed at the
    // model's 1.5x oversubscription. Owned by the Environment — this namespace holds exactly one.
    ensure(
        &Api::<LimitRange>::namespaced(ctx.client.clone(), ns),
        &k8s::limit_range(ns, &e.spec.owner, "environment", &k8s::env_unit_resources(), Some(owner_ref)),
        ctx,
    )
    .await?;
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
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &vol.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: ctx.runtime_class.as_deref(),
        default_image: &ctx.default_image,
    };
    // Every declared folder must exist before a subPath binds it — and `validate_mount` here is a
    // security check, not a formality: `create_dir_all` on an unvalidated folder is itself the
    // escape, mkdir -p'ing outside the subvolume before a pod ever starts.
    // On a blocking thread: `create_dir_all` is sync IO, and the pool can be a network-backed or
    // busy disk. Same rule the module doc states for the btrfs work.
    // The worktree, not `live/` itself: the pod mounts the worktree and binds `volumes/{folder}`
    // as a subPath INSIDE it, so a folder made one level up is invisible to every service.
    let live = ctx.engine.pool.worktree(&id, &wt);
    let services = e.spec.services.clone();
    tokio::task::spawn_blocking(move || mkdir_env_mounts(&live, &services))
        .await
        .map_err(|e| ReconcileErr(format!("mkdir panicked: {e}")))?
        .map_err(ReconcileErr)?;

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
        return Ok(Action::requeue(TICK));
    }
    // Every intercept decided BEFORE anything is rendered: each service is in exactly one of the
    // three states below, and the rendering is a straight read of that decision.
    let (wishes, plan) = intercept_plan(e, ctx).await;
    let services: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    let slices: Api<EndpointSlice> = Api::namespaced(ctx.client.clone(), ns);
    for svc in &e.spec.services {
        let decided = plan.get(svc.name.as_str());
        let intercepted = match decided {
            Some(Intercepting::Force { .. }) => true,
            // Nothing is known, or the grace has not run out: render what the LAST pass rendered,
            // which is exactly what `intercepted_by` records.
            Some(Intercepting::Keep) => was_intercepted(&prev, &svc.name),
            _ => false,
        };
        let mut set = k8s::service_statefulset(svc, &e.name_any(), &id, &e.spec.owner, &pod_ctx).map_err(ReconcileErr)?;
        if intercepted {
            // The real service is STOPPED while its traffic goes elsewhere. Leaving it running is
            // wrong for anything that acts on its own rather than only answering — a queue consumer
            // would take messages the workspace never sees, a scheduler would fire twice.
            if let Some(spec) = set.spec.as_mut() {
                spec.replicas = Some(0);
            }
        }
        ensure(deployments, &set, ctx).await?;
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
        let slice = format!("{}-intercept", svc.name);
        match decided {
            Some(Intercepting::Force { pod_ip, .. }) => {
                let ic = wishes[svc.name.as_str()];
                ensure(&slices, &k8s::intercept_slice(svc, &e.name_any(), &e.spec.owner, owner_ref, ic, Some(pod_ip)), ctx).await?;
            }
            // Inside the grace, or with an unreadable answer: the slice is left exactly as it is.
            Some(Intercepting::Keep) => {}
            // Deleted only when this service has a wish that is not in force, or had one in force
            // last pass — not on every reconcile of every environment that never intercepted
            // anything, which would be one wasted DELETE per service per tick.
            // ponytail: a slice whose wish AND whose status record are both gone is collected only
            // by the Environment's own delete (it is ownerReferenced); a list of the namespace's
            // slices per pass is the upgrade path if one is ever seen stranded.
            _ => {
                if decided.is_some() || was_intercepted(&prev, &svc.name) {
                    forget_applied(ctx, "EndpointSlice", ns, &slice);
                    delete_ignoring_404(&slices, &slice).await?;
                }
            }
        }
    }
    intercept_policies(e, ns, &prev, &plan, owner_ref, ctx).await?;
    // Read each StatefulSet back rather than reporting `ready: true` from having applied it. A
    // service whose image will not pull, or whose pod cannot schedule, was previously reported
    // ready the instant its object existed — so `kubectl wait --for=condition=Ready
    // environment` returned before anything was listening, and the only thing that noticed was a
    // connectivity check failing two steps later.
    let mut service_status = Vec::with_capacity(e.spec.services.len());
    for svc in &e.spec.services {
        // What is actually IN FORCE, never the wish: a stopped workspace leaves its intercept in
        // spec and this reports `None`, which is what the web and the CLI show.
        let by = match plan.get(svc.name.as_str()) {
            Some(Intercepting::Force { ws, .. }) => Some(ws.name_any()),
            Some(Intercepting::Keep) => prev_intercepted_by(&prev, &svc.name),
            _ => None,
        };
        service_status.push(deployment_status(deployments, &svc.name, by).await?);
    }
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
            if let Some(ic) = intercept_condition(&plan, gen) {
                c.push(ic);
            }
            // A retirement in progress is told HERE, on the running environment, and nowhere else:
            // this write is the wholesale rewrite that used to erase the decommission beat's mark
            // every 15 s.
            super::with_drain_notice(&prev.conditions, c, decommissioning, gen)
        },
        volume_ref: Some(id.clone()),
        ..prev
    };
    write_env_status(e, st, ctx).await?;
    // A held intercept has to be looked at again: nothing woke us for the grace running out.
    let holding = plan.values().any(|d| matches!(d, Intercepting::Keep));
    Ok(if all_ready && !holding { Action::await_change() } else { Action::requeue(TICK) })
}

/// One service's observed readiness, from the StatefulSet's own status.
///
/// `readyReplicas >= 1`, not `replicas`: `replicas` is what was asked for, `readyReplicas` is what
/// is actually serving. A missing StatefulSet reports not-ready rather than erroring — it is the
/// ordinary gap between applying it and the API server materializing it.
async fn deployment_status(
    deployments: &Api<StatefulSet>,
    name: &str,
    // Decided by `intercept_plan`, threaded in rather than recomputed: this function reconstructs
    // the whole `ServiceStatus` every pass, so anything it defaults here is stomped every pass.
    intercepted_by: Option<String>,
) -> Result<crd::ServiceStatus, ReconcileErr> {
    let Some(d) = deployments.get_opt(name).await? else {
        return Ok(crd::ServiceStatus { name: name.into(), ready: false, message: Some("statefulset not created yet".into()), intercepted_by });
    };
    let ready = d.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0);
    // An intercepted service is scaled to zero BY US, so zero ready replicas is the converged
    // state, not a fault — reporting it not-ready would park the environment at `ServicesNotReady`
    // and requeue it forever for as long as somebody is debugging.
    if let Some(ws) = &intercepted_by {
        return Ok(crd::ServiceStatus {
            name: name.into(),
            ready: true,
            message: Some(format!("intercepted by {ws}")),
            intercepted_by: intercepted_by.clone(),
        });
    }
    Ok(crd::ServiceStatus {
        name: name.into(),
        ready: ready >= 1,
        message: (ready < 1).then(|| "no ready replicas".to_string()),
        intercepted_by,
    })
}

/// How long the intercepting workspace's pod may be unreachable before the real service comes
/// back up.
///
/// A pod restarting is unreachable for a few seconds, and bouncing the real StatefulSet up and
/// down around every restart would be worse than the gap. Stopped, deleted and detached are NOT
/// graced: each is a deliberate act, observed as itself rather than as an absence.
const INTERCEPT_GRACE_SECS: i64 = 30;

/// What this pass decided about one intercept — the spec's three states, plus the one thing an
/// unreadable API answer is allowed to do, which is nothing.
enum Intercepting {
    /// In force: the real service off, the slice pointing at this pod.
    Force { ws: Box<crd::Workspace>, pod_ip: String },
    /// Either nothing is known (an API error) or the pod has not been unreachable long enough.
    /// Render what the last pass rendered and look again — a blip in the API server must never
    /// flap a service, and an unreadable answer is not evidence of anything.
    Keep,
    /// Not in force. The wish STAYS in spec; the rendering goes back to the ordinary one and the
    /// condition says why.
    Off { reason: &'static str, message: String, ws: Option<Box<crd::Workspace>> },
}

/// The wish per service (first entry wins) and what this pass decided about each.
///
/// Keyed by service name, and a wish naming a service this environment does not declare is
/// dropped: `/v1` refuses one, and a hand-edited object must not make the controller flap.
#[allow(clippy::type_complexity)]
async fn intercept_plan<'a>(
    e: &'a crd::Environment,
    ctx: &Arc<Ctx>,
) -> (std::collections::HashMap<&'a str, &'a crd::Intercept>, std::collections::HashMap<&'a str, Intercepting>) {
    let mut wishes: std::collections::HashMap<&str, &crd::Intercept> = std::collections::HashMap::new();
    let mut plan: std::collections::HashMap<&str, Intercepting> = std::collections::HashMap::new();
    for ic in &e.spec.intercepts {
        if !e.spec.services.iter().any(|s| s.name == ic.service) || wishes.contains_key(ic.service.as_str()) {
            continue;
        }
        wishes.insert(&ic.service, ic);
        plan.insert(&ic.service, decide_intercept(ic, &e.name_any(), ctx).await);
    }
    (wishes, plan)
}

/// One intercept's fate, from the Workspace and its pod. Never errors: an unreadable answer is
/// `Keep`, which changes nothing at all.
async fn decide_intercept(ic: &crd::Intercept, env_name: &str, ctx: &Arc<Ctx>) -> Intercepting {
    let off = |reason, message: String, w: Option<crd::Workspace>| Intercepting::Off { reason, message, ws: w.map(Box::new) };
    let w = match Api::<crd::Workspace>::all(ctx.client.clone()).get_opt(&ic.workspace).await {
        Ok(Some(w)) => w,
        Ok(None) => return off("WorkspaceGone", format!("{} no longer exists", ic.workspace), None),
        Err(_) => return Intercepting::Keep,
    };
    if w.spec.desired_state == DesiredState::Stopped {
        return off("WorkspaceStopped", format!("{} is stopped", ic.workspace), Some(w));
    }
    // `spec` only, never `crd::attached_environment`'s condition fallback: that reads back a
    // DETACHED workspace's last attachment, which is the one answer this must not accept.
    if w.spec.attached_environment.as_deref() != Some(env_name) {
        return off("WorkspaceDetached", format!("{} is not attached to this environment", ic.workspace), Some(w));
    }
    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    let pod = match w.status.as_ref().and_then(|s| s.pod_ref.clone()) {
        Some(name) => match Api::<Pod>::namespaced(ctx.client.clone(), &ns).get_opt(&name).await {
            Ok(p) => p,
            Err(_) => return Intercepting::Keep,
        },
        None => None,
    };
    let ready = pod.as_ref().map(pod_ready);
    let ip = pod.as_ref().and_then(|p| p.status.as_ref()?.pod_ip.clone());
    // Read live and never stored: a pod IP changes on every recreate, and a stale one in status is
    // a wrong answer that looks right — the same rule `bins/gateway/src/resolve.rs` already states.
    if let (Some((true, _)), Some(ip)) = (ready, ip) {
        return Intercepting::Force { ws: Box::new(w), pod_ip: ip };
    }
    // The clock is the POD's own `Ready` condition, or — with no pod at all — the Workspace's.
    // Neither is a field this controller invented, and both are stamped by whoever observed the
    // transition, so the grace measures the real outage rather than this pass's first sight of it.
    // No clock anywhere means we cannot call the outage recent, and the safe answer is the real
    // service: fall back rather than hold traffic on a pod nobody can date.
    let since = ready
        .and_then(|(_, t)| t)
        .or_else(|| w.status.as_ref().and_then(|s| condition_time(&s.conditions, "Ready")));
    let waited = since.map_or(INTERCEPT_GRACE_SECS, |t| k8s_openapi::jiff::Timestamp::now().as_second() - t);
    if waited < INTERCEPT_GRACE_SECS {
        return Intercepting::Keep;
    }
    off("PodUnreachable", format!("{}'s pod has been unreachable for {waited}s", ic.workspace), Some(w))
}

/// A pod's `Ready` truth and the instant it last changed, in one read.
fn pod_ready(p: &Pod) -> (bool, Option<i64>) {
    p.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .find(|c| c.type_ == "Ready")
        .map_or((false, None), |c| (c.status == "True", c.last_transition_time.as_ref().map(|t| t.0.as_second())))
}

fn condition_time(conds: &[Condition], kind: &str) -> Option<i64> {
    conds.iter().find(|c| c.type_ == kind).map(|c| c.last_transition_time.0.as_second())
}

/// What the LAST pass rendered for this service, read off the one record of it.
fn prev_intercepted_by(prev: &crd::EnvironmentStatus, svc: &str) -> Option<String> {
    prev.service_status.iter().find(|s| s.name == svc)?.intercepted_by.clone()
}

fn was_intercepted(prev: &crd::EnvironmentStatus, svc: &str) -> bool {
    prev_intercepted_by(prev, svc).is_some()
}

/// The environment → workspace direction, which `allow_internet_egress` denies by default: without
/// this pair an in-force intercept renders perfectly and delivers nothing.
///
/// Written from HERE and not from the workspace's own pass because the wish is this object's, and
/// this pass already holds the Workspace the workspace-side half needs.
async fn intercept_policies(
    e: &crd::Environment,
    ns: &str,
    prev: &crd::EnvironmentStatus,
    plan: &std::collections::HashMap<&str, Intercepting>,
    owner_ref: &OwnerReference,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    let here: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), ns);
    let mut in_force: std::collections::HashSet<String> = Default::default();
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
    // and is not serving, and one the last pass recorded as in force.
    // ponytail: a grant whose wish AND whose status record are both gone (a release that raced a
    // lost status write) is left until the Environment is deleted, which collects it; a label
    // selector over the namespace's policies is the upgrade path.
    let mut stale: Vec<(String, Option<&crd::Workspace>)> = Vec::new();
    for d in plan.values() {
        if let Intercepting::Off { ws: Some(w), .. } = d {
            stale.push((w.name_any(), Some(&**w)));
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
        // The workspace-side half only when its namespace is actually known. A workspace that is
        // GONE takes it with it — it is ownerReferenced — and the egress half above is what the
        // traffic needed anyway, so an inert ingress rule opens nothing.
        if let Some(w) = ws {
            let ws_ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
            let in_ws: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ws_ns);
            delete_ignoring_404(&in_ws, &k8s::intercept_policy_name(&id)).await?;
            forget_applied(ctx, "NetworkPolicy", &ws_ns, &k8s::intercept_policy_name(&id));
        }
    }
    Ok(())
}

/// One `Intercepted` condition for the whole environment: the first intercept that is NOT in
/// force, since that is the one somebody has to act on, and otherwise that they are.
///
/// ponytail: one condition for every intercept, so a second not-in-force intercept is invisible
/// until the first is dealt with; a per-service condition type is the upgrade path.
fn intercept_condition(plan: &std::collections::HashMap<&str, Intercepting>, gen: i64) -> Option<Condition> {
    if plan.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = plan.keys().copied().collect();
    names.sort_unstable();
    for n in &names {
        if let Intercepting::Off { reason, message, .. } = &plan[n] {
            return Some(crd::condition("Intercepted", false, reason, &format!("{n}: {message}"), gen));
        }
    }
    Some(crd::condition("Intercepted", true, "InForce", &format!("intercepted: {}", names.join(", ")), gen))
}

/// Pods in `ns` that can still be WRITING. A Succeeded or Failed pod holds no file handles and is
/// never collected on its own, so counting every pod in the namespace waits for something that
/// will not happen — a restore would hang forever behind a job that finished days ago. A pod that
/// is already terminating still counts: it has not exited yet.
async fn writing_pods(ns: &str, ctx: &Arc<Ctx>) -> Result<usize, ReconcileErr> {
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
async fn restore_gate(
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
/// Waited for HERE, in this pass: a database exits in about a second, and a restore or a stop is
/// the one moment a person is watching the clock, so handing the wait to the requeue would price
/// every one at a full tick. Bounded well under the pods' grace period; a service that is still
/// shutting down after this falls back to the pod watch, which wakes the pass that finishes.
async fn drain_services(
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
    // 20 × 500 ms, not 40 × 250: the same 10 s ceiling at half the pod LISTs. A service that
    // finishes its writes 250 ms sooner is not worth a doubled API cost on every stop and every
    // restore of every environment.
    for _ in 0..20 {
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        remaining = writing_pods(ns, ctx).await?;
    }
    Ok(remaining)
}

/// Every declared volume is a folder inside the env's ONE subvolume — mkdir -p each before a pod
/// binds it as a subPath.
fn mkdir_env_mounts(live: &std::path::Path, services: &[model::Service]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for svc in services {
        for m in &svc.mounts {
            if seen.insert(m.folder.clone()) {
                // `create_dir_all` on an unvalidated folder is itself the escape — it would
                // happily mkdir -p outside the subvolume before a pod ever ran.
                model::validate_mount(m)?;
                std::fs::create_dir_all(live.join("volumes").join(&m.folder)).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(folder: &str) -> model::Service {
        serde_json::from_value(serde_json::json!({
            "name": "db", "image": "mongo:7", "command": [], "env": {},
            "ports": [], "mounts": [{"path": "/data/db", "folder": folder}],
        }))
        .unwrap()
    }

    /// `create_dir_all` on an unvalidated folder IS the escape — it would happily mkdir -p outside
    /// the subvolume before a pod ever bound it as a subPath. `validate_mount` is tested in
    /// `model.rs`; this asserts the controller actually calls it, which is where the escape lives.
    #[test]
    fn a_traversing_folder_makes_no_directory_and_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        for folder in ["../../etc", "..", "a/b", "/abs", ""] {
            assert!(mkdir_env_mounts(&live, &[svc(folder)]).is_err(), "accepted {folder:?}");
        }
        assert!(!tmp.path().join("etc").exists(), "nothing was created outside the subvolume");
        assert!(std::fs::read_dir(live.join("volumes")).map(|mut d| d.next().is_none()).unwrap_or(true));
    }

    /// The ordinary folder is made, once, under `volumes/`.
    #[test]
    fn a_valid_folder_is_created_under_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        mkdir_env_mounts(&live, &[svc("dbdata"), svc("dbdata")]).unwrap();
        assert!(live.join("volumes/dbdata").is_dir());
    }
}
