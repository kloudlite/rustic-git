//! The stop-before-teardown cut both parent kinds share: cut a final sync point and, once it is
//! Ready, let the caller tear down. Nothing waits here for a replica — that judgement is the
//! `Replicated` condition below, and the placement rule that reads it.
//!
//! Kind-agnostic on purpose — a `Workspace` and an `Environment` share no status type, and this is
//! the one place a stop request's lifecycle is decided.

use super::{owner_ref_of_kind, Ctx, ReconcileErr};
use crate::sync;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use k8s_openapi::api::core::v1::Node;
use kube::api::{ListParams, PostParams};
use kube::{Api, Resource, ResourceExt};
use kloudlite_workspaces::crd;
use std::collections::HashSet;
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
    // No `Failed`: `snapshot_worktree`'s cut is keep-biased and never marks a `Snapshot` `Error` —
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
    state: crd::SnapshotState,
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
            // A sync point, not a snapshot: a stop is the last moment the worktree's bytes can be
            // replicated, and a `push` here would put a snapshot nobody asked for on the history the
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
                    transient: true,
                    state: Some(state),
                },
            );
            snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
            // Owned by the parent so the CR's own events map back to it — that watch is what wakes
            // the `Waiting` arm. The cascade delete matters too: the CR outlives the teardown as
            // the stopped worktree's sync point, so deleting the parent must take it with it.
            snap.metadata.owner_references = Some(vec![owner_ref_of_kind(parent)?]);
            snap.metadata.labels = Some(crd::snapshot_labels(owner, volume));
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
                Ok(Err(e)) => tracing::warn!(name = %worktree, reason = "read", error = %e.0, "sync.generation.failed"),
                Err(e) => tracing::warn!(name = %worktree, reason = "panicked", error = %e, "sync.generation.failed"),
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

/// Where a volume should start next. `None` means "here" — the owner keeps it; `Some(node)` means
/// this pass has already released the pin and un-placed every parent, so the named node's claim
/// takes it over.
///
/// Only the OWNER runs this, and only when the volume is MOVABLE (no parent on it running).
/// Nothing is ever moved while running and nothing is copied from a live tree; a stopped sibling
/// on a volume with a running parent therefore starts on the owner, because that is where the
/// volume is.
///
/// The candidate set is `{owner} ∪ {nodes up to date for EVERY stopped parent on the volume}` —
/// every parent, because a node that holds one worktree's cut and not another's would strand the
/// other. The choice is rendezvous on the volume id, so it is deterministic (a retry lands on the
/// same answer), even by count, and computed identically by every node with no coordinator.
///
/// If the preferred node never claims (it died in between), nothing is stuck: the volume is
/// released, so the dead-node sweep's own rule lets any up-to-date node take it.
pub async fn start_placement(
    ctx: &Arc<Ctx>,
    volume: &crd::Volume,
    parents: &[crate::listing::Parent],
) -> Result<Option<String>, ReconcileErr> {
    let id = volume.name_any();
    // Not movable: decided locally, with no API calls at all — this runs on every start.
    if parents.iter().any(|p| p.is_live_worktree()) {
        return Ok(None);
    }
    // Only the owner may give a volume away: it is the one node that certainly is not mid-takeover,
    // and a non-owner's release would race the owner's own reconcile of the same volume.
    if volume.spec.node_name != ctx.node {
        return Ok(None);
    }
    let lp = ListParams::default().fields(&format!("spec.volume={id}"));
    let rows = Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&lp).await?.items;
    let nodes = Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await?.items;
    let (floor, now) = (crate::peer::node_dead_secs(&ctx.settings), k8s_openapi::jiff::Timestamp::now());
    // A dead or draining node is no candidate: handing it the volume would strand every parent
    // until the sweep took it back.
    let live: Vec<crd::VolumeReplica> = rows
        .into_iter()
        .filter(|r| r.spec.node != ctx.node)
        .filter(|r| !crate::peer::unplaceable(nodes.iter().find(|n| n.name_any() == r.spec.node), floor, now))
        .collect();

    // Intersection across every parent: a candidate must be up to date for ALL of them.
    let mut candidates: Option<HashSet<String>> = None;
    for p in parents {
        let newest = crate::peer::newest_transient(ctx, &id, &p.name).await?;
        let ok: HashSet<String> =
            crate::peer::up_to_date_nodes(&p.name, newest.as_deref(), &live).into_iter().collect();
        candidates = Some(match candidates {
            None => ok,
            Some(prev) => prev.intersection(&ok).cloned().collect(),
        });
    }
    let mut set: Vec<String> = candidates.unwrap_or_default().into_iter().collect();
    // The owner is always a candidate: it holds the bytes by construction.
    set.push(ctx.node.clone());
    set.sort();
    let Some(preferred) = crate::peer::preferred_node(&id, &set) else { return Ok(None) };
    if preferred == ctx.node {
        return Ok(None);
    }
    // The two-step move, deliberately kept over an owner-writes-the-target handoff: a handoff
    // would need the admission policy to allow ANY `nodeName` change, and this reuses the CAS the
    // takeover path already proved. Pin first, parents second — the reverse leaves parents
    // claimable on a node that does not own the volume. A crash BETWEEN them leaves a cleared pin
    // with placed parents: here the node the parents still name is this live owner, whose own
    // `resolve_volume` takes the empty pin back (`spec.node_name.is_empty()` → `take_volume`). That
    // heal never runs when a DEAD node's sweep crashes there — the node it needs is the gone one —
    // which is why `sweep_volumes` treats an empty pin with placed parents as its own case.
    if !crate::controller::volume::release_volume(ctx, &id, &ctx.node).await? {
        return Ok(None); // someone else moved it first; next pass re-decides against the new owner
    }
    for p in parents {
        crate::peer::unplace_parent(ctx, p).await;
    }
    tracing::info!(volume = %id, node = %preferred, reason = "spread", "volume.moved");
    Ok(Some(preferred))
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
