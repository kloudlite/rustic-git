//! The bench reconciler, its claim and its dead-node release, against the mocked API server.

use super::*;
use kloudlite_workspaces::kube_test::{get, not_found};

const BENCH: &str = "bench-1";
const BENCH_STATUS: &str = "/apis/kloudlite.io/v1alpha1/benches/bench-1/status";
const FINISHED_AT: &str = "2026-09-13T10:00:00Z";

fn ns() -> String {
    crd::ws_namespace("alice", "acme")
}
fn pods_path() -> String {
    format!("/api/v1/namespaces/{}/pods", ns())
}
fn pod_path() -> String {
    format!("{}/bench", pods_path())
}

fn bench_json(spec: serde_json::Value, status: serde_json::Value) -> serde_json::Value {
    let mut s = serde_json::json!({"owner": "alice", "team": "acme", "image": "bench:1", "desiredState": "running"});
    for (k, v) in spec.as_object().unwrap() {
        s[k] = v.clone();
    }
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Bench",
        "metadata": {"name": BENCH, "uid": "bench-uid", "generation": 1, "resourceVersion": "7",
                     "labels": {"kloudlite.io/owner": "alice", "kloudlite.io/kind": "bench", "kloudlite.io/team": "acme"}},
        "spec": s, "status": status,
    })
}

fn bench(spec: serde_json::Value, status: serde_json::Value) -> crd::Bench {
    serde_json::from_value(bench_json(spec, status)).unwrap()
}

fn placed() -> serde_json::Value {
    serde_json::json!({"phase": "starting", "nodeName": "node-a"})
}

/// Binding, namespace, policy apply, user-key and the status write: everything before the pod.
fn up_to_the_pod(pod: Route) -> Vec<Route> {
    vec![
        ready_binding(),
        get(format!("/api/v1/namespaces/{}", ns()), serde_json::json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns()}})),
        kloudlite_workspaces::kube_test::patch(
            format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/bench-{BENCH}", ns()),
            serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": format!("bench-{BENCH}")}}),
        ),
        get(format!("/api/v1/namespaces/{}/secrets/user-key", ns()), serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "user-key"}})),
        kloudlite_workspaces::kube_test::patch(BENCH_STATUS, bench_json(serde_json::json!({}), placed())),
        kloudlite_workspaces::kube_test::post(pods_path(), serde_json::json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "bench"}})),
        Route { method: "DELETE", path: pod_path(), status: 200, body: serde_json::json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "bench"}}) },
        pod,
    ]
}

fn pod_json(command: &[&str], phase: &str, ready: bool, terminated: Option<i32>) -> serde_json::Value {
    let mut cs = serde_json::json!({"name": "bench", "ready": ready, "restartCount": 0, "image": "bench:1", "imageID": ""});
    if let Some(code) = terminated {
        cs["state"] = serde_json::json!({"terminated": {"exitCode": code, "finishedAt": FINISHED_AT}});
    }
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "bench", "namespace": ns()},
        "spec": {"containers": [{"name": "bench", "command": command}]},
        "status": {"phase": phase, "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}],
                   "containerStatuses": [cs]},
    })
}

fn homes_pool() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("homes")).unwrap();
    tmp
}

fn last_status(rec: &Recorder) -> serde_json::Value {
    rec.sent("PATCH", BENCH_STATUS).last().expect("a status write")["status"].clone()
}

fn has_cond(st: &serde_json::Value, t: &str, status: &str, reason: &str) -> bool {
    st["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == t && c["status"] == status && c["reason"] == reason))
}

#[tokio::test]
async fn a_bench_on_a_node_without_the_share_parks_and_starts_no_pod() {
    let tmp = homes_pool();
    let (ctx, rec) = ctx_without_homes_export(tmp.path(), up_to_the_pod(not_found(pod_path())));
    kloudlite_agent::controller::reconcile_bench(Arc::new(bench(serde_json::json!({}), placed())), ctx).await.unwrap();
    assert!(has_cond(&last_status(&rec), "Ready", "False", "FolderNotReady"), "{}", last_status(&rec));
    assert!(rec.sent("POST", &pods_path()).is_empty() && !rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")));
}

#[tokio::test]
async fn a_running_bench_makes_its_folder_and_one_pod() {
    let tmp = homes_pool();
    let (ctx, rec) = ctx_with_homes_export(tmp.path(), up_to_the_pod(not_found(pod_path())), Arc::new(FakeNix::default()), Some("unused".into()));
    kloudlite_agent::controller::reconcile_bench(Arc::new(bench(serde_json::json!({}), placed())), ctx).await.unwrap();
    assert!(tmp.path().join("homes/.benches/acme/alice").is_dir());
    let sent = rec.sent("POST", &pods_path());
    assert_eq!(sent.len(), 1, "{:?}", rec.calls());
    assert_eq!(sent[0]["spec"]["containers"][0]["command"], serde_json::json!(["harness-bench"]));
    let vols = sent[0]["spec"]["volumes"].as_array().unwrap();
    assert!(vols.iter().any(|v| v["hostPath"]["path"].as_str().is_some_and(|p| p.ends_with("/.benches/acme/alice"))), "{vols:?}");
}

#[tokio::test]
async fn an_idle_exit_removes_the_pod_and_only_a_later_wake_brings_it_back() {
    let tmp = homes_pool();
    let exited = get(pod_path(), pod_json(&["harness-bench"], "Succeeded", false, Some(0)));
    let (ctx, rec) = ctx_with_homes_export(tmp.path(), up_to_the_pod(exited), Arc::new(FakeNix::default()), Some("unused".into()));
    kloudlite_agent::controller::reconcile_bench(Arc::new(bench(serde_json::json!({}), placed())), ctx).await.unwrap();
    assert_eq!(rec.calls().iter().filter(|c| **c == format!("DELETE {}", pod_path())).count(), 1);
    let st = last_status(&rec);
    assert_eq!(st["phase"], "idle");
    assert!(has_cond(&st, "Ready", "False", "Idle"), "{st}");
    assert_eq!(st["idleSince"], FINISHED_AT);
    assert!(st.get("podRef").is_none(), "{st}");

    // Second pass: asleep, nobody asked.
    let (ctx, rec) = ctx_with_homes_export(tmp.path(), up_to_the_pod(not_found(pod_path())), Arc::new(FakeNix::default()), Some("unused".into()));
    kloudlite_agent::controller::reconcile_bench(Arc::new(bench(serde_json::json!({}), st.clone())), ctx).await.unwrap();
    assert!(!rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods") || c.starts_with("DELETE")), "{:?}", rec.calls());

    // Third pass: a wake one second after the exit.
    let (ctx, rec) = ctx_with_homes_export(tmp.path(), up_to_the_pod(not_found(pod_path())), Arc::new(FakeNix::default()), Some("unused".into()));
    let want = ctx.settings.load().bench_idle_secs.to_string();
    kloudlite_agent::controller::reconcile_bench(Arc::new(bench(serde_json::json!({"wakeAt": "2026-09-13T10:00:01Z"}), st)), ctx).await.unwrap();
    let sent = rec.sent("POST", &pods_path());
    assert_eq!(sent.len(), 1, "{:?}", rec.calls());
    let env = sent[0]["spec"]["containers"][0]["env"].as_array().unwrap();
    assert!(env.iter().any(|e| e["name"] == "KL_BENCH_IDLE_SECS" && e["value"] == want.as_str()), "{env:?}");
    assert!(last_status(&rec).get("idleSince").is_none(), "the write that records podRef clears idleSince");
}

#[tokio::test]
async fn stopping_a_bench_leaves_no_pod_at_all() {
    let tmp = homes_pool();
    let running = get(pod_path(), pod_json(&["harness-bench"], "Running", true, None));
    let (ctx, rec) = ctx_with_homes_export(tmp.path(), up_to_the_pod(running), Arc::new(FakeNix::default()), Some("unused".into()));
    let stopped = bench(serde_json::json!({"desiredState": "stopped"}), placed());
    kloudlite_agent::controller::reconcile_bench(Arc::new(stopped.clone()), ctx).await.unwrap();
    assert_eq!(rec.calls().iter().filter(|c| **c == format!("DELETE {}", pod_path())).count(), 1);
    assert!(rec.sent("POST", &pods_path()).is_empty());

    let (ctx, rec) = ctx_with_homes_export(tmp.path(), up_to_the_pod(not_found(pod_path())), Arc::new(FakeNix::default()), Some("unused".into()));
    kloudlite_agent::controller::reconcile_bench(Arc::new(stopped), ctx).await.unwrap();
    assert!(rec.sent("POST", &pods_path()).is_empty());
    let st = last_status(&rec);
    assert_eq!(st["phase"], "stopped");
    assert!(has_cond(&st, "Ready", "False", "Stopped"), "{st}");
}

#[tokio::test]
async fn an_unplaced_bench_is_claimed_without_touching_a_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![Route { method: "PUT", path: BENCH_STATUS.into(), status: 200, body: bench_json(serde_json::json!({}), placed()) }, binding_route()],
    );
    kloudlite_agent::claim::claim_bench(&bench(serde_json::json!({}), serde_json::Value::Null), &ctx).await.unwrap();
    let sent = rec.sent("PUT", BENCH_STATUS);
    assert_eq!(sent.len(), 1, "{:?}", rec.calls());
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
    assert!(has_cond(&sent[0]["status"], "Placed", "True", "Claimed"));
    assert!(!rec.calls().iter().any(|c| c.contains("/apis/kloudlite.io/v1alpha1/volumes")), "{:?}", rec.calls());
}

#[tokio::test]
async fn a_bench_on_a_dead_node_is_released_for_another_node() {
    let tmp = tempfile::tempdir().unwrap();
    let dead = bench_json(serde_json::json!({}), serde_json::json!({"phase": "ready", "nodeName": "n-dead"}));
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            get("/apis/kloudlite.io/v1alpha1/benches", serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "BenchList", "metadata": {}, "items": [dead.clone()]})),
            get("/apis/kloudlite.io/v1alpha1/benches/bench-1", dead.clone()),
            Route { method: "PUT", path: BENCH_STATUS.into(), status: 200, body: dead },
        ],
    );
    let node: k8s_openapi::api::core::v1::Node = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Node", "metadata": {"name": "n-dead"},
        "status": {"conditions": [{"type": "Ready", "status": "False", "lastTransitionTime": rfc3339_ago(3600)}]}
    }))
    .unwrap();
    kloudlite_agent::peer::sweeps::release_benches(&ctx, &[node], 180, k8s_openapi::jiff::Timestamp::now()).await;
    let sent = rec.sent("PUT", BENCH_STATUS);
    assert_eq!(sent.len(), 1, "{:?}", rec.calls());
    assert_eq!(sent[0]["status"]["nodeName"], "");
    assert!(has_cond(&sent[0]["status"], "Placed", "False", "NodeDead"), "{}", sent[0]);
}
