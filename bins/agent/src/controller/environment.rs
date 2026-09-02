//! The `Environment` reconciler: one volume, a namespace of StatefulSets, and the restore gate.
//! Split out of `controller.rs` unchanged.

use super::stop::{replicated_condition, running_condition, stop_name, stop_push, StopPush};
use super::workspace::replaced;
use super::{delete_ignoring_404, ensure, forget_applied, heal_labels, kept_conditions, migrate_and_seed_baseline, owner_ref_of_kind, resolve_volume, settle, write_status, conditions_eq, Ctx, Outcome, ReconcileErr, Resolved, API_NAMESPACE, API_SERVICE_ACCOUNT, TICK};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{LimitRange, Namespace, Pod, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::RoleBinding;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::crd::{self, DesiredState};
use rustic_git_workspaces::k8s;
use rustic_git_workspaces::model;
use std::sync::Arc;
use std::time::Duration;

pub async fn apply_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
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

    let ns = crd::env_namespace(&id);
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
    if prev.phase == crd::Phase::Stopped {
        if let Some(siblings) = crate::listing::parents_on_volume(ctx, &id).await {
            if let Some(node) = super::stop::start_placement(ctx, &vol, &siblings).await? {
                tracing::info!(environment = %e.name_any(), %node, "handed over on start");
                return Ok(Action::await_change());
            }
        }
    }
    run_environment(e, &vol, &ns, &deployments, &owner_ref, prev, gen, ctx).await
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
        let conditions = replaced(&prev.conditions, replicated);
        if !conditions_eq(&prev.conditions, &conditions) {
            let st = crd::EnvironmentStatus { conditions, ..prev };
            write_env_status(e, st, ctx).await?;
        }
        return Ok(Action::requeue(TICK));
    }
    // Stopped at an OLDER generation: the services were torn down after a push that landed,
    // and nothing has run since, so there is nothing new on disk to push. A restore is the
    // common way here (`restore_gate` above bumps the generation), and pushing the freshly
    // restored subvolume as a new commit is a snapshot nobody asked for. Observe and stop.
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
    match stop_push(&stop_name(e), &e.spec.owner, &vol.name_any(), &e.name_any(), e, ctx).await? {
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
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr> {
    let id = vol.name_any();

    // Same worktree materialization a workspace does before any pod is built, and the same
    // HeadUnknown guard: an environment claimed onto this node for a volume with commits but no
    // recorded head yet must wait for Task 5/6 to write one rather than checking out empty next
    // to real history. Task 4 left this arm to this task — see `apply_workspace`'s twin.
    migrate_and_seed_baseline(ctx, &id, &e.spec.owner).await?;
    // `apply_workspace`'s re-host arm, same rule: a node that has never run this worktree starts
    // from the newest sync point rather than the last commit, so a node death costs one
    // `WS_SYNC_SECS` of edits. Resolved before the guard below — a transient is a Snapshot CR, so
    // `has_commits` sees it too.
    // `e.name_any()`, not `id`: `spec.worktree` on a Snapshot is what `sync.rs`'s
    // `live_worktrees` wrote there, which is the Environment's own name. They are the same string
    // for every environment today — the volume is named after the environment — and this arm is
    // simply keyed on the field that actually names the worktree.
    let synced = if ctx.engine.pool.worktree(&id, &id).exists() {
        None
    } else {
        crate::snapshot::latest_transient(ctx, &id, &e.name_any()).await?
    };
    let effective_head = synced.or_else(|| prev.head.clone());
    if effective_head.is_none() && crate::claim::has_commits(ctx, &id).await? {
        let st = crd::EnvironmentStatus {
            phase: crd::Phase::Creating,
            observed_generation: None,
            conditions: vec![crd::condition(
                "Ready", false, "HeadUnknown", "volume has commits but this environment has no recorded head yet", gen,
            )],
            ..prev.clone()
        };
        write_env_status(e, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    let (engine, vol_id, ws_id, head) = (ctx.engine.clone(), id.clone(), id.clone(), effective_head);
    let quota_gb = vol.spec.quota_gb;
    let result = tokio::task::spawn_blocking(move || {
        engine.checkout(&vol_id, head.as_deref(), &ws_id)?;
        engine.set_quota_worktree(&vol_id, &ws_id, quota_gb)?;
        Ok::<_, rustic_git_workspaces::engine::ops::EngErr>(())
    })
    .await
    .map_err(|e| ReconcileErr(e.to_string()))?;
    match result {
        Ok(()) => {}
        Err(e) if e.0 == rustic_git_workspaces::engine::commit::WORKTREE_EXISTS => {}
        Err(e) => return Err(ReconcileErr(e.0)),
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
    let live = ctx.engine.pool.live(&id);
    let services = e.spec.services.clone();
    tokio::task::spawn_blocking(move || mkdir_env_mounts(&live, &services))
        .await
        .map_err(|e| ReconcileErr(format!("mkdir panicked: {e}")))?
        .map_err(ReconcileErr)?;

    let services: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    for svc in &e.spec.services {
        let set = k8s::service_statefulset(svc, &id, &e.spec.owner, &pod_ctx).map_err(ReconcileErr)?;
        ensure(deployments, &set, ctx).await?;
        ensure(&services, &k8s::service_clusterip(svc, &id, &e.spec.owner, owner_ref), ctx).await?;
    }
    // Read each StatefulSet back rather than reporting `ready: true` from having applied it. A
    // service whose image will not pull, or whose pod cannot schedule, was previously reported
    // ready the instant its object existed — so `kubectl wait --for=condition=Ready
    // environment` returned before anything was listening, and the only thing that noticed was a
    // connectivity check failing two steps later.
    let mut service_status = Vec::with_capacity(e.spec.services.len());
    for svc in &e.spec.services {
        service_status.push(deployment_status(deployments, &svc.name).await?);
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
            c
        },
        volume_ref: Some(id.clone()),
        ..prev
    };
    write_env_status(e, st, ctx).await?;
    Ok(if all_ready { Action::await_change() } else { Action::requeue(TICK) })
}

/// One service's observed readiness, from the StatefulSet's own status.
///
/// `readyReplicas >= 1`, not `replicas`: `replicas` is what was asked for, `readyReplicas` is what
/// is actually serving. A missing StatefulSet reports not-ready rather than erroring — it is the
/// ordinary gap between applying it and the API server materializing it.
async fn deployment_status(deployments: &Api<StatefulSet>, name: &str) -> Result<crd::ServiceStatus, ReconcileErr> {
    let Some(d) = deployments.get_opt(name).await? else {
        return Ok(crd::ServiceStatus { name: name.into(), ready: false, message: Some("statefulset not created yet".into()) });
    };
    let ready = d.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0);
    Ok(crd::ServiceStatus {
        name: name.into(),
        ready: ready >= 1,
        message: (ready < 1).then(|| "no ready replicas".to_string()),
    })
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
        // Commit model: the wish IS a commit, so a freshly granted one INITIALIZES this
        // environment's head — once, against the recorded wish, never against `head` itself.
        // Comparing `head` to the wish is what shipped, and it silently undid every push: a push
        // advances `head` to the new commit, the next pass sees `head != wish` and stamps it back,
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
    for _ in 0..40 {
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
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
            && a.compatible_nodes == b.compatible_nodes
            && a.volume_ref == b.volume_ref
            && a.service_status == b.service_status
            // See `write_ws_status`'s twin comment: without these, a head-only advance is a no-op.
            && a.head == b.head
            && a.durable == b.durable
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
