//! Stopping an environment: the stop cut, the StatefulSets torn down once it is Ready, and the
//! `Replicated` condition that says whether another node holds that cut.

use super::*;


/// Tear the environment down, fail-closed: the services drain, the environment's own subvolume is
/// pushed, and only a push that has LANDED lets the StatefulSets go.
pub(crate) async fn stop_environment(
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
        // The teardown still runs (2026-09-12): a generation bump while stopped is usually a spec
        // edit, and one that ADDS a service adds a StatefulSet nothing else would ever delete —
        // stamping `observed_generation` first made this pass the last one, so the new service ran
        // in a stopped environment until somebody started and stopped it again.
        for svc in &e.spec.services {
            forget_applied(ctx, "StatefulSet", ns, &svc.name);
            delete_ignoring_404(deployments, &svc.name).await?;
        }
        // And `Replicated` is recomputed before the stamp for the same reason: with
        // `observed_generation` matching, the branch above is what every later pass takes, so a
        // condition left at the old generation here would be the one this object keeps.
        let replicated = replicated_condition(ctx, &id, &e.name_any(), vol.spec.replicas, &prev.conditions, gen).await?;
        let conditions = replaced(&cleared_node_dead(&prev.conditions), replicated);
        let st = crd::EnvironmentStatus { observed_generation: Some(gen), volume_ref: Some(id), conditions, ..prev };
        write_env_status(e, st, ctx).await?;
        return Ok(Action::requeue(TICK));
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
    //
    // Unless there is nothing on disk to cut. A cut protects bytes some writer produced, and an
    // environment whose worktree was never materialised has none — a builder is created `Stopped`
    // from birth, so `btrfs subvolume snapshot` of a path that does not exist failed every tick
    // and parked it in `FlushBeforeStop` forever. The worktree's existence IS the signal the
    // workspace path gets from its `pod_ref`. A recorded service short-circuits the stat, and is
    // also the safe direction: anything that ever ran is cut unconditionally, because losing its
    // last state is exactly what this path exists to prevent. Phase is deliberately not evidence —
    // the `Waiting` arm below writes `Running` itself, so the stuck builder looked like it ran.
    let wt = e.name_any();
    let ran = !prev.service_status.is_empty() || {
        let (engine, vol_id, worktree) = (ctx.engine.clone(), id.clone(), wt.clone());
        tokio::task::spawn_blocking(move || engine.pool.worktree(&vol_id, &worktree).exists())
            .await
            .map_err(|err| ReconcileErr(format!("worktree stat panicked: {err}")))?
    };
    if !ran {
        // An unfulfillable request, not a pending one: there is no subvolume to snapshot and never
        // will be under this generation, so an earlier pass's `stop-{env}` is deleted rather than
        // left Working forever. A `Ready` one is somebody's sync point — never ours to delete.
        let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
        let name = stop_name(e);
        let pending = api
            .get_opt(&name)
            .await?
            .is_some_and(|s| s.status.as_ref().map(|st| st.phase) != Some(crd::Phase::Ready));
        if pending {
            delete_ignoring_404(&api, &name).await?;
        }
    } else {
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
        }
    }
    for svc in &e.spec.services {
        forget_applied(ctx, "StatefulSet", ns, &svc.name);
        delete_ignoring_404(deployments, &svc.name).await?;
    }
    // Poke every placeable peer: the cut exists NOW, and waiting out the pull beat is what used to
    // make a cross-node start take minutes. Best-effort by construction — the ticker still comes.
    // Only when there WAS a cut: nothing new left this node otherwise, and waking the fleet for a
    // no-op stop is a cluster-wide listing per tick.
    if ran {
        let live = crate::peer::placeable_nodes(ctx).await;
        crate::peer::wake_peers(ctx, &live, &ctx.peer_secret).await;
    }
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
