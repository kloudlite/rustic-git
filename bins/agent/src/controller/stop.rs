//! The stop-before-teardown gate both parent kinds share: cut a final sync point, wait until
//! another node's replica reports `Synced` at or after it, then let the caller tear down.
//!
//! Kind-agnostic on purpose — a `Workspace` and an `Environment` share no status type, and this is
//! the one place a stop request's lifecycle is decided. Split out of `controller.rs` unchanged.

use super::{owner_ref_of_kind, Ctx, ReconcileErr};
use crate::sync;
use kube::api::PostParams;
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::crd;
use std::sync::Arc;
use std::time::Duration;

/// What a fixed-name stop request says about its push: landed, failed, or still to wait for
/// (including "just created it"). The caller writes ITS OWN status — the two parent kinds share
/// no status type, and this is the one place the request's lifecycle is decided.
pub(crate) enum StopPush {
    /// `unreplicated` is `Some(why)` when no other node holds the final sync point — the teardown
    /// proceeds anyway (a stop that never finishes is worse than one whose last seconds live on
    /// one node), but the caller must say so in its condition, and the string says WHY: a timed-out
    /// flush and a region with nowhere to replicate TO are the same outcome for very different
    /// reasons, and an operator reading the condition needs to tell them apart.
    Landed { unreplicated: Option<&'static str> },
    // No `Failed`: `commit_worktree`'s cut is keep-biased and never marks a `Snapshot` `Error` —
    // it just retries `Working` forever (Task 8; see `stop_push`'s doc). A wedged stop-push is a
    // `Waiting` that never lands, not a distinct failure state.
    Waiting,
}

/// The stop-before-teardown gate a stopping parent waits on: create the request once, then wait
/// until its own status says `done`.
///
/// The parent generation a stop request was created for. An annotation, not a label: nothing
/// selects on it, and a label is a view of `spec` while this is a fact about the request itself.
const STOP_GENERATION: &str = "rustic-git.io/stop-generation";

/// `stop-{parent}-{generation}`: one object per STOP, keyed by the parent's generation (Start and
/// Stop each bump it), so a retried pass converges on the same name and a later stop gets a new
/// one. The name must never repeat across stops: a replica that pulled `stop-{ws}` once already
/// held a subvolume under that name, the next stop's cut found it present and reported Ready
/// without cutting, and the flush gate then counted a copy of the PREVIOUS stop as this one's —
/// nothing since the last sync beat ever left the node. The caller KEEPS the object once its
/// teardown completes — it is the stopped worktree's one remaining sync point, and the thing a
/// re-host on another node checks out in preference to `head`; `retain`'s one-transient-per-
/// worktree rule removes it (and any older-generation stop) once something newer is Ready.
///
/// Only `Landed` proceeds. `Failed` leaves the parent running with `Ready=False` and nothing torn
/// down: a parent torn down without a landed push loses its last state for good, so a push that
/// failed must stop the teardown rather than wave it through. `await_change` is safe there because
/// both parent controllers watch `Snapshot` and map it back by ownerReference — the parent
/// is woken by the request's own status moving, and by an operator deleting it and letting the
/// `None` arm below create a fresh one.
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
    let gen = parent.meta().generation.unwrap_or(0).to_string();
    match phase {
        Some(crd::Phase::Ready) => {
            let ready_at = cr.as_ref().and_then(|s| s.status.as_ref()).and_then(|st| st.ready_at.clone());
            match flush_gate(volume, ready_at.as_deref(), ctx).await? {
                StopPush::Waiting if flush_expired(cr.as_ref()) => {
                    tracing::warn!(%volume, "stop: no replica holds the final sync point; tearing down anyway");
                    Ok(StopPush::Landed { unreplicated: Some(FLUSH_TIMED_OUT) })
                }
                other => Ok(other),
            }
        }
        // Still being cut — bounded by the same clock, so a wedged `commit_worktree` cannot park
        // the teardown forever.
        Some(_) if flush_expired(cr.as_ref()) => {
            tracing::warn!(%volume, "stop: the final sync point never became Ready; tearing down anyway");
            Ok(StopPush::Landed { unreplicated: Some(FLUSH_TIMED_OUT) })
        }
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
            snap.metadata.annotations.get_or_insert_with(Default::default).insert(STOP_GENERATION.to_string(), gen);
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

/// `WS_STOP_FLUSH_TIMEOUT_SECS`, default 600. Read at call time, not cached: it is a bound on how
/// long a person waits for a stop, and the only reason it is configurable at all is that a slow
/// link makes the right number a property of the cluster.
pub fn flush_timeout() -> Duration {
    Duration::from_secs(std::env::var("WS_STOP_FLUSH_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600))
}

/// The whole wait's bound, measured from the stop request's OWN creation rather than from the
/// cut's `readyAt`. `readyAt` only exists once the cut succeeded, and `commit_worktree` is
/// keep-biased — a worktree whose cut can never succeed retries `Working` forever — so a bound on
/// the replicated leg alone would let a broken worktree park its parent's teardown for good.
/// A request with no `creationTimestamp` (only a fixture) never expires.
fn flush_expired(cr: Option<&crd::Snapshot>) -> bool {
    let Some(created) = cr.and_then(|s| s.metadata.creation_timestamp.as_ref()) else { return false };
    // Whole seconds via jiff (what `k8s_openapi::Time` wraps): a bound measured in minutes has no
    // use for sub-second precision, and a negative age (clock skew) is zero elapsed.
    let age = k8s_openapi::jiff::Timestamp::now().as_second() - created.0.as_second();
    age.max(0) as u64 >= flush_timeout().as_secs()
}

/// Ready is not landed: the point of the final sync point is that ANOTHER node holds it, so the
/// gate waits until some replica reports `Synced` at or after the cut became Ready.
async fn flush_gate(volume: &str, ready_at: Option<&str>, ctx: &Arc<Ctx>) -> Result<StopPush, ReconcileErr> {
    // A single-node region has nowhere for the bytes to go, so no peer can EVER report `Synced`
    // and the wait below can only end in the timeout — ten minutes added to every stop in the
    // region to learn what the node list already says. Keep-biased: a list error is "we do not
    // know", which falls through to the real wait rather than waving the teardown through.
    // This is also why a never-started environment no longer waits: its volume has no replica
    // anywhere, and on a single-node region that is now answered immediately.
    if crate::peer::pool_nodes(&ctx.client).await.is_ok_and(|nodes| nodes.iter().all(|n| n == &ctx.node)) {
        return Ok(StopPush::Landed { unreplicated: Some(NO_PEERS) });
    }
    // A cut from before `readyAt` existed gives nothing to compare a `lastSyncAt` against, and
    // waiting on a comparison that can never be made would park the stop forever.
    let Some(ready) = ready_at.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) else {
        return Ok(StopPush::Landed { unreplicated: Some(NO_READY_AT) });
    };
    // Server-side now that `VolumeReplica` declares `.spec.volume` selectable — this ran once per
    // 15 s tick per stopping parent as a full-cluster replica scan. The `spec.volume` re-check
    // below stays: a field selector narrows a query and is never what decides anything.
    let lp = kube::api::ListParams::default().fields(&format!("spec.volume={volume}"));
    let list = Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&lp).await?;
    // `lastSyncAt` is the instant that pass LISTED the volume's snapshots, not when it wrote its
    // row (see `VolumeReplicaStatus::last_sync_at`) — which is the only reason `>= readyAt` proves
    // the replica's listing actually saw this cut. Parsed, never string-compared: two nodes may
    // stamp the same instant with different offsets.
    let replicated = list.items.iter().filter(|r| r.spec.volume == volume).any(|r| {
        r.spec.node != ctx.node
            && r.status.as_ref().is_some_and(|st| {
                st.phase == "Synced"
                    && st
                        .last_sync_at
                        .as_deref()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .is_some_and(|t| t >= ready)
            })
    });
    Ok(if replicated { StopPush::Landed { unreplicated: None } } else { StopPush::Waiting })
}

/// The three honest `FlushUnreplicated` messages. Separate constants because they are the only
/// thing that tells an operator whether to go looking for a broken link or not.
const NO_PEERS: &str = "no other node in the region holds replicas";

const NO_READY_AT: &str = "the final sync point recorded no readyAt to prove replication against";

const FLUSH_TIMED_OUT: &str = "stopped without a replica holding the final sync point";
