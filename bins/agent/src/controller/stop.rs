//! The stop-before-teardown cut both parent kinds share: cut a final sync point and, once it is
//! Ready, let the caller tear down. Nothing waits here for a replica — that judgement is the
//! `Replicated` condition below, and the placement rule that reads it.
//!
//! Kind-agnostic on purpose — a `Workspace` and an `Environment` share no status type, and this is
//! the one place a stop request's lifecycle is decided.

use super::{owner_ref_of_kind, Ctx, ReconcileErr};
use crate::sync;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::api::{ListParams, PostParams};
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::crd;
use std::sync::Arc;

/// What a fixed-name stop request says about its push: landed, or still being cut. The caller
/// writes ITS OWN status — the two parent kinds share no status type, and this is the one place
/// the request's lifecycle is decided.
///
/// `Landed` carries nothing. It used to carry WHY the final sync point had not been replicated,
/// because the stop waited for that and gave up after ten minutes; the wait moved into placement
/// (a stopped parent starts on its own node until a peer is up to date), so a stop is never
/// "unreplicated", only not-yet-replicated — which the `Replicated` condition says, on the object,
/// for as long as it is true.
pub(crate) enum StopPush {
    Landed,
    // No `Failed`: `commit_worktree`'s cut is keep-biased and never marks a `Snapshot` `Error` —
    // it just retries `Working` forever. A wedged stop-push is a `Waiting` that never lands, not a
    // distinct failure state, and it keeps the pod up rather than tearing down without a cut.
    Waiting,
}

/// `stop-{parent}-{generation}`: one object per STOP, keyed by the parent's generation (Start and
/// Stop each bump it), so a retried pass converges on the same name and a later stop gets a new
/// one. The name must never repeat across stops: a replica that pulled `stop-{ws}` once already
/// held a subvolume under that name, the next stop's cut found it present and reported Ready
/// without cutting, so a copy of the PREVIOUS stop passed for this one's — nothing since the last
/// sync beat ever left the node. The caller KEEPS the object once its
/// teardown completes — it is the stopped worktree's one remaining sync point, and the thing a
/// re-host on another node checks out in preference to `head`; `retain`'s one-transient-per-
/// worktree rule removes it (and any older-generation stop) once something newer is Ready.
///
/// Only `Landed` proceeds: a parent torn down without a landed cut loses its last state for good,
/// so a cut that has not happened must stop the teardown rather than be waved through.
/// `await_change` is safe there because both parent controllers watch `Snapshot` and map it back
/// by ownerReference — the parent is woken by the request's own status moving, and by an operator
/// deleting it and letting the `None` arm below create a fresh one.
pub(crate) fn stop_name<P: ResourceExt>(parent: &P) -> String {
    format!("stop-{}-{}", parent.name_any(), parent.meta().generation.unwrap_or(0))
}

pub(crate) async fn stop_push<P>(
    name: &str,
    owner: &str,
    volume: &str,
    worktree: &str,
    parent: &P,
    ctx: &Arc<Ctx>,
) -> Result<StopPush, ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt,
{
    let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    // A CR being deleted is ABSENT. The stale path below deletes this object, and a `Ready` one
    // that is still terminating (a finalizer holds it) would otherwise read as a landed push for
    // the NEXT stop — tearing that one down without pushing at all.
    let cr = api.get_opt(name).await?.filter(|s| s.metadata.deletion_timestamp.is_none());
    let phase = cr.as_ref().map(|s| s.status.as_ref().map(|st| st.phase).unwrap_or(crd::Phase::Pending));
    match phase {
        Some(crd::Phase::Ready) => Ok(StopPush::Landed),
        // Still being cut. Unbounded on purpose: the cut is local btrfs work with nothing to wait
        // on but itself, and tearing a pod down before it lands is the one thing that loses data.
        Some(_) => Ok(StopPush::Waiting),
        None => {
            // A sync point, not a commit: a stop is the last moment the worktree's bytes can be
            // replicated, and a `push` here would put a commit nobody asked for on the history the
            // user sees. Its parent is this node's newest sync point so the puller sends a delta.
            let parent_sync = crate::snapshot::latest_transient(ctx, volume, worktree).await?.unwrap_or_default();
            let mut snap = crd::Snapshot::new(
                name,
                crd::SnapshotSpec {
                    volume: volume.to_string(),
                    owner: owner.to_string(),
                    worktree: worktree.to_string(),
                    parent: parent_sync,
                    message: Some("stopping".to_string()),
                    pinned: false,
                    transient: true,
                },
            );
            snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
            // Owned by the parent so the CR's own events map back to it — that watch is what wakes
            // the `Waiting` arm. The cascade delete matters too: the CR outlives the teardown as
            // the stopped worktree's sync point, so deleting the parent must take it with it.
            snap.metadata.owner_references = Some(vec![owner_ref_of_kind(parent)?]);
            snap.metadata.labels = Some(crd::commit_labels(owner, volume));
            // The label both parent controllers select their watch by. A view, like every label
            // here — the ownerReference above is what the mapper actually reads.
            snap.metadata.labels.get_or_insert_with(Default::default).insert(crd::STOP_LABEL.to_string(), parent.name_any());
            // The same stamp the sync beat writes, read the same way. Without it the beat sees a
            // sync point it did not cut, reads its generation as 0, and cuts one redundant
            // transient after every single stop.
            let (engine, vol, wt) = (ctx.engine.clone(), volume.to_string(), worktree.to_string());
            match tokio::task::spawn_blocking(move || engine.generation(&vol, &wt)).await {
                Ok(Ok(g)) => {
                    snap.metadata.annotations.get_or_insert_with(Default::default).insert(sync::SYNCED_GENERATION.to_string(), g.to_string());
                }
                // Keep-biased, as the beat is: an unreadable generation costs one extra sync point
                // later, while failing the stop costs the teardown.
                Ok(Err(e)) => tracing::warn!(worktree = %worktree, error = %e.0, "stop: reading the worktree generation"),
                Err(e) => tracing::warn!(worktree = %worktree, error = %e, "stop: generation task panicked"),
            }
            match api.create(&PostParams::default(), &snap).await {
                // Lost the race with our own earlier pass; it is the same CR either way.
                Ok(_) => {}
                Err(kube::Error::Api(s)) if s.code == 409 => {}
                Err(err) => return Err(err.into()),
            }
            Ok(StopPush::Waiting)
        }
    }
}

/// THE "is it replicated" truth, computed in exactly one place — the owner's reconcile of a
/// stopped parent — and read everywhere else (the dead-node sweep, `/v1`, the web). One
/// field-selected `VolumeReplica` list plus one field-selected `Snapshot` list per reconcile of a
/// stopped parent; both are cheap and neither runs for a running one.
///
/// `replicas: 1` is not a second reason: no standby can ever be up to date, so the answer is the
/// same `False/AwaitingReplica` with a message that says it will never change. An operator reads
/// one condition either way.
pub async fn replicated_condition(
    ctx: &Arc<Ctx>,
    volume: &str,
    worktree: &str,
    replicas: u32,
    prev: &[Condition],
    gen: i64,
) -> Result<Condition, ReconcileErr> {
    let was = prev.iter().find(|c| c.type_ == REPLICATED);
    if replicas <= 1 {
        return Ok(crd::condition_since(was, REPLICATED, false, "AwaitingReplica", NO_REPLICA_CONFIGURED, gen));
    }
    let newest = crate::peer::newest_transient(ctx, volume, worktree).await?;
    let lp = ListParams::default().fields(&format!("spec.volume={volume}"));
    let rows = Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&lp).await?;
    // Never my own row: the point of the final sync point is that ANOTHER node holds it. The
    // `spec.volume` re-check stays — a field selector narrows a query and is never what decides.
    let held = rows
        .items
        .iter()
        .filter(|r| r.spec.volume == volume && r.spec.node != ctx.node)
        .any(|r| crate::peer::up_to_date(r, worktree, newest.as_deref()));
    Ok(if held {
        crd::condition_since(was, REPLICATED, true, "Replicated", "another node holds the final sync point", gen)
    } else {
        crd::condition_since(was, REPLICATED, false, "AwaitingReplica", "no other node holds the final sync point yet", gen)
    })
}

/// While the parent runs, no other node is an option whatever the copies hold — its live edits are
/// on this disk only. Written in the same status write that records the pod, so the answer is
/// never a stale leftover from the last stop.
pub(crate) fn running_condition(prev: &[Condition], gen: i64) -> Condition {
    let was = prev.iter().find(|c| c.type_ == REPLICATED);
    crd::condition_since(was, REPLICATED, false, "Running", "running here; its live edits are on this node only", gen)
}

pub(crate) const REPLICATED: &str = "Replicated";

const NO_REPLICA_CONFIGURED: &str = "no replica is configured for this volume";
