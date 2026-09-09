//! placement claims.

use super::*;


pub(crate) const WS_STATUS: &str = "/apis/kloudlite.io/v1alpha1/workspaces/ws-1/status";
pub(crate) const WORKSPACES_LIST: &str = "/apis/kloudlite.io/v1alpha1/workspaces";
pub(crate) const ENVIRONMENTS_LIST: &str = "/apis/kloudlite.io/v1alpha1/environments";
pub(crate) const BINDINGS: &str = "/apis/kloudlite.io/v1alpha1/ownerbindings";

pub(crate) fn ws_json(status: serde_json::Value) -> serde_json::Value {
    let mut o = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1",
        "kind": "Workspace",
        // `resourceVersion` is not decoration here: the claim carries it, and a test that omits it
        // would pass against a forced apply — the exact primitive this design refuses.
        "metadata": {"name": "ws-1", "uid": "ws-uid-1", "generation": 1, "resourceVersion": "42",
                     "labels": {"kloudlite.io/owner": "alice", "kloudlite.io/kind": "workspace",
                                "kloudlite.io/team": ""}},
        "spec": {"owner": "alice", "team": "", "name": "web", "region": "r1",
                 "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"},
    });
    // An object that has never been reconciled has NO status at all, not an empty one: `phase` is
    // required by the schema, so `status: {}` is a shape the API server can never return.
    if status != serde_json::json!({}) {
        o["status"] = status;
    }
    o
}

pub(crate) fn workspace(status: serde_json::Value) -> crd::Workspace {
    serde_json::from_value(ws_json(status)).unwrap()
}

/// The owner's binding, already NamespaceReady — the gate every workspace pass has to get past.
pub(crate) fn ready_binding() -> Route {
    kloudlite_workspaces::kube_test::get(
        format!("/apis/kloudlite.io/v1alpha1/ownerbindings/{}", crd::binding_name("r1", "alice")),
        serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
                           "metadata": {"name": "r1-alice"},
                           "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"},
                           "status": {"conditions": [{"type": "NamespaceReady", "status": "True",
                                                      "reason": "Converged", "message": "ok",
                                                      "lastTransitionTime": "2026-08-27T00:00:00Z"}]}}),
    )
}

/// The owner's own namespace, present — the other half `namespace_ready` now checks alongside the
/// binding condition.
pub(crate) fn ready_namespace() -> Route {
    kloudlite_workspaces::kube_test::get(
        format!("/api/v1/namespaces/{}", crd::ws_namespace("alice", "")),
        serde_json::json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "ws-alice"}}),
    )
}

pub(crate) fn binding_route() -> Route {
    kloudlite_workspaces::kube_test::post(
        BINDINGS,
        serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
                           "metadata": {"name": "r1-alice"},
                           "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"}}),
    )
}

/// The claim is ONE status write, and it is a status write — an API-authored spec is never touched
/// by a controller. Everything downstream (the Volume's node, the pod's hostPath and nodeSelector)
/// is derived from this one field.
///
/// It is a PUT (`replace_status`), not a forced apply: this is the one write in the system that
/// must be able to lose, and it carries the object's `resourceVersion` so that losing is a 409.
#[tokio::test]
async fn an_unplaced_workspace_is_claimed_with_one_optimistic_status_write() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );

    kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();

    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "exactly one status write");
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
    assert!(sent[0]["status"]["compatibleNodes"].is_null(), "compatibleNodes is dead and never written: {}", sent[0]);
    // The schema declares `status.phase` required; a write without it is a 422 from a real server.
    assert_eq!(sent[0]["status"]["phase"], "pending", "every status write carries a phase: {}", sent[0]);
    assert_eq!(
        sent[0]["metadata"]["resourceVersion"], "42",
        "without the resourceVersion the write cannot conflict, and the claim cannot race: {}", sent[0]
    );
    assert!(
        sent[0]["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Placed"),
        "the claim records itself as a condition: {}", sent[0]
    );
    assert!(rec.calls().iter().any(|c| c == &format!("POST {BINDINGS}")), "the binding must exist after a claim");
    assert!(
        !rec.calls().iter().any(|c| c == "PATCH /apis/kloudlite.io/v1alpha1/workspaces/ws-1"),
        "a controller never patches an API-authored spec: {:?}", rec.calls()
    );
}


/// The owner→node pin is gone: the home is a directory on a region-shared NFS mount every node
/// serves, so a binding naming another node no longer says anything about where this object's data
/// is. Placement is `may_claim` alone — and the binding, which still ensures namespaces, is now
/// reconciled on every node.
#[tokio::test]
async fn a_binding_on_another_node_no_longer_blocks_a_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                format!("/apis/kloudlite.io/v1alpha1/ownerbindings/{}", crd::binding_name("r1", "alice")),
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-alice"},
                                   "spec": {"owner": "alice", "region": "r1", "nodeName": "node-b"}}),
            ),
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );

    kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();

    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "a binding elsewhere no longer defers the claim: {:?}", rec.calls());
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
}

/// A node that cannot serve homes must not claim at all: `apply_workspace` would park the object
/// at `HomeNotReady` forever, and nothing ever un-places a live node's claim.
#[tokio::test]
async fn a_node_without_a_homes_export_does_not_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx_without_homes_export(tmp.path(), vec![]);

    kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "no claim without a shared home: {:?}", rec.calls());
}

/// An already-placed object is not re-claimed; a stop keeps `status.nodeName` precisely so a later
/// start reconciles on the same node with no placement step.
#[tokio::test]
async fn an_already_placed_workspace_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let w = workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a"}));

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "{:?}", rec.calls());
}

/// Losing the race must be a REAL conflict. `Patch::Apply(..).force()` never conflicts — it is the
/// wrong primitive for the one write in this system that must race — so the claim is an optimistic
/// write carrying the object's `resourceVersion`, and a 409 means another node won.
///
/// A 409 is not assumed to mean "placed": the claim RE-READS and runs the same decision again, so
/// a peer that only widened `compatibleNodes` does not scare this node off a claim it may still
/// make. Here the peer really did place it, so the re-read decides "leave it alone".
///
/// The loser must also not create the OwnerBinding: only the node whose claim actually won should
/// ever write it, or two nodes race to author the owner's one binding object.
#[tokio::test]
async fn a_claim_that_loses_the_race_re_reads_and_binds_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let conflict = Route {
        method: "PUT",
        path: WS_STATUS.into(),
        status: 409,
        body: serde_json::json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": "Conflict", "code": 409,
            "message": "the object has been modified; please apply your changes to the latest version"
        }),
    };
    let won_by_peer = ws_json(serde_json::json!({"phase": "pending", "nodeName": "node-b"}));
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![conflict, kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces/ws-1", won_by_peer)],
    );

    let action = kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "the winner's write is our wake-up");
    assert!(
        rec.calls().iter().any(|c| c == "GET /apis/kloudlite.io/v1alpha1/workspaces/ws-1"),
        "a 409 must re-read and re-decide, not assume: {:?}", rec.calls()
    );
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("POST")),
        "the loser must not bind the owner to a node it did not win: {:?}", rec.calls()
    );
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "one attempt, then re-read and yield — not a retry loop");
}

/// A clone holds nothing of its own, so it places by the ONE rule read over the SOURCE worktree:
/// this node has no replica of `ws-src` at all, so it must not claim. `source_nodes`' pin to the
/// source's `nodeName` is gone — a released or dead-node source is now claimable by any node that
/// is up to date for it.
#[tokio::test]
async fn a_clone_is_not_claimed_by_a_node_that_is_behind_on_its_source() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            // The source volume has snapshots and lives on node-b, so node-a claims only if it is up
            // to date for the source worktree.
            kloudlite_workspaces::kube_test::get(
                "/apis/kloudlite.io/v1alpha1/volumes/ws-src",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "ws-src", "uid": "src-uid"},
                                   "spec": {"owner": "alice", "nodeName": "node-b", "region": "r1", "quotaGb": 20}}),
            ),
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200,
                    body: snapshot_list_of("Snapshot", vec![snapshot_cr("ws-src-a", "ws-src")]) },
            kloudlite_workspaces::kube_test::not_found(format!(
                "/apis/kloudlite.io/v1alpha1/volumereplicas/{}",
                crd::replica_name("ws-src", "node-a")
            )),
        ],
    );
    let mut w = workspace(serde_json::json!({}));
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into(), commit: None }),
    });

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(
        rec.sent("PUT", WS_STATUS).is_empty(),
        "node-a is not up to date for ws-src and must not claim its clone: {:?}", rec.calls()
    );
}

/// A restore that arrives while its source volume is MID-HANDOVER: the owner has released its pin
/// (`spec.nodeName` empty) and the taker has not claimed yet. Nobody is owner, this node's replica
/// does not hold the push the restore is pinned to, and neither does anyone else's — every node
/// declines. What must not happen is what did: `await_change` as the answer, so the restore sat at
/// "creating" until the probe gave up, because the object that changes next is the Volume, not the
/// workspace. A decline of this kind comes back on a timer.
#[tokio::test]
async fn a_restore_declined_mid_handover_is_retried_rather_than_forgotten() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                "/apis/kloudlite.io/v1alpha1/volumes/ws-src",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "ws-src", "uid": "src-uid"},
                                   "spec": {"owner": "alice", "nodeName": "", "region": "r1", "quotaGb": 20}}),
            ),
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200,
                    body: snapshot_list_of("Snapshot", vec![snapshot_cr("ws-src-a", "ws-src")]) },
            kloudlite_workspaces::kube_test::not_found(format!(
                "/apis/kloudlite.io/v1alpha1/volumereplicas/{}",
                crd::replica_name("ws-src", "node-a")
            )),
        ],
    );
    let mut w = workspace(serde_json::json!({}));
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::SeededFrom { volume: "ws-src".into(), snapshot: "ws-src-a".into() }),
    });

    let action = kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "nobody may claim it yet: {:?}", rec.calls());
    assert_ne!(action, kube::runtime::controller::Action::await_change(), "a mid-handover decline must come back on its own");
}

/// The same clone, on a node whose replica HOLDS the source worktree's newest transient: claimed,
/// with no "same node as the source" rule anywhere in it.
#[tokio::test]
async fn a_clone_is_claimed_by_a_node_up_to_date_for_the_source_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let mut synced = volume_replica("ws-src", "node-a", "Synced");
    synced["status"]["branches"] = serde_json::json!({"ws-src": "sync-ws-src-9"});
    let mut transient = snapshot_cr("sync-ws-src-9", "ws-src");
    transient["spec"]["worktree"] = "ws-src".into();
    transient["spec"]["transient"] = true.into();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                "/apis/kloudlite.io/v1alpha1/volumes/ws-src",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "ws-src", "uid": "src-uid"},
                                   "spec": {"owner": "alice", "nodeName": "node-b", "region": "r1", "quotaGb": 20}}),
            ),
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200,
                    body: snapshot_list_of("Snapshot", vec![transient]) },
            kloudlite_workspaces::kube_test::get(
                format!("/apis/kloudlite.io/v1alpha1/volumereplicas/{}", crd::replica_name("ws-src", "node-a")),
                synced,
            ),
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    let mut w = workspace(serde_json::json!({}));
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into(), commit: None }),
    });

    kloudlite_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "up to date for the source: claimed: {:?}", rec.calls());
}

/// A node an operator is draining takes no new work, and the claim is where that has to bite: a
/// drain that keeps being handed fresh workspaces never finishes.
#[tokio::test]
async fn a_decommissioning_node_claims_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![kloudlite_workspaces::kube_test::get(
            "/api/v1/nodes/node-a",
            serde_json::json!({"apiVersion": "v1", "kind": "Node",
                               "metadata": {"name": "node-a", "labels": {crd::DECOMMISSION_LABEL: "true"}},
                               "status": {"conditions": [{"type": "Ready", "status": "True",
                                                          "lastTransitionTime": rfc3339_ago(60)}]}}),
        )],
    );

    kloudlite_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "a draining node must not claim: {:?}", rec.calls());
}
