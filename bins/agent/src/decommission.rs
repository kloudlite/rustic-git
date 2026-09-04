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
pub use crate::crd::DECOMMISSION_STATUS;

/// `WS_DECOMMISSION_SECS`, default 30 — fast, because everything it does is idempotent and cheap,
/// and because the thing it is waiting for (a person stopping their workspace) deserves a prompt
/// answer when it happens.
pub(crate) fn beat_interval(settings: &crate::controller::Settings) -> std::time::Duration {
    std::time::Duration::from_secs(settings.load().decommission_secs)
}

/// Drained is a conjunction: nothing running here, no volume owned here, no replica row here, and
/// nothing whose durability still depends on this node. Anything short of that is progress, and the
/// counts say which of the four is holding it.
pub(crate) fn drain_status(running: usize, owned: usize, copies: usize, thin: usize, now: &str) -> String {
    if running == 0 && owned == 0 && copies == 0 && thin == 0 {
        format!("{}{now}", crate::crd::DRAINED_PREFIX)
    } else {
        format!("draining running={running} owned={owned} copies={copies} thin={thin}")
    }
}

/// Volumes whose bytes are still HERE — owned, or held as a copy — and which other nodes do not
/// yet hold `spec.replicas - 1` Synced copies of.
///
/// The other three counts say "is anything still on this node"; this one says "would deleting the
/// VM cost anyone a replica". A volume can be released and its last copy re-homed in the same beat
/// the operator reads `drained`, so without this the gate would open on a volume that is one node
/// away from having no redundancy at all. Synced only, and on OTHER nodes only: a Syncing row is a
/// transfer in progress, not durability, and this node's own copy is the one about to vanish.
fn thin_volumes(beat: &crate::listing::Beat, me: &str) -> usize {
    beat.volumes
        .iter()
        .filter(|v| {
            let name = v.name_any();
            let here = v.spec.node_name == me || beat.replicas.iter().any(|r| r.spec.volume == name && r.spec.node == me);
            if !here {
                return false;
            }
            let elsewhere = beat
                .replicas
                .iter()
                .filter(|r| r.spec.volume == name && r.spec.node != me && r.status.as_ref().is_some_and(|st| st.phase == "Synced"))
                .count();
            elsewhere < v.spec.replicas.saturating_sub(1) as usize
        })
        .count()
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

    // 1. Running parents keep running, and are told why the next start lands elsewhere — by their
    //    OWN reconcile (`controller::with_drain_notice`), not from here. This beat wrote the
    //    condition once and the parent's running arm, which rewrites conditions wholesale every
    //    TICK, erased it seconds later: the annotation said `running=1` while the workspace it
    //    named carried nothing. Counting them (step 4) is all this beat ever needed to do.

    // 2. Release owned volumes as they become releasable — the dead-node sweep's own three arms,
    //    the same function, called with this node as the "unavailable" owner and a different word.
    //    Release ONLY (`mark_running: false`): this node is alive, so a volume the arms say to
    //    Mark is a volume somebody is happily using, and `Unavailable`/`Degraded` on it would be
    //    a lie the API and `/v1`'s `interrupted()` both act on.
    let mine: HashSet<String> = [ctx.node.clone()].into_iter().collect();
    crate::peer::sweep_volumes(ctx, &beat, &mine, "Decommissioned", false).await;

    // 3. Copies settle on their own: `unplaceable` already dropped this node from every other
    //    node's rendezvous, and its own retire pass drops each copy once the replacement is
    //    Synced. Nothing to do here — deliberately.

    // 4. Progress, or the stamp that gates deleting the VM. Counted off THIS beat's listing, so a
    //    volume released a moment ago still counts until the next beat sees it gone — `drained`
    //    can lag by one beat, but it can never be stamped early.
    let running = beat.parents.iter().filter(|p| p.is_live_worktree()).count();
    let owned = beat.volumes.iter().filter(|v| v.spec.node_name == ctx.node).count();
    let copies = beat.replicas.iter().filter(|r| r.spec.node == ctx.node).count();
    let thin = thin_volumes(&beat, &ctx.node);
    let status = drain_status(running, owned, copies, thin, &chrono::Utc::now().to_rfc3339());
    // Sticky: the stamp answers "when did this node drain", so the FIRST beat that saw nothing
    // left is the one that owns it. Rewriting a fresh `now` every 30 s would turn the operator's
    // gate into "when did we last look", and lose the only timestamp anyone wants. A node that
    // starts hosting work again writes `draining …` over it immediately.
    if status.starts_with(crate::crd::DRAINED_PREFIX)
        && me.and_then(|n| n.metadata.annotations.as_ref())
            .and_then(|a| a.get(DECOMMISSION_STATUS))
            .is_some_and(|v| v.starts_with(crate::crd::DRAINED_PREFIX))
    {
        return;
    }
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
    use crate::testsupport::test_ctx;
    use rustic_git_workspaces::kube_test::{get, Route};

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

    fn node_drained(name: &str, status: &str) -> serde_json::Value {
        let mut n = node_decommissioning(name);
        n["metadata"]["annotations"] = serde_json::json!({ DECOMMISSION_STATUS: status });
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
                "nodeName": node, "volumeRef": volume,
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
        assert_eq!(drain_status(2, 3, 1, 0, "2026-09-03T10:00:00Z"), "draining running=2 owned=3 copies=1 thin=0");
        // Nothing left ON the node, but a volume it still holds bytes for is one copy short: the
        // gate stays shut, because deleting the VM is what would cost that copy.
        assert_eq!(drain_status(0, 0, 0, 1, "2026-09-03T10:00:00Z"), "draining running=0 owned=0 copies=0 thin=1");
        assert_eq!(drain_status(0, 0, 0, 0, "2026-09-03T10:00:00Z"), "drained 2026-09-03T10:00:00Z");
    }

    /// A running parent is NEVER stopped, never un-pinned and never marked: it is the person's,
    /// and the node waits. The drain notice they read is written by the parent's own reconcile
    /// (`controller::with_drain_notice`) — this beat only counts what is left.
    #[tokio::test]
    async fn a_drain_leaves_a_running_parent_completely_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_decommissioning("node-a")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-a")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws_running("ws-run", "node-a", "vol-1")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_decommissioning("node-a") },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        decommission_beat(&ctx).await;

        // A negative alone passes for the wrong reason against a mock that 404s everything. Pin
        // what the pass DID do, so a code change that calls something else fails here rather than
        // silently satisfying the absence.
        assert_eq!(
            rec.calls(),
            vec![
                format!("GET {NODES}"),
                format!("GET {VOLUMES}"),
                format!("GET {VOLREPLICAS}"),
                format!("GET {WORKSPACES}"),
                format!("GET {ENVIRONMENTS}"),
                "PATCH /api/v1/nodes/node-a".to_string(),
            ],
            "the beat lists, then stamps its own node's status — nothing else"
        );
        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "a drain stops nothing, ever: {:?}", rec.calls());
        // No parent write of ANY kind: the beat's mark was erased by the next running-arm
        // reconcile 15 s later, so writing it here was churn that fixed nothing. The route list
        // carries no parent-status route at all — a write would 404 against the mock.
        assert!(
            !rec.calls().iter().any(|c| c.contains("/workspaces/ws-run")),
            "a running parent is not touched by the beat at all: {:?}",
            rec.calls()
        );
        // The node is ALIVE and the workspace is happily running: nothing about it is degraded or
        // unavailable, and saying so would make `/v1`'s `interrupted()` 409 a clone of it.
        assert!(
            !rec.calls().iter().any(|c| c.contains("/volumes/vol-1")),
            "a drain never marks or releases a running volume: {:?}",
            rec.calls()
        );
        let ann = rec.sent("PATCH", "/api/v1/nodes/node-a").remove(0);
        assert_eq!(ann["metadata"]["annotations"]["rustic-git.io/decommission-status"], "draining running=1 owned=1 copies=0 thin=1");
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
        let ws = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-1/status").remove(0);
        assert_eq!(ws["status"]["nodeName"], "");
        // A stopped, fully replicated workspace is HEALTHY: it is being moved, not broken. The
        // word the claim itself owns is `Placed`, and `Degraded` here would paint it red in the
        // web for a routine retirement.
        let cond = ws["status"]["conditions"].as_array().unwrap().iter().find(|c| c["reason"] == "Decommissioned").expect("the condition");
        assert_eq!(cond["type"], "Placed");
        assert_eq!(cond["status"], "False");
        assert!(!ws["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Degraded"), "{ws}");
    }

    /// The gate is about the VM, not the node object: a node holding somebody's only other copy is
    /// not drained, however little is running on it. Deleting it there is the dead-node path.
    #[tokio::test]
    async fn a_node_still_holding_a_copy_is_not_stamped_drained() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_decommissioning("node-a"), node_ready_json("node-b")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-b")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![replica_of("vol-1", "node-a", "Synced")]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_decommissioning("node-a") },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        decommission_beat(&ctx).await;

        // `replicas: 2` and the only Synced copy off this node is... none: the owner's own bytes
        // are not a replica row, so this node's copy is still half the durability.
        assert_eq!(
            rec.sent("PATCH", "/api/v1/nodes/node-a").remove(0)["metadata"]["annotations"][DECOMMISSION_STATUS],
            "draining running=0 owned=0 copies=1 thin=1"
        );
    }

    /// The stamp answers "when did this node drain", so only the first beat that saw nothing left
    /// writes it — and a node that picks work back up says so at once.
    #[tokio::test]
    async fn the_drained_stamp_is_written_once_and_overwritten_only_by_progress() {
        let stamped = "drained 2026-01-01T00:00:00Z";
        let empty = |name: &str| {
            vec![
                Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![]) },
                Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
                Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![]) },
                Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
                Route { method: "PATCH", path: format!("/api/v1/nodes/{name}"), status: 200, body: node_drained(name, stamped) },
            ]
        };

        let tmp = tempfile::tempdir().unwrap();
        let mut routes = vec![Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_drained("node-a", stamped)]) }];
        routes.extend(empty("node-a"));
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        decommission_beat(&ctx).await;
        assert!(!rec.calls().iter().any(|c| c.starts_with("PATCH")), "an already-drained node keeps its stamp: {:?}", rec.calls());

        // Work landed back on it (a person restarted something the drain never stopped): the
        // stamp is wrong now and must go, on the very next beat.
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_drained("node-a", stamped)]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-a")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1".into(), status: 200, body: vol_at_rv("vol-1", "", "10") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status".into(), status: 200, body: vol_owned("vol-1", "") },
            Route { method: "PATCH", path: "/api/v1/nodes/node-a".into(), status: 200, body: node_drained("node-a", stamped) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        decommission_beat(&ctx).await;
        assert_eq!(
            rec.sent("PATCH", "/api/v1/nodes/node-a").remove(0)["metadata"]["annotations"][DECOMMISSION_STATUS],
            "draining running=0 owned=1 copies=0 thin=1"
        );
    }
}
