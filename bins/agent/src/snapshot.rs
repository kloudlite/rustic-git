//! `Snapshot` reconciler: cuts the commit, advances the worktree's head, and retains. The OLD
//! `SnapshotRequest` kind and its push-to-the-registry reconciler are gone (Task 8) — a commit is
//! the CR now, not a request the CR asked to be fulfilled, so the two-object split (and the
//! `apply`/`cleanup` finalizer dance it needed) doesn't exist any more either.

use crate::controller::{patch_status, write_env_status, write_ws_status, Ctx, ReconcileErr, TICK};
use kube::api::ListParams;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use rustic_git_workspaces::crd::{self, VolumeSource};
use std::sync::Arc;

// -------------------------------------------------------------------------------------------
// The NEW `Snapshot` kind: cuts the commit, advances the worktree's head, and retains.
//
// No finalizer — a `Snapshot`'s bytes are content-addressed btrfs, and the CR is only ever
// deleted by retention (below) or a client, both of which mean "this record is done being
// useful", never "wait for something in flight". Because there is no finalizer, this reconciler
// never sees a delete event for one; the local subvolume it left behind is reaped by
// `peer::pull_volume`'s own diff against the surviving CR set — the "least new machinery" the
// task brief asks for, since that diff (and the per-volume worktree/replica loop around it)
// already exists for the pull side.
// -------------------------------------------------------------------------------------------

/// `WS_SNAPSHOT_KEEP`, default 10 — how many commits of the chain rooted at the just-cut head
/// retention keeps before it starts deleting the tail.
fn snapshot_keep() -> usize {
    // `.max(1)`: `WS_SNAPSHOT_KEEP=0` from a config typo must never let `skip(0)` consider the
    // commit just cut — the tip is always implicitly kept, same as `git gc` never expiring HEAD.
    std::env::var("WS_SNAPSHOT_KEEP").ok().and_then(|v| v.parse().ok()).unwrap_or(10).max(1)
}

/// Where `worktree` (a Workspace or Environment name) is running, if it names one that still
/// exists and still points at `volume` — a stale or foreign `spec.worktree` cuts nothing rather
/// than snapshotting the wrong disk.
///
/// A home has no Workspace/Environment at all (Task 7c: homes join the commit model) — its
/// Volume names the cutting node directly (`spec.nodeName`, the same field every pod affinity
/// already trusts), and it has exactly one worktree, named after the volume's own id. Checked
/// FIRST and unconditionally: a home Volume never has a same-named Workspace/Environment to
/// confuse this with (workspace/environment ids are minted `ws-`/`env-`, `home_volume_name`
/// is not), so there is no ambiguity to resolve.
async fn worktree_node(ctx: &Arc<Ctx>, volume: &str, worktree: &str) -> Result<Option<(&'static str, String)>, ReconcileErr> {
    if let Some(v) = Api::<crd::Volume>::all(ctx.client.clone()).get_opt(volume).await? {
        if crd::is_home_volume(&v) {
            return Ok(Some(("Home", v.spec.node_name.clone())));
        }
    }
    if let Some(w) = Api::<crd::Workspace>::all(ctx.client.clone()).get_opt(worktree).await? {
        if let Some(s) = &w.status {
            if s.volume_ref.as_deref() == Some(volume) {
                return Ok(Some(("Workspace", s.node_name.clone())));
            }
        }
        return Ok(None);
    }
    if let Some(e) = Api::<crd::Environment>::all(ctx.client.clone()).get_opt(worktree).await? {
        if let Some(s) = &e.status {
            if s.volume_ref.as_deref() == Some(volume) {
                return Ok(Some(("Environment", s.node_name.clone())));
            }
        }
    }
    Ok(None)
}

/// The reconciler for the new `Snapshot` kind, gated on `ctx.commit_model` — inert until Task
/// 7's cutover, same as every other commit-model arm.
pub async fn reconcile_commit(s: Arc<crd::Snapshot>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    if !ctx.commit_model {
        return Ok(Action::await_change());
    }
    // `Ready` is immutable (module doc on `SnapshotSpec`), and anything but `Working` has either
    // already been cut or is a transient shape nothing here produces — no-op either way.
    let phase = s.status.as_ref().map(|st| st.phase).unwrap_or(crd::Phase::Pending);
    if phase != crd::Phase::Working {
        return Ok(Action::await_change());
    }
    let Some((kind, node)) = worktree_node(&ctx, &s.spec.volume, &s.spec.worktree).await? else {
        // F1: NOT `await_change()`. Every node runs this same reconcile, so "not mine" is usually
        // right — but the commits controller watches ONLY Snapshots, so if this is a push racing
        // `volumeRef` visibility (or a pod mid-move), nothing else will ever wake this object, and
        // it sits `Working` forever with no condition: a silently hung user push. Requeue instead.
        return Ok(Action::requeue(TICK));
    };
    if node != ctx.node {
        return Ok(Action::await_change());
    }

    let name = s.name_any();
    let (engine, volume, worktree) = (ctx.engine.clone(), s.spec.volume.clone(), s.spec.worktree.clone());
    let cut_name = name.clone();
    let result = tokio::task::spawn_blocking(move || engine.commit_worktree(&volume, &worktree, &cut_name))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?;
    if let Err(e) = result {
        // Keep-biased: a failed cut leaves the CR `Working` and no CR/disk mismatch — the next
        // pass calls `commit_worktree` again, which converges on the same destination path.
        tracing::warn!(snapshot = %name, error = %e.0, "commit: cutting the snapshot failed; will retry");
        return Ok(Action::requeue(TICK));
    }
    // ponytail: no `sizeBytes` — a `du -s` over a btrfs subvolume walks every inode, which is
    // exactly the write-amplifying scan the sync-before-snapshot comment in `commit.rs` warns
    // about paying for on the hot path. Add it as a background sweep (or read the qgroup, which
    // this pool already maintains for quota) if the UI ever needs it.
    patch_status(
        &Api::<crd::Snapshot>::all(ctx.client.clone()),
        &name,
        "Snapshot",
        serde_json::json!({"phase": crd::Phase::Ready}),
    )
    .await?;

    if kind == "Home" {
        // No Workspace/Environment status to advance — a home's "head" is read back by walking
        // the chain for its own no-parent Ready tip (`newest_ready_commit`), not written anywhere.
        // Record the cut's own generation so the periodic beat's `homes_to_push` (unchanged: it
        // already compares live-vs-recorded generation, the SAME numbers the old `push_env` beat
        // used) knows this commit covers up to here — `pushed_generation` reads the just-cut RO
        // snapshot itself, never live re-read after, for the same race reason the old beat noted.
        let (engine, vol, cut) = (ctx.engine.clone(), s.spec.volume.clone(), name.clone());
        match tokio::task::spawn_blocking(move || engine.pushed_generation(&vol, &cut)).await {
            Ok(Ok(g)) => {
                if let Err(e) = ctx.engine.pool.record_pushed_gen(&s.spec.volume, g) {
                    tracing::warn!(volume = %s.spec.volume, error = %e, "commit: recording the home's pushed generation");
                }
            }
            Ok(Err(e)) => tracing::warn!(volume = %s.spec.volume, error = %e.0, "commit: reading the home's cut generation"),
            Err(e) => tracing::warn!(volume = %s.spec.volume, error = %e, "commit: generation task panicked"),
        }
    } else {
        advance_head(&ctx, kind, &s.spec.worktree, &name).await?;
    }
    retain(&ctx, &s.spec.volume, &name).await;

    Ok(Action::await_change())
}

/// A home's own newest Ready commit — the Ready snapshot on `volume` that no other Ready
/// snapshot names as its parent. A home never branches and has exactly one worktree, so this is
/// unambiguous; it stands in for the `status.head` a Workspace/Environment would have, since a
/// home has neither.
pub(crate) async fn newest_ready_commit(ctx: &Arc<Ctx>, volume: &str) -> Result<Option<String>, ReconcileErr> {
    let list = Api::<crd::Snapshot>::all(ctx.client.clone())
        .list(&ListParams::default().fields(&format!("spec.volume={volume}")))
        .await?;
    let ready: Vec<crd::Snapshot> =
        list.items.into_iter().filter(|s| s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready)).collect();
    let parents: std::collections::HashSet<&str> = ready.iter().map(|s| s.spec.parent.as_str()).filter(|p| !p.is_empty()).collect();
    Ok(ready.iter().find(|s| !parents.contains(s.name_any().as_str())).map(|s| s.name_any()))
}

/// `status.head = name` on the worktree's own Workspace/Environment — a guarded status write,
/// F1's preserve pattern: GET the object fresh, merge `head` onto its CURRENT status, and write
/// the whole thing back, so this write (which owns only `head`) never prunes `volumeRef`,
/// `podRef`, `packages`, or anything else another writer already put there.
async fn advance_head(ctx: &Arc<Ctx>, kind: &str, worktree: &str, name: &str) -> Result<(), ReconcileErr> {
    match kind {
        "Workspace" => {
            let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
            let Some(w) = api.get_opt(worktree).await? else { return Ok(()) };
            let prev = w.status.clone().unwrap_or_default();
            write_ws_status(&w, crd::WorkspaceStatus { head: Some(name.to_string()), ..prev }, ctx).await
        }
        _ => {
            let api: Api<crd::Environment> = Api::all(ctx.client.clone());
            let Some(e) = api.get_opt(worktree).await? else { return Ok(()) };
            let prev = e.status.clone().unwrap_or_default();
            write_env_status(&e, crd::EnvironmentStatus { head: Some(name.to_string()), ..prev }, ctx).await
        }
    }
}

/// Every worktree head on `volume`, across both parent kinds — commits retention must never
/// delete, no matter how far back in the keep-window they fall. List errors propagate: a
/// half-seen head set is exactly the case that would let retention delete a commit someone is
/// still standing on.
async fn worktree_heads(ctx: &Arc<Ctx>, volume: &str) -> Result<std::collections::HashSet<String>, ReconcileErr> {
    // F4: matched on the commit NAME's `{volume}-` prefix (`crd::snapshot_name`), not on
    // `volume_ref` — a worktree whose status is mid-rebuild has `volumeRef` momentarily unset,
    // and filtering on it there would make its head briefly invisible to retention. The prefix
    // match is exact (commit names are volume-prefixed random hex) and cheap.
    let prefix = format!("{volume}-");
    let mut heads = std::collections::HashSet::new();
    for w in Api::<crd::Workspace>::all(ctx.client.clone()).list(&ListParams::default()).await?.items {
        if let Some(h) = w.status.as_ref().and_then(|s| s.head.clone()) {
            if h.starts_with(&prefix) {
                heads.insert(h);
            }
        }
        // F4: a clone's grafted commit is unprotected until its FIRST checkout writes
        // `status.head` — a busy source can sweep it out of the keep-window in that gap, and
        // the clone lands `NoSuchCommit` forever. The spec already names it, so retention reads
        // it from there too, same list this loop is already paying for.
        if let Some(VolumeSource::CloneOf { commit: Some(c), .. }) = w.spec.storage.as_ref().and_then(|s| s.source.as_ref()) {
            if c.starts_with(&prefix) {
                heads.insert(c.clone());
            }
        }
    }
    for e in Api::<crd::Environment>::all(ctx.client.clone()).list(&ListParams::default()).await?.items {
        if let Some(h) = e.status.as_ref().and_then(|s| s.head.clone()) {
            if h.starts_with(&prefix) {
                heads.insert(h);
            }
        }
        if let Some(VolumeSource::CloneOf { commit: Some(c), .. }) = e.spec.storage.as_ref().and_then(|s| s.source.as_ref()) {
            if c.starts_with(&prefix) {
                heads.insert(c.clone());
            }
        }
    }
    // A home has no Workspace/Environment status to read a head off — pin its own newest Ready
    // commit explicitly, the same way a head is pinned above, so it survives retention even at
    // `WS_SNAPSHOT_KEEP=1` and even though `retain`'s own `skip(keep)` already protects chain[0]
    // (the just-cut tip) as a side effect of `keep`'s `.max(1)` floor — this is the durable,
    // config-independent guarantee the brief calls for, not a hope that a floor never moves.
    if let Some(v) = Api::<crd::Volume>::all(ctx.client.clone()).get_opt(volume).await? {
        if crd::is_home_volume(&v) {
            if let Some(tip) = newest_ready_commit(ctx, volume).await? {
                heads.insert(tip);
            }
        }
    }
    Ok(heads)
}

/// Delete every `Ready` commit on `head`'s chain beyond `WS_SNAPSHOT_KEEP`, except pinned ones and
/// any commit that is currently some worktree's head. v1 has no branches (`SnapshotSpec` carries
/// none), so "per branch chain" collapses to the one chain reached by walking `spec.parent` from
/// the commit just cut — the newest end of every chain this node could possibly be responsible
/// for right now.
///
/// Keep-biased throughout: any list error aborts the WHOLE pass with nothing deleted, same rule
/// `pull_volume` and the GC sweep both follow — retention is a nice-to-have, a wrongly deleted
/// commit is not recoverable.
async fn retain(ctx: &Arc<Ctx>, volume: &str, head: &str) {
    let snap_api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let list = match snap_api.list(&ListParams::default().fields(&format!("spec.volume={volume}"))).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(%volume, error = %e, "retention: listing snapshots; nothing deleted this pass");
            return;
        }
    };
    let by_name: std::collections::HashMap<String, crd::Snapshot> = list
        .items
        .into_iter()
        .filter(|s| s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready))
        .map(|s| (s.name_any(), s))
        .collect();
    let heads = match worktree_heads(ctx, volume).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(%volume, error = %e, "retention: listing worktree heads; nothing deleted this pass");
            return;
        }
    };

    // Walk the chain, newest (head) first — ORDER comes only from `spec.parent`, per the CRD's
    // own doc comment, never from a listing or creation-time sort.
    let mut chain = Vec::new();
    let mut cur = Some(head.to_string());
    while let Some(name) = cur {
        let Some(s) = by_name.get(&name) else { break };
        cur = (!s.spec.parent.is_empty()).then(|| s.spec.parent.clone());
        chain.push(name);
    }

    let keep = snapshot_keep();
    for name in chain.into_iter().skip(keep) {
        let s = &by_name[&name];
        if s.spec.pinned || heads.contains(&name) {
            continue;
        }
        if let Err(e) = snap_api.delete(&name, &Default::default()).await {
            tracing::warn!(%volume, snapshot = %name, error = %e, "retention: delete failed; left for the next pass");
        }
    }
}

#[cfg(test)]
mod commit_tests {
    use super::*;
    use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
    use rustic_git_workspaces::kube_test::{mock_client, not_found, Recorder, Route};

    struct NoopNix;
    #[async_trait::async_trait]
    impl crate::nix::Nix for NoopNix {
        async fn build(&self, _expr: &str, _timeout: std::time::Duration) -> Result<std::path::PathBuf, String> {
            Ok(std::path::PathBuf::from("/tmp"))
        }
        async fn ping(&self) -> Result<(), String> {
            Ok(())
        }
        async fn collect_garbage(&self) -> Result<u64, String> {
            Ok(0)
        }
    }

    /// `commit_model` on, unconditionally — every test in this module is exercising the new
    /// `Snapshot` kind, so there is no "flag off" case to cover here (that lives beside the
    /// checkout guard in `bins/agent/tests/reconcile.rs`).
    fn test_ctx(pool: &std::path::Path, node: &str, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
        let (client, rec) = mock_client(routes);
        let engine = Engine::new(EnginePool::new(pool));
        std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
        std::env::set_var("WS_COMMIT_MODEL", "1");
        let ctx = Ctx::new(
            client,
            Arc::new(engine),
            node.into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            vec![],
            Arc::new(NoopNix),
            pool.join("profiles"),
        );
        (Arc::new(ctx), rec)
    }

    fn snapshot(name: &str, volume: &str, worktree: &str, parent: &str, pinned: bool, phase: crd::Phase) -> Arc<crd::Snapshot> {
        let v = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": name, "uid": format!("{name}-uid"), "generation": 1},
            "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": parent, "pinned": pinned},
            "status": {"phase": phase},
        });
        Arc::new(serde_json::from_value(v).unwrap())
    }

    /// A Workspace whose `status.nodeName`/`volumeRef` say it runs `volume` on `node`, with a
    /// `podRef` standing in for "everything else a status write must not prune" — F1's own shape.
    fn ws_status_json(node: &str, volume: &str, head: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-1", "uid": "ws-uid", "generation": 1},
            "spec": {"owner": "alice", "team": "", "name": "web", "region": "r1", "image": "img", "desiredState": "running"},
            "status": {"phase": "ready", "nodeName": node, "volumeRef": volume, "podRef": "pod-x", "head": head},
        })
    }

    /// A home Volume: `ownerReferences[0].kind == "OwnerBinding"` is the whole test `is_home_volume`
    /// reads, per its own doc comment — a name is a convention, never the link.
    fn home_vol_json(name: &str, node: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": name, "uid": format!("{name}-uid"), "generation": 1,
                         "ownerReferences": [{"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                                              "name": "r1-alice", "uid": "ob-uid", "controller": true, "blockOwnerDeletion": true}]},
            "spec": {"owner": "alice", "team": "", "nodeName": node, "region": "r1", "quotaGb": 2},
            "status": {"phase": "ready", "subvolumePresent": true},
        })
    }

    const WS_GET: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1";
    const WS_STATUS: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1/status";
    const SNAP_STATUS: &str = "/apis/rustic-git.io/v1alpha1/snapshots/vol-1-a/status";
    const SNAPSHOTS_LIST: &str = "/apis/rustic-git.io/v1alpha1/snapshots";
    const WORKSPACES_LIST: &str = "/apis/rustic-git.io/v1alpha1/workspaces";
    const ENVIRONMENTS_LIST: &str = "/apis/rustic-git.io/v1alpha1/environments";

    fn list_of(kind: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"apiVersion": "v1", "kind": format!("{kind}List"), "items": items})
    }

    /// Cutting on the node that runs the worktree: the CR goes Ready, and the workspace's
    /// `status.head` advances WITHOUT losing `podRef` — the F1 preserve pattern this write reuses.
    /// `commit_worktree` never shells to real `btrfs`: the destination `snap/{name}` dir already
    /// exists, so its own convergence check (`dst.exists()`) short-circuits before any command
    /// runs — the same trick `commit_model_checkout_converges_on_an_existing_worktree` uses.
    #[tokio::test]
    async fn cut_on_my_node_sets_ready_and_advances_head_preserving_other_status_fields() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/live/ws-1")).unwrap();
        let routes = vec![
            Route { method: "GET", path: WS_GET.into(), status: 200, body: ws_status_json("node-a", "vol-1", None) },
            Route {
                method: "PATCH",
                path: SNAP_STATUS.into(),
                status: 200,
                body: serde_json::json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                    "metadata": {"name": "vol-1-a", "uid": "vol-1-a-uid"},
                    "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false},
                    "status": {"phase": "ready"},
                }),
            },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_status_json("node-a", "vol-1", Some("vol-1-a")) },
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![]) },
            Route { method: "GET", path: WORKSPACES_LIST.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::await_change());

        let snap_sent = rec.sent("PATCH", SNAP_STATUS);
        assert_eq!(snap_sent.len(), 1);
        assert_eq!(snap_sent[0]["status"]["phase"], "ready");

        let ws_sent = rec.sent("PATCH", WS_STATUS);
        assert_eq!(ws_sent.len(), 1, "exactly one head write");
        assert_eq!(ws_sent[0]["status"]["head"], "vol-1-a");
        assert_eq!(ws_sent[0]["status"]["podRef"], "pod-x", "the head write must not prune podRef");
        assert_eq!(ws_sent[0]["status"]["nodeName"], "node-a", "or nodeName");
    }

    /// The worktree named by `spec.worktree` runs on a DIFFERENT node — every node runs this same
    /// reconcile, so ignoring here is correct: that other node's own pass cuts it.
    #[tokio::test]
    async fn a_working_snapshot_not_on_this_node_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route { method: "GET", path: WS_GET.into(), status: 200, body: ws_status_json("node-b", "vol-1", None) }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::await_change());
        assert!(rec.calls().iter().all(|c| !c.starts_with("PATCH")), "not mine: nothing written");
    }

    /// F1: an unresolvable worktree (neither a Workspace nor an Environment answers — a push
    /// racing `volumeRef` visibility, or a pod mid-move) must NOT `await_change()`. The commits
    /// controller watches ONLY `Snapshot`s, so nothing else would ever wake this object again —
    /// `await_change` there is a silently hung user push, healed only by an agent restart.
    #[tokio::test]
    async fn an_unresolvable_worktree_requeues_instead_of_awaiting() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![not_found(WS_GET), not_found("/apis/rustic-git.io/v1alpha1/environments/ws-1")];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::requeue(TICK), "must requeue, not await a watch that never fires");
        assert!(rec.calls().iter().all(|c| !c.starts_with("PATCH")), "nothing written for an unresolved worktree");
    }

    /// Task 7c: a home has no Workspace/Environment — the Snapshot's volume names the cutting
    /// node directly through `Volume.spec.nodeName`, and the worktree is the volume's own id. The
    /// CR goes `Ready` and nothing is written to any Workspace/Environment status (there is none).
    #[tokio::test]
    async fn a_home_snapshot_cuts_on_the_volumes_own_node() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/home-alice/snap/home-alice-a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/home-alice/live/home-alice")).unwrap();
        let routes = vec![
            Route { method: "GET", path: "/apis/rustic-git.io/v1alpha1/volumes/home-alice".into(), status: 200, body: home_vol_json("home-alice", "node-a") },
            Route {
                method: "PATCH",
                path: "/apis/rustic-git.io/v1alpha1/snapshots/home-alice-a/status".into(),
                status: 200,
                body: serde_json::json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                    "metadata": {"name": "home-alice-a", "uid": "home-alice-a-uid"},
                    "spec": {"volume": "home-alice", "owner": "alice", "worktree": "home-alice", "parent": "", "pinned": false},
                    "status": {"phase": "ready"},
                }),
            },
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![]) },
            Route { method: "GET", path: WORKSPACES_LIST.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let s = snapshot("home-alice-a", "home-alice", "home-alice", "", false, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::await_change());

        let snap_sent = rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/snapshots/home-alice-a/status");
        assert_eq!(snap_sent.len(), 1);
        assert_eq!(snap_sent[0]["status"]["phase"], "ready");
        assert!(
            rec.calls().iter().all(|c| !c.starts_with("PATCH /apis/rustic-git.io/v1alpha1/workspaces")
                && !c.starts_with("PATCH /apis/rustic-git.io/v1alpha1/environments")),
            "a home has no Workspace/Environment status to advance: {:?}", rec.calls()
        );
    }

    /// A non-home Snapshot must still resolve via the Workspace/Environment path — the home check
    /// runs first and unconditionally, so this proves it does not swallow the ordinary case.
    #[tokio::test]
    async fn a_non_home_snapshot_still_resolves_via_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/live/ws-1")).unwrap();
        let routes = vec![
            // Not a home: `is_home_volume` is false with no ownerReferences at all.
            Route {
                method: "GET",
                path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1".into(),
                status: 200,
                body: serde_json::json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
                    "metadata": {"name": "vol-1", "uid": "vol-1-uid", "generation": 1},
                    "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 2},
                    "status": {"phase": "ready"},
                }),
            },
            Route { method: "GET", path: WS_GET.into(), status: 200, body: ws_status_json("node-a", "vol-1", None) },
            Route {
                method: "PATCH",
                path: SNAP_STATUS.into(),
                status: 200,
                body: serde_json::json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                    "metadata": {"name": "vol-1-a", "uid": "vol-1-a-uid"},
                    "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false},
                    "status": {"phase": "ready"},
                }),
            },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_status_json("node-a", "vol-1", Some("vol-1-a")) },
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![]) },
            Route { method: "GET", path: WORKSPACES_LIST.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::await_change());
        let ws_sent = rec.sent("PATCH", WS_STATUS);
        assert_eq!(ws_sent.len(), 1, "a non-home still advances the Workspace's head");
        assert_eq!(ws_sent[0]["status"]["head"], "vol-1-a");
    }

    /// F1's requeue-not-await discipline must survive the new home check: a Volume that answers
    /// 404 (name unknown to this node at all) still falls through to the old Workspace/Environment
    /// lookup and, finding neither, requeues.
    #[tokio::test]
    async fn an_unknown_volume_still_requeues() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            not_found("/apis/rustic-git.io/v1alpha1/volumes/vol-ghost"),
            not_found("/apis/rustic-git.io/v1alpha1/workspaces/ws-1"),
            not_found("/apis/rustic-git.io/v1alpha1/environments/ws-1"),
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let s = snapshot("vol-ghost-a", "vol-ghost", "ws-1", "", false, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::requeue(TICK));
        assert!(rec.calls().iter().all(|c| !c.starts_with("PATCH")), "nothing written for an unknown volume");
    }

    /// A home has no Workspace/Environment `status.head` to fold into `worktree_heads` — this
    /// proves the added home arm names its own newest Ready commit (the one no other Ready
    /// commit calls its parent) anyway, so retention's protected set is never empty for a home
    /// no matter how a config change (`WS_SNAPSHOT_KEEP=1`, say) shrinks the ordinary keep-window
    /// floor.
    #[tokio::test]
    async fn worktree_heads_protects_a_homes_newest_ready_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let older = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "home-alice-old", "uid": "old-uid"},
            "spec": {"volume": "home-alice", "owner": "alice", "worktree": "home-alice", "parent": "", "pinned": false},
            "status": {"phase": "ready"},
        });
        let tip = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "home-alice-tip", "uid": "tip-uid"},
            "spec": {"volume": "home-alice", "owner": "alice", "worktree": "home-alice", "parent": "home-alice-old", "pinned": false},
            "status": {"phase": "ready"},
        });
        let routes = vec![
            Route { method: "GET", path: WORKSPACES_LIST.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "GET", path: "/apis/rustic-git.io/v1alpha1/volumes/home-alice".into(), status: 200, body: home_vol_json("home-alice", "node-a") },
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![older, tip]) },
        ];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);

        let heads = worktree_heads(&ctx, "home-alice").await.unwrap();

        assert!(heads.contains("home-alice-tip"), "the home's newest Ready commit must be in the protected set: {heads:?}");
        assert!(!heads.contains("home-alice-old"), "only the tip is the newest, not the whole chain");
    }

    /// A chain of 13 commits, `WS_SNAPSHOT_KEEP=10`: the tail beyond the keep window is `c2, c1,
    /// c0` (oldest three) — `c1` is pinned and `c0` is some worktree's current head, so only `c2`
    /// is actually deleted. This is the durable-floor case the brief calls out: a head this far
    /// back in the chain still survives the sweep.
    #[tokio::test]
    async fn retention_deletes_beyond_keep_sparing_pinned_and_heads() {
        std::env::set_var("WS_SNAPSHOT_KEEP", "10");
        let tmp = tempfile::tempdir().unwrap();
        // vol-1-c12 -> vol-1-c11 -> ... -> vol-1-c0, oldest has no parent. Names carry the
        // `vol-1-` prefix `worktree_heads`'s F4 match relies on — a real commit name always does
        // (`crd::snapshot_name`).
        let name = |i: i32| format!("vol-1-c{i}");
        let mut items = Vec::new();
        for i in 0..13 {
            let parent = if i == 0 { String::new() } else { name(i - 1) };
            let pinned = i == 1;
            items.push(serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                "metadata": {"name": name(i), "uid": format!("{}-uid", name(i))},
                "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": parent, "pinned": pinned},
                "status": {"phase": "ready"},
            }));
        }
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", items) },
            Route {
                method: "GET",
                path: WORKSPACES_LIST.into(),
                status: 200,
                body: list_of("Workspace", vec![ws_status_json("node-a", "vol-1", Some(&name(0)))]),
            },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
            Route {
                method: "DELETE",
                path: format!("{SNAPSHOTS_LIST}/{}", name(2)),
                status: 200,
                body: serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Success"}),
            },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "vol-1", &name(12)).await;

        let deletes: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes, vec![format!("DELETE {SNAPSHOTS_LIST}/{}", name(2))], "only the unpinned, non-head tail entry is deleted: {deletes:?}");
    }

    /// F4: `worktree_heads` matches on the commit name's `{volume}-` prefix, not on
    /// `status.volumeRef` — a worktree mid-rebuild has `volumeRef` momentarily unset, and this
    /// proves its head still survives a sweep that would otherwise consider it fair game.
    #[tokio::test]
    async fn retention_spares_a_head_whose_worktree_status_has_no_volume_ref_yet() {
        // No `WS_SNAPSHOT_KEEP` override: it is process-global and tests run in parallel in this
        // binary (the file's own F3 note on `commit_model` living on `Ctx` rather than env is the
        // same lesson) — two commits never reach even the smallest realistic keep window anyway,
        // so the default proves the point without racing `retention_deletes_beyond_keep_...`'s own
        // override.
        let tmp = tempfile::tempdir().unwrap();
        let older = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "vol-1-old", "uid": "old-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false},
            "status": {"phase": "ready"},
        });
        let tip = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "vol-1-tip", "uid": "tip-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "vol-1-old", "pinned": false},
            "status": {"phase": "ready"},
        });
        // `volumeRef` is absent — a status caught mid-rebuild — but `head` still names the
        // volume's own oldest commit, and that must be enough to protect it.
        let ws = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-1", "uid": "ws-uid"},
            "spec": {"owner": "alice", "team": "", "name": "web", "region": "r1", "image": "img", "desiredState": "running"},
            "status": {"phase": "ready", "nodeName": "node-a", "head": "vol-1-old"},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![older, tip]) },
            Route { method: "GET", path: WORKSPACES_LIST.into(), status: 200, body: list_of("Workspace", vec![ws]) },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "vol-1", "vol-1-tip").await;

        assert!(rec.calls().iter().all(|c| !c.starts_with("DELETE")), "a volumeRef-less head must still be spared: {:?}", rec.calls());
    }

    /// F4: a clone's grafted commit (`cloneOf.commit` in the WORKSPACE SPEC) is unprotected
    /// until its first checkout writes `status.head` — a busy source pushing past the keep window
    /// in that gap would otherwise sweep the very commit the clone is pinned to, and the clone
    /// lands `NoSuchCommit` forever. This clone has never had a `head` of its own (fresh, never
    /// checked out), so only the SPEC names its commit — and that alone must be enough to spare
    /// it.
    #[tokio::test]
    async fn retention_spares_a_spec_only_clone_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let older = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "vol-1-old", "uid": "old-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-src", "parent": "", "pinned": false},
            "status": {"phase": "ready"},
        });
        let tip = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "vol-1-tip", "uid": "tip-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-src", "parent": "vol-1-old", "pinned": false},
            "status": {"phase": "ready"},
        });
        // No `status.head` at all — a fresh clone that has never checked out. Only the spec's
        // `cloneOf.commit` names `vol-1-old`.
        let clone = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-clone", "uid": "clone-uid"},
            "spec": {"owner": "alice", "team": "", "name": "clone", "region": "r1", "image": "img", "desiredState": "running",
                     "storage": {"quotaGb": 20, "source": {"cloneOf": {"volume": "vol-1", "commit": "vol-1-old"}}}},
            "status": {"phase": "creating", "nodeName": "node-a"},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![older, tip]) },
            Route { method: "GET", path: WORKSPACES_LIST.into(), status: 200, body: list_of("Workspace", vec![clone]) },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "vol-1", "vol-1-tip").await;

        assert!(rec.calls().iter().all(|c| !c.starts_with("DELETE")), "a spec-only clone commit must survive: {:?}", rec.calls());
    }

    /// Keep-biased: a `Snapshot`-list error must delete nothing at all, not even the obviously
    /// stale end of a chain it happened to already know about.
    #[tokio::test]
    async fn retention_does_nothing_on_a_snapshot_list_error() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "vol-1", "c11").await;

        assert!(rec.calls().iter().all(|c| !c.starts_with("DELETE")), "a list error must delete nothing: {:?}", rec.calls());
    }
}
