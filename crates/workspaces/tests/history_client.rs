//! The ClickHouse client against a canned HTTP server — no real ClickHouse. What matters here is
//! the wire shape (JSONEachRow in, JSONCompact out), that inserts land in OUR database, and that a
//! server error is an error rather than silently-empty history — the failure mode that would make
//! the console quietly lie.

use axum::{routing::post, Router};
use rustic_git_workspaces::history::{schema, History, HistoryError};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<String>>>;

/// A canned ClickHouse: records each request body, answers `reply` with `status`.
async fn canned(status: u16, reply: &'static str) -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().route(
        "/",
        post(move |body: String| {
            let s = s.clone();
            async move {
                s.lock().unwrap().push(body);
                (axum::http::StatusCode::from_u16(status).unwrap(), reply)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn insert_qualifies_the_table_and_sends_one_line_per_row() {
    let (url, seen) = canned(200, "").await;
    let h = History::new(&url, "default", "");
    h.insert(
        "events",
        &[
            serde_json::json!({"id": "a", "kind": "workspace.created"}),
            serde_json::json!({"id": "b", "kind": "workspace.deleted"}),
        ],
    )
    .await
    .unwrap();
    let body = seen.lock().unwrap()[0].clone();
    // Qualified: `default` belongs to the OTel collector, and an unqualified INSERT would write
    // into whatever database the connection happened to default to.
    assert!(body.starts_with("INSERT INTO rustic.events FORMAT JSONEachRow\n"), "{body}");
    assert_eq!(body.lines().filter(|l| l.starts_with('{')).count(), 2);
    assert!(body.contains(r#""kind":"workspace.created""#));
}

#[tokio::test]
async fn an_empty_insert_makes_no_request_at_all() {
    let (url, seen) = canned(200, "").await;
    let h = History::new(&url, "default", "");
    h.insert("events", &[]).await.unwrap();
    assert!(seen.lock().unwrap().is_empty(), "an empty batch must not cost a round trip");
}

#[tokio::test]
async fn query_returns_the_json_compact_data_rows_and_passes_sql_through() {
    let reply = r#"{"meta":[{"name":"ts","type":"DateTime"},{"name":"value","type":"Float64"}],
                    "data":[["2026-09-04 10:00:00",3],["2026-09-04 11:00:00",5]],"rows":2}"#;
    let (url, seen) = canned(200, reply).await;
    let h = History::new(&url, "default", "");
    let rows = h.query("SELECT MetricName FROM otel_metrics_sum").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1][1], serde_json::json!(5));
    let body = seen.lock().unwrap()[0].clone();
    // Verbatim: a series query names the OTel tables in `default` itself, so the client must not
    // rewrite the FROM clause.
    assert!(body.starts_with("SELECT MetricName FROM otel_metrics_sum"), "{body}");
    assert!(body.ends_with(" FORMAT JSONCompact"));
}

/// A 500 must surface. Swallowing it would ack a Redis batch whose rows never landed, and the
/// stream is the only place those rows still existed.
#[tokio::test]
async fn a_server_error_is_an_error_not_an_empty_result() {
    let (url, _) = canned(500, "Code: 60. DB::Exception: Unknown table").await;
    let h = History::new(&url, "default", "");
    match h.query("SELECT 1").await {
        Err(HistoryError::Server { status, body }) => {
            assert_eq!(status, 500);
            assert!(body.contains("Unknown table"));
        }
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[tokio::test]
async fn from_env_is_none_without_a_url() {
    std::env::remove_var("RUSTIC_GIT_CLICKHOUSE_URL");
    assert!(History::from_env().is_none());
}

#[tokio::test]
async fn migrate_applies_every_statement_once() {
    let (url, seen) = canned(200, r#"{"meta":[],"data":[],"rows":0}"#).await;
    let h = History::new(&url, "default", "");
    let applied = schema::migrate(&h).await.unwrap();
    assert_eq!(applied as usize, schema::MIGRATIONS.len());
    let bodies = seen.lock().unwrap().clone();
    assert!(bodies.iter().any(|b| b.contains("CREATE DATABASE IF NOT EXISTS rustic")));
    assert!(bodies.iter().any(|b| b.contains("CREATE TABLE IF NOT EXISTS rustic.schema_migrations")));
    for (_, sql) in schema::MIGRATIONS {
        let head = sql.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
        assert!(bodies.iter().any(|b| b.contains(&head)), "migration not sent: {head}");
    }
    assert!(bodies.iter().any(|b| b.contains("INSERT INTO rustic.schema_migrations")));
}

/// Every statement must name our database. One unqualified CREATE would put a table of ours in
/// the collector's database, where its own migrations may later collide with it.
#[test]
fn every_migration_targets_the_rustic_database() {
    for (v, sql) in schema::MIGRATIONS {
        assert!(sql.contains("rustic."), "migration {v} names no rustic. object: {sql}");
    }
}

#[test]
fn every_migration_version_is_unique_and_ordered() {
    let versions: Vec<u32> = schema::MIGRATIONS.iter().map(|(v, _)| *v).collect();
    let mut sorted = versions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(versions, sorted, "migration versions must be unique and ascending");
}

/// The one test that talks to a real ClickHouse. Run it against a local ClickStack or a plain
/// `docker run -p 8123:8123 clickhouse/clickhouse-server`:
/// `RUSTIC_GIT_CLICKHOUSE_URL=http://localhost:8123 cargo test -p rustic-git-workspaces \
///   --test history_client -- --ignored`
///
/// The two materialized views over `otel_metrics_*` are skipped when those tables do not exist —
/// on a bare ClickHouse the collector has never run, and failing there would make this test
/// useless for the common case it exists to cover (our own four tables).
#[tokio::test]
#[ignore]
async fn migrations_apply_against_a_real_clickhouse() {
    let Some(h) = History::from_env() else {
        panic!("RUSTIC_GIT_CLICKHOUSE_URL must be set to run this test");
    };
    assert!(h.healthy().await, "ClickHouse did not answer");
    schema::migrate(&h).await.expect("first migrate");
    assert_eq!(schema::migrate(&h).await.expect("second migrate"), 0, "migrate must be idempotent");
    h.insert(
        "events",
        &[serde_json::json!({
            "ts": "2026-09-04 10:00:00.000", "id": "test:1:created", "kind": "test.event",
            "actor": "t@example.com", "owner": "acme", "target": "ws-1", "region": "central",
            "attrs": "{}"
        })],
    )
    .await
    .expect("insert");
    let rows = h
        .query("SELECT count() FROM rustic.events FINAL WHERE id = 'test:1:created'")
        .await
        .unwrap();
    assert_eq!(rows[0][0], serde_json::json!(1));
}

/// DDL answers 200 with an empty body, and `migrate` runs every CREATE through `query` — an empty
/// body is "no rows", never a parse failure.
#[tokio::test]
async fn an_empty_body_is_zero_rows_and_healthy() {
    let (url, _) = canned(200, "").await;
    let h = History::new(&url, "default", "");
    assert!(h.query("CREATE DATABASE IF NOT EXISTS rustic").await.unwrap().is_empty());
    assert!(h.healthy().await);
}

/// `healthy` is what the history routes' 503 decision reads, so a refusing server must be unhealthy
/// rather than "answered, so fine".
#[tokio::test]
async fn healthy_is_false_when_the_server_refuses() {
    let (url, _) = canned(500, "Code: 516. DB::Exception: Authentication failed").await;
    assert!(!History::new(&url, "default", "").healthy().await);
}

/// The table name is interpolated into SQL. It is refused on an allow-list, never escaped.
#[tokio::test]
async fn an_unsafe_table_name_is_refused_before_any_request() {
    let (url, seen) = canned(200, "").await;
    let h = History::new(&url, "default", "");
    assert!(h
        .insert("rustic.events; DROP", &[serde_json::json!({})])
        .await
        .is_err());
    assert!(seen.lock().unwrap().is_empty(), "a refused name must not reach the server");
}

/// Both hourly tables end as ReplacingMergeTree: `tick_once` truncates `ts` to the hour, so two
/// admin replicas beating in one hour write rows identical on the whole sort key and the second is
/// a duplicate, not an observation. Asserted on the rebuild statement, since an engine cannot be
/// ALTERed and the migrations that shipped must never be edited.
#[test]
fn the_hourly_tables_end_as_replacing_merge_trees() {
    for table in ["usage_hourly", "fleet_hourly"] {
        let create = schema::MIGRATIONS
            .iter()
            .filter(|(_, sql)| sql.contains(&format!("rustic.{table}_v2 (")))
            .next_back()
            .unwrap_or_else(|| panic!("{table} is never rebuilt"))
            .1;
        assert!(create.contains("ENGINE = ReplacingMergeTree"), "{table}: {create}");
        assert!(
            schema::MIGRATIONS
                .iter()
                .any(|(_, sql)| sql.contains(&format!("EXCHANGE TABLES rustic.{table} AND"))),
            "{table} is rebuilt but never swapped in"
        );
    }
}
