//! what the node asks the API server for.

use super::*;


/// Every watch `run` opens is scoped to this node, and the ones that cannot be (a request names
/// no node) are label-selected down to the objects this node acts on. The mock answers every list
/// with nothing and every watch with a body the watcher cannot parse, so the controllers spin up,
/// ask, and back off — which is enough to see the selectors they ask WITH.
#[tokio::test(flavor = "multi_thread")]
async fn every_watch_is_scoped_to_this_node_or_label_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let list = |kind: &str| {
        serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"),
                           "metadata": {"resourceVersion": "1"}, "items": []})
    };
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes", list("Volume")),
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", list("Workspace")),
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/environments", list("Environment")),
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/ownerbindings", list("OwnerBinding")),
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/snapshots", list("Snapshot")),
            kloudlite_workspaces::kube_test::get("/api/v1/pods", list("Pod")),
            kloudlite_workspaces::kube_test::get("/apis/apps/v1/statefulsets", list("StatefulSet")),
        ],
    );
    let running = tokio::spawn(kloudlite_agent::controller::run(ctx));
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    running.abort();

    let reqs = rec.requests();
    let of = |path: &str| -> Vec<String> { reqs.iter().filter(|r| r.starts_with(&format!("GET {path}?"))).cloned().collect() };
    let volumes = of("/apis/kloudlite.io/v1alpha1/volumes");
    assert!(!volumes.is_empty(), "the Volume watch never opened: {reqs:?}");
    // The heartbeat's capped list is the one unscoped Volume request there is.
    for r in volumes.iter().filter(|r| !r.contains("limit=1&") && !r.ends_with("limit=1")) {
        assert!(r.contains("fieldSelector=spec.nodeName%3Dnode-a"), "an unscoped Volume request: {r}");
    }
    let parents = [of("/apis/kloudlite.io/v1alpha1/workspaces"), of("/apis/kloudlite.io/v1alpha1/environments")].concat();
    assert!(!parents.is_empty(), "no parent watch opened: {reqs:?}");
    for r in &parents {
        assert!(r.contains("fieldSelector=status.nodeName%3D"), "an unscoped parent request: {r}");
    }
    for r in of("/apis/apps/v1/statefulsets") {
        assert!(r.contains("labelSelector=kloudlite.io%2Fkind%3Denvironment"), "every StatefulSet in the cluster: {r}");
    }
    let snaps = of("/apis/kloudlite.io/v1alpha1/snapshots");
    assert!(
        snaps.iter().any(|r| r.contains("labelSelector=kloudlite.io%2Fstop-of")),
        "the env controller's stop-push watch is not label-selected: {snaps:?}"
    );
}


/// A converged workspace reconciles on every pod event, and a converged pass must not re-apply
/// its children — only the pod is read every time, to observe liveness.
#[tokio::test]
async fn a_converged_workspace_does_not_re_apply_its_children_on_the_next_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _fake) = ws_ctx_with_nix(tmp.path());
    let ws = ready_workspace("ws-1", vec![]);
    apply_until_settled(&ws, &ctx).await;
    // The fixture never feeds a pass's status back into `ws`, so every other pass restarts the
    // (instant) fake profile build and returns before the pod; the passes that do reach the pod
    // are the converged ones, and it is those that must apply nothing. How many passes that
    // takes depends on timing (a slow CI runner is still mid-build after settling), so keep
    // going until two of them have been seen.
    let pod_get = "GET /api/v1/namespaces/ws-alice/pods/ws-1";
    let mut converged: Vec<Vec<String>> = Vec::new();
    for _ in 0..10 {
        let before = rec.calls().len();
        let _ = kloudlite_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
        let pass: Vec<String> = rec.calls()[before..].to_vec();
        if pass.iter().any(|c| c == pod_get) {
            converged.push(pass);
            if converged.len() == 2 {
                break;
            }
        } else {
            wait_idle(&ctx).await;
        }
    }
    assert_eq!(converged.len(), 2, "never saw two converged passes: {:?}", rec.calls());
}

/// The shared home replaces the home Volume (spec 2026-09-01): a node with no `WS_HOMES_EXPORT`
/// has nowhere to mount an owner's home, so it must park the workspace rather than start a pod
/// that would hostPath an empty local dir in the home's place.
#[tokio::test]
async fn a_node_without_a_homes_export_parks_the_workspace_instead_of_starting_a_pod() {
    let tmp = tempfile::tempdir().unwrap();
    // resolve_volume and the namespace-ready check both run before the homes-export gate, so
    // this fixture still needs a Ready Volume and a Ready binding to reach it — same shapes as
    // `ws_ctx_with_ssh`'s, minus the SSH/pod routes the homes-export gate never lets it reach.
    let vol = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    let routes = vec![
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-1", vol),
        ready_binding(),
        ready_namespace(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx_without_homes_export(tmp.path(), routes);
    let w = ready_workspace("ws-1", vec![]);

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "HomeNotReady");
    assert!(rec.calls().iter().all(|c| !c.contains("/pods")), "no pod while unmounted: {:?}", rec.calls());
}

/// Who may ssh in is a cluster fact (`OwnerKeys`) the api writes and every node's agent renders.
/// The pod mounts that file as a `type: File` hostPath, so a pod started before the projection
/// reaches this node is an opaque kubelet mount failure; park until the file is there.
#[tokio::test]
async fn a_node_without_the_owners_keys_parks_the_workspace_instead_of_starting_a_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _) = ws_ctx_with_nix(tmp.path());
    // The default image is the only one that runs sshd, and so the only one that mounts the file.
    let mut w = ready_workspace("ws-1", vec![]);
    w.spec.image = kloudlite_workspaces::model::DEFAULT_WS_IMAGE.into();

    let action = apply_until_settled(&w, &ctx).await;
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    let st = rec.sent("PATCH", WS_STATUS);
    let last = st.last().unwrap()["status"]["conditions"].as_array().unwrap().clone();
    let ready = last.iter().find(|c| c["type"] == "Ready").expect("a Ready condition");
    assert_eq!(ready["reason"], "KeysNotReady", "{last:?}");
    assert!(rec.calls().iter().all(|c| !c.contains("/pods")), "no pod without keys: {:?}", rec.calls());
}

/// The mirror of the above: only the DEFAULT image mounts the keys file (it is the only one that
/// runs sshd), so a user's own image must not be held behind a projection it never reads.
#[tokio::test]
async fn a_custom_image_workspace_is_not_parked_on_keys_it_never_mounts() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _) = ws_ctx_with_nix(tmp.path());
    let w = ready_workspace("ws-1", vec![]);

    let _ = apply_until_settled(&w, &ctx).await;
    let reasons: Vec<_> = rec
        .sent("PATCH", WS_STATUS)
        .iter()
        .filter_map(|s| s["status"]["conditions"][0]["reason"].as_str().map(str::to_string))
        .collect();
    assert!(!reasons.iter().any(|r| r == "KeysNotReady"), "{reasons:?}");
}
