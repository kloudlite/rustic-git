//! `/admin/slo*` through the router, against a canned ClickHouse.
//!
//! Two properties matter here and neither is about SQL: no ClickHouse is 503 with the sentence the
//! console keys its placeholder off (never a 500 — a deployment without ClickStack is supported),
//! and the probe's `PUT` refuses a report that does not describe the run it is filed under.

use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_workspaces::api::{admin::router, ApiState};
use kloudlite_git_workspaces::history::History;
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
