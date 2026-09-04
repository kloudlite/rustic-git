//! The worktree gate: everything a Workspace and an Environment do IDENTICALLY between "my
//! Volume is Ready" and "my pod may start" — migration baseline, newest transient, effective
//! head, checkout, quota. The two kinds differ only in which status struct carries the answer,
//! which is why the gate returns a decision and the callers write their own status.
//!
//! It exists because the two copies of this sequence had already begun to drift, and every
//! divergence between them is a bug in whichever kind was not the one being edited.

use super::{migrate_and_seed_baseline, Ctx, ReconcileErr, TICK};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::runtime::controller::Action;
use kube::ResourceExt;
use kloudlite_git_workspaces::crd;
use std::sync::Arc;

/// What the shared gate decided. The two callers turn it into their own status type — that
/// difference is the only real one between the two ~120-line blocks this replaces.
pub(crate) enum WorktreeGate {
    /// Nothing to wait for: the worktree is checked out and quota is set. Neither caller needs the
    /// resolved head back — a graft (the one thing that reads it) keys on `prev.head`/`clone_commit`,
    /// which the caller already has — so the brief's `head` field is dropped rather than carried
    /// unread; see the task report.
    Ready,
    /// Wait, with the condition reason and message the caller must write. `NoSuchSnapshot` is the
    /// one permanent reason: the caller settles it (needs the object itself, which this function
    /// never receives) instead of writing a normal `Wait` status — `action` is unused there.
    Wait { reason: &'static str, message: String, action: Action },
}

/// The start-time spread, for either parent kind. Only the OWNER may give a volume away, only on
/// a start (the one moment the parent's status still says `Stopped`), and never when anything on
/// the volume is running — `start_placement` holds all three rules; this is the shared call.
pub(crate) async fn start_spread(
    parent_kind: &'static str,
    parent_name: &str,
    volume_id: &str,
    volume: &crd::Volume,
    prev_phase: crd::Phase,
    ctx: &Arc<Ctx>,
) -> Result<Option<String>, ReconcileErr> {
    if prev_phase != crd::Phase::Stopped {
        return Ok(None);
    }
    let Some(siblings) = crate::listing::parents_on_volume(ctx, volume_id).await else { return Ok(None) };
    let Some(node) = super::stop::start_placement(ctx, volume, &siblings).await? else { return Ok(None) };
    tracing::info!(kind = %parent_kind, name = %parent_name, %node, reason = "handover", "volume.moved");
    Ok(Some(node))
}

/// Migrate/seed, resolve the effective head, gate on `HeadUnknown`/`SnapshotPending`/`NoSuchSnapshot`,
/// then checkout and quota. `parent_name` is the worktree's own name — the workspace's or the
/// environment's id, never the volume's, matching `sync.rs`'s `live_worktrees`.
///
/// `owner`, `owner_ref` and `state` are not in the task brief's signature — `migrate_and_seed_baseline`
/// needs all three and neither caller can be reached without them, so they were added here rather
/// than re-derived; see the task report.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn worktree_gate(
    parent_name: &str,
    parent_kind: &'static str,
    volume: &crd::Volume,
    storage: &Option<crd::WorkspaceStorage>,
    prev_head: Option<&str>,
    owner: &str,
    owner_ref: OwnerReference,
    state: crd::SnapshotState,
    ctx: &Arc<Ctx>,
) -> Result<WorktreeGate, ReconcileErr> {
    let id = volume.name_any();

    // Lazy per-volume migration, before anything mounts — a no-op every pass after the first.
    migrate_and_seed_baseline(ctx, volume, owner_ref, owner, state).await?;

    // Re-host: a node that has never run this worktree checks out its LATEST SYNC POINT in
    // preference to `head`, because the sync beat replicated it after the last snapshot. Only when
    // there is no worktree here yet: a live worktree is never swapped under a running pod.
    //
    // Resolved BEFORE the `HeadUnknown` guard below: a transient IS a Snapshot CR, so `has_snapshots`
    // is true when a sync point is all this volume has, and parking there would strand a parent
    // that has perfectly good state to start from.
    let have_worktree = ctx.engine.pool.worktree(&id, parent_name).exists();
    let synced = if have_worktree { None } else { crate::snapshot::latest_transient(ctx, &id, parent_name).await? };
    // A clone/restore pinned to a snapshot already knows its head — grafted by `/v1` at clone/restore
    // time, not guessed here — so it never sees `HeadUnknown` and never bootstraps empty next to
    // the source's real history, even on the very first pass.
    let clone_commit = super::clone_commit(storage);
    let effective_head = synced.or_else(|| prev_head.map(str::to_string)).or_else(|| clone_commit.map(str::to_string));
    // `!have_worktree` is part of the guard, not an optimization: a MIGRATED volume has its
    // worktree on disk already and its baseline is a sync point, so it has records and no head
    // forever. The guard is about never checking out an EMPTY worktree next to real history.
    if effective_head.is_none() && !have_worktree && crate::claim::has_snapshots(ctx, &id).await? {
        return Ok(WorktreeGate::Wait {
            reason: "HeadUnknown",
            message: format!("volume has snapshots but this {} has no recorded head yet", parent_kind.to_lowercase()),
            action: Action::requeue(TICK),
        });
    }
    // Keyed on `effective_head`, not on `prev_head`: a volume whose only state is a sync point has
    // `prev_head == None` but is going to check that sync point out, never the clone snapshot — so
    // failing this parent on a swept snapshot it was never going to use would kill one that has
    // perfectly good state. Only a volume that would ACTUALLY resolve to the clone snapshot can be
    // permanently broken by that snapshot being gone.
    if let Some(commit) = clone_commit {
        let phase = if effective_head.as_deref() == Some(commit) {
            crate::claim::snapshot_phase(ctx, &id, commit).await?
        } else {
            Some(crd::Phase::Ready)
        };
        // `/v1` creates the cut microseconds before the object, so the first reconcile almost
        // always finds it still `Working` — one tick away, never Permanent.
        if crate::claim::snapshot_pending(phase) {
            return Ok(WorktreeGate::Wait {
                reason: "SnapshotPending",
                message: format!("waiting for snapshot {commit} to be cut"),
                action: Action::requeue(TICK),
            });
        }
        if phase != Some(crd::Phase::Ready) {
            // Permanent: only the caller can settle it (it needs the object), so `action` here is
            // never read — the caller recognizes this reason and calls `settle` itself instead.
            return Ok(WorktreeGate::Wait {
                reason: "NoSuchSnapshot",
                message: format!("clone snapshot {commit} is not a ready snapshot of volume {id}"),
                action: Action::await_change(),
            });
        }
    }
    // `WORKTREE_EXISTS` converges a race (this pass and an earlier one both reaching here, or a
    // pod restart finding its own worktree already there) into a no-op rather than an error.
    let (engine, vol_id, wt_id, head) = (ctx.engine.clone(), id.clone(), parent_name.to_string(), effective_head.clone());
    let quota_gb = volume.spec.quota_gb;
    let result = tokio::task::spawn_blocking(move || {
        engine.checkout(&vol_id, head.as_deref(), &wt_id)?;
        // Quota the worktree the instant it exists — waiting for the volume's next reconcile pass
        // would leave a freshly checked-out worktree briefly unquota'd.
        engine.set_quota_worktree(&vol_id, &wt_id, quota_gb)?;
        Ok::<_, kloudlite_git_workspaces::engine::ops::EngErr>(())
    })
    .await
    .map_err(|e| ReconcileErr(e.to_string()))?;
    match result {
        Ok(()) => {}
        Err(e) if e.0 == kloudlite_git_workspaces::engine::snapshot::WORKTREE_EXISTS => {}
        Err(e) => return Err(ReconcileErr(e.0)),
    }
    Ok(WorktreeGate::Ready)
}
