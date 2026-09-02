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
/// A home lives on shared NFS now (Task 5: the home push/commit beats are gone), so a Snapshot
/// naming a home volume no longer resolves to anything here — it falls through to the
/// Workspace/Environment lookups below, both of which miss, and the caller requeues.
async fn worktree_node(ctx: &Arc<Ctx>, volume: &str, worktree: &str) -> Result<Option<(&'static str, String)>, ReconcileErr> {
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

/// The reconciler for the `Snapshot` kind.
pub async fn reconcile_commit(s: Arc<crd::Snapshot>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // `Ready` is immutable (module doc on `SnapshotSpec`), and anything but `Working` has either
    // already been cut or is a transient shape nothing here produces — no-op either way.
    // A commit CR with NO status yet is one that has just been created and never cut: `status` is
    // a SUBRESOURCE, so the status block a creator puts in the object literal is dropped by the
    // API server on create, and every commit is therefore born status-less. Defaulting that to
    // anything but `Working` makes the controller `await_change()` a CR nothing will ever touch
    // again — every push, every migration baseline and every home beat hanging forever, which is
    // exactly what shipped. Missing status means "not cut yet", which is `Working`.
    let phase = s.status.as_ref().map(|st| st.phase).unwrap_or(crd::Phase::Working);
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
    let api = Api::<crd::Snapshot>::all(ctx.client.clone());
    if s.spec.transient {
        record_post_cut_generation(&ctx, &api, &name, &s.spec.volume, &s.spec.worktree).await;
    }
    patch_status(
        &api,
        &name,
        "Snapshot",
        serde_json::json!({"phase": crd::Phase::Ready, "readyAt": chrono::Utc::now().to_rfc3339()}),
    )
    .await?;

    // A transient (sync point) never advances the worktree's head — it replicates a live
    // worktree continuously without ever becoming a commit the user sees or clones from.
    if !s.spec.transient {
        advance_head(&ctx, kind, &s.spec.worktree, &name).await?;
    }
    retain(&ctx, &s.spec.volume, &name).await;

    Ok(Action::await_change())
}

/// Re-stamp a sync point's `SYNCED_GENERATION` with the generation the worktree has AFTER the cut.
///
/// Taking a read-only snapshot of a subvolume bumps that subvolume's own generation by one, so the
/// value the beat stamped before cutting is always one behind reality — and `due` then finds every
/// idle worktree due, forever. Re-reading here is the only place the post-cut value exists.
///
/// Keep-biased: a failed re-read leaves the pre-cut value, which costs one redundant cut next tick
/// and nothing else. A metadata merge patch — annotations are not spec, so the agent's
/// spec-is-read-only admission policy has nothing to say about it.
async fn record_post_cut_generation(ctx: &Arc<Ctx>, api: &Api<crd::Snapshot>, name: &str, volume: &str, worktree: &str) {
    let (engine, vol, wt) = (ctx.engine.clone(), volume.to_string(), worktree.to_string());
    let gen = match tokio::task::spawn_blocking(move || engine.generation(&vol, &wt)).await {
        Ok(Ok(g)) => g,
        Ok(Err(e)) => {
            tracing::warn!(snapshot = %name, error = %e.0, "commit: re-reading the post-cut generation");
            return;
        }
        Err(e) => {
            tracing::warn!(snapshot = %name, error = %e, "commit: post-cut generation task panicked");
            return;
        }
    };
    let body = serde_json::json!({"metadata": {"annotations": {crate::sync::SYNCED_GENERATION: gen.to_string()}}});
    if let Err(e) = api.patch(name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&body)).await {
        tracing::warn!(snapshot = %name, error = %e, "commit: recording the post-cut generation");
    }
}

/// The newest sync point of `(volume, worktree)`: the `Ready` transient carrying the highest
/// `SYNCED_GENERATION` annotation, or `None` if this worktree has none.
///
/// Generation, not creation time: the annotation is the btrfs generation the sync beat actually
/// replicated, and it is the only ordering that survives clock skew between nodes. A transient cut
/// by the stop path carries no annotation at all — read as generation 0, so it loses to any
/// annotated one but still wins over nothing.
///
/// Candidates are intersected with what this node actually HOLDS (`local_commits`, a plain listing
/// of `snap/`). A replica one pull cycle behind sees a `Ready` transient whose subvolume has not
/// landed here, and checking that out fails `NO_SUCH_RECORD` — a PERMANENT error, where falling
/// back to `head` would have started the worktree perfectly well. Not local is simply not a
/// candidate, so the fallback chain in the caller does the rest.
pub(crate) async fn latest_transient(ctx: &Arc<Ctx>, volume: &str, worktree: &str) -> Result<Option<String>, ReconcileErr> {
    let local: std::collections::HashSet<String> =
        ctx.engine.local_commits(volume).map_err(|e| ReconcileErr(e.0))?.into_iter().collect();
    let list = Api::<crd::Snapshot>::all(ctx.client.clone())
        .list(&ListParams::default().fields(&format!("spec.volume={volume}")))
        .await?;
    let mut best: Option<(u64, String)> = None;
    for s in list.items {
        let ready = s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready);
        if !s.spec.transient || s.spec.worktree != worktree || !ready || !local.contains(&s.name_any()) {
            continue;
        }
        let gen = s.annotations().get(crate::sync::SYNCED_GENERATION).and_then(|g| g.parse::<u64>().ok()).unwrap_or(0);
        if best.as_ref().is_none_or(|(b, _)| gen >= *b) {
            best = Some((gen, s.name_any()));
        }
    }
    Ok(best.map(|(_, name)| name))
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
    //
    // ponytail: cluster-wide, not owner-scoped — a shared clone or a restore-to-new can put a
    // worktree owned by a DIFFERENT owner on this volume (`CloneOf`), and an unpushed clone's head
    // is its clone commit with no `Snapshot` under any owner to derive a selector from. Upgrade
    // path: a volume-keyed label on parents, if this scan ever shows up in profiling.
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
    let ready: std::collections::HashMap<String, crd::Snapshot> = list
        .items
        .into_iter()
        .filter(|s| s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready))
        .map(|s| (s.name_any(), s))
        .collect();

    // A transient cut isn't a chain member at all (Task 3: it never carries `spec.parent` into a
    // commit's ancestry and never advances a head), so it gets its own pass: one sync point per
    // worktree, full stop. Order matters — the newer transient's `spec.parent` names the OLDER
    // one as its btrfs send parent (Task 2), so deleting the older one before the newer reaches
    // `Ready` would delete a still-needed send parent out from under a `Working` cut. Since this
    // function only runs after `patch_status(Ready)` for `head` itself, any OTHER transient still
    // seen here is either already `Ready` (safe to delete) or not yet `Ready` at all (filtered out
    // of `ready` above, so it is never considered) — never a `Working` cut caught mid-flight.
    // ponytail: this arm deliberately ignores `heads` (and `spec.pinned`) — a sync point is never
    // a chain member and nothing user-facing names one, so nothing can hold a reference to it.
    // If `clone`, `restore` or any other verb ever names a sync point by id, this arm is wrong and
    // must consult `worktree_heads` the way the commit path below does.
    if ready.get(head).is_some_and(|s| s.spec.transient) {
        let worktree = ready[head].spec.worktree.clone();
        // A replica mid-receive of the older transient just deleted here fails that one pull and
        // self-heals on its next: the beat re-lists and re-sends against whatever `Ready` transient
        // is current then, so a delete racing an in-flight send is a retry, not data loss.
        for (name, s) in &ready {
            if name != head && s.spec.transient && s.spec.worktree == worktree {
                if let Err(e) = snap_api.delete(name, &Default::default()).await {
                    tracing::warn!(%volume, snapshot = %name, error = %e, "retention: delete failed; left for the next pass");
                }
            }
        }
        return;
    }

    // Commit chain walk: transients are never chain members and must never be treated as one —
    // filtered out here so a transient sharing a worktree can't be mistaken for this chain's
    // parent, and is never a delete candidate on the commit path.
    let by_name: std::collections::HashMap<String, crd::Snapshot> =
        ready.into_iter().filter(|(_, s)| !s.spec.transient).collect();
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

    fn test_ctx(pool: &std::path::Path, node: &str, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
        let (client, rec) = mock_client(routes);
        let engine = Engine::new(EnginePool::new(pool));
        std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
        let ctx = Ctx::new(
            client,
            Arc::new(engine),
            node.into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            vec![],
            Some("test:/".into()),
            Arc::new(NoopNix),
            pool.join("profiles"),
        );
        (Arc::new(ctx), rec)
    }

    fn snapshot(name: &str, volume: &str, worktree: &str, parent: &str, pinned: bool, transient: bool, phase: crd::Phase) -> Arc<crd::Snapshot> {
        let v = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": name, "uid": format!("{name}-uid"), "generation": 1},
            "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": parent, "pinned": pinned, "transient": transient},
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
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, false, crd::Phase::Working);

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
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, false, crd::Phase::Working);

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
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, false, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::requeue(TICK), "must requeue, not await a watch that never fires");
        assert!(rec.calls().iter().all(|c| !c.starts_with("PATCH")), "nothing written for an unresolved worktree");
    }

    /// A non-home Snapshot must still resolve via the Workspace/Environment path.
    #[tokio::test]
    async fn a_non_home_snapshot_still_resolves_via_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/live/ws-1")).unwrap();
        let routes = vec![
            // A plain workspace volume, no home Volume kind involved.
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
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, false, crd::Phase::Working);

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
        let s = snapshot("vol-ghost-a", "vol-ghost", "ws-1", "", false, false, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::requeue(TICK));
        assert!(rec.calls().iter().all(|c| !c.starts_with("PATCH")), "nothing written for an unknown volume");
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

    /// A transient (sync point) cut must never advance the worktree's head — it replicates a live
    /// worktree continuously and is never a commit the user checks out or clones from. No
    /// `WS_STATUS` route is registered at all, so a wrongly-issued write 404s and the reconcile
    /// itself would fail; the recorder assertion is the belt.
    #[tokio::test]
    async fn a_transient_cut_does_not_advance_head() {
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
                    "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false, "transient": true},
                    "status": {"phase": "ready"},
                }),
            },
            Route {
                method: "GET",
                path: SNAPSHOTS_LIST.into(),
                status: 200,
                body: list_of("Snapshot", vec![serde_json::json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                    "metadata": {"name": "vol-1-a", "uid": "vol-1-a-uid"},
                    "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false, "transient": true},
                    "status": {"phase": "ready"},
                })]),
            },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, true, crd::Phase::Working);

        let action = reconcile_commit(s, ctx).await.unwrap();
        assert_eq!(action, kube::runtime::controller::Action::await_change());

        assert_eq!(rec.sent("PATCH", SNAP_STATUS).len(), 1, "the cut still goes Ready");
        assert!(rec.sent("PATCH", WS_STATUS).is_empty(), "no head write for a transient: {:?}", rec.calls());
    }

    /// Keep-biased post-cut re-stamp: the test engine is a tmpdir with no btrfs, so `generation`
    /// errors, and the annotation patch must simply not happen — the pre-cut value the beat wrote
    /// survives, costing one redundant cut and nothing else. The SUCCESS path (a PATCH carrying
    /// `synced-generation`) needs a real `btrfs subvolume show`: `Engine` is concrete here, with no
    /// seam to inject a generation reader, so it is covered by `tests/ws_e2e.sh` and the live
    /// cluster check, not here — faking a reader would only assert that the fake was called.
    #[tokio::test]
    async fn an_unreadable_post_cut_generation_leaves_the_annotation_alone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/live/ws-1")).unwrap();
        let ready = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "vol-1-a", "uid": "vol-1-a-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false, "transient": true},
            "status": {"phase": "ready"},
        });
        let routes = vec![
            Route { method: "GET", path: WS_GET.into(), status: 200, body: ws_status_json("node-a", "vol-1", None) },
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: ready.clone() },
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![ready]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let s = snapshot("vol-1-a", "vol-1", "ws-1", "", false, true, crd::Phase::Working);

        reconcile_commit(s, ctx).await.unwrap();

        assert_eq!(rec.sent("PATCH", SNAP_STATUS).len(), 1, "the cut still goes Ready");
        assert!(
            rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/snapshots/vol-1-a").is_empty(),
            "no annotation write when the generation cannot be read: {:?}",
            rec.calls()
        );
    }

    /// One sync point per worktree: cutting a new `Ready` transient deletes the previous `Ready`
    /// transient of the SAME worktree only — a sibling worktree's own transient is untouched.
    #[tokio::test]
    async fn a_new_ready_transient_deletes_the_previous_one_for_its_worktree_only() {
        let tmp = tempfile::tempdir().unwrap();
        let old = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "sync-ws-1-a", "uid": "a-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false, "transient": true},
            "status": {"phase": "ready"},
        });
        let new = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "sync-ws-1-b", "uid": "b-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "sync-ws-1-a", "pinned": false, "transient": true},
            "status": {"phase": "ready"},
        });
        let other = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "sync-ws-2-c", "uid": "c-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-2", "parent": "", "pinned": false, "transient": true},
            "status": {"phase": "ready"},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![old, new, other]) },
            Route {
                method: "DELETE",
                path: format!("{SNAPSHOTS_LIST}/sync-ws-1-a"),
                status: 200,
                body: serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Success"}),
            },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "vol-1", "sync-ws-1-b").await;

        let deletes: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes, vec![format!("DELETE {SNAPSHOTS_LIST}/sync-ws-1-a")], "only the same worktree's older transient is deleted: {deletes:?}");
    }

    /// The previous transient is the btrfs send parent of a still-`Working` new one — deleting it
    /// before the new one reaches `Ready` would pull the send parent out from under an in-flight
    /// cut. A `Working` snapshot is filtered out of the `Ready` set entirely, so retention never
    /// even considers it here.
    #[tokio::test]
    async fn a_working_previous_transient_is_never_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let old = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "sync-ws-1-a", "uid": "a-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false, "transient": true},
            "status": {"phase": "working"},
        });
        let new = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "sync-ws-1-b", "uid": "b-uid"},
            "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "sync-ws-1-a", "pinned": false, "transient": true},
            "status": {"phase": "ready"},
        });
        let routes = vec![Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![old, new]) }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "vol-1", "sync-ws-1-b").await;

        assert!(rec.calls().iter().all(|c| !c.starts_with("DELETE")), "a still-Working previous transient must survive: {:?}", rec.calls());
    }

    /// `latest_transient` orders by the `SYNCED_GENERATION` annotation, never by listing order:
    /// a commit and a `Working` transient are both ignored, an unannotated transient (the stop
    /// path cuts one) reads as generation 0 and loses, and the highest generation wins — but only
    /// among transients whose subvolume is actually ON THIS POOL: `sync-ws-1-newest` has the
    /// highest generation of all and is deliberately absent from `snap/`, standing in for a replica
    /// one pull cycle behind, and must lose to the highest LOCAL one.
    #[tokio::test]
    async fn latest_transient_picks_the_highest_synced_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = |name: &str, worktree: &str, transient: bool, phase: &str, gen: Option<&str>| {
            let mut v = serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                "metadata": {"name": name, "uid": format!("{name}-uid")},
                "spec": {"volume": "vol-1", "owner": "alice", "worktree": worktree, "parent": "", "pinned": false, "transient": transient},
                "status": {"phase": phase},
            });
            if let Some(g) = gen {
                v["metadata"]["annotations"] = serde_json::json!({crate::sync::SYNCED_GENERATION: g});
            }
            v
        };
        let items = vec![
            snap("vol-1-commit", "ws-1", false, "ready", None),
            snap("sync-ws-1-none", "ws-1", true, "ready", None),
            snap("sync-ws-1-hi", "ws-1", true, "ready", Some("9")),
            snap("sync-ws-1-lo", "ws-1", true, "ready", Some("4")),
            snap("sync-ws-1-working", "ws-1", true, "working", Some("99")),
            snap("sync-ws-2-other", "ws-2", true, "ready", Some("99")),
            snap("sync-ws-1-newest", "ws-1", true, "ready", Some("99")),
        ];
        // Everything the pool holds — note `sync-ws-1-newest` is NOT here.
        for name in ["vol-1-commit", "sync-ws-1-none", "sync-ws-1-hi", "sync-ws-1-lo", "sync-ws-1-working", "sync-ws-2-other"] {
            std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap").join(name)).unwrap();
        }
        let routes = vec![Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", items) }];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);

        assert_eq!(latest_transient(&ctx, "vol-1", "ws-1").await.unwrap().as_deref(), Some("sync-ws-1-hi"));
        assert_eq!(latest_transient(&ctx, "vol-1", "ws-3").await.unwrap(), None, "a worktree with no sync point resolves to nothing");
    }

    /// A commit CR is created WITHOUT status — `status` is a SUBRESOURCE, so the block a creator
    /// puts in the object literal is dropped by the API server. The reconcile must read that as
    /// `Working` and cut it; any other default stalls every push and migration baseline forever.
    #[tokio::test]
    async fn a_commit_with_no_status_at_all_is_still_cut() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: WS_GET.into(), status: 200, body: ws_status_json("node-a", "vol-1", None) },
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                "metadata": {"name": "vol-1-a", "uid": "vol-1-a-uid"},
                "spec": {"volume": "vol-1", "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false},
                "status": {"phase": "ready"},
            })},
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_status_json("node-a", "vol-1", Some("vol-1-a")) },
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", vec![]) },
            Route { method: "GET", path: WORKSPACES_LIST.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        // The shape the API server actually stores on create: no status at all.
        let mut s = (*snapshot("vol-1-a", "vol-1", "ws-1", "", false, false, crd::Phase::Working)).clone();
        s.status = None;
        let action = reconcile_commit(std::sync::Arc::new(s), ctx).await.unwrap();
        // The cut itself needs btrfs, which this box has not got — it fails and takes the
        // keep-biased retry. That is fine: what this pins is that the reconcile ENGAGED at all.
        // Before the fix a status-less CR returned `await_change()` immediately and was never
        // looked at again, which is the production hang; a requeue proves it is being worked.
        assert_ne!(
            action,
            kube::runtime::controller::Action::await_change(),
            "a status-less commit must not be ignored — it is a commit that has never been cut"
        );
        let _ = &rec;
    }

    /// A volume can carry commits and worktrees from more than one owner — a shared clone or a
    /// restore-to-new lands a second owner's worktree on the SAME volume (CLAUDE.md's "Workspaces
    /// and environments"). Retention must scan every owner's parents, not just the one whose
    /// commit it just cut, or a foreign owner's head goes undetected and gets swept. Same 13-commit
    /// chain and `WS_SNAPSHOT_KEEP=10` as `retention_deletes_beyond_keep_sparing_pinned_and_heads`
    /// (both set it to the default value, so no race with tests that leave it unset) — but here
    /// the tail entry that test deletes (`c2`) is instead a DIFFERENT owner's head.
    #[tokio::test]
    async fn retention_spares_another_owners_head_on_a_shared_volume() {
        std::env::set_var("WS_SNAPSHOT_KEEP", "10");
        let tmp = tempfile::tempdir().unwrap();
        let name = |i: i32| format!("vol-1-c{i}");
        let mut items = Vec::new();
        for i in 0..13 {
            let parent = if i == 0 { String::new() } else { name(i - 1) };
            // c1 pinned, exactly as the sibling test — ownership of a COMMIT is not what protects
            // it, only being pinned or being some worktree's head is, and this test isolates the
            // "different owner's head" case from those two.
            let pinned = i == 1;
            let owner = if i == 2 { "bob" } else { "alice" };
            items.push(serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                "metadata": {"name": name(i), "uid": format!("{}-uid", name(i))},
                "spec": {"volume": "vol-1", "owner": owner, "worktree": "ws-1", "parent": parent, "pinned": pinned},
                "status": {"phase": "ready"},
            }));
        }
        // Bob's own worktree, on the SAME volume as alice's chain, owned by a different owner —
        // its head is `c2`, the exact tail entry the sibling test deletes when nothing protects it.
        let bob_ws = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-bob", "uid": "bob-uid"},
            "spec": {"owner": "bob", "team": "", "name": "bob", "region": "r1", "image": "img", "desiredState": "running"},
            "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-1", "head": name(2)},
        });
        // c0, alice's own head, is protected the same way the sibling test protects it.
        let alice_ws = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-1", "uid": "ws-1-uid"},
            "spec": {"owner": "alice", "team": "", "name": "web", "region": "r1", "image": "img", "desiredState": "running"},
            "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-1", "head": name(0)},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: list_of("Snapshot", items) },
            Route { method: "GET", path: WORKSPACES_LIST.into(), status: 200, body: list_of("Workspace", vec![alice_ws, bob_ws]) },
            Route { method: "GET", path: ENVIRONMENTS_LIST.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "vol-1", &name(12)).await;

        assert!(
            rec.calls().iter().all(|c| !c.starts_with("DELETE")),
            "a different owner's head on a shared volume must still be spared: {:?}",
            rec.calls()
        );
    }

    fn snap(name: &str, volume: &str, worktree: &str, transient: bool, parent: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": name, "uid": format!("{name}-uid")},
            "spec": {"volume": volume, "owner": "alice", "worktree": worktree,
                     "parent": parent, "pinned": false, "transient": transient},
            "status": {"phase": "ready"},
        })
    }

    /// One Ready transient per worktree, and NOTHING else: a commit is not a sync point, and
    /// another worktree's sync point belongs to that worktree. The arm ignores `heads` and
    /// `pinned` deliberately (see its ponytail note), so this scoping is its whole safety argument.
    #[tokio::test]
    async fn the_transient_arm_spares_commits_and_other_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let items = vec![
            snap("v1-newsync", "v1", "ws-1", true, "v1-oldsync"),
            snap("v1-oldsync", "v1", "ws-1", true, ""),
            snap("v1-commit", "v1", "ws-1", false, ""),
            snap("v1-otherws", "v1", "ws-2", true, ""),
        ];
        let routes = vec![Route {
            method: "GET",
            path: "/apis/rustic-git.io/v1alpha1/snapshots".into(),
            status: 200,
            body: list_of("Snapshot", items),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "v1", "v1-newsync").await;

        let deleted: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deleted, vec!["DELETE /apis/rustic-git.io/v1alpha1/snapshots/v1-oldsync".to_string()], "{deleted:?}");
    }

    /// Keep-bias: a snapshot list this pass could not make deletes nothing at all.
    #[tokio::test]
    async fn a_snapshot_list_error_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route {
            method: "GET",
            path: "/apis/rustic-git.io/v1alpha1/snapshots".into(),
            status: 500,
            body: serde_json::json!({}),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        retain(&ctx, "v1", "v1-newsync").await;
        assert!(rec.calls().iter().all(|c| !c.starts_with("DELETE")), "{:?}", rec.calls());
    }
}
