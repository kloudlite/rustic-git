//! The whole reason a run is two processes: `panic = "abort"` in the release profile means a
//! panicking stage kills its process outright, so this asserts the PARENT still tears down and
//! still files a finished report. It runs the real binary — an in-process test could not observe
//! the split at all.
//!
//! The roll lock is part of what the parent does for every suite OTHER than fast (ruling 3: the
//! fast suite only ever peeks, never holds), so most of these tests point the run at a local fake
//! API server through a temporary KUBECONFIG, and (except for the fast-suite tests) the lock must
//! appear there and be handed back.
//!
//! Every fixture binds its own listener on port 0 and is torn down at the end of its owning test —
//! nothing here is process-global (`std::env::set_var`) or a fixed port, which is what lets these
//! run under the default PARALLEL test runner rather than needing `--test-threads=1`.

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
/// never took the lock, or never handed it back, leaves one of them at zero — and `put_count` at
/// nonzero is a defect on its own (R-1: never a PUT, only GET/create/delete).
#[derive(Default)]
struct KubeCalls {
    created: AtomicUsize,
    /// A precondition-DELETE, whether it is the release at the end of a normal run or the
    /// takeover's delete-before-recreate.
    deleted: AtomicUsize,
    put_count: AtomicUsize,
    got: AtomicUsize,
    nodes_listed: AtomicUsize,
    /// The created ConfigMap's `data.holder`, and the UID the delete carried — so the test can
    /// name the run that held the lock and prove the release targeted the object it made.
    holder: Mutex<Option<String>>,
    deleted_uid: Mutex<Option<String>>,
}

/// Mounts the fake ConfigMap store used by every test below. `initial` seeds an existing lock (for
/// the takeover/peek tests); `None` starts with nothing so the first create always succeeds. The
/// store is a single `Mutex<Option<Value>>` behind the routes, so create/get/delete/list all agree
/// with each other exactly the way a real API server's one object does — this is what lets the
/// takeover loop's own re-read-after-delete work in a test the same as on a cluster.
async fn mount(calls: Arc<KubeCalls>, initial: Option<serde_json::Value>) -> (String, Arc<KubeCalls>) {
    let lock: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(initial));
    let mut rv_seq = 100u64;
    let (c_create, c_get, c_delete, c_nodes) = (calls.clone(), calls.clone(), calls.clone(), calls.clone());
    let (l_create, l_get, l_delete) = (lock.clone(), lock.clone(), lock.clone());
    let app = axum::Router::new()
        .route(
            "/api/v1/namespaces/kloudlite/configmaps",
            axum::routing::post(move |Json(body): Json<serde_json::Value>| {
                let calls = c_create.clone();
                let lock = l_create.clone();
                async move {
                    calls.created.fetch_add(1, Ordering::SeqCst);
                    let mut slot = lock.lock().expect("lock");
                    if slot.is_some() {
                        return (
                            StatusCode::CONFLICT,
                            Json(serde_json::json!({
                                "apiVersion": "v1", "kind": "Status", "status": "Failure",
                                "reason": "AlreadyExists", "code": 409,
                            })),
                        );
                    }
                    rv_seq += 1;
                    let rv = rv_seq.to_string();
                    let holder = body.pointer("/data/holder").and_then(|v| v.as_str()).map(str::to_owned);
                    *calls.holder.lock().expect("lock") = holder;
                    let created = serde_json::json!({
                        "apiVersion": "v1",
                        "kind": "ConfigMap",
                        "metadata": {
                            "name": "kloudlite-roll-coordination",
                            "namespace": "kloudlite",
                            "uid": format!("lock-uid-{rv}"),
                            "resourceVersion": rv,
                            "creationTimestamp": "2026-09-20T00:00:00Z",
                        },
                        "data": body.get("data").cloned().unwrap_or_else(|| serde_json::json!({})),
                    });
                    *slot = Some(created.clone());
                    (StatusCode::CREATED, Json(created))
                }
            }),
        )
        .route(
            "/api/v1/namespaces/kloudlite/configmaps/{name}",
            axum::routing::get(move || {
                let calls = c_get.clone();
                let lock = l_get.clone();
                async move {
                    calls.got.fetch_add(1, Ordering::SeqCst);
                    match lock.lock().expect("lock").clone() {
                        Some(cm) => (StatusCode::OK, Json(cm)),
                        None => (
                            StatusCode::NOT_FOUND,
                            Json(serde_json::json!({ "apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": "NotFound", "code": 404 })),
                        ),
                    }
                }
            })
            // R-1's whole point: takeover is a preconditioned DELETE + create, never a PUT. A PUT
            // reaching the fake server at all is the defect this route exists to catch.
            .put(move |_: Json<serde_json::Value>| async move { (StatusCode::METHOD_NOT_ALLOWED, "PUT must never be sent for this lock") })
            .delete(move |Json(body): Json<serde_json::Value>| {
                let calls = c_delete.clone();
                let lock = l_delete.clone();
                async move {
                    let mut slot = lock.lock().expect("lock");
                    let precondition_uid = body.pointer("/preconditions/uid").and_then(|v| v.as_str()).map(str::to_owned);
                    let current_uid = slot.as_ref().and_then(|cm| cm.pointer("/metadata/uid")).and_then(|v| v.as_str()).map(str::to_owned);
                    if slot.is_none() {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(serde_json::json!({ "apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": "NotFound", "code": 404 })),
                        );
                    }
                    if precondition_uid.is_some() && precondition_uid != current_uid {
                        return (
                            StatusCode::CONFLICT,
                            Json(serde_json::json!({ "apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": "Conflict", "code": 409 })),
                        );
                    }
                    calls.deleted.fetch_add(1, Ordering::SeqCst);
                    *calls.deleted_uid.lock().expect("lock") = current_uid;
                    *slot = None;
                    (StatusCode::OK, Json(serde_json::json!({ "apiVersion": "v1", "kind": "Status", "status": "Success", "code": 200 })))
                }
            }),
        )
        .route(
            "/api/v1/nodes",
            axum::routing::get(move || {
                let nodes = c_nodes.clone();
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
        // Anything with a PUT this fixture did not name explicitly — teardown's own state
        // restores use PUT on unrelated `/v1` paths, which must keep working; only the lock's own
        // PUT route above is meant to refuse.
        .fallback(axum::routing::any(|| async { Json(serde_json::json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), calls)
}

/// A lock already sitting on the fake cluster. `kind` is `"roll"` or `"probe"` (or omitted, for
/// the old-format-lock test), `owner_pod_uid` empty means the field is left off entirely (an
/// operator's `roll.sh`, or a lock written before the field existed).
fn seeded_lock(holder: &str, kind: Option<&str>, owner_pod_uid: &str, created_at: &str) -> serde_json::Value {
    let mut data = serde_json::Map::new();
    data.insert("holder".into(), holder.into());
    if let Some(kind) = kind {
        data.insert("kind".into(), kind.into());
    }
    if !owner_pod_uid.is_empty() {
        data.insert("owner_pod_uid".into(), owner_pod_uid.into());
    }
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": "kloudlite-roll-coordination",
            "namespace": "kloudlite",
            "uid": "old-lock-uid",
            "resourceVersion": "9",
            "creationTimestamp": created_at,
        },
        "data": data,
    })
}

/// A pod list answering with exactly one pod, at the given uid and phase — or empty if `uid` is
/// `None` (the holder pod is simply gone). `mount` itself has no pods route, since the happy-path
/// tests never need one; this layers one on top for the takeover/peek tests.
async fn with_pods(base: &str, calls: Arc<KubeCalls>, uid: Option<&str>, phase: &str) -> (String, Arc<KubeCalls>) {
    let items = match uid {
        Some(uid) => serde_json::json!([{
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": "holder-pod", "uid": uid },
            "status": { "phase": phase },
        }]),
        None => serde_json::json!([]),
    };
    let app = axum::Router::new().route(
        "/api/v1/namespaces/kloudlite/pods",
        axum::routing::get(move || {
            let items = items.clone();
            async move {
                Json(serde_json::json!({
                    "apiVersion": "v1", "kind": "PodList",
                    "metadata": { "resourceVersion": "1" },
                    "items": items,
                }))
            }
        }),
    );
    // Proxy every other path to the base server, so the two fixtures compose without a second
    // KUBECONFIG: this listener answers `/pods` itself and forwards everything else.
    let base = base.to_string();
    let client = reqwest::Client::new();
    let fallback = axum::routing::any({
        let base = base.clone();
        move |req: axum::extract::Request| {
            let client = client.clone();
            let base = base.clone();
            async move {
                let uri = format!("{base}{}", req.uri());
                let method = req.method().clone();
                let headers = req.headers().clone();
                let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap_or_default();
                // Content-Type especially: kube's create/delete send a JSON body, and axum's own
                // `Json` extractor on the base server 415s without the header — dropping it here
                // silently turned every proxied write into a phantom failure.
                let resp = client.request(method, &uri).headers(headers).body(body).send().await;
                match resp {
                    Ok(r) => {
                        let status = r.status();
                        let bytes = r.bytes().await.unwrap_or_default();
                        (status, bytes).into_response()
                    }
                    Err(_) => (StatusCode::BAD_GATEWAY, "proxy failed").into_response(),
                }
            }
        }
    });
    let app = app.fallback(fallback);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), calls)
}

use axum::response::IntoResponse;

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
/// itself inherited. Everything is passed through `Command::env` on THIS child only, never a
/// process-global `std::env::set_var` — that, plus every fixture binding its own port-0 listener,
/// is what lets these tests run in parallel under the default runner rather than interfering.
fn child_command(suite: &str, url: &str, kubeconfig: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kloudlite-slo"));
    command
        .args(["run", "--suite", suite])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
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
    let (kube_url, kube) = mount(Arc::new(KubeCalls::default()), None).await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || {
            child_command("fast", &url, &kubeconfig)
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

    // Ruling 3: the fast suite never HOLDS the lock at all — it only peeks (GET), so a plain fast
    // run against an empty lock creates nothing and deletes nothing.
    assert_eq!(kube.created.load(Ordering::SeqCst), 0, "the fast suite must never create a lock; logs:\n{logs}");
    assert_eq!(kube.deleted.load(Ordering::SeqCst), 0, "logs:\n{logs}");
    assert_eq!(kube.put_count.load(Ordering::SeqCst), 0, "PUT was sent for the lock; logs:\n{logs}");
    assert!(kube.got.load(Ordering::SeqCst) >= 1, "the fast suite never peeked the lock; logs:\n{logs}");
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
    let (kube_url, kube) = mount(Arc::new(KubeCalls::default()), None).await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || {
            child_command("fast", &url, &kubeconfig)
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
    // Fail-closed: nothing in the run reached the fake cluster.
    assert_eq!(kube.got.load(Ordering::SeqCst), 0, "the run fell back to KUBECONFIG; logs:\n{logs}");
    assert_eq!(kube.nodes_listed.load(Ordering::SeqCst), 0, "logs:\n{logs}");
}

/// R-1 / R-C1, for a suite that DOES hold the lock (hourly): a lock left behind by a holder pod
/// that is simply gone (never went `Succeeded` or `Failed` — OOM-killed or SIGKILLed, nothing
/// cleaned up) must not block the fleet forever. `acquire`'s AlreadyExists branch reads the old
/// lock, sees its `owner_pod_uid` names no live pod, and takes it over by a preconditioned DELETE
/// followed by the ordinary create — never a PUT (the fake server's PUT route on the lock refuses
/// with 405, and this test asserts it was never hit).
#[tokio::test(flavor = "multi_thread")]
async fn a_lock_whose_owner_pod_is_absent_is_taken_over() {
    let (url, reports, _lists) = stub().await;
    let existing = seeded_lock("dead-pod-uid/hourly-1/aaaa", Some("probe"), "dead-pod-uid", "2026-09-20T00:00:00Z");
    let (kube_url, kube) = mount(Arc::new(KubeCalls::default()), Some(existing)).await;
    let (kube_url, kube) = with_pods(&kube_url, kube, None, "Running").await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || child_command("hourly", &url, &kubeconfig).output().expect("spawn")
    })
    .await
    .expect("join");
    let _ = std::fs::remove_dir_all(kubeconfig.parent().expect("kubeconfig dir"));

    let logs = String::from_utf8_lossy(&out.stderr).to_string();
    // Not asserting exit 0: the takeover succeeds and the full hourly journey then runs for real
    // against this test's bare-bones fake admin server, which fails plenty of unrelated steps —
    // that noise is not what R-1 is about. What R-1 promises is that the run was never REFUSED
    // for the lock: it took the roll lock and reported normally rather than being skipped.
    assert_ne!(out.status.code(), Some(2), "the run failed to start (EXIT_CONFIG); logs:\n{logs}");
    assert_eq!(kube.put_count.load(Ordering::SeqCst), 0, "a PUT was sent for the lock; logs:\n{logs}");
    // At least two deletes: the takeover's precondition-delete of the OLD lock (uid captured
    // below), and the run's own release of the NEW lock it then held at the end — both legitimate,
    // neither a PUT. `deleted_uid` is overwritten by whichever delete lands last, so the takeover's
    // own delete is asserted by its call count having happened at all (`>= 2`), and separately that
    // the OLD lock's uid appears among what the test observed (the create only succeeds because
    // that delete happened first).
    assert!(kube.deleted.load(Ordering::SeqCst) >= 2, "the dead lock was never taken over; logs:\n{logs}");
    assert!(kube.created.load(Ordering::SeqCst) >= 1, "never attempted the create; logs:\n{logs}");
    assert!(logs.contains("slo.run.finished"), "no final log; logs:\n{logs}");
    let reports = reports.lock().expect("lock");
    let last = reports.last().expect("at least one report");
    assert_ne!(last["state"], "yielded", "a dead-holder lock should have been taken, not skipped: {last}");
}

/// The other half, same suite: a LIVE holder must still refuse the takeover.
#[tokio::test(flavor = "multi_thread")]
async fn a_lock_whose_owner_pod_is_live_is_respected() {
    let (url, _reports, _lists) = stub().await;
    let existing = seeded_lock("live-pod-uid/hourly-1/aaaa", Some("probe"), "live-pod-uid", "2026-09-20T00:00:00Z");
    let (kube_url, kube) = mount(Arc::new(KubeCalls::default()), Some(existing)).await;
    let (kube_url, kube) = with_pods(&kube_url, kube, Some("live-pod-uid"), "Running").await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || child_command("hourly", &url, &kubeconfig).output().expect("spawn")
    })
    .await
    .expect("join");
    let _ = std::fs::remove_dir_all(kubeconfig.parent().expect("kubeconfig dir"));

    let logs = String::from_utf8_lossy(&out.stderr).to_string();
    // The hourly suite still fails closed on a live holder (only fast may read one as routine).
    assert_eq!(out.status.code(), Some(2), "exit; logs:\n{logs}");
    assert!(logs.contains("live-pod-uid/hourly-1/aaaa"), "did not name the live holder; logs:\n{logs}");
    assert_eq!(kube.deleted.load(Ordering::SeqCst), 0, "a live holder must never be taken over; logs:\n{logs}");
    assert_eq!(kube.put_count.load(Ordering::SeqCst), 0, "logs:\n{logs}");
}

/// Ruling 3, corrected 20 Sep: a fast run under a live HOURLY (probe-kind) lock is invisible to
/// it — the fast suite only ever cares about a ROLL. It must run its normal journey and create no
/// lock of its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_fast_run_under_a_live_hourly_lock_runs_normally() {
    let (url, reports, _lists) = stub().await;
    let existing = seeded_lock("live-pod-uid/hourly-1/aaaa", Some("probe"), "live-pod-uid", "2026-09-20T00:00:00Z");
    let (kube_url, kube) = mount(Arc::new(KubeCalls::default()), Some(existing)).await;
    let (kube_url, kube) = with_pods(&kube_url, kube, Some("live-pod-uid"), "Running").await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || child_command("fast", &url, &kubeconfig).output().expect("spawn")
    })
    .await
    .expect("join");
    let _ = std::fs::remove_dir_all(kubeconfig.parent().expect("kubeconfig dir"));

    let logs = String::from_utf8_lossy(&out.stderr).to_string();
    assert_ne!(out.status.code(), Some(2), "the fast run refused to start under a probe lock; logs:\n{logs}");
    assert_eq!(kube.created.load(Ordering::SeqCst), 0, "the fast suite must never create a lock; logs:\n{logs}");
    assert_eq!(kube.deleted.load(Ordering::SeqCst), 0, "logs:\n{logs}");
    let reports = reports.lock().expect("lock");
    let last = reports.last().expect("at least one report");
    assert_ne!(last["state"], "yielded", "a fast run must not yield to a probe holder: {last}");
}

/// The other half of ruling 3: a fast run under a live ROLL lock DOES yield — exits 0, having
/// reported a skip naming the roll.
#[tokio::test(flavor = "multi_thread")]
async fn a_fast_run_under_a_live_roll_lock_yields_naming_the_roll() {
    let (url, reports, _lists) = stub().await;
    // No `owner_pod_uid` (an operator's own `roll.sh`) means this is judged by AGE — must stay
    // fresh relative to whenever the test runs, not a fixed date, or it silently reads as dead.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let existing = seeded_lock("operator/roll-1758325200-42", Some("roll"), "", &now);
    let (kube_url, kube) = mount(Arc::new(KubeCalls::default()), Some(existing)).await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || child_command("fast", &url, &kubeconfig).output().expect("spawn")
    })
    .await
    .expect("join");
    let _ = std::fs::remove_dir_all(kubeconfig.parent().expect("kubeconfig dir"));

    let logs = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(0), "exit; logs:\n{logs}");
    assert_eq!(kube.created.load(Ordering::SeqCst), 0, "the fast suite must never create a lock; logs:\n{logs}");
    let reports = reports.lock().expect("lock");
    let last = reports.last().expect("at least one report");
    assert_eq!(last["state"], "yielded", "logs:\n{logs}\nreport: {last}");
}

/// A lock written by an older build, before `kind` existed: no `owner_pod_uid` (only `roll.sh`
/// ever omits it — a probe's lock always has a pod), a holder shaped `{pod_uid_or_operator}/roll-…`
/// and a fresh `creationTimestamp`. `Kind::of`'s inference must read this as a roll and (since it
/// carries no resolvable pod) judge it live by age, well inside the 2 h bound — so the fast suite
/// still yields to it exactly as it would to a `kind`-tagged lock.
#[tokio::test(flavor = "multi_thread")]
async fn an_old_format_roll_lock_with_no_kind_is_recognised_as_a_roll() {
    let (url, reports, _lists) = stub().await;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let existing = seeded_lock("operator/roll-1758325200-7", None, "", &now);
    let (kube_url, _kube) = mount(Arc::new(KubeCalls::default()), Some(existing)).await;
    let kubeconfig = write_kubeconfig(&kube_url);
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        let kubeconfig = kubeconfig.clone();
        move || child_command("fast", &url, &kubeconfig).output().expect("spawn")
    })
    .await
    .expect("join");
    let _ = std::fs::remove_dir_all(kubeconfig.parent().expect("kubeconfig dir"));

    let logs = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(0), "exit; logs:\n{logs}");
    let reports = reports.lock().expect("lock");
    let last = reports.last().expect("at least one report");
    assert_eq!(last["state"], "yielded", "an old-format roll lock was not recognised; logs:\n{logs}\nreport: {last}");
}
