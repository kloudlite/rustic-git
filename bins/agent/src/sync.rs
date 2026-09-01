//! The sync beat: one transient `Snapshot` per live worktree whose btrfs generation has moved.
//!
//! A push is the only thing that writes history; replication can only carry what has been cut, so
//! between two pushes a replica holds nothing of the work in progress. This beat closes that gap
//! by cutting a SYNC POINT — `spec.transient`, never a parent, never a head, retained one per
//! worktree — so the puller has something recent to fetch. It is deliberately a beat and not a
//! reconcile: the thing it reacts to (bytes changing under a running pod) produces no Kubernetes
//! event at all.
//!
//! Keep-biased throughout: every per-object failure warns and moves on, and an unreadable
//! generation cuts NOTHING rather than cutting a redundant snapshot on every pass.

use crate::controller::{owner_ref_of_kind, Ctx};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, ListParams, PostParams};
use kube::ResourceExt;
use rustic_git_workspaces::crd;
use std::sync::Arc;

/// The generation the sync point was cut FROM, on the `Snapshot` itself. An annotation rather than
/// a spec field because it is this beat's private bookkeeping — nothing else in the commit model
/// has any use for a btrfs transaction id.
pub const SYNCED_GENERATION: &str = "rustic-git.io/synced-generation";

/// `WS_SYNC_SECS`, default 60. Lives beside the beat, as `peer::replica_interval` does.
pub fn sync_interval() -> std::time::Duration {
    std::time::Duration::from_secs(std::env::var("WS_SYNC_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60))
}

/// The whole decision: cut only when the worktree's generation is past the last one we cut from.
/// Never-synced (`None`) is always due; equal or lower is a worktree nothing has touched since,
/// and re-cutting it would cost a subvolume and a send for identical bytes.
pub fn due(current: u64, recorded: Option<u64>) -> bool {
    recorded.is_none_or(|g| current > g)
}

/// Sync points are not addressed by name by anything, so the name only has to be unique and
/// recognisable — the random suffix is what lets a new one exist while the previous is still being
/// retained away.
pub fn sync_name(worktree: &str) -> String {
    format!("sync-{worktree}-{}", crd::short_hex())
}

/// A worktree running on this node right now, with everything the create needs.
struct Live {
    volume: String,
    worktree: String,
    owner: String,
    owner_ref: OwnerReference,
}

/// One pass, spawned beside `pull_beat`. Lists what runs here, then cuts at most one sync point per
/// worktree.
pub async fn sync_beat(ctx: &Arc<Ctx>) {
    for live in live_worktrees(ctx).await {
        sync_one(ctx, &live).await;
    }
}

/// Workspaces and Environments whose pod runs on THIS node — the same `status.nodeName == me` plus
/// `status.volume_ref` rule `peer::interesting_volumes` uses, minus its replication half: a standby
/// has no worktree to read a generation from. A workspace also needs a pod (`pod_ref`): without one
/// nothing is writing, so its last sync point is already current.
async fn live_worktrees(ctx: &Arc<Ctx>) -> Vec<Live> {
    let mut out = Vec::new();
    // Server-side scoping, the same selector the parent watches use: this beat has no business
    // pulling every workspace in the cluster over the wire once a minute. The `node_name` check
    // below stays anyway — a field selector narrows a query, and is never the thing that decides
    // whether this node may act on an object.
    let mine = ListParams::default().fields(&format!("status.nodeName={}", ctx.node));
    match Api::<crd::Workspace>::all(ctx.client.clone()).list(&mine).await {
        Ok(list) => {
            for w in &list.items {
                let Some(st) = w.status.as_ref() else { continue };
                if st.node_name != ctx.node || st.phase == crd::Phase::Stopped || st.pod_ref.is_none() {
                    continue;
                }
                let (Some(volume), Ok(owner_ref)) = (st.volume_ref.clone(), owner_ref_of_kind(w)) else { continue };
                out.push(Live { volume, worktree: w.name_any(), owner: w.spec.owner.clone(), owner_ref });
            }
        }
        Err(e) => tracing::warn!(error = %e, "sync: listing workspaces"),
    }
    match Api::<crd::Environment>::all(ctx.client.clone()).list(&mine).await {
        Ok(list) => {
            for e in &list.items {
                let Some(st) = e.status.as_ref() else { continue };
                if st.node_name != ctx.node || st.phase == crd::Phase::Stopped {
                    continue;
                }
                let (Some(volume), Ok(owner_ref)) = (st.volume_ref.clone(), owner_ref_of_kind(e)) else { continue };
                out.push(Live { volume, worktree: e.name_any(), owner: e.spec.owner.clone(), owner_ref });
            }
        }
        Err(e) => tracing::warn!(error = %e, "sync: listing environments"),
    }
    out
}

async fn sync_one(ctx: &Arc<Ctx>, live: &Live) {
    let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let list = match api.list(&ListParams::default().fields(&format!("spec.volume={}", live.volume))).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(volume = %live.volume, error = %e, "sync: listing snapshots");
            return;
        }
    };

    let mut recorded: Option<u64> = None;
    let mut parent = String::new();
    for s in &list.items {
        if !s.spec.transient || s.spec.worktree != live.worktree {
            continue;
        }
        match s.status.as_ref().map(|st| st.phase) {
            // One cut in flight at a time — the same rule `create_commit` applies, and the reason
            // this beat can run on a tick without piling snapshots onto a slow btrfs.
            Some(crd::Phase::Working) => return,
            Some(crd::Phase::Ready) => {
                let gen = s.annotations().get(SYNCED_GENERATION).and_then(|g| g.parse::<u64>().ok());
                if gen >= recorded {
                    recorded = gen;
                    parent = s.name_any();
                }
            }
            _ => {}
        }
    }

    let (engine, volume, worktree) = (ctx.engine.clone(), live.volume.clone(), live.worktree.clone());
    let gen = match tokio::task::spawn_blocking(move || engine.generation(&volume, &worktree)).await {
        Ok(Ok(g)) => g,
        // Keep-biased: an unreadable generation is "we do not know", and cutting on "we do not
        // know" would cut on every single pass. A node without btrfs never syncs at all.
        Ok(Err(e)) => {
            tracing::warn!(worktree = %live.worktree, error = %e, "sync: reading the worktree generation");
            return;
        }
        Err(e) => {
            tracing::warn!(worktree = %live.worktree, error = %e, "sync: generation task panicked");
            return;
        }
    };
    if !due(gen, recorded) {
        return;
    }

    let name = sync_name(&live.worktree);
    let mut snap = crd::Snapshot::new(
        &name,
        crd::SnapshotSpec {
            volume: live.volume.clone(),
            owner: live.owner.clone(),
            worktree: live.worktree.clone(),
            // The previous sync point, so the puller can send a delta against what a replica
            // already holds. Empty on the first one, exactly as a root commit is.
            parent,
            message: None,
            pinned: false,
            transient: true,
        },
    );
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, size_bytes: None, ready_at: None });
    // Owned by the worktree's object: deleting the workspace is the whole delete, and the sync
    // point has no meaning without it.
    snap.metadata.owner_references = Some(vec![live.owner_ref.clone()]);
    snap.metadata.labels = Some(crd::commit_labels(&live.owner, &live.volume));
    snap.metadata.annotations.get_or_insert_with(Default::default).insert(SYNCED_GENERATION.to_string(), gen.to_string());
    match api.create(&PostParams::default(), &snap).await {
        Ok(_) => tracing::info!(%name, worktree = %live.worktree, generation = gen, "sync: cut a sync point"),
        // Lost a race with our own previous pass; the CR is there either way.
        Err(kube::Error::Api(s)) if s.code == 409 => {}
        Err(e) => tracing::warn!(worktree = %live.worktree, error = %e, "sync: creating the sync point"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_only_when_the_generation_moved() {
        assert!(due(5, None));
        assert!(due(6, Some(5)));
        assert!(!due(5, Some(5)));
        assert!(!due(4, Some(5)));
    }
}
