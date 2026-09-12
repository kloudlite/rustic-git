//! Stop, delete and migrate: the stop cut and pod teardown, the `WORKTREE_FINALIZER` that drops
//! the worktree and detaches or releases the Volume, and the one-time baseline a pre-snapshot
//! workspace is seeded with.

use super::*;


/// Stop the workspace: cut a final sync point, then delete the pod. The cut is what the wait is
/// for — once it is Ready the worktree's last minute of work exists as a snapshot, and whether any
/// PEER holds a copy of it is the `Replicated` condition's answer, not a gate. The home is on the
/// shared NFS mount and needs no push of its own (spec 2026-09-01).
pub(crate) async fn stop_workspace(
    w: &crd::Workspace,
    prev: crd::WorkspaceStatus,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr> {
    let id = prev.volume_ref.clone().unwrap_or_else(|| w.name_any());
    // Already stopped: the teardown is done, but `Replicated` is not a one-shot fact — a peer
    // catches up minutes later, and the condition is what tells the UI (and the placement rule)
    // that this may now start elsewhere. Recomputed each pass, written only when it actually
    // changed, so a converged workspace is idle.
    if prev.phase == crd::Phase::Stopped {
        let replicated = replicated_condition(ctx, &id, &w.name_any(), replicas_of(ctx, &id), &prev.conditions, gen).await?;
        let conditions = replaced(&cleared_node_dead(&prev.conditions), replicated);
        if prev.observed_generation != Some(gen) || !conditions_eq(&prev.conditions, &conditions) {
            let st = crd::WorkspaceStatus { observed_generation: Some(gen), conditions, ..prev };
            write_ws_status(w, st, ctx).await?;
        }
        return Ok(Action::requeue(TICK));
    }
    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    // The workspace's OWN name, never `id` (which is `volume_ref` — the SOURCE volume for a
    // shared-volume clone). Deleting by `id` here would stop the clone by killing its source's
    // pod, taking a running workspace down with it.
    //
    // Nothing ran, nothing to cut: with no pod there is no writer, so the worktree holds exactly
    // what its last snapshot or sync point already does. An environment cannot read its pods the
    // same way — `drain_services` scales its StatefulSets to zero on the way in, so "no pods now"
    // says nothing — so it asks the same question of the disk instead: whether its worktree was
    // ever materialised.
    let cut = prev.pod_ref.is_some();
    if cut {
        match stop_push(&stop_name(w), &w.spec.owner, &id, &w.name_any(), w, crd::SnapshotState::of_workspace(w), ctx).await? {
            StopPush::Landed => {}
            StopPush::Waiting => {
                let conditions = ws_conditions(
                    &prev,
                    crd::condition("Progressing", true, "FlushBeforeStop", "waiting for the final sync point", gen),
                );
                // Deliberately NOT `phase: stopped`: the pod is still up, and observed_generation
                // stays unset so the already-stopped guard above does not swallow the next pass.
                let st = crd::WorkspaceStatus { observed_generation: None, conditions, ..prev };
                write_ws_status(w, st, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
        }
    }
    delete_ignoring_404(&Api::<Pod>::namespaced(ctx.client.clone(), &ns), &w.name_any()).await?;
    // The `stop-{ws}-{gen}` CR is KEPT. It is a transient now, not a snapshot: `status.head` never
    // names it, so deleting it here would leave the stopped worktree with no sync point anywhere —
    // the last beat's transient was already reclaimed when this one turned Ready, and every
    // replica's `pull_volume` drops a CR-less subvolume within a cycle. A later re-host would then
    // fall all the way back to `head`, losing exactly what the cut above just took.
    //
    // Poke every placeable peer: the cut exists NOW, and waiting out the pull beat is what used to
    // make a cross-node start take minutes. Best-effort by construction — the ticker still comes.
    // Only when there WAS a cut: a workspace that never ran has nothing new for a peer to fetch,
    // so waking the whole fleet would be a cluster-wide listing per no-op stop.
    if cut {
        let live = crate::peer::placeable_nodes(ctx).await;
        crate::peer::wake_peers(ctx, &live, &ctx.peer_secret).await;
    }
    // `ws_conditions`, not a bare vec: a stop that dropped `PackagesReady` left the web
    // showing "installing packages…" for a workspace that is simply off.
    let replicated = replicated_condition(ctx, &id, &w.name_any(), replicas_of(ctx, &id), &prev.conditions, gen).await?;
    let conditions = replaced(&cleared_node_dead(&ws_conditions(&prev, stopped_condition(gen))), replicated);
    let st = crd::WorkspaceStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        pod_ref: None,
        conditions,
        ..prev
    };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::requeue(TICK))
}


pub async fn cleanup_workspace_worktree(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // `volumeRef` names the volume this worktree lives under — its OWN volume normally, the
    // SOURCE volume for a shared clone (see `resolve_volume`'s `shared` arm). Either way the
    // worktree under it is named by this workspace's own id, same as every checkout call.
    let volume = w.status.as_ref().and_then(|s| s.volume_ref.clone());
    cleanup_parent(w, volume, |w: &crd::Workspace| w.status.as_ref().and_then(|s| s.volume_ref.clone()), ctx).await
}


/// The delete path shared by both parents. Every step is idempotent, because a failed detach
/// requeues the whole thing:
///   1. drop the parent's worktree — `{pool}/vol/{volume}/live/{id}`, the LIVE subvolume only;
///      `snap/` (the snapshots) is never touched, which is the whole point of the exercise.
///   2. delete every SYNC POINT of that worktree, whatever its phase — replication state for a
///      worktree that no longer exists, which nothing else ever reclaims — except one an
///      unmaterialized `SeededFrom` Volume still names, which is a rescue clone's only source. A
///      snapshot (a push) is never touched here, by any parent, for any reason.
///   3. if a snapshot remains on the Volume — this worktree's or another's — detach it so that
///      snapshot outlives its parent; otherwise leave the ownerReference and let GC delete the
///      Volume as it always did.
///
/// `volume` is the CACHED status — which may be stale by a whole reconcile. A restore deleted 1.4 s
/// after it was created was applied from a cache whose status was still empty (2026-09-12,
/// centralindia-k3s): the stalled apply attached this parent to a Volume, this cleanup saw
/// `volumeRef: null`, returned Ok on the spot, and the finalizer wrapper REMOVED the finalizer on
/// that Ok — leaving an owner entry on a Volume nothing would ever detach, so GC took the Volume
/// and the Ready push snapshot on it. So a missing `volumeRef` is a question, never an answer:
/// re-read the parent live, and failing that go looking for our uid on the Volumes themselves.
pub(crate) async fn cleanup_parent<K, F>(parent: &K, volume: Option<String>, volume_of: F, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>
where
    K: Resource<DynamicType = ()> + ResourceExt + Clone + std::fmt::Debug + serde::de::DeserializeOwned,
    F: Fn(&K) -> Option<String>,
{
    let id = parent.name_any();
    let uid = parent.uid().unwrap_or_default();
    let volume = match volume {
        Some(v) => v,
        None => {
            let api: Api<K> = Api::all(ctx.client.clone());
            let live = api.get_opt(&id).await.map_err(|e| ReconcileErr(e.to_string()))?.as_ref().and_then(&volume_of);
            match live {
                Some(v) => v,
                None => return detach_unknown_owners(&id, &uid, ctx).await,
            }
        }
    };
    let uid = uid.as_str();
    let (engine, wt) = (ctx.engine.clone(), id.clone());
    let vol = volume.clone();
    tokio::task::spawn_blocking(move || engine.drop_worktree(&vol, &wt))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(|e| ReconcileErr(e.0))?;

    // One list serves both remaining steps: what to delete, and whether a snapshot remains.
    // `spec.volume` is a selectable field, so this is a server-side filter. The two sets cannot
    // overlap — one is `transient`, the other is not — so the check needs no re-read.
    let snaps: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let items = snaps
        .list(&kube::api::ListParams::default().fields(&format!("spec.volume={volume}")))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .items;
    // The same predicate `retain` applies, for the same reason: an interrupted parent's rescue
    // clone (`VolumeSource::SeededFrom`) names one of these cuts by id and has not copied the bytes
    // yet, so deleting it settles that clone `Permanent/NoSuchSnapshot` — the documented recovery
    // path destroyed by an ordinary delete. An Err, never an empty set: a half-seen listing is
    // exactly the case that deletes what is still needed, and the finalizer retries the whole pass.
    let seeded = crate::snapshot::seeded_from_cuts(ctx, &volume).await?;
    for s in items.iter().filter(|s| !s.is_snapshot() && s.spec.worktree == id && !seeded.contains(&s.name_any())) {
        delete_ignoring_404(&snaps, &s.name_any()).await?;
    }
    // Not scoped to this worktree: a snapshot of a SIBLING worktree is just as good a reason
    // to keep the Volume alive — the bytes it names live on the same subvolume tree.
    //
    // Any phase but `Error`, NOT just `Ready`: a push still being cut when its parent was
    // deleted is exactly the case that must not lose the Volume — GC would take the subvolume out
    // from under the cut and leave an orphan record naming a Volume that is gone. A
    // status-less record has never been cut at all and counts the same way; only `Error` is a
    // record that will never name bytes.
    let has_snapshot = items.iter().any(|s| {
        s.is_snapshot() && s.status.as_ref().is_none_or(|st| st.phase != crd::Phase::Error)
    });
    if has_snapshot {
        // Siblings on one volume (a source, its clone, a restore) are deleted together and lose
        // this CAS to each other; `detach_volume` re-reads on every call, so a few tries in place
        // settle it now rather than after a RETRY-long requeue the probe's orphan check outwaits.
        let mut detached = false;
        for _ in 0..3 {
            if super::super::volume::detach_volume(ctx, &volume, uid).await.map_err(|e| ReconcileErr(e.to_string()))? {
                detached = true;
                break;
            }
        }
        if !detached {
            // Someone else kept rewriting the owner list under us. An Err, not a requeue: the
            // finalizer combinator REMOVES the finalizer on any Ok from Cleanup, which would let
            // GC take the Volume — and the snapshots — while we were still trying to detach it.
            return Err(ReconcileErr(format!("volume {volume}: owner references changed under the detach")));
        }
    }
    Ok(Action::await_change())
}


/// The backstop for a cleanup that knows of no volume at all: sweep the Volumes for our own
/// ownerReference and detach every one we find. A parent that left an entry behind is a Volume
/// Kubernetes GC will collect — with its snapshots — the moment we are the last owner, and an Ok
/// from Cleanup is what removes the finalizer, so this has to run BEFORE the Ok, not on a later
/// beat that no longer has an object to run on.
///
/// A cluster-wide list is affordable here: a region holds hundreds of Volumes, not millions, and
/// this path only runs on a delete that already lost its `volumeRef`.
async fn detach_unknown_owners(id: &str, uid: &str, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let vols: Api<crd::Volume> = Api::all(ctx.client.clone());
    let items = vols.list(&kube::api::ListParams::default()).await.map_err(|e| ReconcileErr(e.to_string()))?.items;
    for v in items.iter().filter(|v| v.owner_references().iter().any(|o| o.uid == uid)) {
        let name = v.name_any();
        tracing::warn!(parent = %id, volume = %name, "cleanup.owner.unknown");
        // Same three tries as the ordinary detach: siblings deleted together lose this CAS to
        // each other, and a lost detach must be an Err so the finalizer stays on.
        let mut detached = false;
        for _ in 0..3 {
            if super::super::volume::detach_volume(ctx, &name, uid).await.map_err(|e| ReconcileErr(e.to_string()))? {
                detached = true;
                break;
            }
        }
        if !detached {
            return Err(ReconcileErr(format!("volume {name}: owner references changed under the detach")));
        }
    }
    Ok(Action::await_change())
}


/// Task 7b: a volume claimed on this node may still be on the
/// OLD layout (`live` itself is the single RW subvolume, pre-dating this whole feature) — the
/// pod that's about to mount it needs `live/{volume}` instead. `Engine::migrate_volume` does the
/// physical rename and returns `true` only the one time it actually moved anything; that's the
/// signal to mint the migration-baseline `Snapshot` CR (CR-first, same shape `create_snapshot` in
/// `api.rs` uses for a normal push) — the EXISTING `reconcile_snapshot`/`advance_head` machinery
/// then cuts it and marks it Ready, so this function only ever needs to run once per volume, not
/// re-implement any of that. It does NOT advance `status.head` — a sync point never does — which is
/// why the `HeadUnknown` guard below has to let a migrated volume through on the worktree it
/// already has on disk rather than on a head.
///
/// A worktree named after the volume's own id is exactly what a pre-model workspace already is
/// (workspace id == volume id, module doc in `snapshot.rs`) and exactly what `checkout`'s
/// `WORKTREE_EXISTS` guard converges on right below this call — so the caller needs no branch for
/// "just migrated" vs. "always was snapshot-model-native".
///
/// Owned by the PARENT (Workspace/Environment), not the Volume, unlike a push (`api.rs`): a
/// baseline only ever exists because a pre-model volume was migrated under one specific parent,
/// and a Volume that only ever had its baseline is not worth keeping once that parent is gone — so
/// the baseline dies with the parent rather than outliving it as an orphan CR for a workspace that
/// no longer exists (13 were found on the cluster that way before this had an owner at all). A
/// A push is different: it is worth keeping across a re-clone/re-attach of the same volume, so it
/// stays owned by the Volume.
pub(crate) async fn migrate_and_seed_baseline(
    ctx: &Arc<Ctx>,
    vol: &crd::Volume,
    parent_ref: OwnerReference,
    owner: &str,
    state: crd::SnapshotState,
) -> Result<bool, ReconcileErr> {
    let id = &vol.name_any();
    let (engine, vol_id) = (ctx.engine.clone(), id.to_string());
    let migrated = tokio::task::spawn_blocking(move || engine.migrate_volume(&vol_id))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(|e| ReconcileErr(e.0))?;
    if !migrated {
        return Ok(false);
    }
    let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let name = crd::snapshot_name(id);
    let mut snap = crd::Snapshot::new(
        &name,
        crd::SnapshotSpec {
            volume: id.to_string(),
            owner: owner.to_string(),
            worktree: id.to_string(),
            parent: String::new(),
            // A SYNC POINT, not a push: nobody asked for it, so it must not show up in history as
            // a snapshot the person took, and it must not keep the Volume alive after its parent
            // is deleted (`cleanup_parent` keeps snapshots, drops sync points). It exists only so
            // a peer has something of a pre-model volume to replicate.
            message: None,
            transient: true,
            state: Some(state),
        },
    );
    snap.metadata.labels = Some(crd::snapshot_labels(owner, id));
    snap.metadata.owner_references = Some(vec![parent_ref]);
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
    // Same convergence rule as everything else in this cutover: a retry that finds the CR already
    // there (crash between the rename above landing and this create) is not an error.
    match api.create(&PostParams::default(), &snap).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(true),
        Err(e) => Err(ReconcileErr(e.to_string())),
    }
}
