//! Batch 2 (2026-09-12): the agent decides from its two cluster-wide reflector caches, not from a
//! GET per environment pass and a pair of cluster-wide LISTs per volume decision. What these hold
//! is the part that cannot be tested any other way: the store answers for an object this node does
//! NOT host, and a store that has not listed yet is never read as "gone".

use super::*;
use kloudlite_agent::controller::{decide_intercept, Intercepting};

const WS_GET: &str = "GET /apis/kloudlite.io/v1alpha1/workspaces/ws-1";

/// A workspace claimed by node-b, with a ready pod — what an intercept is normally serving.
fn intercepting_ws(node: &str) -> crd::Workspace {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-1", "uid": "ws-1-uid"},
        "spec": {"owner": "alice", "team": "", "name": "ws-1", "region": "r1", "image": "",
                 "packages": [], "desiredState": "running", "attachedEnvironment": "env-1"},
        "status": {"phase": "ready", "nodeName": node, "volumeRef": "vol-1", "podRef": "ws-alice/ws-1"},
    }))
    .unwrap()
}

fn intercept() -> crd::Intercept {
    serde_json::from_value(serde_json::json!({"service": "api", "workspace": "ws-1", "ports": []})).unwrap()
}

fn ready_pod() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws-1", "namespace": "ws-alice"},
        "status": {"podIP": "10.1.2.3", "conditions": [{"type": "Ready", "status": "True"}]},
    })
}

/// The whole point of the unfiltered store: the intercepting workspace is on ANOTHER node, so the
/// controller's own node-scoped store cannot answer — and the answer still costs no Workspace GET.
/// The pod stays a GET on purpose (per-namespace, short-lived).
#[tokio::test]
async fn an_intercept_is_forced_from_the_store_with_no_workspace_get() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1", ready_pod())]);
    ctx.remember_parents(vec![intercepting_ws("node-b")], vec![]);

    let d = decide_intercept(&intercept(), "env-1", &Default::default(), &ctx).await;

    assert!(matches!(d, Intercepting::Force { ref pod_ip, .. } if pod_ip == "10.1.2.3"), "forced from the store");
    assert!(!rec.calls().iter().any(|c| c == WS_GET), "the workspace came from the store: {:?}", rec.calls());
}

/// An empty cache is "not known yet", never "gone". Read as gone this would scale the real service
/// back up and drop an intercept that is perfectly healthy — on every agent restart.
#[tokio::test]
async fn a_store_that_has_not_listed_yet_releases_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    // Deliberately NOT seeded: the reflector has not finished its first list.

    let d = decide_intercept(&intercept(), "env-1", &Default::default(), &ctx).await;

    assert!(matches!(d, Intercepting::Keep { since: None }), "an unlisted store decides nothing");
    assert!(rec.calls().is_empty(), "and asks the API server for nothing: {:?}", rec.calls());
}

/// A workspace the store DOES hold and that is really gone is still `WorkspaceGone` — the guard
/// above must not have turned the release path off altogether.
#[tokio::test]
async fn a_workspace_missing_from_a_listed_store_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![]);
    ctx.remember_parents(vec![], vec![]);

    let d = decide_intercept(&intercept(), "env-1", &Default::default(), &ctx).await;

    assert!(matches!(d, Intercepting::Off { reason: "WorkspaceGone", .. }));
}

/// The per-volume sibling set the placement decision reads: cluster-wide, and from the store —
/// this parent is on node-b and no LIST is made for it.
#[tokio::test]
async fn parents_on_volume_names_a_parent_only_the_store_knows() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    ctx.remember_parents(vec![intercepting_ws("node-b")], vec![]);

    let siblings = kloudlite_agent::listing::parents_on_volume(&ctx, "vol-1").await.expect("a full view");

    assert_eq!(siblings.len(), 1);
    assert_eq!(siblings[0].name, "ws-1");
    assert_eq!(siblings[0].node_name, "node-b");
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("GET /apis/kloudlite.io/v1alpha1/workspaces")),
        "no LIST: {:?}",
        rec.calls()
    );
}
