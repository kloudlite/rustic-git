//! in-place restore.

use super::*;


pub(crate) const DEP_PATCH: &str = "/apis/apps/v1/namespaces/env-1/statefulsets/db";
pub(crate) const POD_LIST: &str = "/api/v1/namespaces/env-1/pods";
pub(crate) const VOL_PATCH: &str = "/apis/kloudlite.io/v1alpha1/volumes/env-1";

pub(crate) const WISH_AT: &str = "2026-08-27T00:00:00Z";

pub(crate) fn restoring_env(restored_to: Option<&str>) -> (crd::Environment, serde_json::Value) {
    let mut o = env_json(serde_json::json!({"phase": "running", "nodeName": "node-a"}));
    o["spec"]["services"] =
        serde_json::json!([{"name": "db", "image": "mongo", "command": [], "env": {}, "mounts": []}]);
    o["spec"]["restore"] = serde_json::json!({"snapshotId": "snap-7", "volume": "env-1",
                                              "owner": "acme", "requestedAt": WISH_AT});
    let mut vol = env_vol();
    if let Some(id) = restored_to {
        vol["status"]["restoredTo"] = serde_json::json!(id);
        vol["status"]["restoreRequestedAt"] = serde_json::json!(WISH_AT);
    }
    (serde_json::from_value(o).unwrap(), vol)
}

/// `(name, phase)` — the phase is what decides whether a pod can still be WRITING.
pub(crate) fn pod_list(pods: &[(&str, &str)]) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "PodList", "metadata": {"resourceVersion": "1"},
        "items": pods.iter().map(|(n, phase)| serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": n, "namespace": "env-1"},
            "status": {"phase": phase},
        })).collect::<Vec<_>>(),
    })
}

/// Never restore under a running service: the Deployments go to zero replicas and their pods have
/// to be GONE before the wish reaches the Volume. A subvolume swapped under an open database is
/// corruption nobody can attribute afterwards.
#[tokio::test]
async fn a_restore_wish_scales_the_services_to_zero_before_it_reaches_the_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(None);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", vol.clone()),
            Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
            kloudlite_workspaces::kube_test::get(POD_LIST, pod_list(&[])),
            Route { method: "PATCH", path: VOL_PATCH.into(), status: 200, body: vol },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    let action = kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    assert_eq!(rec.sent("PATCH", DEP_PATCH)[0]["spec"]["replicas"], 0);
    let calls = rec.calls();
    let scaled = calls.iter().position(|c| c == &format!("PATCH {DEP_PATCH}")).unwrap();
    let wished = calls.iter().position(|c| c == &format!("PATCH {VOL_PATCH}")).unwrap();
    assert!(scaled < wished, "the scale-down comes first: {calls:?}");
    assert_eq!(rec.sent("PATCH", VOL_PATCH)[0]["spec"]["restoreTo"]["snapshotId"], "snap-7");
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["type"], "Restoring");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["status"], "True");
}

/// A pod still terminating is a process still writing. The wish waits.
#[tokio::test]
async fn a_restore_waits_for_the_pods_to_actually_be_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(None);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
            kloudlite_workspaces::kube_test::get(POD_LIST, pod_list(&[("db-0", "Running")])),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert!(!rec.calls().iter().any(|c| c == &format!("PATCH {VOL_PATCH}")), "{:?}", rec.calls());
    assert_eq!(rec.sent("PATCH", ENV_STATUS_PATH).last().unwrap()["status"]["conditions"][0]["reason"], "Draining");
}

/// The Volume reports the wished-for snapshot live: the gate is done, so the pass falls through to
/// the ordinary converge — which re-applies every Deployment, and THAT is the scale back up. It
/// must write no second wish and scale nothing down; a gate that fired again here would be an
/// infinite restore loop, since `spec.restore` is deliberately never cleared.
#[tokio::test]
async fn a_matching_restored_to_neither_scales_down_nor_re_wishes() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(Some("snap-7"));
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    // The converge past the gate needs a namespace this mock does not answer for, so the pass
    // errors there. What is under test is everything BEFORE that point.
    let _ = kloudlite_agent::controller::apply_environment(&e, &ctx).await;
    let calls = rec.calls();
    assert!(!calls.iter().any(|c| c == &format!("PATCH {DEP_PATCH}")), "no scale-down: {calls:?}");
    assert!(!calls.iter().any(|c| c == &format!("PATCH {VOL_PATCH}")), "no second wish: {calls:?}");
    assert!(!calls.iter().any(|c| c == &format!("GET {POD_LIST}")), "the gate never ran: {calls:?}");
}

/// A granted wish stays in `spec.restore` forever, so the gate meets it on every pass. It may
/// INITIALIZE `head` — once — and must never re-derive it afterwards: a push advances `head` to a
/// new snapshot, and a gate that compared `head` against the wish would stamp it straight back, so
/// an environment that was ever restored could never move past its restore point. What shipped
/// did exactly that.
#[tokio::test]
async fn a_granted_wish_never_drags_head_back_off_a_pushed_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut e, vol) = restoring_env(Some("snap-7"));
    // The state after a push: the wish was applied and recorded long ago, and `head` has since
    // moved on to a snapshot the snapshot reconciler cut.
    let mut st = e.status.clone().unwrap_or_default();
    st.head = Some("env-1-aaaaaaaa".into());
    st.restored_to = Some("snap-7".into());
    st.restore_requested_at = Some(WISH_AT.into());
    e.status = Some(st);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    // As in the sibling test above, the converge past the gate needs a namespace this mock does
    // not answer for; what is under test is every status write before that point.
    let _ = kloudlite_agent::controller::apply_environment(&e, &ctx).await;
    for w in rec.sent("PATCH", ENV_STATUS_PATH) {
        let head = w["status"]["head"].as_str();
        assert_ne!(head, Some("snap-7"), "the granted wish must not drag `head` back: {w}");
    }
}

/// The other half: a wish this environment has NOT recorded is applied — `head` is initialized to
/// the restore point and the wish is recorded, so the pass above can tell "applied" from "fresh".
#[tokio::test]
async fn a_freshly_granted_wish_initializes_head_and_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(Some("snap-7"));
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    let _ = kloudlite_agent::controller::apply_environment(&e, &ctx).await;
    let sent = rec.sent("PATCH", ENV_STATUS_PATH);
    let first = sent.first().expect("the grant writes status");
    assert_eq!(first["status"]["head"], "snap-7");
    assert_eq!(first["status"]["restoredTo"], "snap-7");
    assert_eq!(first["status"]["restoreRequestedAt"], WISH_AT);
}

/// A finished pod is not a writer. `Succeeded`/`Failed` pods are never collected on their own, so
/// counting every pod in the namespace waits for something that will not happen — the restore hangs
/// behind a job that ended days ago.
#[tokio::test]
async fn a_finished_pod_does_not_block_the_drain() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(None);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", vol.clone()),
            Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
            kloudlite_workspaces::kube_test::get(POD_LIST, pod_list(&[("seed-1", "Succeeded"), ("old-1", "Failed")])),
            Route { method: "PATCH", path: VOL_PATCH.into(), status: 200, body: vol },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(rec.sent("PATCH", VOL_PATCH).len(), 1, "the drain is done: {:?}", rec.calls());
}

/// Restoring the SAME snapshot again is a legitimate ask — after undoing a restore by hand, or
/// after a bad afternoon. Comparing snapshot ids alone made the second ask a silent no-op, so the
/// guard compares the (snapshotId, requestedAt) PAIR on both sides.
#[tokio::test]
async fn a_second_wish_for_the_same_snapshot_restores_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut e, vol) = restoring_env(Some("snap-7"));
    let mut spec = e.spec.restore.clone().unwrap();
    spec.requested_at = "2026-08-28T09:00:00Z".into();
    e.spec.restore = Some(spec);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", vol.clone()),
            Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
            kloudlite_workspaces::kube_test::get(POD_LIST, pod_list(&[])),
            Route { method: "PATCH", path: VOL_PATCH.into(), status: 200, body: vol },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    kloudlite_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    let sent = rec.sent("PATCH", VOL_PATCH);
    assert_eq!(sent.len(), 1, "the newer wish is a new restore: {:?}", rec.calls());
    assert_eq!(sent[0]["spec"]["restoreTo"]["requestedAt"], "2026-08-28T09:00:00Z");
}

/// The scale back up is `service_statefulset`'s own replica count — the gate does not restore it by
/// hand, the ordinary converge does.
#[test]
fn a_service_statefulset_is_one_replica() {
    let svc = kloudlite_workspaces::model::Service {
        name: "db".into(),
        image: "mongo".into(),
        command: vec![],
        env: Default::default(),
        mounts: vec![],
        ports: vec![],
        resources: None,
    };
    let dep = kloudlite_workspaces::k8s::service_statefulset(&svc, "env-1", "env-1", "acme", &test_pod_ctx()).unwrap();
    assert_eq!(dep.spec.unwrap().replicas, Some(1));
}
