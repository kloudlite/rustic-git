//! `/admin/slo*` through the router, against a canned ClickHouse.
//!
//! Two properties matter here and neither is about SQL: no ClickHouse is 503 with the sentence the
//! console keys its placeholder off (never a 500 — a deployment without ClickStack is supported),
//! and the probe's `PUT` refuses a report that does not describe the run it is filed under.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{admin::router, ApiState};
use kloudlite_workspaces::history::History;
use serde_json::{json, Value};
use std::sync::Arc;

fn jwt() -> Arc<Jwt> {
    Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap())
}

fn token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

async fn clickhouse(data: Value) -> String {
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |_body: String| {
            let data = data.clone();
            async move { axum::Json(json!({ "data": data })) }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/")
}

async fn serve(state: ApiState) -> (String, Arc<Jwt>) {
    let jwt = state.jwt.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    (format!("http://{addr}"), jwt)
}

/// A webhook that counts what it received, so a test can assert "exactly one line" rather than
/// "some line eventually".
async fn webhook() -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    let got = Arc::new(std::sync::Mutex::new(Vec::new()));
    let g = got.clone();
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let g = g.clone();
            async move {
                g.lock().unwrap().push(body);
                "ok"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/"), got)
}

fn report(run_id: &str) -> Value {
    json!({
        "run_id": run_id, "suite": "fast", "region": "central",
        "started": "2026-09-05T10:00:00Z", "finished": null,
        "state": "running", "stage": "1 · Identity", "steps": [],
    })
}

/// Every route, read and write: a missing ClickHouse is 503 with that exact body. The probe reads
/// it as "retry, then exit non-zero"; the console reads it as "draw the flat placeholder".
#[tokio::test]
async fn without_clickhouse_every_slo_route_is_503() {
    let (base, jwt) = serve(ApiState::new(jwt())).await;
    let c = reqwest::Client::new();
    for path in ["/admin/slo", "/admin/slo/runs", "/admin/slo/runs/fast-1", "/admin/slo/coverage", "/admin/slo/pipeline", "/admin/slo/marker/fast-1"] {
        let r = c.get(format!("{base}{path}")).bearer_auth(token(&jwt)).send().await.unwrap();
        assert_eq!(r.status(), 503, "{path}");
        assert_eq!(r.text().await.unwrap(), "history unavailable", "{path}");
    }
    let r = c
        .put(format!("{base}/admin/slo/runs/fast-1"))
        .bearer_auth(token(&jwt))
        .json(&report("fast-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

/// The path id and the body's `run_id` are the same fact twice. A mismatch would file a report
/// under a run it does not describe, so it never reaches the insert.
#[tokio::test]
async fn a_report_filed_under_the_wrong_run_is_refused() {
    let url = clickhouse(json!([])).await;
    let state = ApiState::new(jwt()).with_history(Arc::new(History::new(&url, "", "")));
    let (base, jwt) = serve(state).await;
    let r = reqwest::Client::new()
        .put(format!("{base}/admin/slo/runs/fast-1"))
        .bearer_auth(token(&jwt))
        .json(&report("fast-2"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("fast-2"));

    // …and a run id that is not `{suite}-{digits}` is refused by the same 400, from `validate`.
    let r = reqwest::Client::new()
        .put(format!("{base}/admin/slo/runs/fast-abc"))
        .bearer_auth(token(&jwt))
        .json(&report("fast-abc"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

/// A run nobody reported is a 404, not an empty 200 — the console links run ids from other pages,
/// and a blank detail page would read as "this run had no steps".
#[tokio::test]
async fn an_unknown_run_is_a_404() {
    let url = clickhouse(json!([])).await;
    let state = ApiState::new(jwt()).with_history(Arc::new(History::new(&url, "", "")));
    let (base, jwt) = serve(state).await;
    let r = reqwest::Client::new()
        .get(format!("{base}/admin/slo/runs/fast-9"))
        .bearer_auth(token(&jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

/// The claim gate is the parent router's, and it runs before any of these handlers — the probe is
/// a superadmin caller like every other admin client, with no credential of its own.
#[tokio::test]
async fn the_slo_routes_are_behind_the_superadmin_gate() {
    let (base, _) = serve(ApiState::new(jwt())).await;
    let r = reqwest::Client::new().get(format!("{base}/admin/slo")).send().await.unwrap();
    assert_eq!(r.status(), 401);
}

/// The probe's happy path, and the one thing the webhook exists for: a FAILED run is one line to
/// whoever is on call, carrying the step that failed. Exactly one — a report is one event.
#[tokio::test]
async fn a_failed_report_is_stored_and_notified_once() {
    let (hook, got) = webhook().await;
    let url = clickhouse(json!([])).await;
    let state = ApiState::new(jwt())
        .with_history(Arc::new(History::new(&url, "", "")))
        .with_slo_webhook(Some(hook));
    let (base, jwt) = serve(state).await;
    let mut body = report("fast-3");
    body["state"] = json!("failed");
    body["steps"] = json!([{
        "slo_id": "git.push.ok", "ts": "2026-09-05T10:00:01Z", "ok": false, "ms": 12,
        "skipped": false, "detail": "connection refused", "stage": "2 · Git",
    }]);
    let r = reqwest::Client::new()
        .put(format!("{base}/admin/slo/runs/fast-3"))
        .bearer_auth(token(&jwt))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 1, "one failed run is one line: {got:?}");
    assert_eq!(got[0]["kind"], "slo.run.failed");
    assert_eq!(got[0]["failed_step"], "git.push.ok");
    assert_eq!(got[0]["detail"], "connection refused");
}

/// A run that passed is not news. The webhook is for a broken journey, and a line per green run
/// would train everyone to ignore the channel.
#[tokio::test]
async fn a_passing_report_notifies_nobody() {
    let (hook, got) = webhook().await;
    let url = clickhouse(json!([])).await;
    let state = ApiState::new(jwt())
        .with_history(Arc::new(History::new(&url, "", "")))
        .with_slo_webhook(Some(hook));
    let (base, jwt) = serve(state).await;
    let mut body = report("fast-4");
    body["state"] = json!("passed");
    body["finished"] = json!("2026-09-05T10:05:00Z");
    let r = reqwest::Client::new()
        .put(format!("{base}/admin/slo/runs/fast-4"))
        .bearer_auth(token(&jwt))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(got.lock().unwrap().is_empty(), "a green run must not page anyone");
}
