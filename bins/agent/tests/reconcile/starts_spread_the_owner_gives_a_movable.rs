//! starts spread: the owner gives a movable volume away.

use super::*;


/// A bare `Ctx` on a named node with EXACTLY the routes given — no defaults appended, because
/// these tests assert on the absence of calls as much as on their content.
pub(crate) fn ctx_with_node(pool: &std::path::Path, node: &str, mut routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
    // `start_placement` narrows its candidates to the nodes that have room, which reads what is
    // scheduled on each. A test that says nothing about pods means an empty cluster, not a 404.
    if !routes.iter().any(|r| r.method == "GET" && r.path == "/api/v1/pods") {
        routes.push(kloudlite_workspaces::kube_test::get(
            "/api/v1/pods",
            serde_json::json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []}),
        ));
    }
    let (client, rec) = mock_client(routes);
    let profiles = pool.join("profiles");
    let _ = std::fs::create_dir_all(&profiles);
    std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/kloudlite-workspace:deadbeef");
    (
        Arc::new(Ctx::new(
            client,
            Arc::new(Engine::new(Pool::new(pool))),
            node.into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            true,
            Some("127.0.0.1:/".into()),
            "registry.kloudlite.io".into(),
            Arc::new(FakeNix::default()),
            profiles,
            test_settings(),
        )),
        rec,
    )
}

pub(crate) fn transient(name: &str, volume: &str, worktree: &str, generation: u64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name, "uid": format!("uid-{name}"),
                     "annotations": {"kloudlite.io/synced-generation": generation.to_string()}},
        "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": "",
                 "transient": true},
        "status": {"phase": "ready"},
    })
}

pub(crate) fn node_ready(name: &str) -> serde_json::Value {
    serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": name},
                       "status": {"conditions": [{"type": "Ready", "status": "True",
                                                  "lastTransitionTime": rfc3339_ago(60)}]}})
}

pub(crate) fn vol_owned(name: &str, node: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1},
        "spec": {"owner": "alice", "nodeName": node, "region": "r1", "quotaGb": 10},
    })
}

pub(crate) fn vol_obj(name: &str, node: &str) -> crd::Volume {
    serde_json::from_value(vol_owned(name, node)).unwrap()
}

pub(crate) fn placed_ws(name: &str, node: &str) -> serde_json::Value {
    let mut o = ws_json(serde_json::json!({"phase": "ready", "nodeName": node, "volumeRef": name}));
    o["metadata"] = serde_json::json!({"name": name, "uid": format!("{name}-uid")});
    o
}

pub(crate) fn parent_at(name: &str, volume: &str, phase: crd::Phase, pod: Option<&str>) -> kloudlite_agent::listing::Parent {
    kloudlite_agent::listing::Parent {
        kind: "Workspace",
        name: name.into(),
        volume: volume.into(),
        owner: "alice".into(),
        node_name: "node-a".into(),
        head: None,
        phase,
        pod_ref: pod.map(Into::into),
        owner_ref: Default::default(),
        replicated: true,
        state: crd::SnapshotState::Workspace {
            image: "alpine:3.20".into(),
            packages: vec![],
            locks: vec![],
            resources: Default::default(),
            quota_gb: 5,
            attached_environment: None,
        },
    }
}

pub(crate) fn stopped_parent(name: &str, volume: &str) -> kloudlite_agent::listing::Parent {
    parent_at(name, volume, crd::Phase::Stopped, None)
}

pub(crate) fn running_parent(name: &str, volume: &str) -> kloudlite_agent::listing::Parent {
    parent_at(name, volume, crd::Phase::Ready, Some("ws-alice/p"))
}

/// A movable volume — nothing on it running — spreads: the OWNER computes the preferred node over
/// {itself} ∪ {nodes up to date for every stopped parent on it}, and hands the volume over when
/// that is not itself. Only the owner may give a volume away; it is the one node that certainly
/// is not mid-takeover.
#[tokio::test]
async fn a_movable_volume_whose_preferred_node_is_a_peer_is_released_and_un_placed() {
    let tmp = tempfile::tempdir().unwrap();
    let peer_holds = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-1.node-b"},
        "spec": {"volume": "vol-1", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "stop-ws-1-3", "ws-2": "stop-ws-2-1"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [peer_holds]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [
                    transient("stop-ws-1-3", "vol-1", "ws-1", 7), transient("stop-ws-2-1", "vol-1", "ws-2", 4)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "NodeList", "items": [node_ready("node-a"), node_ready("node-b")]}) },
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1".into(), status: 200, body: vol_owned("vol-1", "") },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1".into(), status: 200, body: placed_ws("ws-1", "node-a") },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-2".into(), status: 200, body: placed_ws("ws-2", "node-a") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: placed_ws("ws-1", "") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-2/status".into(), status: 200, body: placed_ws("ws-2", "") },
    ];
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", routes);
    // Both parents stopped: the volume is movable.
    let parents = vec![stopped_parent("ws-1", "vol-1"), stopped_parent("ws-2", "vol-1")];

    let chosen = kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-1", "node-a"), &parents).await.unwrap();

    // node-b wins the rendezvous for "vol-1" over {node-a, node-b} — deterministic, so this is a
    // fixed expectation, not a coin flip.
    assert_eq!(chosen.as_deref(), Some("node-b"));
    let ops = rec.sent("PATCH", "/apis/kloudlite.io/v1alpha1/volumes/vol-1").remove(0);
    assert_eq!(ops[0], serde_json::json!({"op": "test", "path": "/spec/nodeName", "value": "node-a"}));
    assert_eq!(ops[1], serde_json::json!({"op": "replace", "path": "/spec/nodeName", "value": ""}));
    for name in ["ws-1", "ws-2"] {
        let sent = rec.sent("PUT", &format!("/apis/kloudlite.io/v1alpha1/workspaces/{name}/status"));
        assert_eq!(sent[0]["status"]["nodeName"], "", "every parent on the volume follows, not just the started one");
        let conds = sent[0]["status"]["conditions"].as_array().cloned().unwrap_or_default();
        // A routine spread is not a failure: `Placed=False/Moving`, never the sweep's `Degraded`.
        assert!(conds.iter().any(|c| c["type"] == "Placed" && c["status"] == "False" && c["reason"] == "Moving"), "{conds:?}");
        assert!(!conds.iter().any(|c| c["type"] == "Degraded"), "{conds:?}");
    }
}

/// The candidate set is the INTERSECTION over every parent, not a union: node-b holds ws-1's stop
/// cut but not ws-2's, so it would strand ws-2 — and the owner keeps the volume.
#[tokio::test]
async fn a_node_up_to_date_for_only_one_of_two_parents_is_no_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let partial = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-1.node-b"},
        "spec": {"volume": "vol-1", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "stop-ws-1-3"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [partial]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [
                    transient("stop-ws-1-3", "vol-1", "ws-1", 7), transient("stop-ws-2-1", "vol-1", "ws-2", 4)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "NodeList", "items": [node_ready("node-a"), node_ready("node-b")]}) },
    ];
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", routes);
    let parents = vec![stopped_parent("ws-1", "vol-1"), stopped_parent("ws-2", "vol-1")];

    assert_eq!(kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-1", "node-a"), &parents).await.unwrap(), None);
    assert!(!rec.calls().iter().any(|c| c.starts_with("PATCH") || c.starts_with("PUT")), "{:?}", rec.calls());
}

/// A volume with a RUNNING parent is not movable: a stopped sibling starts on the owner, because
/// that is where the volume is and nothing is ever moved out from under a running pod.
#[tokio::test]
async fn a_volume_with_a_running_sibling_never_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", vec![]);
    let parents = vec![running_parent("ws-1", "vol-1"), stopped_parent("ws-2", "vol-1")];

    assert_eq!(kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-1", "node-a"), &parents).await.unwrap(), None);
    assert!(rec.calls().is_empty(), "not movable is decided locally, with no API calls at all: {:?}", rec.calls());
}

/// Only the OWNER hands a volume away: a node reconciling a parent whose volume is pinned
/// elsewhere decides nothing and writes nothing.
#[tokio::test]
async fn a_node_that_does_not_own_the_volume_releases_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", vec![]);
    let parents = vec![stopped_parent("ws-1", "vol-1")];

    assert_eq!(kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-1", "node-b"), &parents).await.unwrap(), None);
    assert!(rec.calls().is_empty(), "{:?}", rec.calls());
}

/// No up-to-date replica: the candidate set is exactly {owner}, so it starts here. This is the
/// `replicas: 1` case and the "the stop cut has not landed anywhere yet" case, both.
#[tokio::test]
async fn with_no_up_to_date_replica_the_owner_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let behind = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-1.node-b"}, "spec": {"volume": "vol-1", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "sync-ws-1-old"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [behind]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-1", "ws-1", 7)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "NodeList", "items": [node_ready("node-a"), node_ready("node-b")]}) },
    ];
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", routes);
    let parents = vec![stopped_parent("ws-1", "vol-1")];

    assert_eq!(kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-1", "node-a"), &parents).await.unwrap(), None);
    assert!(!rec.calls().iter().any(|c| c.starts_with("PATCH")), "nothing is released when there is nowhere to go");
}

/// Preferred == owner: the common case, and it must cost nothing but the read.
#[tokio::test]
async fn when_the_owner_is_preferred_it_starts_here_with_no_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let holds = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-3.node-b"}, "spec": {"volume": "vol-3", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "stop-ws-1-3"}},
    });
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [holds]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-3", "ws-1", 7)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "NodeList", "items": [node_ready("node-a"), node_ready("node-b")]}) },
    ];
    // "vol-3" is the id whose rendezvous over {node-a, node-b} scores node-a top — the same
    // deterministic hash `preferred_node`'s own test asserts, used here so this stays fixed.
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", routes);
    let parents = vec![stopped_parent("ws-1", "vol-3")];
    assert_eq!(kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-3", "node-a"), &parents).await.unwrap(), None);
    assert!(!rec.calls().iter().any(|c| c.starts_with("PATCH") || c.starts_with("PUT")));
}

/// The narrowing rule, against the case above: the SAME volume, whose rendezvous still scores the
/// owner top, moves to the peer once the owner has no room for it. This is the pile-up that put
/// three workspaces on an 8-vCPU node and left the fourth `Pending` while a bigger node idled —
/// the hash spread evenly over the candidates and knew nothing about how full each one was.
#[tokio::test]
async fn a_full_owner_hands_a_movable_volume_to_a_peer_with_room() {
    let tmp = tempfile::tempdir().unwrap();
    let holds = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-3.node-b"}, "spec": {"volume": "vol-3", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "stop-ws-1-3"}},
    });
    let sized = |name: &str| {
        serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": name,
                           "labels": {"kloudlite.io/pool": "true"}},
                           "status": {"allocatable": {"cpu": "8", "memory": "33554432Ki"},
                                      "conditions": [{"type": "Ready", "status": "True",
                                                      "lastTransitionTime": rfc3339_ago(60)}]}})
    };
    // node-a is full: four default workspaces at 2 vCPU each is the whole 8 vCPU. Three would
    // still fit a fourth exactly, which is the arithmetic `fits` is deliberately strict about.
    let busy = |n: u32| {
        serde_json::json!({"apiVersion": "v1", "kind": "Pod",
                           "metadata": {"name": format!("ws-{n}"), "namespace": "ws-alice"},
                           "spec": {"nodeName": "node-a", "containers": [{"name": "c",
                                    "resources": {"requests": {"cpu": "2", "memory": "4Gi"}}}]},
                           "status": {"phase": "Running"}})
    };
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [holds]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-3", "ws-1", 7)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "NodeList", "items": [sized("node-a"), sized("node-b")]}) },
        Route { method: "GET", path: "/api/v1/pods".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "PodList", "items": [busy(1), busy(2), busy(3), busy(4)]}) },
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-3".into(), status: 200, body: vol_owned("vol-3", "") },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1".into(), status: 200, body: placed_ws("ws-1", "node-a") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: placed_ws("ws-1", "") },
    ];
    let (ctx, _rec) = ctx_with_node(tmp.path(), "node-a", routes);
    let parents = vec![stopped_parent("ws-1", "vol-3")];
    let to = kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-3", "node-a"), &parents).await.unwrap();
    assert_eq!(to.as_deref(), Some("node-b"), "the owner is out of room, so the up-to-date peer takes it");
}

/// A RETIRING owner hands over even when the hash prefers it: `ws.cross.node` labels the owner
/// `kloudlite.io/decommission=true`, stops the workspace and starts it, and expects it back on a
/// peer. With the owner left in the candidate set the hash kept "vol-3" on node-a (its preferred
/// node, see the test above), the workspace restarted on the draining node, and the drain could
/// never finish because a running parent pins its volume.
#[tokio::test]
async fn a_retiring_owner_hands_a_movable_volume_to_an_up_to_date_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let holds = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-3.node-b"}, "spec": {"volume": "vol-3", "node": "node-b"},
        "status": {"phase": "Synced", "branches": {"ws-1": "stop-ws-1-3"}},
    });
    let retiring = serde_json::json!({"apiVersion": "v1", "kind": "Node",
        "metadata": {"name": "node-a", "labels": {"kloudlite.io/decommission": "true"}},
        "status": {"conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": rfc3339_ago(60)}]}});
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [holds]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-3", "ws-1", 7)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "NodeList", "items": [retiring, node_ready("node-b")]}) },
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-3".into(), status: 200, body: vol_owned("vol-3", "") },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1".into(), status: 200, body: placed_ws("ws-1", "node-a") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: placed_ws("ws-1", "") },
    ];
    let (ctx, _rec) = ctx_with_node(tmp.path(), "node-a", routes);
    let parents = vec![stopped_parent("ws-1", "vol-3")];
    let to = kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-3", "node-a"), &parents).await.unwrap();
    assert_eq!(to.as_deref(), Some("node-b"), "the owner is being retired, so the up-to-date peer takes it");
}

/// The same retiring owner, a beat EARLIER: the stop cut is seconds old and no peer has pulled it
/// yet. The live drill lost exactly this race — start followed stop at once, both replicas were
/// Synced five seconds after the pod was already back on the draining node. A retiring owner with
/// nobody up to date must release with no target rather than start, so the first peer to hold the
/// cut claims it.
#[tokio::test]
async fn a_retiring_owner_with_no_peer_ready_releases_rather_than_starting() {
    let tmp = tempfile::tempdir().unwrap();
    let behind = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": "vol-3.node-b"}, "spec": {"volume": "vol-3", "node": "node-b"},
        "status": {"phase": "Syncing", "branches": {}},
    });
    let retiring = serde_json::json!({"apiVersion": "v1", "kind": "Node",
        "metadata": {"name": "node-a", "labels": {"kloudlite.io/decommission": "true"}},
        "status": {"conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": rfc3339_ago(60)}]}});
    let routes = vec![
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/volumereplicas".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "VolumeReplicaList", "items": [behind]}) },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/snapshots".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": [transient("stop-ws-1-3", "vol-3", "ws-1", 7)]}) },
        Route { method: "GET", path: "/api/v1/nodes".into(), status: 200,
                body: serde_json::json!({"apiVersion": "v1", "kind": "NodeList", "items": [retiring, node_ready("node-b")]}) },
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-3".into(), status: 200, body: vol_owned("vol-3", "") },
        Route { method: "GET", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1".into(), status: 200, body: placed_ws("ws-1", "node-a") },
        Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: placed_ws("ws-1", "") },
    ];
    let (ctx, rec) = ctx_with_node(tmp.path(), "node-a", routes);
    let parents = vec![stopped_parent("ws-1", "vol-3")];
    let out = kloudlite_agent::controller::start_placement(&ctx, &vol_obj("vol-3", "node-a"), &parents).await.unwrap();
    assert!(out.is_some(), "a retiring owner does not start the parent on itself");
    let ops = rec.sent("PATCH", "/apis/kloudlite.io/v1alpha1/volumes/vol-3").remove(0);
    assert_eq!(ops[1], serde_json::json!({"op": "replace", "path": "/spec/nodeName", "value": ""}), "released with no target");
    assert_eq!(rec.sent("PUT", "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status").len(), 1, "the parent is un-placed for a peer to claim");
}

/// F5 (drill, 2026-09-03): a workspace stopped on a node that then died kept the sweep's
/// `Degraded=True/NodeDead` after the node came back Ready — nothing cleared it, so `/v1` went on
/// answering `start` with 409 "interrupted" for as long as the object lived. The owner reconciling
/// its own stopped object is the proof its node is alive.
#[tokio::test]
async fn a_stopped_parent_reconciled_by_its_owner_drops_node_dead() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = replicated_routes("stop-ws-1-3");
    routes.push(Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) });
    let (ctx, rec) = ctx(tmp.path(), routes);

    let mut j = ws_json(serde_json::json!({
        "phase": "stopped", "nodeName": "node-a", "volumeRef": "vol-1", "observedGeneration": 1,
        "conditions": [
            {"type": "Ready", "status": "True", "reason": "Stopped", "message": "pushed and stopped",
             "lastTransitionTime": "2000-01-01T00:00:00Z"},
            {"type": "Degraded", "status": "True", "reason": "NodeDead", "message": "owner node-a is unavailable",
             "lastTransitionTime": "2000-01-01T00:00:00Z"},
        ],
    }));
    j["spec"]["desiredState"] = serde_json::json!("stopped");
    let w: crd::Workspace = serde_json::from_value(j).unwrap();

    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let st = rec.sent("PATCH", WS_STATUS).last().expect("a status write").clone();
    let conds = st["status"]["conditions"].as_array().unwrap().clone();
    assert!(!conds.iter().any(|c| c["type"] == "Degraded"), "NodeDead must be gone: {conds:?}");
    assert!(conds.iter().any(|c| c["type"] == "Ready" && c["reason"] == "Stopped"), "and the rest kept: {conds:?}");
}

/// Round 2 of the drill fixes: the self-dead guard was only in the pull beat, so a partitioned
/// agent (kubelet down, API server reachable) went on reconciling every 15 s — clearing the
/// sweep's `NodeDead` and letting `/v1` accept `start` on a node the cluster reads as dead. A dead
/// node writes NOTHING.
#[tokio::test]
async fn a_parent_whose_own_node_is_dead_is_not_reconciled_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = vec![Route {
        method: "GET",
        path: "/api/v1/nodes/node-a".into(),
        status: 200,
        body: serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "node-a"},
                                 "status": {"conditions": [{"type": "Ready", "status": "False",
                                                            "lastTransitionTime": "2000-01-01T00:00:00Z"}]}}),
    }];
    routes.extend(replicated_routes("stop-ws-1-3"));
    let (ctx, rec) = ctx(tmp.path(), routes);

    let mut j = ws_json(serde_json::json!({
        "phase": "stopped", "nodeName": "node-a", "volumeRef": "vol-1", "observedGeneration": 1,
        "conditions": [
            {"type": "Degraded", "status": "True", "reason": "NodeDead", "message": "owner node-a is unavailable",
             "lastTransitionTime": "2000-01-01T00:00:00Z"},
        ],
    }));
    j["spec"]["desiredState"] = serde_json::json!("stopped");
    let w: crd::Workspace = serde_json::from_value(j).unwrap();

    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    assert_eq!(rec.calls(), vec!["GET /api/v1/nodes/node-a".to_string()], "one read, no writes: {:?}", rec.calls());
}

pub(crate) const VOL_WS_SRC: &str = "/apis/kloudlite.io/v1alpha1/volumes/ws-src";

/// Design rule 6: a restored (or cloned) working copy is grafted onto the SOURCE's volume, and it
/// must become an owner of that Volume — otherwise only the snapshots keep it alive and deleting
/// the last one collects the subvolume its pod runs on. The patch is `add`, because a detached
/// volume — the ordinary restore target — has no `ownerReferences` key at all.
#[tokio::test]
async fn a_restored_workspace_attaches_itself_to_the_source_volume() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/ws-src/live/ws-1")).unwrap();
    let routes = vec![
        source_workspace_exists("ws-src"),
        live_parent_ws(),
        kloudlite_workspaces::kube_test::get(VOL_WS_SRC, ready_source_volume("ws-src")),
        Route { method: "PATCH", path: VOL_WS_SRC.into(), status: 200, body: ready_source_volume("ws-src") },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/snapshots/ws-src-aaaaaaaa", ready_snapshot("ws-src-aaaaaaaa", "ws-src")),
        ready_binding(),
        ready_namespace(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());

    let _ = kloudlite_agent::controller::apply_workspace(&cloned_workspace("ws-src-aaaaaaaa", None), &ctx).await;

    let patches = rec.sent("PATCH", VOL_WS_SRC);
    assert_eq!(patches.len(), 1, "one attach patch: {:?}", rec.calls());
    // Guarded: a `test` that the key is absent, then the `add` — two parents attaching to one
    // detached volume at once must not overwrite each other's entry.
    assert_eq!(patches[0][0]["op"], "test");
    assert_eq!(patches[0][0]["path"], "/metadata/ownerReferences");
    assert!(patches[0][0]["value"].is_null(), "{:?}", patches[0][0]);
    assert_eq!(patches[0][1]["op"], "add");
    assert_eq!(patches[0][1]["path"], "/metadata/ownerReferences");
    assert_eq!(patches[0][1]["value"][0]["uid"], "ws-uid-1");
    assert_eq!(patches[0][1]["value"][0]["kind"], "Workspace");
    // Only ONE ownerReference may be the controller, and that is the volume's creator's.
    assert_eq!(patches[0][1]["value"][0]["controller"], false);
}

/// The attach is idempotent: the entry it wrote is read back on the next pass and nothing is
/// patched again. A controller that re-patched every reconcile would rewrite the list forever.
#[tokio::test]
async fn a_second_reconcile_does_not_re_attach() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/ws-src/live/ws-1")).unwrap();
    let mut attached = ready_source_volume("ws-src");
    attached["metadata"]["ownerReferences"] = serde_json::json!([
        {"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace", "name": "ws-1",
         "uid": "ws-uid-1", "controller": false, "blockOwnerDeletion": false}
    ]);
    let routes = vec![
        source_workspace_exists("ws-src"),
        kloudlite_workspaces::kube_test::get(VOL_WS_SRC, attached),
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/snapshots/ws-src-aaaaaaaa", ready_snapshot("ws-src-aaaaaaaa", "ws-src")),
        ready_binding(),
        ready_namespace(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());

    let _ = kloudlite_agent::controller::apply_workspace(&cloned_workspace("ws-src-aaaaaaaa", None), &ctx).await;

    assert!(rec.sent("PATCH", VOL_WS_SRC).is_empty(), "already an owner: {:?}", rec.calls());
}

/// The other end of the same rule: deleting the restored workspace removes EXACTLY its own entry
/// from the source volume's owner list — the source's own controller reference stays, so the
/// volume is still its child.
#[tokio::test]
async fn deleting_a_restored_workspace_removes_only_its_own_owner_entry() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let source_ref = serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
                                        "name": "ws-src", "uid": "src-uid", "controller": true});
    let mine = serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
                                  "name": "ws-1", "uid": "ws-uid-1", "controller": false});
    let mut vol = ready_source_volume("ws-src");
    vol["metadata"]["ownerReferences"] = serde_json::json!([source_ref, mine]);
    let routes = vec![
        kloudlite_workspaces::kube_test::get(SNAPS, snap_list(vec![snapshot_record("ws-src-aaaaaaaa", "ws-src", "ws-src")])),
        kloudlite_workspaces::kube_test::get(VOL_WS_SRC, vol.clone()),
        Route { method: "PATCH", path: VOL_WS_SRC.into(), status: 200, body: vol },
        Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-src"})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = deleting_ws(workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-src"})));

    kloudlite_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    let patches = rec.sent("PATCH", VOL_WS_SRC);
    assert_eq!(patches.len(), 1, "one detach patch: {:?}", rec.calls());
    assert_eq!(patches[0][1]["op"], "replace");
    assert_eq!(patches[0][1]["value"].as_array().unwrap().len(), 1, "only mine goes: {:?}", patches[0]);
    assert_eq!(patches[0][1]["value"][0]["uid"], "src-uid");
}
