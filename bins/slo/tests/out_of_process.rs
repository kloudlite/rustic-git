//! The whole reason a run is two processes: `panic = "abort"` in the release profile means a
//! panicking stage kills its process outright, so this asserts the PARENT still tears down and
//! still files a finished report. It runs the real binary — an in-process test could not observe
//! the split at all.

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

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_stage_still_yields_a_finished_report_and_a_teardown() {
    let (url, reports, lists) = stub().await;
    let out = tokio::task::spawn_blocking({
        let url = url.clone();
        move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_kloudlite-slo"))
                .args(["run", "--suite", "fast"])
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("KLOUDLITE_SLO_TEST_PANIC", "1")
                .env("KLOUDLITE_ADMIN_API_URL", &url)
                .env("KLOUDLITE_API_URL", &url)
                .env("KLOUDLITE_WEB_URL", &url)
                .env("KLOUDLITE_URL", &url)
                .env("KLOUDLITE_REGISTRY", "127.0.0.1:1")
                .env("KLOUDLITE_SSH_HOST", "127.0.0.1")
                .env("KLOUDLITE_REGION", "test")
                .env("KLOUDLITE_JWT_SECRET", "0123456789abcdef0123456789abcdef")
                .output()
                .expect("spawn")
        }
    })
    .await
    .expect("join");

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
}
