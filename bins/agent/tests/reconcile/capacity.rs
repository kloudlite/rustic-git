//! capacity.

use super::*;


/// An 8-vCPU node with 7 vCPU already requested by pods assigned to it — the shape that stranded a
/// real workspace: the claim landed, the pod was `FailedScheduling: Insufficient cpu`, and nothing
/// ever un-places a live node's claim.
pub(crate) fn crowded(created_ago: Option<i64>) -> Vec<Route> {
    let mut v = vec![
        kloudlite_workspaces::kube_test::get(
            "/api/v1/nodes/node-a",
            serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "node-a"},
                               "status": {"allocatable": {"cpu": "8", "memory": "33554432Ki"},
                                          "conditions": [{"type": "Ready", "status": "True",
                                                          "lastTransitionTime": rfc3339_ago(60)}]}}),
        ),
        kloudlite_workspaces::kube_test::get(
            "/api/v1/pods",
            serde_json::json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": [
                {"metadata": {"name": "busy", "namespace": "ws-bob"},
                 "spec": {"containers": [{"name": "c", "resources": {"requests": {"cpu": "7", "memory": "8Gi"}}}]},
                 "status": {"phase": "Running"}},
                // Terminated: its capacity is back, and counting it would refuse claims on a node
                // full of yesterday's finished pods.
                {"metadata": {"name": "done", "namespace": "ws-bob"},
                 "spec": {"containers": [{"name": "c", "resources": {"requests": {"cpu": "8", "memory": "8Gi"}}}]},
                 "status": {"phase": "Succeeded"}}]}),
        ),
    ];
    if created_ago.is_some() {
        v.push(Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) });
        v.push(kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces/ws-1", ws_json(serde_json::json!({}))));
    }
    v
}

/// A default workspace requests 2 vCPU; only 1 is left. The node must decline so a peer with room
/// takes it — and it must not write the "nobody has room" condition yet, because a peer claiming
/// this a second later is the normal case.
#[tokio::test]
async fn a_node_without_room_declines_a_fresh_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), crowded(None));

    kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "a node with 1 vCPU free must not take a 2 vCPU workspace: {:?}", rec.calls());
    assert!(
        rec.requests().iter().any(|c| c.contains("/api/v1/pods?") && c.contains("spec.nodeName%3Dnode-a")),
        "the pod list must be scoped to this node: {:?}", rec.requests()
    );
}

/// The same node, but the workspace has been sitting unplaced past the bound: somebody has to say
/// why, or it stays `Creating` forever with no explanation. Every node that declines writes the
/// same condition and `mark_parent_of`'s idle check absorbs the duplicates.
#[tokio::test]
async fn a_workspace_nothing_can_fit_gets_the_no_capacity_condition() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), crowded(Some(600)));
    let mut w = ws_json(serde_json::json!({}));
    w["metadata"]["creationTimestamp"] = serde_json::json!(rfc3339_ago(600));

    kloudlite_agent::claim::claim_workspace(&serde_json::from_value(w).unwrap(), &ctx).await.unwrap();

    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "one condition write, and it is not a claim: {:?}", rec.calls());
    assert!(sent[0]["status"]["nodeName"].as_str().unwrap_or_default().is_empty(), "declining never places: {}", sent[0]);
    let c = sent[0]["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Placed").expect("Placed");
    assert_eq!(c["status"], "False");
    assert_eq!(c["reason"], "NoCapacity");
    assert!(c["message"].as_str().unwrap().contains("2000m cpu"), "the message names what it needs: {c}");
    assert!(c["message"].as_str().unwrap().contains("4096 MiB"), "{c}");
}

/// The hand-off, which is the whole point of declining: the crowded node writes nothing, and the
/// SAME parent offered to a node with room is claimed by it.
#[tokio::test]
async fn a_parent_a_full_node_declined_is_claimed_by_a_node_with_room() {
    let tmp = tempfile::tempdir().unwrap();
    let (full, full_rec) = ctx(tmp.path(), crowded(None));
    let w = workspace(serde_json::json!({}));
    kloudlite_agent::claim::claim_workspace(&w, &full).await.unwrap();
    assert!(full_rec.sent("PUT", WS_STATUS).is_empty(), "the full node declines: {:?}", full_rec.calls());

    let (roomy, roomy_rec) = ctx_on_node(
        "node-b",
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
        Arc::new(FakeNix::default()),
        Some("127.0.0.1:/".into()),
    );
    kloudlite_agent::claim::claim_workspace(&w, &roomy).await.unwrap();
    let sent = roomy_rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "the node with room takes it: {:?}", roomy_rec.calls());
    assert_eq!(sent[0]["status"]["nodeName"], "node-b");
}

/// And the condition does not outlive the problem: a claim replaces the whole condition array, so
/// a parent that was `NoCapacity` comes back as `Placed=True/Claimed` with no trace of it.
#[tokio::test]
async fn a_claim_clears_a_no_capacity_condition() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    let w = workspace(serde_json::json!({
        "phase": "pending", "nodeName": "",
        "conditions": [{"type": "Placed", "status": "False", "reason": "NoCapacity",
                        "message": "no node has room for it: it requests 2000m cpu and 4096 MiB",
                        "lastTransitionTime": rfc3339_ago(600)}]}));

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();

    let conds = rec.sent("PUT", WS_STATUS)[0]["status"]["conditions"].clone();
    let conds = conds.as_array().unwrap();
    assert!(conds.iter().any(|c| c["type"] == "Placed" && c["status"] == "True" && c["reason"] == "Claimed"), "{conds:?}");
    assert!(!conds.iter().any(|c| c["reason"] == "NoCapacity"), "the reason must not survive the claim: {conds:?}");
}

/// A burst: four workspaces created at once all read a pod list with none of the others in it,
/// because the claim writes `status.nodeName` a whole reconcile before the pod exists. Counting
/// what is CLAIMED here — three 2-vCPU workspaces on an 8-vCPU node — is what stops the fourth.
#[tokio::test]
async fn workspaces_claimed_here_but_not_yet_running_are_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let claimed = |name: &str| {
        let mut w = ws_json(serde_json::json!({"phase": "pending", "nodeName": "node-a"}));
        w["metadata"]["name"] = serde_json::json!(name);
        w
    };
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![kloudlite_workspaces::kube_test::get(
            WORKSPACES_LIST,
            serde_json::json!({"apiVersion": "v1", "kind": "WorkspaceList", "metadata": {},
                               "items": [claimed("ws-a"), claimed("ws-b"), claimed("ws-c"), claimed("ws-d")]}),
        )],
    );

    kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert!(
        rec.sent("PUT", WS_STATUS).is_empty(),
        "4 claimed x 2 vCPU fills an 8 vCPU node, and not one of them has a pod yet: {:?}", rec.calls()
    );
}

/// A STOPPED workspace placed here holds nothing: its pod is gone and its capacity really is free.
#[tokio::test]
async fn a_stopped_workspace_placed_here_costs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let stopped = |name: &str| {
        let mut w = ws_json(serde_json::json!({"phase": "stopped", "nodeName": "node-a"}));
        w["metadata"]["name"] = serde_json::json!(name);
        w["spec"]["desiredState"] = serde_json::json!("stopped");
        w
    };
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                WORKSPACES_LIST,
                serde_json::json!({"apiVersion": "v1", "kind": "WorkspaceList", "metadata": {},
                                   "items": [stopped("ws-a"), stopped("ws-b"), stopped("ws-c")]}),
            ),
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );

    kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "stopped parents free their capacity: {:?}", rec.calls());
}

/// A parent RELEASED by the sweep hours after it was created must not be declared `NoCapacity` on
/// the first decline: the grace runs from the `Placed` transition, which is when it became
/// unplaced, so the peer that is about to take it gets its turn and `Moving` survives.
#[tokio::test]
async fn a_just_released_parent_keeps_its_moving_condition() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), crowded(Some(600)));
    let mut w = ws_json(serde_json::json!({
        "phase": "pending", "nodeName": "",
        "conditions": [{"type": "Placed", "status": "False", "reason": "Moving",
                        "message": "released so an up-to-date node can start it",
                        "lastTransitionTime": rfc3339_ago(5)}]}));
    w["metadata"]["creationTimestamp"] = serde_json::json!(rfc3339_ago(86400));

    kloudlite_agent::claim::claim_workspace(&serde_json::from_value(w).unwrap(), &ctx).await.unwrap();
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "5s unplaced is inside the grace: {:?}", rec.calls());
}

/// The correctness path is untouched: a parent whose bytes are HERE is claimed however full this
/// node is — no other node can run it correctly, and packing never outranks that. The Pod list is
/// not even issued.
#[tokio::test]
async fn a_workspace_whose_snapshots_are_here_claims_regardless_of_capacity() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = crowded(None);
    routes.push(Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![snapshot_cr("vol-1-a", "vol-1")]) });
    routes.push(kloudlite_workspaces::kube_test::get(
        format!("/apis/kloudlite.io/v1alpha1/volumereplicas/{}", crd::replica_name("vol-1", "node-a")),
        volume_replica("vol-1", "node-a", "Synced"),
    ));
    routes.push(Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) });
    routes.push(binding_route());
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "the data is here: claim it: {:?}", rec.calls());
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
    assert!(!rec.calls().iter().any(|c| c == "GET /api/v1/pods"), "no capacity check on the non-fresh path: {:?}", rec.calls());
}
