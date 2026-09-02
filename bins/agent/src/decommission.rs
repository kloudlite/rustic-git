//! The decommission beat: the PLANNED version of node death, with one difference — whatever is
//! running here keeps running.
//!
//! It stops nothing. The node takes no new work (that is `peer::unplaceable`, which every node
//! applies to it), its copies are re-homed by ordinary rendezvous, and each volume it owns is
//! released as the people using it stop. Draining therefore takes as long as the people take;
//! an operator who needs the node sooner stops those workspaces through `/v1` like anyone else.
//!
//! Runs only on the node that carries the label, and only that node writes its own annotation.

use crate::controller::Ctx;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::ResourceExt;
use std::collections::HashSet;
use std::sync::Arc;

/// The operator's one window into a drain, rewritten each beat and readable with
/// `kubectl describe node`. ONE key: `draining …` while there is work left, `drained <RFC 3339>`
/// when there is not. Two keys would be two things to check and one to forget.
pub const DECOMMISSION_STATUS: &str = "rustic-git.io/decommission-status";

/// `WS_DECOMMISSION_SECS`, default 30 — fast, because everything it does is idempotent and cheap,
/// and because the thing it is waiting for (a person stopping their workspace) deserves a prompt
/// answer when it happens.
pub(crate) fn beat_interval() -> std::time::Duration {
    std::time::Duration::from_secs(std::env::var("WS_DECOMMISSION_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30))
}

/// Drained is a conjunction: nothing running here, no volume owned here, no replica row here.
/// Anything short of that is progress, and the counts say which of the three is holding it.
pub(crate) fn drain_status(running: usize, owned: usize, copies: usize, now: &str) -> String {
    if running == 0 && owned == 0 && copies == 0 {
        format!("drained {now}")
    } else {
        format!("draining running={running} owned={owned} copies={copies}")
    }
}

pub async fn decommission_beat(ctx: &Arc<Ctx>) {
    let nodes = match Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(error = %e, "decommission: listing nodes; doing nothing this beat");
            return;
        }
    };
    let me = nodes.iter().find(|n| n.name_any() == ctx.node);
    // Abort semantics, for free: the label gone means the beat does nothing at all — not even a
    // status rewrite. Copies already re-homed stay; this node is a rendezvous candidate again.
    if !crate::peer::decommissioning(me) {
        return;
    }
    // Keep-biased like every other beat: a half-listed cluster releases nothing.
    let Some(beat) = crate::listing::beat(ctx).await else { return };

    // 1. Running parents keep running, and are told why the next start lands elsewhere. The person
    //    decides when their edits are safe to leave this node; a drain never decides it for them.
    for p in beat.parents.iter().filter(|p| p.is_live_worktree()) {
        crate::peer::mark_parent(
            ctx,
            p,
            ("Decommissioning", true),
            "NodeLeaving",
            "this node is being retired; stop when convenient and the next start lands elsewhere",
            false,
        )
        .await;
    }

    // 2. Release owned volumes as they become releasable — the dead-node sweep's own three arms,
    //    the same function, called with this node as the "unavailable" owner and a different word.
    let mine: HashSet<String> = [ctx.node.clone()].into_iter().collect();
    crate::peer::sweep_volumes(ctx, &beat, &mine, "Decommissioned").await;

    // 3. Copies settle on their own: `unplaceable` already dropped this node from every other
    //    node's rendezvous, and its own retire pass drops each copy once the replacement is
    //    Synced. Nothing to do here — deliberately.

    // 4. Progress, or the stamp that gates deleting the VM. Counted off THIS beat's listing, so a
    //    volume released a moment ago still counts until the next beat sees it gone — `drained`
    //    can lag by one beat, but it can never be stamped early.
    let running = beat.parents.iter().filter(|p| p.is_live_worktree()).count();
    let owned = beat.volumes.iter().filter(|v| v.spec.node_name == ctx.node).count();
    let copies = beat.replicas.iter().filter(|r| r.spec.node == ctx.node).count();
    let status = drain_status(running, owned, copies, &chrono::Utc::now().to_rfc3339());
    let patch = serde_json::json!({"metadata": {"annotations": {DECOMMISSION_STATUS: status}}});
    if let Err(e) = Api::<Node>::all(ctx.client.clone())
        .patch(&ctx.node, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        tracing::warn!(error = %e, "decommission: annotating my own node");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
    use rustic_git_workspaces::kube_test::{get, mock_client, Recorder, Route};

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

    const NODES: &str = "/api/v1/nodes";
    const VOLUMES: &str = "/apis/rustic-git.io/v1alpha1/volumes";
    const VOLREPLICAS: &str = "/apis/rustic-git.io/v1alpha1/volumereplicas";
    const WORKSPACES: &str = "/apis/rustic-git.io/v1alpha1/workspaces";
    const ENVIRONMENTS: &str = "/apis/rustic-git.io/v1alpha1/environments";

    fn list_of(kind: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"apiVersion": "v1", "kind": format!("{kind}List"), "items": items})
    }

    fn node_ready_json(name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": name},
            "status": {"conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": "2000-01-01T00:00:00Z"}]},
        })
    }

    fn node_decommissioning(name: &str) -> serde_json::Value {
        let mut n = node_ready_json(name);
        n["metadata"]["labels"] =
            serde_json::json!({ rustic_git_workspaces::crd::DECOMMISSION_LABEL: "true" });
        n
    }

    fn vol_owned(name: &str, node: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "team": "", "nodeName": node, "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        })
    }

    fn vol_at_rv(name: &str, node: &str, rv: &str) -> serde_json::Value {
        let mut v = vol_owned(name, node);
        v["metadata"]["resourceVersion"] = serde_json::json!(rv);
        v
    }

    fn replica_of(volume: &str, node: &str, phase: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": format!("{volume}.{node}"), "uid": format!("uid-{node}")},
            "spec": {"volume": volume, "node": node},
            "status": {"phase": phase, "branches": {}},
        })
    }

    /// A workspace as the listing reads it. `podRef` is what makes it a LIVE worktree — without
    /// one nothing is writing to the subvolume, which is exactly the distinction the beat turns on.
    fn ws(name: &str, node: &str, volume: &str, desired: &str, pod: bool, replicated: bool) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "name": name, "region": "r1", "image": "img", "desiredState": desired, "packages": []},
            "status": {
                "phase": if pod { "ready" } else { "stopped" },
                "nodeName": node, "compatibleNodes": [node], "volumeRef": volume,
                "podRef": pod.then(|| format!("ws-alice/{name}")),
                "conditions": if replicated {
                    serde_json::json!([{"type": "Replicated", "status": "True", "reason": "Replicated", "message": "", "observedGeneration": 1, "lastTransitionTime": "2000-01-01T00:00:00Z"}])
                } else {
                    serde_json::json!([])
                },
            },
        })
    }

    fn ws_running(name: &str, node: &str, volume: &str) -> serde_json::Value {
        ws(name, node, volume, "running", true, false)
    }

    fn ws_stopped_replicated(name: &str, node: &str, volume: &str) -> serde_json::Value {
        ws(name, node, volume, "stopped", false, true)
    }

    /// One annotation key, not two: an operator greps `decommission-status` and gets the whole
    /// story, in progress or finished. Two keys is two things to remember and one to forget.
    #[test]
    fn the_status_line_carries_progress_then_the_drained_stamp() {
        assert_eq!(drain_status(2, 3, 1, "2026-09-03T10:00:00Z"), "draining running=2 owned=3 copies=1");
        assert_eq!(drain_status(0, 0, 0, "2026-09-03T10:00:00Z"), "drained 2026-09-03T10:00:00Z");
    }

    /// A running parent is NEVER stopped: it is the person's, and the node waits. It is told, in
    /// the one place a person looks, that the next start lands elsewhere.
    #[tokio::test]
    async fn running_parents_are_told_not_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_decommissioning("node-a")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-a")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws_running("ws-run", "node-a", "vol-1")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-run", ws_running("ws-run", "node-a", "vol-1")),
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-run/status".into(), status: 200, body: ws_running("ws-run", "node-a", "vol-1") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status".into(), status: 200, body: vol_owned("vol-1", "node-a") },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_decommissioning("node-a") },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        decommission_beat(&ctx).await;

        assert!(
            !rec.calls().iter().any(|c| c.starts_with("DELETE")),
            "a drain stops nothing, ever: {:?}",
            rec.calls()
        );
        assert!(
            !rec.calls().iter().any(|c| c == "PATCH /apis/rustic-git.io/v1alpha1/volumes/vol-1"),
            "a running parent keeps the pin: {:?}",
            rec.calls()
        );
        let sent = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-run/status").remove(0);
        let cond = sent["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Decommissioning").expect("the condition");
        assert_eq!(cond["reason"], "NodeLeaving");
        assert_eq!(cond["message"], "this node is being retired; stop when convenient and the next start lands elsewhere");
        assert_eq!(sent["status"]["nodeName"], "node-a", "a running worktree never moves");
        let ann = rec.sent("PATCH", "/api/v1/nodes/node-a").remove(0);
        assert_eq!(ann["metadata"]["annotations"]["rustic-git.io/decommission-status"], "draining running=1 owned=1 copies=0");
    }

    /// Drained is a conjunction of the three counts, and the annotation is the operator's gate on
    /// deleting the VM. Nothing else may stamp it.
    #[tokio::test]
    async fn a_node_with_nothing_left_is_stamped_drained() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_decommissioning("node-a"), node_ready_json("node-b")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-b")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![replica_of("vol-1", "node-b", "Synced")]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_decommissioning("node-a") },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        decommission_beat(&ctx).await;

        let ann = rec.sent("PATCH", "/api/v1/nodes/node-a").remove(0);
        let v = ann["metadata"]["annotations"]["rustic-git.io/decommission-status"].as_str().unwrap();
        assert!(v.starts_with("drained "), "{v}");
        assert!(chrono::DateTime::parse_from_rfc3339(v.trim_start_matches("drained ")).is_ok(), "{v}");
    }

    /// Abort: the label is gone, so the beat does nothing at all — not even a status rewrite.
    /// Parents already stopped stay stopped and copies already re-homed stay re-homed.
    #[tokio::test]
    async fn removing_the_label_stops_the_beat() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_ready_json("node-a")]) }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        decommission_beat(&ctx).await;
        assert_eq!(rec.calls(), vec![format!("GET {NODES}")], "{:?}", rec.calls());
    }

    /// A volume with everything stopped and replicated is released by the SAME arm the dead-node
    /// sweep uses — one function, called with a different owner set and a different word.
    #[tokio::test]
    async fn a_releasable_volume_is_released_with_the_decommissioned_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_decommissioning("node-a")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-a")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws_stopped_replicated("ws-1", "node-a", "vol-1")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-1", ws_stopped_replicated("ws-1", "node-a", "vol-1")),
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: ws_stopped_replicated("ws-1", "", "vol-1") },
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1".into(), status: 200, body: vol_at_rv("vol-1", "", "10") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status".into(), status: 200, body: vol_owned("vol-1", "") },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_decommissioning("node-a") },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        decommission_beat(&ctx).await;

        let vol = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status").remove(0);
        assert_eq!(vol["status"]["conditions"][0]["reason"], "Decommissioned");
        assert_eq!(rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-1/status")[0]["status"]["nodeName"], "");
    }
}
