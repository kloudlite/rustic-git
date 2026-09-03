//! The sync beat: one transient `Snapshot` per live worktree whose btrfs generation has moved, or
//! whose definition has changed.
//!
//! A push is the only thing that writes history; replication can only carry what has been cut, so
//! between two pushes a replica holds nothing of the work in progress. This beat closes that gap
//! by cutting a SYNC POINT — `spec.transient`, never a parent, never a head, retained one per
//! worktree — so the puller has something recent to fetch. It is deliberately a beat and not a
//! reconcile: the thing it reacts to (bytes changing under a running pod) produces no Kubernetes
//! event at all.
//!
//! Bytes are not the whole record: every cut freezes the parent's definition (`spec.state`), and a
//! package, image, resources, quota or services change moves no byte at all. A worktree whose
//! newest sync point froze a definition that is no longer the parent's is therefore due as much as
//! one whose generation moved — otherwise a re-host or an interrupted clone comes up with a stale
//! definition it will never be told about.
//!
//! Keep-biased throughout: every per-object failure warns and moves on, and an unreadable
//! generation cuts NOTHING rather than cutting a redundant snapshot on every pass.

use crate::controller::Ctx;
use kube::api::{Api, ListParams, PostParams};
use kube::ResourceExt;
use rustic_git_workspaces::crd;
use std::sync::Arc;

/// The generation the sync point was cut FROM, on the `Snapshot` itself. An annotation rather than
/// a spec field because it is this beat's private bookkeeping — nothing else in the snapshot model
/// has any use for a btrfs transaction id.
///
/// The value is the generation read AFTER the cut (`snapshot::record_post_cut_generation`
/// re-stamps it), not the one this beat writes here: taking a read-only snapshot bumps its SOURCE
/// subvolume's generation by one, so recording the pre-cut value leaves every idle worktree
/// permanently "due" and cutting once per interval forever.
pub use crd::SYNCED_GENERATION;

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

/// The other half of the decision: the newest sync point froze a definition, and the parent's is no
/// longer it. No sync point at all (`None`) is "we have never recorded this definition", which is
/// changed — the same rule `due` applies to a never-synced worktree.
pub fn definition_changed(live: &crd::SnapshotState, recorded: Option<&crd::SnapshotState>) -> bool {
    recorded != Some(live)
}

/// Sync points are not addressed by name by anything, so the name only has to be unique and
/// recognisable — the random suffix is what lets a new one exist while the previous is still being
/// retained away.
pub fn sync_name(worktree: &str) -> String {
    format!("sync-{worktree}-{}", crd::short_hex())
}

/// One pass, spawned beside `pull_beat`. Lists what runs here, then cuts at most one sync point per
/// worktree.
pub async fn sync_beat(ctx: &Arc<Ctx>) {
    // Keep-biased like every other beat: a half-listed cluster cuts nothing. A missed sync point
    // costs one `WS_SYNC_SECS` of freshness on a replica; acting on a partial view costs more.
    let Some(parents) = crate::listing::parents_on_node(ctx).await else { return };
    for p in parents.iter().filter(|p| p.is_live_worktree()) {
        sync_one(ctx, p).await;
    }
}

/// The pure seam: everything about a sync cut's `SnapshotSpec` that does not need a live btrfs
/// read, split out so the state-stamping logic has a test that runs without real btrfs. The
/// generation is not here on purpose — `sync_one` stamps it into the annotation instead.
fn build_sync_spec(live: &crate::listing::Parent, parent: String) -> crd::SnapshotSpec {
    crd::SnapshotSpec {
        volume: live.volume.clone(),
        owner: live.owner.clone(),
        worktree: live.name.clone(),
        // The previous sync point, so the puller can send a delta against what a replica
        // already holds. Empty on the first one, exactly as a root snapshot is.
        parent,
        message: None,
        transient: true,
        state: Some(live.state.clone()),
    }
}

async fn sync_one(ctx: &Arc<Ctx>, live: &crate::listing::Parent) {
    let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let list = match api.list(&ListParams::default().fields(&format!("spec.volume={}", live.volume))).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(volume = %live.volume, error = %e, "sync: listing snapshots");
            return;
        }
    };

    let mut recorded: Option<u64> = None;
    let mut recorded_state: Option<crd::SnapshotState> = None;
    let mut parent = String::new();
    for s in &list.items {
        if !s.spec.transient || s.spec.worktree != live.name {
            continue;
        }
        match s.status.as_ref().map(|st| st.phase) {
            // One cut in flight at a time — the same rule `create_snapshot` applies, and the reason
            // this beat can run on a tick without piling snapshots onto a slow btrfs.
            Some(crd::Phase::Working) => {
                tracing::debug!(worktree = %live.name, snapshot = %s.name_any(), "sync: a transient is Working, skipping this pass");
                return;
            }
            Some(crd::Phase::Ready) => {
                let gen = s.annotations().get(SYNCED_GENERATION).and_then(|g| g.parse::<u64>().ok());
                if gen >= recorded {
                    recorded = gen;
                    recorded_state = s.spec.state.clone();
                    parent = s.name_any();
                }
            }
            _ => {}
        }
    }

    let (engine, volume, worktree) = (ctx.engine.clone(), live.volume.clone(), live.name.clone());
    let gen = match tokio::task::spawn_blocking(move || engine.generation(&volume, &worktree)).await {
        Ok(Ok(g)) => g,
        // Keep-biased: an unreadable generation is "we do not know", and cutting on "we do not
        // know" would cut on every single pass. A node without btrfs never syncs at all.
        Ok(Err(e)) => {
            tracing::warn!(worktree = %live.name, error = %e, "sync: reading the worktree generation");
            return;
        }
        Err(e) => {
            tracing::warn!(worktree = %live.name, error = %e, "sync: generation task panicked");
            return;
        }
    };
    // The bytes may be identical and the cut still worth taking: it is the RECORD that differs,
    // and a sync point is the only place the definition reaches another node.
    if !due(gen, recorded) && !definition_changed(&live.state, recorded_state.as_ref()) {
        return;
    }

    let name = sync_name(&live.name);
    let mut snap = crd::Snapshot::new(&name, build_sync_spec(live, parent));
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
    // Owned by the worktree's object: deleting the workspace is the whole delete, and the sync
    // point has no meaning without it.
    snap.metadata.owner_references = Some(vec![live.owner_ref.clone()]);
    snap.metadata.labels = Some(crd::snapshot_labels(&live.owner, &live.volume));
    snap.metadata.annotations.get_or_insert_with(Default::default).insert(SYNCED_GENERATION.to_string(), gen.to_string());
    match api.create(&PostParams::default(), &snap).await {
        Ok(_) => tracing::info!(%name, worktree = %live.name, generation = gen, "sync: cut a sync point"),
        // Lost a race with our own previous pass; the CR is there either way.
        Err(kube::Error::Api(s)) if s.code == 409 => {}
        Err(e) => tracing::warn!(worktree = %live.name, error = %e, "sync: creating the sync point"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
    use rustic_git_workspaces::kube_test::{mock_client, Recorder, Route};

    struct NoopNix;
    #[async_trait::async_trait]
    impl crate::nix::Nix for NoopNix {
        async fn build(&self, _e: &str, _t: std::time::Duration) -> Result<std::path::PathBuf, String> {
            Ok(std::path::PathBuf::from("/tmp"))
        }
        async fn ping(&self) -> Result<(), String> {
            Ok(())
        }
        async fn collect_garbage(&self) -> Result<u64, String> {
            Ok(0)
        }
    }

    fn test_ctx(pool: &std::path::Path, node: &str, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
        let (client, rec) = mock_client(routes);
        std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
        (
            Arc::new(Ctx::new(
                client,
                Arc::new(Engine::new(EnginePool::new(pool))),
                node.into(),
                pool.to_string_lossy().into(),
                "r1".into(),
                vec![],
                Some("test:/".into()),
                Arc::new(NoopNix),
                pool.join("profiles"),
            )),
            rec,
        )
    }

    #[test]
    fn due_only_when_the_generation_moved() {
        assert!(due(5, None));
        assert!(due(6, Some(5)));
        assert!(!due(5, Some(5)));
        assert!(!due(4, Some(5)));
    }

    /// The sync beat reads this node's parents through the shared listing — and a listing that
    /// could not be completed cuts nothing, rather than treating an empty view as "no worktrees".
    #[tokio::test]
    async fn a_failed_parent_listing_cuts_no_sync_points() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route {
            method: "GET",
            path: "/apis/rustic-git.io/v1alpha1/workspaces".into(),
            status: 500,
            body: serde_json::json!({}),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        sync_beat(&ctx).await;
        assert!(rec.calls().iter().all(|c| !c.starts_with("POST")), "{:?}", rec.calls());
    }

    fn live_fixture() -> crate::listing::Parent {
        crate::listing::Parent {
            kind: "Workspace",
            name: "ws-1".into(),
            volume: "vol-1".into(),
            owner: "alice".into(),
            node_name: "node-a".into(),
            head: None,
            phase: crd::Phase::Ready,
            pod_ref: Some("ws-alice/ws-1".into()),
            owner_ref: Default::default(),
            replicated: false,
            state: crd::SnapshotState::Workspace {
                image: "alpine:3.20".into(),
                packages: vec![],
                resources: Default::default(),
                quota_gb: 5,
                attached_environment: None,
            },
        }
    }

    /// The pure seam `build_sync_spec` is what a real cut always goes through, but a real cut
    /// itself needs `Engine::generation` — a live btrfs read, unavailable on this Mac. This is the
    /// state-stamping assertion without one.
    #[test]
    fn the_sync_spec_carries_the_parents_fields_and_definition() {
        let live = live_fixture();
        let spec = build_sync_spec(&live, "sync-ws-1-prev".into());
        assert_eq!(spec.volume, "vol-1");
        assert_eq!(spec.owner, "alice");
        assert_eq!(spec.worktree, "ws-1");
        assert_eq!(spec.parent, "sync-ws-1-prev");
        assert!(spec.transient);
        assert_eq!(spec.state, Some(live.state));
    }

    fn env_fixture(services: Vec<rustic_git_workspaces::model::Service>) -> crate::listing::Parent {
        crate::listing::Parent {
            kind: "Environment",
            pod_ref: None,
            state: crd::SnapshotState::Environment { services, quota_gb: 20 },
            ..live_fixture()
        }
    }

    fn service(name: &str) -> rustic_git_workspaces::model::Service {
        rustic_git_workspaces::model::Service {
            name: name.into(),
            image: "mongo:7".into(),
            command: vec![],
            env: Default::default(),
            mounts: vec![],
            ports: vec![27017],
        }
    }

    /// A package change moves no byte, so the generation says "nothing to do" — but the newest sync
    /// point froze the OLD package list, and a re-host from it would come up with the old profile.
    #[test]
    fn a_definition_change_is_due_even_when_the_generation_stood_still() {
        let mut live = live_fixture();
        let recorded = live.state.clone();
        assert!(!due(5, Some(5)), "the bytes did not move");
        assert!(!definition_changed(&live.state, Some(&recorded)), "an idle worktree cuts nothing");

        let crd::SnapshotState::Workspace { packages, .. } = &mut live.state else { unreachable!() };
        *packages = vec!["ripgrep".into()];
        assert!(definition_changed(&live.state, Some(&recorded)));
        // And the cut that follows carries the NEW list, not the one the parent sync point froze.
        let spec = build_sync_spec(&live, "sync-ws-1-prev".into());
        let Some(crd::SnapshotState::Workspace { packages, .. }) = spec.state else { unreachable!() };
        assert_eq!(packages, vec!["ripgrep".to_string()]);
    }

    /// Environments carry a definition too (services, quota) and go through the same beat — the one
    /// asymmetry is that they have no `podRef` to be live by.
    #[test]
    fn an_environments_service_change_is_due_and_lands_in_the_cut() {
        let recorded = env_fixture(vec![service("db")]);
        let live = env_fixture(vec![service("db"), service("cache")]);
        assert!(!definition_changed(&recorded.state, Some(&recorded.state)));
        assert!(definition_changed(&live.state, Some(&recorded.state)));

        let spec = build_sync_spec(&live, String::new());
        let Some(crd::SnapshotState::Environment { services, .. }) = spec.state else { unreachable!() };
        assert_eq!(services.iter().map(|s| s.name.clone()).collect::<Vec<_>>(), ["db", "cache"]);
    }

    /// A worktree with no sync point at all has never recorded its definition anywhere.
    #[test]
    fn a_worktree_with_no_sync_point_has_a_changed_definition() {
        assert!(definition_changed(&live_fixture().state, None));
    }

    /// The beat only ever sees what `listing::Parent::is_live_worktree` admits — asserted here
    /// rather than assumed, because an environment reaching it depends on `pod_ref: None` being
    /// tolerated for that kind alone.
    #[test]
    fn a_running_environment_is_a_live_worktree() {
        assert!(env_fixture(vec![]).is_live_worktree());
    }
}
