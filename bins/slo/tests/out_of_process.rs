//! The whole reason a run is two processes: `panic = "abort"` in the release profile means a
//! panicking stage kills its process outright, so this asserts the PARENT still tears down and
//! still files a finished report. It runs the real binary — an in-process test could not observe
//! the split at all.
//!
//! The roll lock is part of what the parent does and it is NOT optional, so this test cannot opt
//! out of it the way an in-process fixture does: the run is pointed at a local fake API server
//! through a temporary KUBECONFIG, and the lock must appear there and be handed back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::Json;
use axum::http::StatusCode;

type Reports = Arc<Mutex<Vec<serde_json::Value>>>;

/// Stands in for the admin process: records every report body, answers everything else 200 with
/// an empty array so teardown's list calls find nothing to delete.
async fn stub() -> (String, Reports, Arc<AtomicUsize>) {
    let reports: Reports = Arc::new(Mutex::new(vec![]));
    let lists = Arc::new(AtomicUsize::new(0));
    let (r, l) = (reports.clone(), lists.clone());
    let app = axum::Router::new()
        .route(
            "/admin/slo/runs/{id}",
            axum::routing::put(move |Json(body): Json<serde_json::Value>| {
                let r = r.clone();
                async move {
                    r.lock().expect("lock").push(body);
                    StatusCode::NO_CONTENT
                }
            }),
        )
        // `tel.log.latency` polls this for its full minute otherwise, and this test is about the
        // process split, not about a marker never landing.
        .route(
            "/admin/slo/marker/{id}",
            axum::routing::get(|| async { Json(serde_json::json!({ "found": true, "ts": "" })) }),
        )
        .fallback(axum::routing::any(move || {
            let l = l.clone();
            async move {
                l.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!([]))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), reports, lists)
}

/// What the fake API server saw of the run's roll lock. The counters are the assertion: a run that
/// never took the lock, or never handed it back, leaves one of them at zero.
#[derive(Default)]
struct KubeCalls {
    created: AtomicUsize,
    released: AtomicUsize,
    nodes_listed: AtomicUsize,
    /// The created ConfigMap's `data.holder`, and the UID the delete carried — so the test can
    /// name the run that held the lock and prove the release targeted the object it made.
    holder: Mutex<Option<String>>,
    released_uid: Mutex<Option<String>>,
}

/// Stands in for the cluster the roll lock lives on.
///
/// The shipped binary cannot be talked out of coordination, so the test points it at a local
/// endpoint instead. Three routes are everything the fast suite's parent asks for: create the lock
/// ConfigMap, delete it, and list nodes for teardown's drill sweep. Anything else fails to answer
/// as a real object (`[]` is nobody's `NodeList`), so a stage that tried to reach a live cluster
/// has nowhere to go.
async fn kube_stub() -> (String, Arc<KubeCalls>) {
    let calls = Arc::new(KubeCalls::default());
    let (created, released, nodes) = (calls.clone(), calls.clone(), calls.clone());
    let app = axum::Router::new()
        .route(
            "/api/v1/namespaces/kloudlite/configmaps",
            axum::routing::post(move |Json(body): Json<serde_json::Value>| {
                let created = created.clone();
                async move {
                    created.created.fetch_add(1, Ordering::SeqCst);
                    *created.holder.lock().expect("lock") =
                        body.pointer("/data/holder").and_then(|v| v.as_str()).map(str::to_owned);
                    (
                        StatusCode::CREATED,
                        // A real API server answers the create with the stored object, and the lock
                        // refuses to exist without a UID and a resourceVersion.
                        Json(serde_json::json!({
                            "apiVersion": "v1",
                            "kind": "ConfigMap",
                            "metadata": {
                                "name": "kloudlite-roll-coordination",
                                "namespace": "kloudlite",
                                "uid": "lock-uid",
                                "resourceVersion": "1",
                            },
                            "data": body.get("data").cloned().unwrap_or_else(|| serde_json::json!({})),
                        })),
                    )
                }
            }),
        )
        .route(
            "/api/v1/namespaces/kloudlite/configmaps/{name}",
            axum::routing::delete(move |Json(body): Json<serde_json::Value>| {
                let released = released.clone();
                async move {
                    released.released.fetch_add(1, Ordering::SeqCst);
                    *released.released_uid.lock().expect("lock") =
                        body.pointer("/preconditions/uid").and_then(|v| v.as_str()).map(str::to_owned);
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({ "apiVersion": "v1", "kind": "Status", "status": "Success", "code": 200 })),
                    )
                }
            }),
        )
        .route(
            "/api/v1/nodes",
            axum::routing::get(move || {
                let nodes = nodes.clone();
                async move {
                    nodes.nodes_listed.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({
                        "apiVersion": "v1",
                        "kind": "NodeList",
                        "metadata": { "resourceVersion": "1" },
                        "items": [],
                    }))
                }
            }),
        )
        .fallback(axum::routing::any(|| async { Json(serde_json::json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), calls)
}

/// A temporary, explicit KUBECONFIG for the fake endpoint and nothing else. `env_clear` on the
/// child also drops `KUBERNETES_SERVICE_HOST`, so `kube::Config::infer` finds this file and cannot
/// fall back to a ServiceAccount: it is the only cluster the real binary can reach.
fn write_kubeconfig(server: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "slo-out-of-process-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("kubeconfig dir");
    let path = dir.join("kubeconfig");
    let doc = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Config",
        "clusters": [{ "name": "fake", "cluster": { "server": server } }],
        "contexts": [{ "name": "fake", "context": { "cluster": "fake", "user": "fake" } }],
        "current-context": "fake",
        "users": [{ "name": "fake", "user": {} }],
    });
    std::fs::write(&path, serde_json::to_vec(&doc).expect("kubeconfig json")).expect("write kubeconfig");
    path
}

/// The environment both children above share: the fake admin endpoint, a KUBECONFIG for the fake
/// cluster, and nothing else — `env_clear` drops whatever in-cluster variables the test process
/// itself inherited. A test adds only the one variable it is about.
fn child_command(url: &str, kubeconfig: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kloudlite-slo"));
    command
        .args(["run", "--suite", "fast"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        // Deliberately the value the REMOVED runtime opt-out used: the knob no longer exists, so
        // setting it must not keep the lock from being taken.
        .env("KLOUDLITE_SLO_COORDINATION", "0")
        .env("KUBECONFIG", kubeconfig)
        .env("KLOUDLITE_ADMIN_API_URL", url)
        .env("KLOUDLITE_API_URL", url)
        .env("KLOUDLITE_WEB_URL", url)
        .env("KLOUDLITE_URL", url)
        .env("KLOUDLITE_REGISTRY", "127.0.0.1:1")
        .env("KLOUDLITE_SSH_HOST", "127.0.0.1")
        .env("KLOUDLITE_REGION", "test")
        .env("KLOUDLITE_JWT_SECRET", "0123456789abcdef0123456789abcdef");
    command
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_stage_still_yields_a_finished_report_and_a_teardown() {
    let (url, reports, lists) = stub().await;
    let (kube_url, kube) = kube_stub().await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || {
            child_command(&url, &kubeconfig)
                .env("KLOUDLITE_SLO_TEST_PANIC", "1")
                .output()
                .expect("spawn")
        }
    })
    .await
    .expect("join");
    let _ = std::fs::remove_dir_all(kubeconfig.parent().expect("kubeconfig dir"));

    let logs = String::from_utf8_lossy(&out.stderr).to_string();
    // The child aborted, so the run is a failure — but it is a REPORTED failure.
    assert_eq!(out.status.code(), Some(1), "exit; logs:\n{logs}");
    assert!(logs.contains("slo.teardown.completed"), "teardown did not run; logs:\n{logs}");
    assert!(logs.contains("slo.run.finished"), "no final log; logs:\n{logs}");

    let reports = reports.lock().expect("lock");
    let last = reports.last().expect("at least one report");
    assert!(!last["finished"].is_null(), "final report has no finished: {last}");
    assert_eq!(last["state"], "failed");
    assert_eq!(last["stage"], "11 · Teardown");
    // Every report in the run is one row: the child must not open a run id of its own.
    assert!(reports.iter().all(|r| r["run_id"] == last["run_id"]), "two run ids");
    // Teardown really swept — six `/v1` collections plus the request queue.
    assert!(lists.load(Ordering::SeqCst) >= 7, "teardown listed {} times", lists.load(Ordering::SeqCst));

    // The lock is fail-closed, so the parent must have created it on the fake cluster and handed
    // it back: a run that skipped coordination, or leaked the lock, leaves one of these at zero.
    assert_eq!(kube.created.load(Ordering::SeqCst), 1, "the run never took the roll lock; logs:\n{logs}");
    assert_eq!(kube.released.load(Ordering::SeqCst), 1, "the run never released the roll lock; logs:\n{logs}");
    let holder = kube.holder.lock().expect("lock").clone().expect("the created lock recorded no holder");
    assert!(holder.starts_with("manual/fast-"), "the lock was held by {holder}");
    let released_uid = kube.released_uid.lock().expect("lock").clone();
    assert_eq!(released_uid.as_deref(), Some("lock-uid"), "the release did not carry the created lock's uid");
    // Teardown's drill sweep listed nodes on that same client — the typed, empty NodeList the
    // fixture answers with is what lets the sweep finish without reaching a real cluster.
    assert!(kube.nodes_listed.load(Ordering::SeqCst) >= 1, "teardown never swept nodes; logs:\n{logs}");
}

/// The same fixture, with the pod's environment naming a cluster the client cannot build from.
/// An in-cluster environment is the only cluster a pod has, so a malformed one must fail the run
/// outright rather than reach the valid KUBECONFIG — which points at the fake endpoint and would
/// have taken the lock there.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_incluster_env_cannot_fall_back_to_the_kubeconfig() {
    let (url, _reports, _lists) = stub().await;
    let (kube_url, kube) = kube_stub().await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || {
            child_command(&url, &kubeconfig)
                // Either variable is enough to mean "in a pod", and `Config::incluster` cannot
                // parse this port, so the client has nowhere to go but up to the caller.
                .env("KUBERNETES_SERVICE_HOST", "127.0.0.1")
                .env("KUBERNETES_SERVICE_PORT", "notaport")
                .output()
                .expect("spawn")
        }
    })
    .await
    .expect("join");
    let _ = std::fs::remove_dir_all(kubeconfig.parent().expect("kubeconfig dir"));

    let logs = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "exit; logs:\n{logs}");
    assert!(logs.contains("failed to parse cluster port"), "the run failed for something else; logs:\n{logs}");
    // Fail-closed: nothing in the run reached the fake cluster, and its lock was never touched.
    assert_eq!(kube.created.load(Ordering::SeqCst), 0, "the run fell back to KUBECONFIG; logs:\n{logs}");
    assert_eq!(kube.released.load(Ordering::SeqCst), 0, "logs:\n{logs}");
    assert_eq!(kube.nodes_listed.load(Ordering::SeqCst), 0, "logs:\n{logs}");
}
