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
    for path in ["/admin/slo", "/admin/slo/runs", "/admin/slo/runs/fast-1", "/admin/slo/coverage", "/admin/slo/pipeline", "/admin/slo/marker/fast-1", "/admin/slo/exclusions"] {
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
    // Both exclusion writes too: a window excluded against no ClickHouse would be a decision
    // nobody stored, answered 200.
    let r = c
        .post(format!("{base}/admin/slo/exclusions"))
        .bearer_auth(token(&jwt))
        .json(&exclusion())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    let r = c
        .delete(format!("{base}/admin/slo/exclusions/x-1"))
        .bearer_auth(token(&jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

fn exclusion() -> Value {
    json!({
        "from": "2026-09-15T04:00:00Z", "to": "2026-09-15T10:00:00Z",
        "slo_ids": [], "note": "k3s apiserver watch-cache freeze",
    })
}

/// The three ways a window is not a decision anybody can read back later: no reason, no window,
/// or a window so long it has stopped being an incident and become the target.
#[tokio::test]
async fn a_window_without_a_reason_or_with_a_bad_span_is_refused() {
    let url = clickhouse(json!([])).await;
    let state = ApiState::new(jwt()).with_history(Arc::new(History::new(&url, "", "").unwrap()));
    let (base, jwt) = serve(state).await;
    let c = reqwest::Client::new();
    let post = |body: Value| {
        let (c, base, t) = (c.clone(), base.clone(), token(&jwt));
        async move { c.post(format!("{base}/admin/slo/exclusions")).bearer_auth(t).json(&body).send().await.unwrap() }
    };
    let mut blank = exclusion();
    blank["note"] = json!("   ");
    assert_eq!(post(blank).await.status(), 422);

    let mut backwards = exclusion();
    backwards["to"] = json!("2026-09-15T04:00:00Z");
    assert_eq!(post(backwards).await.status(), 422);

    let mut forever = exclusion();
    forever["to"] = json!("2026-09-30T04:00:00Z");
    assert_eq!(post(forever).await.status(), 422);

    let mut unknown = exclusion();
    unknown["slo_ids"] = json!(["nope.at.all"]);
    let r = post(unknown).await;
    assert_eq!(r.status(), 422);
    assert!(r.text().await.unwrap().contains("nope.at.all"));

    // The happy path answers the stored row, id and all.
    let r = post(exclusion()).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["id"].as_str().unwrap().starts_with("x-"));
    // The actor is the handle every other admin write is audited under, not the email.
    assert_eq!(body["by"], "root");
}

/// An exclusion moves every budget on the console, so it is an admin write like any other: one
/// audit row per decision, naming the actor, the window's id and the note.
#[tokio::test]
async fn excluding_and_unexcluding_each_write_one_audit_row() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = Arc::new(
        kloudlite_storage::store::Store::open(
            Arc::new(object_store::memory::InMemory::new()),
            tmp.path().join("cache"),
            false,
        )
        .await
        .unwrap(),
    );
    let url = clickhouse(json!([])).await;
    let state = ApiState::new(jwt())
        .with_history(Arc::new(History::new(&url, "", "").unwrap()))
        .with_keys(keys.clone());
    let (base, jwt) = serve(state).await;
    let c = reqwest::Client::new();
    let r = c
        .post(format!("{base}/admin/slo/exclusions"))
        .bearer_auth(token(&jwt))
        .json(&exclusion())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let id = r.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string();
    let r = c
        .delete(format!("{base}/admin/slo/exclusions/{id}"))
        .bearer_auth(token(&jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let rows = audit_rows(&keys).await;
    let of = |action: &str| -> Vec<Value> {
        rows.iter().filter(|r| r["action"] == action).cloned().collect()
    };
    let excluded = of("slo-exclude");
    assert_eq!(excluded.len(), 1, "one decision is one row: {rows:?}");
    assert_eq!(excluded[0]["actor"], "root");
    assert_eq!(excluded[0]["target"], id);
    assert_eq!(excluded[0]["reason"], "k3s apiserver watch-cache freeze");
    let unexcluded = of("slo-unexclude");
    assert_eq!(unexcluded.len(), 1, "{rows:?}");
    assert_eq!(unexcluded[0]["target"], id);
}

/// Every audit row the admin process wrote, straight off the object store.
async fn audit_rows(keys: &kloudlite_storage::store::Store) -> Vec<Value> {
    use futures::StreamExt;
    use slatedb::object_store::ObjectStoreExt;
    let mut out = Vec::new();
    let mut list = keys.os.list(Some(&slatedb::object_store::path::Path::from("audit")));
    while let Some(Ok(meta)) = list.next().await {
        let bytes = keys.os.get(&meta.location).await.unwrap().bytes().await.unwrap();
        out.push(serde_json::from_slice(&bytes).unwrap());
    }
    out
}

/// The exclusion routes sit behind the same claim gate as the rest of `/admin/slo*`.
#[tokio::test]
async fn the_exclusion_routes_are_behind_the_superadmin_gate() {
    let (base, _) = serve(ApiState::new(jwt())).await;
    let c = reqwest::Client::new();
    assert_eq!(c.get(format!("{base}/admin/slo/exclusions")).send().await.unwrap().status(), 401);
    let r = c.post(format!("{base}/admin/slo/exclusions")).json(&exclusion()).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(c.delete(format!("{base}/admin/slo/exclusions/x-1")).send().await.unwrap().status(), 401);
}

/// The path id and the body's `run_id` are the same fact twice. A mismatch would file a report
/// under a run it does not describe, so it never reaches the insert.
#[tokio::test]
async fn a_report_filed_under_the_wrong_run_is_refused() {
    let url = clickhouse(json!([])).await;
    let state = ApiState::new(jwt()).with_history(Arc::new(History::new(&url, "", "").unwrap()));
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
    let state = ApiState::new(jwt()).with_history(Arc::new(History::new(&url, "", "").unwrap()));
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
        .with_history(Arc::new(History::new(&url, "", "").unwrap()))
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
        .with_history(Arc::new(History::new(&url, "", "").unwrap()))
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
