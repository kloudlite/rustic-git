//! `/admin/history/*` through the router, against a canned ClickHouse.
//!
//! What is asserted is the contract the console keys off: the exact JSON shape, the summary math,
//! a 404 for anything not in the catalogue, and 503 — never 500 — when there is no ClickHouse at
//! all, which is a supported deployment rather than an outage.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{admin::router, ApiState};
use rustic_git_workspaces::history::History;
use serde_json::{json, Value};
use std::sync::Arc;

fn jwt() -> Arc<Jwt> {
    Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap())
}

/// One canned `FORMAT JSONCompact` answer for every query, plus the SQL each request carried so a
/// test can assert what was actually asked.
async fn clickhouse(data: Value) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |body: String| {
            let s = s.clone();
            let data = data.clone();
            async move {
                s.lock().unwrap().push(body);
                axum::Json(json!({ "data": data }))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/"), seen)
}

async fn serve(state: ApiState) -> (String, Arc<Jwt>) {
    let jwt = state.jwt.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    (format!("http://{addr}"), jwt)
}

async fn get(base: &str, path: &str, jwt: &Jwt) -> (u16, String) {
    let token = jwt
        .mint_admin("root@example.com", "Root", Some("root"), true)
        .unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

async fn with_history(data: Value) -> (String, Arc<Jwt>, Arc<std::sync::Mutex<Vec<String>>>) {
    let (url, seen) = clickhouse(data).await;
    let state = ApiState::new(jwt()).with_history(Arc::new(History::new(&url, "u", "p")));
    let (base, jwt) = serve(state).await;
    (base, jwt, seen)
}

#[tokio::test]
async fn a_series_answers_points_and_a_summary() {
    // The middle bucket arrives QUOTED: ClickHouse's JSONCompact quotes 64-bit integers, so a
    // `count()` is a string on the wire and reading it as a number only draws every count as zero.
    let (base, jwt, seen) = with_history(json!([
        ["2026-09-01 00:00:00", 3.0],
        ["2026-09-02 00:00:00", "9"],
        ["2026-09-03 00:00:00", 5.0],
    ]))
    .await;
    let (status, body) = get(&base, "/admin/history/live_workspaces?range=30d&step=1d", &jwt).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v,
        json!({
            "series": [
                {"ts": "2026-09-01T00:00:00Z", "value": 3.0},
                {"ts": "2026-09-02T00:00:00Z", "value": 9.0},
                {"ts": "2026-09-03T00:00:00Z", "value": 5.0},
            ],
            "summary": {"last": 5.0, "delta": 2.0, "min": 3.0, "max": 9.0},
        })
    );
    // The route's own parameters reached the statement, not defaults.
    let sql = seen.lock().unwrap().join("\n");
    assert!(sql.contains("toStartOfDay") && sql.contains("INTERVAL 30 DAY"), "{sql}");
}

/// A fresh cluster returns nothing, and the console must still render: zeros, never `null`.
#[tokio::test]
async fn an_empty_series_is_zeros_not_nulls() {
    let (base, jwt, _) = with_history(json!([])).await;
    let (status, body) = get(&base, "/admin/history/pool_used", &jwt).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["series"].as_array().unwrap().len(), 0);
    assert_eq!(v["summary"], json!({"last": 0.0, "delta": 0.0, "min": 0.0, "max": 0.0}));
}

#[tokio::test]
async fn an_unknown_series_or_a_malformed_parameter_is_a_404() {
    let (base, jwt, seen) = with_history(json!([])).await;
    for path in [
        "/admin/history/nonsense",
        "/admin/history/pool_used?range=1y",
        "/admin/history/pool_used?step=1s",
        // `usage` without its owner must not become a query across every owner.
        "/admin/history/usage",
        "/admin/history/usage?owner=acme",
    ] {
        let (status, body) = get(&base, path, &jwt).await;
        assert_eq!(status, 404, "{path}: {body}");
    }
    assert!(seen.lock().unwrap().is_empty(), "a 404 must query nothing");
}

/// A quote in a caller-supplied filter is refused, not escaped — and nothing reaches ClickHouse.
#[tokio::test]
async fn a_quoted_filter_is_refused() {
    let (base, jwt, seen) = with_history(json!([])).await;
    for path in [
        "/admin/history/pool_used?region=eu%27%3B%20DROP%20TABLE%20rustic.events%3B%20--",
        "/admin/history/events?owner=a%27%20OR%20%271%27%3D%271",
        "/admin/history/events?cursor=%27%29%3B%20DROP",
        "/admin/history/events?cursor=no-separator",
    ] {
        let (status, body) = get(&base, path, &jwt).await;
        assert_eq!(status, 404, "{path}: {body}");
    }
    assert!(seen.lock().unwrap().is_empty(), "a refused filter must query nothing");
}

#[tokio::test]
async fn events_page_newest_first_with_a_cursor_only_on_a_full_page() {
    let (base, jwt, seen) = with_history(json!([[
        "id-1",
        "2026-09-03 12:00:00",
        "admin.drain",
        "root@example.com",
        "acme",
        "node-2",
        "eu-west",
        "{\"detail\":\"planned\"}"
    ]]))
    .await;
    let (status, body) = get(&base, "/admin/history/events?kind=admin.drain&limit=1", &jwt).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["events"][0],
        json!({
            "id": "id-1",
            "ts": "2026-09-03T12:00:00Z",
            "kind": "admin.drain",
            "actor": "root@example.com",
            "owner": "acme",
            "target": "node-2",
            "region": "eu-west",
            "attrs": {"detail": "planned"},
        })
    );
    // A full page offers a cursor; the timeline pages by position, never by offset.
    assert_eq!(v["cursor"], json!("2026-09-03T12:00:00Z|id-1"));
    let sql = seen.lock().unwrap().join("\n");
    assert!(sql.contains("rustic.events FINAL"), "{sql}");
    assert!(sql.contains("kind = 'admin.drain'"), "{sql}");
    assert!(sql.contains("ORDER BY ts DESC, id DESC LIMIT 1"), "{sql}");
}

/// Several admin writes can share a millisecond, so a bare `ts <` boundary would drop every
/// sibling of the row the previous page ended on. The cursor carries the id and the comparison is
/// on the pair.
#[tokio::test]
async fn the_cursor_boundary_is_the_pair_not_the_timestamp() {
    let (base, jwt, seen) = with_history(json!([[
        "id-a", "2026-09-03 12:00:00", "k", "a", "", "", "", "{}"
    ]]))
    .await;
    let (status, body) = get(
        &base,
        "/admin/history/events?cursor=2026-09-03T12%3A00%3A00Z%7Cid-b",
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let sql = seen.lock().unwrap().join("\n");
    assert!(
        sql.contains("(ts, id) < (parseDateTimeBestEffort('2026-09-03T12:00:00Z'), 'id-b')"),
        "{sql}"
    );
    // Two rows sharing a timestamp: paging from the first must be able to reach the second.
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["events"][0]["id"], json!("id-a"));
}

/// A short page is the last page — a cursor there costs every client one empty round trip.
#[tokio::test]
async fn a_short_events_page_offers_no_cursor() {
    let (base, jwt, _) = with_history(json!([["id-1", "2026-09-03 12:00:00", "k", "a", "", "", "", "not json"]])).await;
    let (status, body) = get(&base, "/admin/history/events", &jwt).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["cursor"], Value::Null);
    // Unparsable attrs become an object, because the console reads fields off it directly.
    assert_eq!(v["events"][0]["attrs"], json!({}));
    // An event that names nobody carries `null`, not `""` — the console's `??` falls through an
    // empty string.
    assert_eq!(v["events"][0]["owner"], Value::Null);
    assert_eq!(v["events"][0]["target"], Value::Null);
    assert_eq!(v["events"][0]["region"], Value::Null);
}

#[tokio::test]
async fn without_clickhouse_both_routes_are_503() {
    let (base, jwt) = serve(ApiState::new(jwt())).await;
    for path in ["/admin/history/pool_used", "/admin/history/events"] {
        let (status, body) = get(&base, path, &jwt).await;
        assert_eq!(status, 503, "{path}");
        assert_eq!(body, "history unavailable");
    }
}

/// Both routes sit above `route_layer`, so an unsigned or unprivileged caller never reaches a
/// handler — asserted here rather than trusted, because a route added below that line would still
/// compile.
#[tokio::test]
async fn both_routes_are_behind_the_superadmin_gate() {
    let (base, jwt, seen) = with_history(json!([])).await;
    let ordinary = jwt
        .mint_admin("someone@example.com", "Someone", Some("someone"), false)
        .unwrap();
    for path in ["/admin/history/pool_used", "/admin/history/events"] {
        let c = reqwest::Client::new();
        assert_eq!(c.get(format!("{base}{path}")).send().await.unwrap().status(), 401);
        let code = c
            .get(format!("{base}{path}"))
            .bearer_auth(&ordinary)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(code, 403, "{path}");
    }
    assert!(seen.lock().unwrap().is_empty(), "no handler ran before the claim check");
}
