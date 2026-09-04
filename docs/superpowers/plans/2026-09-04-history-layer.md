# History Layer Implementation Plan (sub-project A, on ClickStack)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up ClickStack (ClickHouse + OpenTelemetry collectors + HyperDX) as the fleet's telemetry substrate, and give the admin process a `rustic` database it owns — events, hourly usage and fleet folds, and alert-state transitions — plus a history API the console reads.

**Architecture:** Telemetry is **not ours to write**. The official ClickStack Helm charts run ClickHouse, a gateway OTel collector and HyperDX on AKS; an `opentelemetry-collector-contrib` agent collector in every cluster scrapes the pods already annotated `prometheus.io/scrape`, reads node/pod resource usage from `kubeletstats`/`k8s_cluster`, ships pod logs, stamps `region`, and exports OTLP to the gateway, which writes the exporter's standard tables in the `default` database (`otel_metrics_gauge`, `otel_metrics_sum`, `otel_metrics_histogram`, `otel_logs`, `otel_traces`). We add nothing to that path — no monitor binary, no ingest routes, no metrics tables of our own.

What *is* ours is the `rustic` database, and the admin process (`bins/api`, `RUSTIC_GIT_API_ROLE=admin`) is its only writer: it migrates the schema at boot, consumes the Redis `events` stream in a second consumer group, turns per-region Kubernetes watch transitions into `events` rows with idempotent ids, dual-writes every audit row, runs hourly `usage_hourly`/`fleet_hourly` beats folded fresh from the CRDs, and evaluates `deploy/alerts.md` every 30 s as SQL over `otel_metrics_*` with real `for` windows, writing transitions to `rustic.alerts`. `rustic-git-agent` gains a small stats beat for the two gauges only it can know (btrfs pool bytes, running working copies); CPU, memory and load come from `kubeletstats`. Everything is optional: without `RUSTIC_GIT_CLICKHOUSE_URL` every process runs exactly as today and the history routes answer `503 history unavailable`.

**Tech Stack:** Rust (axum 0.8, reqwest 0.13, tokio, kube-rs, chrono, serde_json), ClickHouse HTTP interface, ClickStack Helm charts (`clickstack-operators`, `clickstack`), `opentelemetry-collector-contrib`, HyperDX, Redis streams (existing `Cache` helpers), Kubernetes CRDs (`rustic-git.io/v1alpha1`).

**Spec:** `docs/superpowers/specs/2026-09-04-history-and-console-v2-design.md` (§A "History layer — on ClickStack" and §"Not doing"; §B and §C are separate plans)

## Global Constraints

- **Two databases, one writer each.** `default` belongs to the OTel collector — we only ever SELECT from it. `rustic` is ours and **the admin process is its only writer** (`bins/api`, `RUSTIC_GIT_API_ROLE=admin`). No other binary opens a ClickHouse connection at all.
- **We write no telemetry pipeline.** No monitor binary, no `/ingest/*` routes, no `samples` table. Metrics, logs and traces arrive through OpenTelemetry. If a number is missing, the fix is collector config, not Rust.
- **`RUSTIC_GIT_CLICKHOUSE_URL` is optional everywhere.** A process without it runs exactly as today; history routes answer `503 history unavailable`, which the web renders as a flat placeholder, never an error page. `RUSTIC_GIT_HYPERDX_URL` is optional the same way — an unset value means no "Open in HyperDX" link rather than a dead one.
- **The Redis `events` stream is a nudge, never the record** (CLAUDE.md). If Redis is down the `history` consumer idles and the kube watches and hourly beats keep writing. No ClickHouse row may depend on a stream entry having arrived.
- **Usage is computed from the CRDs on every run and never cached.** The hourly beats re-fold `owners::fleet` and the clusters fold; never derive a row from an earlier row.
- **`spec.owner` is truth, labels are a view.** Never authorize or attribute on a label.
- **At-least-once must never double-count.** `rustic.events` and `rustic.alerts` are `ReplacingMergeTree` keyed on `id`; every reader queries `FINAL`. Event ids are deterministic: `{uid}:{resourceVersion}:{transition}`.
- **One catalogue, two evaluators.** `deploy/alerts.md` is the single source: HyperDX alerts are created from it for paging, and the admin process evaluates the same rules in SQL for the console. A disagreement between them is a bug in one of them, never a mystery — so both must name the rule identically.
- Retention: `rustic.events` none (it is the record), `rustic.usage_hourly` and `rustic.fleet_hourly` 2 years, `rustic.alerts` 400 days, `rustic.metrics_5m` 400 days. Raw OTel metrics keep the exporter's own 30-day `ttl`.
- OTel exporter schema facts this plan relies on (verified against the collector-contrib `clickhouseexporter` README): tables `otel_metrics_gauge`, `otel_metrics_sum`, `otel_metrics_histogram`, `otel_logs`, `otel_traces`; the gauge and sum tables carry `TimeUnix`, `MetricName`, `Value`, `Attributes` (Map), `ResourceAttributes` (Map), `ScopeName`, `StartTimeUnix`. Anything beyond those columns must be checked with `DESCRIBE TABLE` before it is used.
- House style: comments explain WHY, never what. Deliberate shortcuts are marked `// ponytail: <ceiling and upgrade path>`.
- Commit subjects are imperative sentence case, no tool attribution.
- CI gates on `cargo clippy --workspace -- -D warnings`; the test job also runs `--all-targets`, so no new warning in a file you touch.

---

## File Structure

**New files:**
- `crates/workspaces/src/history/mod.rs` — the `History` handle: HTTP client, `insert`, `query`, `healthy`, database qualification.
- `crates/workspaces/src/history/schema.rs` — numbered migrations: the `rustic` database, its four tables, and the `metrics_5m` rollup + materialized views over the OTel tables.
- `crates/workspaces/src/history/events.rs` — `EventRow`, the id scheme, the audit mapper, the Redis consumer and the kube-watch transition mapper.
- `crates/workspaces/src/history/beats.rs` — the hourly `usage_hourly` / `fleet_hourly` beats.
- `crates/workspaces/src/history/alerts.rs` — the catalogue as SQL with real `for` windows, and the 30 s evaluator that writes transitions.
- `crates/workspaces/src/history/series.rs` — the named series → one SQL statement each.
- `crates/workspaces/src/api/admin/history.rs` — `GET /admin/history/{series}` and `/admin/history/events`.
- `bins/agent/src/stats.rs` — the node gauge beat and its parsers.
- `deploy/clickstack/{README.md,operators-values.yaml,clickstack-values.yaml,otel-agent-aks.yaml}` — the Helm value files and the exact commands.
- `deploy/k3s/otel-agent.yaml` — the per-region collector: ServiceAccount, ClusterRole (header table), Deployment, ConfigMap.
- Tests: `crates/workspaces/tests/history_client.rs`, `history_events.rs`, `history_state.rs`, `history_beats.rs`, `history_alerts.rs`, `history_series.rs`; `bins/agent/tests/stats.rs`.

**Modified:**
- `crates/workspaces/src/lib.rs`, `crates/workspaces/src/api/mod.rs`, `crates/workspaces/src/api/admin.rs`.
- `crates/workspaces/src/api/admin/{owners,clusters}.rs` (expose the folds `pub(crate)`), `monitoring.rs` (scrape code deleted, reads `rustic.alerts`).
- `bins/api/src/main.rs`, `bins/agent/src/lib.rs`.
- `deploy/rustic-git.yaml`, `deploy/k3s/agent-peer.yaml`, `deploy/k3s/README.md`, `deploy/alerts.md`, `CLAUDE.md`, `tests/ws_e2e.sh`.

No new binary, no workspace member, no Dockerfile or CI change: nothing in this plan compiles a new executable.

---

## Task 1: The `history` module — client and the `rustic` schema

**Files:**
- Create: `crates/workspaces/src/history/mod.rs`
- Create: `crates/workspaces/src/history/schema.rs`
- Modify: `crates/workspaces/src/lib.rs`
- Test: `crates/workspaces/tests/history_client.rs`

**Interfaces:**
- Consumes: nothing (first task).
- Produces:
  - `pub struct History` with `pub fn new(url: &str, user: &str, password: &str) -> History`, `pub fn from_env() -> Option<History>`
  - `pub async fn History::insert(&self, table: &str, rows: &[serde_json::Value]) -> Result<(), HistoryError>` — `table` is written unqualified by the caller and qualified to `rustic.` here.
  - `pub async fn History::query(&self, sql: &str) -> Result<Vec<Vec<serde_json::Value>>, HistoryError>` — `JSONCompact`, returns the `data` rows. SQL is passed through verbatim, so a query may name `rustic.events` or `otel_metrics_sum` itself.
  - `pub async fn History::healthy(&self) -> bool`
  - `pub enum HistoryError { Http(String), Server { status: u16, body: String } }`
  - `pub const DB: &str = "rustic";`
  - `pub async fn schema::migrate(h: &History) -> Result<u32, HistoryError>`; `pub const schema::MIGRATIONS: &[(u32, &str)]`

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/history_client.rs`:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test history_client`
Expected: FAIL — `unresolved import rustic_git_workspaces::history`.

- [ ] **Step 3: Write the client**

Create `crates/workspaces/src/history/mod.rs`:

```rust
//! ClickHouse over its HTTP interface, as ClickStack deploys it.
//!
//! TWO DATABASES, and the split is the whole design. `default` is the OpenTelemetry collector's:
//! `otel_metrics_gauge`, `otel_metrics_sum`, `otel_metrics_histogram`, `otel_logs`, `otel_traces`,
//! written by the exporter and read here for charts and alert evaluation — we never write it, and
//! its schema is the exporter's to change. `rustic` is ours: `events`, `usage_hourly`,
//! `fleet_hourly`, `alerts`, plus the `metrics_5m` rollup, and the ADMIN process (`bins/api` with
//! `RUSTIC_GIT_API_ROLE=admin`) is its only writer. Nothing else in the fleet constructs a
//! `History`.
//!
//! Deliberately a `reqwest` call and a format string, not a client crate. Two verbs cover every
//! caller — an `INSERT … FORMAT JSONEachRow` and a `SELECT … FORMAT JSONCompact` — and a driver
//! crate would buy connection pooling we do not need (writes are batched on beats) at the cost of
//! a dependency that has to track the server version.
//!
//! Optional by design: `from_env` answers `None` when `RUSTIC_GIT_CLICKHOUSE_URL` is unset, and
//! every caller treats that as "history unavailable" rather than an error, so a deployment without
//! ClickStack behaves exactly as it did before this module existed.

pub mod schema;

use std::time::Duration;

/// Our database. `insert` qualifies with it; a SELECT names its own tables, since the interesting
/// queries read the collector's `default` tables too.
pub const DB: &str = "rustic";

/// A query is on a superadmin's request path (behind a 10 s page poll) and an insert is on a beat;
/// neither may hang a task forever on a wedged server.
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum HistoryError {
    /// Could not reach the server at all — DNS, connect, timeout.
    Http(String),
    /// Reached it and it refused. ClickHouse puts the reason in the body, and a
    /// `Code: 60. DB::Exception: …` is the only useful thing about the failure, so it is carried
    /// rather than discarded.
    Server { status: u16, body: String },
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::Http(e) => write!(f, "clickhouse unreachable: {e}"),
            HistoryError::Server { status, body } => write!(f, "clickhouse {status}: {}", body.trim()),
        }
    }
}

impl std::error::Error for HistoryError {}

#[derive(Clone)]
pub struct History {
    url: String,
    user: String,
    password: String,
    client: reqwest::Client,
}

impl History {
    pub fn new(url: &str, user: &str, password: &str) -> History {
        History {
            url: url.trim_end_matches('/').to_string(),
            user: user.to_string(),
            password: password.to_string(),
            client: reqwest::Client::builder().timeout(TIMEOUT).build().unwrap_or_default(),
        }
    }

    /// `None` is a supported configuration, not a failure: see the module doc. The credentials come
    /// from the ClickStack chart's own ClickHouse Secret (`deploy/clickstack/README.md`).
    pub fn from_env() -> Option<History> {
        let url = std::env::var("RUSTIC_GIT_CLICKHOUSE_URL").ok().filter(|u| !u.is_empty())?;
        let user = std::env::var("RUSTIC_GIT_CLICKHOUSE_USER").unwrap_or_else(|_| "default".into());
        let password = std::env::var("RUSTIC_GIT_CLICKHOUSE_PASSWORD").unwrap_or_default();
        Some(History::new(&url, &user, &password))
    }

    /// One POST of `sql` as the body. Credentials go in headers rather than the query string so the
    /// password never lands in ClickHouse's own `query_log`.
    async fn post(&self, body: String) -> Result<String, HistoryError> {
        let r = self
            .client
            .post(&self.url)
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .body(body)
            .send()
            .await
            .map_err(|e| HistoryError::Http(e.to_string()))?;
        let status = r.status().as_u16();
        let body = r.text().await.map_err(|e| HistoryError::Http(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(HistoryError::Server { status, body });
        }
        Ok(body)
    }

    /// `INSERT INTO rustic.{table} FORMAT JSONEachRow` — one JSON object per line. The database is
    /// added here rather than by each caller: an unqualified insert would land in whatever database
    /// the connection defaulted to, which is the collector's. An empty batch is a no-op, so every
    /// beat can call this unconditionally.
    pub async fn insert(&self, table: &str, rows: &[serde_json::Value]) -> Result<(), HistoryError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut body = format!("INSERT INTO {DB}.{table} FORMAT JSONEachRow\n");
        for r in rows {
            body.push_str(&r.to_string());
            body.push('\n');
        }
        self.post(body).await.map(|_| ())
    }

    /// `{sql} FORMAT JSONCompact`, returning just the `data` rows. Compact because every caller
    /// wants positional values for a chart and the column names are already in the SQL. The SQL is
    /// passed through untouched — a series or alert query names `otel_metrics_sum` in the
    /// collector's database as readily as one of ours.
    pub async fn query(&self, sql: &str) -> Result<Vec<Vec<serde_json::Value>>, HistoryError> {
        let text = self.post(format!("{sql} FORMAT JSONCompact")).await?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| HistoryError::Server { status: 200, body: format!("{e}: {text}") })?;
        Ok(v.get("data")
            .and_then(|d| d.as_array())
            .map(|rows| rows.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect())
            .unwrap_or_default())
    }

    /// A cheap liveness probe for the boot path and the history routes' 503 decision.
    pub async fn healthy(&self) -> bool {
        self.query("SELECT 1").await.is_ok()
    }
}
```

- [ ] **Step 4: Write the schema**

Create `crates/workspaces/src/history/schema.rs`:

```rust
//! The `rustic` database, as numbered migrations the admin process applies at boot.
//!
//! `CREATE … IF NOT EXISTS` plus a recorded version, not a migration framework: a fresh ClickStack
//! becomes usable with no manual step, and an existing one skips what it already has. Never edit a
//! migration that has shipped — add the next number. The version is recorded only after the
//! statement returns, so a half-applied boot retries an idempotent statement rather than skipping
//! it.
//!
//! Migrations 8 and 9 read the COLLECTOR's tables. They are the one place our schema depends on
//! the exporter's, and they are written to fail loudly (a missing source table is a migration
//! error, logged at boot) rather than to silently produce an empty rollup — a chart that is flat
//! because a view never got built looks exactly like a chart that is flat because nothing happened.

use super::{History, HistoryError};

const DATABASE: &str = "CREATE DATABASE IF NOT EXISTS rustic";

/// The bookkeeping table. Applied before anything else and never numbered — it IS the numbering.
const BOOKKEEPING: &str = "CREATE TABLE IF NOT EXISTS rustic.schema_migrations \
    (version UInt32, applied_at DateTime DEFAULT now()) \
    ENGINE = ReplacingMergeTree ORDER BY version";

/// `(version, statement)`, ascending.
pub const MIGRATIONS: &[(u32, &str)] = &[
    // `events` is the record, so no TTL at all. ReplacingMergeTree on `id` is what makes
    // at-least-once safe: a replayed watch, a redelivered Redis entry and a retried insert all
    // collapse to one row, and every reader queries FINAL.
    (
        1,
        "CREATE TABLE IF NOT EXISTS rustic.events (\
            ts DateTime64(3), \
            id String, \
            kind LowCardinality(String), \
            actor String, \
            owner String, \
            target String, \
            region LowCardinality(String), \
            attrs String\
         ) ENGINE = ReplacingMergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (kind, ts, id)",
    ),
    // Recomputed from the CRDs every hour and never derived from an earlier row, so a plain
    // MergeTree is right: two beats in one hour are two honest observations, not a conflict.
    (
        2,
        "CREATE TABLE IF NOT EXISTS rustic.usage_hourly (\
            ts DateTime, \
            owner String, \
            is_team UInt8, \
            dimension LowCardinality(String), \
            used Float64, \
            `limit` Float64\
         ) ENGINE = MergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (owner, dimension, ts) \
           TTL ts + INTERVAL 730 DAY",
    ),
    (
        3,
        "CREATE TABLE IF NOT EXISTS rustic.fleet_hourly (\
            ts DateTime, \
            region LowCardinality(String), \
            nodes_total UInt32, \
            nodes_ready UInt32, \
            agents_ready UInt32, \
            live_workspaces UInt32, \
            live_environments UInt32, \
            snapshots UInt32, \
            disk_gb UInt64, \
            cpu UInt32, \
            memory_gb UInt32, \
            pool_used_bytes UInt64, \
            pool_total_bytes UInt64\
         ) ENGINE = MergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (region, ts) \
           TTL ts + INTERVAL 730 DAY",
    ),
    // Alerts are state TRANSITIONS, not one row per evaluation: the evaluator writes only when a
    // rule changes state, so the table stays small and "when did this start" is a plain lookup.
    // ReplacingMergeTree on `id` for the same at-least-once reason as `events`.
    (
        4,
        "CREATE TABLE IF NOT EXISTS rustic.alerts (\
            ts DateTime, \
            id String, \
            region LowCardinality(String), \
            rule LowCardinality(String), \
            state LowCardinality(String), \
            detail String\
         ) ENGINE = ReplacingMergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (region, rule, ts, id)",
    ),
    // The 5-minute rollup the long sparklines (30 d / 90 d) read. The exporter's own `ttl` drops
    // raw metrics at 30 days, so without this a 90-day chart has nothing to draw past a month.
    // `region` and `node` are lifted out of the attribute maps at write time, because every read
    // filters on them and a Map lookup per row over 400 days of data is the difference between a
    // chart and a timeout.
    (
        5,
        "CREATE TABLE IF NOT EXISTS rustic.metrics_5m (\
            ts DateTime, \
            region LowCardinality(String), \
            node String, \
            metric LowCardinality(String), \
            attributes String, \
            avg_value AggregateFunction(avg, Float64), \
            max_value AggregateFunction(max, Float64), \
            last_value AggregateFunction(argMax, Float64, DateTime)\
         ) ENGINE = AggregatingMergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (region, metric, node, attributes, ts) \
           TTL ts + INTERVAL 400 DAY",
    ),
    // One view per source table. `otel_metrics_gauge` and `otel_metrics_sum` are the exporter's
    // (columns TimeUnix, MetricName, Value, Attributes, ResourceAttributes — verified against the
    // clickhouseexporter README); histograms are deliberately NOT rolled up, since averaging a
    // bucket count is meaningless and no console series asks for one.
    (
        6,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS rustic.metrics_5m_gauge_mv TO rustic.metrics_5m AS \
         SELECT toStartOfFiveMinute(TimeUnix) AS ts, \
                ResourceAttributes['region'] AS region, \
                ResourceAttributes['k8s.node.name'] AS node, \
                MetricName AS metric, \
                toJSONString(Attributes) AS attributes, \
                avgState(Value) AS avg_value, \
                maxState(Value) AS max_value, \
                argMaxState(Value, TimeUnix) AS last_value \
         FROM default.otel_metrics_gauge \
         GROUP BY ts, region, node, metric, attributes",
    ),
    (
        7,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS rustic.metrics_5m_sum_mv TO rustic.metrics_5m AS \
         SELECT toStartOfFiveMinute(TimeUnix) AS ts, \
                ResourceAttributes['region'] AS region, \
                ResourceAttributes['k8s.node.name'] AS node, \
                MetricName AS metric, \
                toJSONString(Attributes) AS attributes, \
                avgState(Value) AS avg_value, \
                maxState(Value) AS max_value, \
                argMaxState(Value, TimeUnix) AS last_value \
         FROM default.otel_metrics_sum \
         GROUP BY ts, region, node, metric, attributes",
    ),
];

/// Applies every migration this server has not recorded yet. Returns how many ran, so boot logs
/// "0" on the common path instead of a wall of statements.
pub async fn migrate(h: &History) -> Result<u32, HistoryError> {
    h.query(DATABASE).await?;
    h.query(BOOKKEEPING).await?;
    let done: Vec<u32> = h
        .query("SELECT version FROM rustic.schema_migrations FINAL")
        .await?
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_u64()).map(|v| v as u32))
        .collect();
    let mut applied = 0;
    for (version, sql) in MIGRATIONS {
        if done.contains(version) {
            continue;
        }
        h.query(sql).await?;
        // Recorded only after the statement returned: a crash in between re-runs an idempotent
        // `CREATE … IF NOT EXISTS`, which is the safe direction to be wrong in.
        h.insert("schema_migrations", &[serde_json::json!({ "version": version })]).await?;
        applied += 1;
    }
    Ok(applied)
}
```

- [ ] **Step 5: Declare the module**

In `crates/workspaces/src/lib.rs`, alongside the existing `pub mod` lines:

```rust
pub mod history;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-workspaces --test history_client`
Expected: PASS (8 tests; the `#[ignore]`d real-ClickHouse test is reported as ignored).

Run: `cargo clippy -p rustic-git-workspaces --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Verify the exporter's columns against a live collector (documentation step)**

Once a ClickStack is reachable (Task 10 stands one up; a `docker run` collector works too), confirm migrations 6 and 7 match reality before trusting a chart built on them:

```bash
curl -s "$RUSTIC_GIT_CLICKHOUSE_URL" --data-binary "DESCRIBE TABLE default.otel_metrics_gauge FORMAT JSONCompact"
```

Expected: `TimeUnix`, `MetricName`, `Value`, `Attributes`, `ResourceAttributes` all present. If a column is named differently in the deployed exporter version, fix migrations 6 and 7 **as new migrations 8 and 9** (`DROP VIEW IF EXISTS` + `CREATE`), never by editing 6 and 7 in place.

- [ ] **Step 8: Commit**

```bash
git add crates/workspaces/src/history crates/workspaces/src/lib.rs crates/workspaces/tests/history_client.rs
git commit -m "Add a ClickHouse client and the rustic database schema"
```

---

## Task 2: Wire ClickHouse into the admin process

**Files:**
- Modify: `crates/workspaces/src/api/mod.rs`
- Modify: `crates/workspaces/src/api/admin.rs`
- Modify: `bins/api/src/main.rs`
- Test: `crates/workspaces/tests/history_state.rs`

**Interfaces:**
- Consumes: `History`, `schema::migrate` (Task 1).
- Produces:
  - `ApiState.history: Option<Arc<History>>`, `pub fn ApiState::with_history(self, h: Arc<History>) -> Self`
  - `pub(crate) fn crate::api::admin::history_or_503(s: &ApiState) -> Result<&History, Response>`

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/history_state.rs`:

```rust
//! `history` is optional state: a process without ClickStack must still build, route and answer —
//! the console renders a flat placeholder, never an error page.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::ApiState;
use rustic_git_workspaces::history::History;
use std::sync::Arc;

fn jwt() -> Arc<Jwt> {
    Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap())
}

#[test]
fn history_is_absent_by_default() {
    assert!(ApiState::new(jwt()).history.is_none());
}

#[test]
fn with_history_attaches_it() {
    let h = Arc::new(History::new("http://127.0.0.1:8123", "default", ""));
    assert!(ApiState::new(jwt()).with_history(h).history.is_some());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test history_state`
Expected: FAIL — `no field 'history' on type 'ApiState'`.

- [ ] **Step 3: Add the state field**

In `crates/workspaces/src/api/mod.rs`, add to `pub struct ApiState` after `peer`:

```rust
    /// ClickHouse (ClickStack's), holding the collector's `default` telemetry and our own `rustic`
    /// database. `None` when `RUSTIC_GIT_CLICKHOUSE_URL` is unset — a supported configuration, not
    /// a degraded one: history routes answer `503 history unavailable` and the console renders a
    /// flat placeholder. Only the ADMIN process ever sets this; the user role never constructs one,
    /// which is what makes "the admin process is the only writer of `rustic`" a fact about the
    /// binary rather than a convention.
    pub history: Option<Arc<crate::history::History>>,
```

Add `history: None,` to `ApiState::new`'s initializer, and the builder next to `with_peer`:

```rust
    pub fn with_history(mut self, history: Arc<crate::history::History>) -> Self {
        self.history = Some(history);
        self
    }
```

- [ ] **Step 4: Add the 503 gate**

In `crates/workspaces/src/api/admin.rs`, next to `require_note`:

```rust
/// The one place a history route turns "no ClickStack" into a response. 503 with this exact
/// sentence, which the web keys its flat-placeholder rendering off — a bare status code would be
/// indistinguishable from a real outage.
pub(crate) fn history_or_503(s: &ApiState) -> Result<&crate::history::History, Response> {
    s.history
        .as_deref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "history unavailable").into_response())
}
```

- [ ] **Step 5: Wire the admin boot**

In `bins/api/src/main.rs`, after the existing `if role == "admin"` block that calls `with_aks`:

```rust
            // ClickHouse is the admin process's alone (design §A1: it is the only writer of the
            // `rustic` database). Optional: an unset URL leaves `history` None, every
            // /admin/history route answers 503, and nothing is recorded — exactly how the
            // deployment behaved before ClickStack existed.
            if role == "admin" {
                match rustic_git_workspaces::history::History::from_env() {
                    Some(h) => {
                        // Migrations at boot, so a fresh ClickStack becomes usable with no manual
                        // step. A failure is LOGGED, not fatal: quota decisions and node drains
                        // must not be held hostage by an analytics store, and the next restart
                        // retries an idempotent set of statements.
                        match rustic_git_workspaces::history::schema::migrate(&h).await {
                            Ok(0) => tracing::info!("clickhouse schema up to date"),
                            Ok(n) => tracing::info!(applied = n, "clickhouse migrations applied"),
                            Err(e) => tracing::error!(error = %e, "clickhouse migrations failed; history will be incomplete until the next restart"),
                        }
                        state = state.with_history(Arc::new(h));
                    }
                    None => tracing::warn!(
                        "RUSTIC_GIT_CLICKHOUSE_URL unset: /admin/history answers 503 and nothing is recorded"
                    ),
                }
            }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-workspaces --test history_state && cargo build -p rustic-git-api`
Expected: PASS, and the binary builds.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/api/mod.rs crates/workspaces/src/api/admin.rs bins/api/src/main.rs crates/workspaces/tests/history_state.rs
git commit -m "Connect the admin process to ClickHouse and migrate at boot"
```

---

## Task 3: Dual-write every audit row as an event

**Files:**
- Create: `crates/workspaces/src/history/events.rs`
- Modify: `crates/workspaces/src/history/mod.rs`
- Modify: `crates/workspaces/src/api/admin.rs`
- Test: `crates/workspaces/tests/history_events.rs`

**Interfaces:**
- Consumes: `History::insert` (Task 1), `ApiState.history` (Task 2).
- Produces:
  - `pub struct EventRow { pub ts: chrono::DateTime<chrono::Utc>, pub id: String, pub kind: String, pub actor: String, pub owner: String, pub target: String, pub region: String, pub attrs: serde_json::Value }`
  - `pub fn EventRow::to_json(&self) -> serde_json::Value`
  - `pub async fn write_events(h: &History, rows: &[EventRow]) -> Result<(), HistoryError>`
  - `pub fn audit_event(ts: &str, actor: &str, action: &str, target: &str, result: &str) -> EventRow`

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/history_events.rs`:

```rust
//! The event row shape and the audit dual write. The object-store audit log stays the append-only
//! legal record; this is the queryable copy, and a failure to write the copy must never affect it.

use rustic_git_workspaces::history::events::{audit_event, EventRow};

#[test]
fn a_row_serializes_in_the_shape_the_events_table_takes() {
    let row = EventRow {
        ts: chrono::DateTime::parse_from_rfc3339("2026-09-04T10:11:12.345Z").unwrap().into(),
        id: "uid-1:4711:created".into(),
        kind: "workspace.created".into(),
        actor: "meera@example.com".into(),
        owner: "acme".into(),
        target: "ws-abc".into(),
        region: "westeurope-k3s".into(),
        attrs: serde_json::json!({"image": "alpine"}),
    };
    let v = row.to_json();
    // ClickHouse's DateTime64(3) over HTTP wants a space, not a `T`, and no zone suffix.
    assert_eq!(v["ts"], serde_json::json!("2026-09-04 10:11:12.345"));
    assert_eq!(v["kind"], serde_json::json!("workspace.created"));
    // `attrs` is a String column: the JSON goes in as text, not as a nested object.
    assert_eq!(v["attrs"], serde_json::json!(r#"{"image":"alpine"}"#));
}

/// The id is what makes at-least-once safe. Two writes of the same audit row must collapse.
#[test]
fn an_audit_event_id_is_deterministic() {
    let a = audit_event("2026-09-04T10:11:12Z", "root@example.com", "drain", "eu/node-1", "ok");
    let b = audit_event("2026-09-04T10:11:12Z", "root@example.com", "drain", "eu/node-1", "ok");
    assert_eq!(a.id, b.id);
    assert_eq!(a.kind, "admin.drain");
    assert_eq!(a.actor, "root@example.com");
    assert_eq!(a.target, "eu/node-1");
    assert_eq!(a.attrs["result"], serde_json::json!("ok"));
}

/// An audit entry that cannot be copied is still an audit entry: dropping it would make the
/// queryable copy silently incomplete, which is worse than a stamped-now timestamp.
#[test]
fn a_bad_timestamp_falls_back_to_now_rather_than_dropping_the_row() {
    let e = audit_event("not-a-timestamp", "root@example.com", "drain", "eu/node-1", "ok");
    assert_eq!(e.kind, "admin.drain");
    assert!(e.ts <= chrono::Utc::now());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test history_events`
Expected: FAIL — `unresolved import rustic_git_workspaces::history::events`.

- [ ] **Step 3: Write the event row**

Create `crates/workspaces/src/history/events.rs`:

```rust
//! `rustic.events` rows: the shape, the id scheme that makes at-least-once delivery safe, and the
//! writers that feed the table (the audit dual write here; the Redis consumer and the kube watches
//! in the tasks that follow).
//!
//! Every producer must compute the SAME id for the same fact, because `events` is a
//! ReplacingMergeTree on `id` and that is the entire deduplication story: a replayed watch, a
//! redelivered Redis entry and a retried insert all collapse to one row.

use super::{History, HistoryError};

/// The wire format ClickHouse's `DateTime64(3)` accepts over HTTP.
const TS_FMT: &str = "%Y-%m-%d %H:%M:%S%.3f";

#[derive(Debug, Clone)]
pub struct EventRow {
    pub ts: chrono::DateTime<chrono::Utc>,
    /// The dedupe key — deterministic, never random. See the module doc.
    pub id: String,
    pub kind: String,
    pub actor: String,
    pub owner: String,
    pub target: String,
    /// `central` for the admin process's own cluster; a region id otherwise.
    pub region: String,
    /// Free-form detail, stored as a JSON *string* (the column is `String`) so a new field costs no
    /// migration and a malformed value can never break the table.
    pub attrs: serde_json::Value,
}

impl EventRow {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ts": self.ts.format(TS_FMT).to_string(),
            "id": self.id,
            "kind": self.kind,
            "actor": self.actor,
            "owner": self.owner,
            "target": self.target,
            "region": self.region,
            "attrs": self.attrs.to_string(),
        })
    }
}

pub async fn write_events(h: &History, rows: &[EventRow]) -> Result<(), HistoryError> {
    let json: Vec<serde_json::Value> = rows.iter().map(EventRow::to_json).collect();
    h.insert("events", &json).await
}

/// One audit row as an event. `kind = "admin.<action>"` per the spec, and the id is the audit row's
/// own coordinates, so re-copying an entry is a no-op rather than a double count.
pub fn audit_event(ts: &str, actor: &str, action: &str, target: &str, result: &str) -> EventRow {
    EventRow {
        // A row whose timestamp will not parse is still worth keeping: stamped now rather than
        // dropped, since the object-store copy carries the original string regardless.
        ts: chrono::DateTime::parse_from_rfc3339(ts)
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now()),
        id: format!("audit:{ts}:{action}:{target}"),
        kind: format!("admin.{action}"),
        actor: actor.to_string(),
        // An admin action names the object acted on, not an owner of it — the Owner timeline reads
        // `target`, and inventing an owner here would attribute a node drain to somebody.
        owner: String::new(),
        target: target.to_string(),
        region: "central".to_string(),
        attrs: serde_json::json!({ "result": result }),
    }
}
```

In `crates/workspaces/src/history/mod.rs`, next to `pub mod schema;`:

```rust
pub mod events;
```

- [ ] **Step 4: Dual-write from the audit writer**

In `crates/workspaces/src/api/admin.rs`, at the end of `pub(crate) async fn audit`, after the `crate::audit::record` call (which borrows `entry`, so the binding is still live):

```rust
    // The queryable copy. The object-store row above is the append-only legal record and has
    // already been written; this is best-effort on purpose — a ClickHouse outage must not cost an
    // audit row — and the id is deterministic, so a retried write of the same entry collapses.
    if let Some(h) = s.history.as_deref() {
        let row = crate::history::events::audit_event(
            &entry.ts,
            &entry.actor,
            &entry.action,
            &entry.target,
            &entry.result,
        );
        if let Err(e) = crate::history::events::write_events(h, &[row]).await {
            tracing::warn!(error = %e, action, target, "audit event not copied to history");
        }
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-workspaces --test history_events && cargo clippy -p rustic-git-workspaces --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/history/events.rs crates/workspaces/src/history/mod.rs crates/workspaces/src/api/admin.rs crates/workspaces/tests/history_events.rs
git commit -m "Copy every admin audit row into the history events table"
```

---

## Task 4: The Redis `history` consumer group

**Files:**
- Modify: `crates/workspaces/src/history/events.rs` (add `stream_event`, `consume_forever`)
- Modify: `crates/workspaces/src/api/mod.rs` (`ApiState.cache`, `with_cache`)
- Modify: `bins/api/src/main.rs` (spawn the consumer in the admin role)
- Test: `crates/workspaces/tests/history_events.rs` (extend)

**Interfaces:**
- Consumes: `EventRow`, `write_events` (Task 3); `rustic_git_storage::cache::Cache` and its `xgroup_create_mkstream` / `xreadgroup` / `xautoclaim` / `xack`; `rustic_git_storage::events::from_fields`.
- Produces:
  - `pub fn stream_event(stream_id: &str, fields: &[(String, String)]) -> Option<EventRow>`
  - `pub async fn consume_forever(cache: Arc<rustic_git_storage::cache::Cache>, history: Arc<History>)`
  - `ApiState.cache: Option<Arc<rustic_git_storage::cache::Cache>>` and `with_cache`

- [ ] **Step 1: Write the failing test**

Append to `crates/workspaces/tests/history_events.rs`:

```rust
use rustic_git_workspaces::history::events::stream_event;

fn field(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

/// A PR event off the `events` stream becomes an event row. The stream entry id is the dedupe key:
/// Redis assigns it once, so a redelivered entry (XAUTOCLAIM after a crash) writes the same row.
#[test]
fn a_stream_entry_becomes_an_event_row_keyed_by_its_stream_id() {
    let fields = vec![
        field("kind", "pull_merged"),
        field("repo", "alice/web"),
        field("number", "7"),
        field("actor", "alice@example.com"),
        field("at_ms", "1788523872000"),
        field("title", "fix the thing"),
        field("base", "main"),
        field("head", "fix-it"),
    ];
    let e = stream_event("1788523872000-0", &fields).expect("a known kind must map");
    assert_eq!(e.id, "stream:1788523872000-0");
    assert_eq!(e.kind, "pull_merged");
    assert_eq!(e.owner, "alice");
    assert_eq!(e.target, "alice/web#7");
    assert_eq!(e.actor, "alice@example.com");
    assert_eq!(e.attrs["title"], serde_json::json!("fix the thing"));
    // The stream is a nudge about a repo, not about a region; `central` is where the record lives.
    assert_eq!(e.region, "central");
}

/// An unknown kind is skipped, never fatal — the same rule `storage::events::from_fields` follows.
/// A future producer must not be able to wedge this consumer.
#[test]
fn an_unknown_stream_kind_is_skipped() {
    let fields = vec![field("kind", "from_the_future"), field("repo", "a/b")];
    assert!(stream_event("1-0", &fields).is_none());
}

/// A repo with no owner segment must not panic or invent one.
#[test]
fn a_malformed_repo_yields_an_empty_owner_rather_than_a_panic() {
    let fields = vec![
        field("kind", "pull_opened"),
        field("repo", "noslash"),
        field("number", "1"),
        field("actor", "a@b.c"),
        field("at_ms", "0"),
    ];
    let e = stream_event("2-0", &fields).expect("a known kind must still map");
    assert_eq!(e.owner, "");
    assert_eq!(e.target, "noslash#1");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test history_events`
Expected: FAIL — `cannot find function stream_event`.

- [ ] **Step 3: Write the mapper and the consumer**

Append to `crates/workspaces/src/history/events.rs`:

```rust
use std::sync::Arc;

/// The one stream every repo's events multiplex onto (`rustic_git_storage::events`), and OUR
/// consumer group on it — separate from the merge worker's, so the two never steal each other's
/// entries and neither depends on the other running.
const STREAM: &str = "events";
const GROUP: &str = "history";
/// How long an entry may sit claimed-but-unacked before `XAUTOCLAIM` hands it back — the same
/// bound the merge worker uses, and for the same reason: long enough that a slow-but-alive
/// consumer is not fought over, short enough that a dead one does not strand a batch.
const CLAIM_STALE_AFTER_MS: u64 = 30_000;
const RECLAIM_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// `xreadgroup` never blocks (it shares a multiplexed connection), so this sleep is what paces the
/// loop and sets the worst-case delay between an entry landing and a row appearing.
const IDLE: std::time::Duration = std::time::Duration::from_secs(2);
const BATCH: usize = 64;

/// One `events`-stream entry as a row. `None` for an entry this build does not understand — an
/// unknown kind must be skipped, never fatal, exactly as `storage::events::from_fields` treats it.
///
/// The dedupe key is the Redis entry id, which Redis assigns once: a redelivered entry writes the
/// same row and the ReplacingMergeTree collapses it, which is what lets the consumer ack AFTER the
/// insert without ever double-counting.
pub fn stream_event(stream_id: &str, fields: &[(String, String)]) -> Option<EventRow> {
    let e = rustic_git_storage::events::from_fields(fields)?;
    // `owner/name` — a repo key that is not of that shape is a producer bug, not a reason to drop
    // the event: keep the row, leave the owner empty rather than inventing an attribution.
    let owner = e.repo.split_once('/').map(|(o, _)| o.to_string()).unwrap_or_default();
    Some(EventRow {
        ts: chrono::DateTime::from_timestamp_millis(e.at_ms).unwrap_or_else(chrono::Utc::now),
        id: format!("stream:{stream_id}"),
        kind: e.kind.as_str().to_string(),
        actor: e.actor,
        owner,
        target: format!("{}#{}", e.repo, e.number),
        // The stream carries repo events, which belong to the git tier, not to a workspace region.
        region: "central".to_string(),
        attrs: serde_json::json!({ "title": e.title, "base": e.base, "head": e.head }),
    })
}

/// The `history` consumer group: read, insert, THEN ack.
///
/// The ack order is the opposite of the merge worker's, deliberately. The worker acks first
/// because its work is idempotent at the destination and a redelivery would merge twice; here the
/// destination dedupes for us (ReplacingMergeTree on `id`), so acking only after a 200 means a
/// ClickHouse outage costs redelivery rather than a lost row.
///
/// The stream stays a nudge, never the record (CLAUDE.md): with Redis absent `xreadgroup` answers
/// empty and this loop simply idles — the kube watches and the hourly beats keep writing, and no
/// row anywhere depends on an entry having arrived.
pub async fn consume_forever(cache: Arc<rustic_git_storage::cache::Cache>, history: Arc<History>) {
    if !cache.connected() {
        // Loud once, at startup: "the activity feed stopped filling in" is much harder to diagnose
        // than a missing RUSTIC_GIT_REDIS_URL named in the logs.
        tracing::warn!("no Redis: the history consumer will idle; kube watches and hourly beats still write");
    }
    cache.xgroup_create_mkstream(STREAM, GROUP).await;
    // Random, not hostname-derived: two admin pods restarted into the same name would otherwise
    // share a consumer identity and XAUTOCLAIM could not tell a dead one from a live one.
    let me = format!("history-{:016x}", rand::random::<u64>());
    let mut last_claim = std::time::Instant::now();
    loop {
        let mut batch = if last_claim.elapsed() >= RECLAIM_EVERY {
            last_claim = std::time::Instant::now();
            cache.xautoclaim(STREAM, GROUP, &me, CLAIM_STALE_AFTER_MS, BATCH).await
        } else {
            Vec::new()
        };
        batch.extend(cache.xreadgroup(STREAM, GROUP, &me, BATCH).await);
        if batch.is_empty() {
            tokio::time::sleep(IDLE).await;
            continue;
        }
        let ids: Vec<String> = batch.iter().map(|(id, _)| id.clone()).collect();
        let rows: Vec<EventRow> =
            batch.iter().filter_map(|(id, fields)| stream_event(id, fields)).collect();
        match write_events(&history, &rows).await {
            // Acked only now. An entry whose kind we skipped is still acked: it will never become
            // a row on any build, so holding it would strand the PEL forever.
            Ok(()) => cache.xack(STREAM, GROUP, &ids).await,
            Err(e) => {
                tracing::warn!(error = %e, n = rows.len(), "history insert failed; leaving the batch unacked for redelivery");
                tokio::time::sleep(IDLE).await;
            }
        }
    }
}
```

- [ ] **Step 4: Carry the cache on `ApiState`**

In `crates/workspaces/src/api/mod.rs`, add to `ApiState` after `history`:

```rust
    /// Redis, for the `history` consumer group only — no request path reads it. `None` in dev and
    /// wherever `RUSTIC_GIT_REDIS_URL` is unset; the consumer then never spawns, which costs the
    /// activity feed its PR half and nothing else (CLAUDE.md: the stream is a nudge, never the
    /// record).
    pub cache: Option<Arc<rustic_git_storage::cache::Cache>>,
```

Add `cache: None,` to `ApiState::new`, and:

```rust
    pub fn with_cache(mut self, cache: Arc<rustic_git_storage::cache::Cache>) -> Self {
        self.cache = Some(cache);
        self
    }
```

- [ ] **Step 5: Spawn it from the admin boot**

In `bins/api/src/main.rs`, inside the admin-role block, right after `state = state.with_history(Arc::new(h));` — restructure that arm to keep the `Arc` so both uses share one handle:

```rust
                    Some(h) => {
                        match rustic_git_workspaces::history::schema::migrate(&h).await {
                            Ok(0) => tracing::info!("clickhouse schema up to date"),
                            Ok(n) => tracing::info!(applied = n, "clickhouse migrations applied"),
                            Err(e) => tracing::error!(error = %e, "clickhouse migrations failed; history will be incomplete until the next restart"),
                        }
                        let h = Arc::new(h);
                        // The second consumer group on the one `events` stream. Spawned only in
                        // the admin role, because it is the only writer of `rustic.events`.
                        let consumer_cache = Arc::new(cache.clone());
                        let consumer_history = h.clone();
                        tokio::spawn(async move {
                            rustic_git_workspaces::history::events::consume_forever(
                                consumer_cache,
                                consumer_history,
                            )
                            .await
                        });
                        state = state.with_cache(Arc::new(cache.clone())).with_history(h);
                    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-workspaces --test history_events && cargo build -p rustic-git-api`
Expected: PASS, and the binary builds.

Run: `cargo clippy -p rustic-git-workspaces -p rustic-git-api --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/history/events.rs crates/workspaces/src/api/mod.rs bins/api/src/main.rs crates/workspaces/tests/history_events.rs
git commit -m "Consume the events stream into history in a second consumer group"
```

---

## Task 5: Kubernetes watches → events, per region

**Files:**
- Create: `crates/workspaces/src/history/watch.rs`
- Modify: `crates/workspaces/src/history/mod.rs`
- Modify: `bins/api/src/main.rs`
- Test: `crates/workspaces/tests/history_watch.rs`

**Interfaces:**
- Consumes: `EventRow`, `write_events` (Task 3); `crd::{Workspace, Environment, Snapshot, Volume, QuotaRequest, Region, Phase}`; `kube::runtime::watcher`.
- Produces:
  - `pub trait Watched: kube::Resource { fn observed(&self) -> Observed; }` — no; instead, three free functions, each a pure mapper with no kube types in its signature beyond the CR:
    - `pub fn workspace_events(prev: Option<&crd::Workspace>, next: &crd::Workspace, region: &str) -> Vec<EventRow>`
    - `pub fn environment_events(prev: Option<&crd::Environment>, next: &crd::Environment, region: &str) -> Vec<EventRow>`
    - `pub fn snapshot_events(prev: Option<&crd::Snapshot>, next: &crd::Snapshot, region: &str) -> Vec<EventRow>`
    - `pub fn volume_events(prev: Option<&crd::Volume>, next: &crd::Volume, region: &str) -> Vec<EventRow>`
    - `pub fn quota_request_events(prev: Option<&crd::QuotaRequest>, next: &crd::QuotaRequest, region: &str) -> Vec<EventRow>`
    - `pub fn region_events(prev: Option<&crd::Region>, next: &crd::Region) -> Vec<EventRow>`
    - `pub fn node_events(prev: Option<&k8s_openapi::api::core::v1::Node>, next: &k8s_openapi::api::core::v1::Node, region: &str) -> Vec<EventRow>`
  - `pub fn event_id(uid: &str, resource_version: &str, transition: &str) -> String`
  - `pub fn deleted_event<K: kube::api::Resource<DynamicType = ()>>(obj: &K, kind: &str, owner: &str, region: &str) -> EventRow`
  - `pub async fn watch_region(client: kube::Client, region: String, history: Arc<History>)` — runs every watcher for one cluster, forever.

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/history_watch.rs`:

```rust
//! Watch transitions → event rows. These mappers are the whole value: the watcher plumbing around
//! them is kube-rs's. The property that matters is idempotence — a restart replays the watch, and
//! every row it re-emits must carry the id it carried the first time.

use k8s_openapi::api::core::v1::{Node, NodeCondition, NodeStatus};
use kube::api::ObjectMeta;
use rustic_git_workspaces::crd::{self, Phase};
use rustic_git_workspaces::history::watch::{event_id, node_events, snapshot_events, workspace_events};

fn meta(name: &str, uid: &str, rv: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.into()),
        uid: Some(uid.into()),
        resource_version: Some(rv.into()),
        ..Default::default()
    }
}

fn ws(uid: &str, rv: &str, phase: Phase) -> crd::Workspace {
    let mut w = crd::Workspace::new("ws-abc", crd::WorkspaceSpec { owner: "acme".into(), ..Default::default() });
    w.metadata = meta("ws-abc", uid, rv);
    w.status = Some(crd::WorkspaceStatus { phase, ..Default::default() });
    w
}

#[test]
fn the_id_is_uid_resource_version_and_transition() {
    assert_eq!(event_id("uid-1", "4711", "created"), "uid-1:4711:created");
}

/// First sight of an object is `created` — a fresh watch has no previous state, and the id makes
/// the replay after a restart collapse onto the same row rather than double-counting.
#[test]
fn first_sight_of_a_workspace_is_created() {
    let rows = workspace_events(None, &ws("uid-1", "1", Phase::Pending), "westeurope-k3s");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "workspace.created");
    assert_eq!(rows[0].id, "uid-1:1:created");
    // spec.owner is truth — never a label.
    assert_eq!(rows[0].owner, "acme");
    assert_eq!(rows[0].target, "ws-abc");
    assert_eq!(rows[0].region, "westeurope-k3s");
}

#[test]
fn a_phase_change_into_ready_is_started_and_into_stopped_is_stopped() {
    let before = ws("uid-1", "1", Phase::Creating);
    let started = workspace_events(Some(&before), &ws("uid-1", "2", Phase::Ready), "eu");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].kind, "workspace.started");
    assert_eq!(started[0].id, "uid-1:2:started");

    let running = ws("uid-1", "2", Phase::Ready);
    let stopped = workspace_events(Some(&running), &ws("uid-1", "3", Phase::Stopped), "eu");
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].kind, "workspace.stopped");
}

/// A reconcile that rewrites status without changing the phase is the overwhelmingly common event.
/// It must produce nothing, or the table fills with noise and every timeline becomes unreadable.
#[test]
fn an_unchanged_phase_produces_no_event() {
    let before = ws("uid-1", "2", Phase::Ready);
    assert!(workspace_events(Some(&before), &ws("uid-1", "3", Phase::Ready), "eu").is_empty());
}

#[test]
fn a_snapshot_becoming_ready_is_one_event() {
    let mut before = crd::Snapshot::new("snap-1", crd::SnapshotSpec { volume: "vol-1".into(), owner: "acme".into(), ..Default::default() });
    before.metadata = meta("snap-1", "uid-s", "1");
    before.status = Some(crd::SnapshotStatus { phase: Phase::Working, ready_at: None });
    let mut after = before.clone();
    after.metadata.resource_version = Some("2".into());
    after.status = Some(crd::SnapshotStatus { phase: Phase::Ready, ready_at: Some("2026-09-04T10:00:00Z".into()) });

    let rows = snapshot_events(Some(&before), &after, "eu");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "snapshot.ready");
    assert_eq!(rows[0].owner, "acme");
}

fn node(name: &str, uid: &str, rv: &str, ready: &str, unschedulable: bool) -> Node {
    Node {
        metadata: meta(name, uid, rv),
        spec: Some(k8s_openapi::api::core::v1::NodeSpec { unschedulable: Some(unschedulable), ..Default::default() }),
        status: Some(NodeStatus {
            conditions: Some(vec![NodeCondition {
                type_: "Ready".into(),
                status: ready.into(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
    }
}

#[test]
fn a_node_going_notready_and_being_cordoned_are_separate_events() {
    let before = node("node-1", "uid-n", "1", "True", false);
    let rows = node_events(Some(&before), &node("node-1", "uid-n", "2", "False", false), "eu");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "node.notready");

    let rows = node_events(Some(&before), &node("node-1", "uid-n", "3", "True", true), "eu");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "node.cordoned");
    // A node belongs to a cluster, not to an owner: inventing one would attribute it to somebody.
    assert_eq!(rows[0].owner, "");
}

/// Both at once must produce both, not whichever the mapper checked first.
#[test]
fn a_node_that_goes_notready_and_cordoned_at_once_produces_both() {
    let before = node("node-1", "uid-n", "1", "True", false);
    let rows = node_events(Some(&before), &node("node-1", "uid-n", "2", "False", true), "eu");
    let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
    assert!(kinds.contains(&"node.notready") && kinds.contains(&"node.cordoned"), "{kinds:?}");
    // Two transitions off one resourceVersion still need distinct ids.
    assert_ne!(rows[0].id, rows[1].id);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test history_watch`
Expected: FAIL — `unresolved import rustic_git_workspaces::history::watch`.

- [ ] **Step 3: Write the mappers**

Create `crates/workspaces/src/history/watch.rs`:

```rust
//! Kubernetes watches turned into `rustic.events` rows: one reflector set per region, plus one for
//! central, in the admin process.
//!
//! Every mapper here is PURE — previous state, next state, out come rows — so the transitions are
//! unit-testable without a cluster, and the watcher plumbing below carries no rules of its own.
//!
//! Idempotence is the whole trick. A restart re-lists every object and a watch bookmark can replay
//! entries, so the id is `{uid}:{resourceVersion}:{transition}`: the same fact observed twice is
//! literally the same row, and `events`' ReplacingMergeTree collapses it. That is why NOTHING here
//! may put a wall-clock timestamp or a random value in the id.
//!
//! `spec.owner` is truth (CLAUDE.md): every `owner` field below reads the spec, never the
//! `rustic-git.io/owner` label, which is a view maintained for label selectors.

use super::{events::EventRow, History};
use crate::crd::{self, Phase};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::api::ResourceExt;
use std::collections::HashMap;
use std::sync::Arc;

pub fn event_id(uid: &str, resource_version: &str, transition: &str) -> String {
    format!("{uid}:{resource_version}:{transition}")
}

/// The one row constructor every mapper goes through, so no mapper can forget the id scheme.
/// `ts` is `now`: the API server gives no transition timestamp we could trust across objects, and
/// the id — not the timestamp — is what makes a replay idempotent.
fn row(
    uid: &str,
    resource_version: &str,
    transition: &str,
    kind: &str,
    owner: &str,
    target: &str,
    region: &str,
    attrs: serde_json::Value,
) -> EventRow {
    EventRow {
        ts: chrono::Utc::now(),
        id: event_id(uid, resource_version, transition),
        kind: kind.to_string(),
        actor: String::new(), // a controller transition has no human actor; audit events carry those
        owner: owner.to_string(),
        target: target.to_string(),
        region: region.to_string(),
        attrs,
    }
}

/// `Pending`/`Creating` → `Ready`/`Running` is a start; anything → `Stopped` is a stop. Everything
/// else is a status rewrite, and a reconcile does hundreds of those — emitting them would bury the
/// timeline in noise.
fn phase_transition(prev: Option<Phase>, next: Phase) -> Option<&'static str> {
    let started = matches!(next, Phase::Ready | Phase::Running);
    let was_started = matches!(prev, Some(Phase::Ready) | Some(Phase::Running));
    match (prev, next) {
        (None, _) => None, // first sight is `created`, handled by the caller
        (Some(p), n) if p == n => None,
        (_, Phase::Stopped) => Some("stopped"),
        _ if started && !was_started => Some("started"),
        _ => None,
    }
}

/// The two rows every parent kind produces: `created` on first sight, then phase transitions.
/// Factored because `Workspace` and `Environment` differ only in the kind word and the field paths.
fn parent_rows(
    uid: &str,
    rv: &str,
    kind_prefix: &str,
    owner: &str,
    target: &str,
    region: &str,
    prev_phase: Option<Phase>,
    next_phase: Phase,
    first_sight: bool,
    attrs: serde_json::Value,
) -> Vec<EventRow> {
    let mut out = Vec::new();
    if first_sight {
        out.push(row(uid, rv, "created", &format!("{kind_prefix}.created"), owner, target, region, attrs.clone()));
        return out;
    }
    if let Some(t) = phase_transition(prev_phase, next_phase) {
        out.push(row(uid, rv, t, &format!("{kind_prefix}.{t}"), owner, target, region, attrs));
    }
    out
}

fn uid_rv<K: ResourceExt>(o: &K) -> (String, String) {
    (o.uid().unwrap_or_default(), o.resource_version().unwrap_or_default())
}

pub fn workspace_events(
    prev: Option<&crd::Workspace>,
    next: &crd::Workspace,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    parent_rows(
        &uid,
        &rv,
        "workspace",
        &next.spec.owner,
        &next.name_any(),
        region,
        prev.and_then(|p| p.status.as_ref()).map(|s| s.phase),
        next.status.as_ref().map(|s| s.phase).unwrap_or_default(),
        prev.is_none(),
        serde_json::json!({ "image": next.spec.image }),
    )
}

pub fn environment_events(
    prev: Option<&crd::Environment>,
    next: &crd::Environment,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    parent_rows(
        &uid,
        &rv,
        "environment",
        &next.spec.owner,
        &next.name_any(),
        region,
        prev.and_then(|p| p.status.as_ref()).map(|s| s.phase),
        next.status.as_ref().map(|s| s.phase).unwrap_or_default(),
        prev.is_none(),
        serde_json::json!({ "services": next.spec.services.len() }),
    )
}

/// A snapshot's only interesting transition is becoming `Ready` — that is the instant its bytes
/// exist and it becomes a restore target. `created` is not emitted: a sync-point cut every beat
/// would drown every other event in the table.
pub fn snapshot_events(
    prev: Option<&crd::Snapshot>,
    next: &crd::Snapshot,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let was_ready = matches!(prev.and_then(|p| p.status.as_ref()).map(|s| s.phase), Some(Phase::Ready));
    let is_ready = matches!(next.status.as_ref().map(|s| s.phase), Some(Phase::Ready));
    if is_ready && !was_ready {
        return vec![row(
            &uid,
            &rv,
            "ready",
            "snapshot.ready",
            &next.spec.owner,
            &next.name_any(),
            region,
            serde_json::json!({ "volume": next.spec.volume, "transient": next.spec.transient }),
        )];
    }
    Vec::new()
}

/// A volume's events are the ones an operator investigates an incident with: it moved node, it was
/// released, or it went `Unavailable` because its node died.
pub fn volume_events(prev: Option<&crd::Volume>, next: &crd::Volume, region: &str) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let mut out = Vec::new();
    let prev_node = prev.and_then(|p| p.status.as_ref()).map(|s| s.node_name.clone()).unwrap_or_default();
    let next_node = next.status.as_ref().map(|s| s.node_name.clone()).unwrap_or_default();
    if prev.is_some() && prev_node != next_node {
        // Losing the pin and gaining one are different facts to an operator reading a timeline.
        let (t, kind) = if next_node.is_empty() { ("released", "volume.released") } else { ("moved", "volume.moved") };
        out.push(row(&uid, &rv, t, kind, &next.spec.owner, &next.name_any(), region,
            serde_json::json!({ "from": prev_node, "to": next_node })));
    }
    let was = prev.and_then(|p| p.status.as_ref()).map(|s| s.phase);
    let is = next.status.as_ref().map(|s| s.phase).unwrap_or_default();
    if is == Phase::Unavailable && was != Some(Phase::Unavailable) {
        out.push(row(&uid, &rv, "unavailable", "volume.unavailable", &next.spec.owner,
            &next.name_any(), region, serde_json::json!({})));
    }
    out
}

/// `QuotaRequest` only, deliberately. Sub-project B introduces the generic `Request` CRD; this
/// mapper emits the same `request.*` kinds so the console's timeline needs no change when B lands.
// ponytail: when `Request` ships, add a sibling mapper rather than generalising this one — the two
// CRDs coexist until the one-shot migration retires `QuotaRequest`, and a union type here would
// have to be unpicked again.
pub fn quota_request_events(
    prev: Option<&crd::QuotaRequest>,
    next: &crd::QuotaRequest,
    region: &str,
) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let state = next.status.as_ref().map(|s| s.state.clone()).unwrap_or_else(|| "pending".into());
    let prev_state = prev.and_then(|p| p.status.as_ref()).map(|s| s.state.clone());
    if prev.is_none() {
        return vec![row(&uid, &rv, "opened", "request.opened", &next.spec.owner, &next.name_any(),
            region, serde_json::json!({ "requestedBy": next.spec.requested_by }))];
    }
    if prev_state.as_deref() != Some(state.as_str()) && (state == "approved" || state == "denied") {
        return vec![row(&uid, &rv, &state, &format!("request.{state}"), &next.spec.owner,
            &next.name_any(), region,
            serde_json::json!({ "decidedBy": next.status.as_ref().and_then(|s| s.decided_by.clone()) }))];
    }
    Vec::new()
}

/// Regions live in the central cluster and belong to no owner. `status` here is the region's own
/// `spec.status` (`active`/`inactive`), which `/v1/regions` writes — a spec field, so it is truth.
pub fn region_events(prev: Option<&crd::Region>, next: &crd::Region) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let was_active = prev.map(|p| p.spec.status == "active");
    let is_active = next.spec.status == "active";
    match was_active {
        Some(w) if w == is_active => Vec::new(),
        _ if is_active => vec![row(&uid, &rv, "activated", "region.activated", "", &next.name_any(), &next.name_any(), serde_json::json!({}))],
        _ => vec![row(&uid, &rv, "deactivated", "region.deactivated", "", &next.name_any(), &next.name_any(), serde_json::json!({}))],
    }
}

fn ready_condition(n: &Node) -> Option<String> {
    n.status.as_ref()?.conditions.as_ref()?.iter().find(|c| c.type_ == "Ready").map(|c| c.status.clone())
}

/// Ready/NotReady, cordon and the two decommission stamps — the four things that explain why work
/// stopped landing on a node. Both may change in one update, so this returns a Vec and every
/// transition gets its own id suffix.
pub fn node_events(prev: Option<&Node>, next: &Node, region: &str) -> Vec<EventRow> {
    let (uid, rv) = uid_rv(next);
    let mut out = Vec::new();
    let was_ready = prev.and_then(ready_condition);
    let is_ready = ready_condition(next);
    if prev.is_some() && was_ready != is_ready {
        let t = if is_ready.as_deref() == Some("True") { "ready" } else { "notready" };
        out.push(row(&uid, &rv, t, &format!("node.{t}"), "", &next.name_any(), region,
            serde_json::json!({ "ready": is_ready })));
    }
    let was_cordoned = prev.and_then(|p| p.spec.as_ref()).and_then(|s| s.unschedulable).unwrap_or(false);
    let is_cordoned = next.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false);
    if prev.is_some() && was_cordoned != is_cordoned && is_cordoned {
        out.push(row(&uid, &rv, "cordoned", "node.cordoned", "", &next.name_any(), region, serde_json::json!({})));
    }
    // The agent stamps its own progress here (`draining running=… ` then `drained <RFC3339>`);
    // the first word is the state, and only the two we name are events.
    let status_word = |n: &Node| {
        n.labels()
            .get("rustic-git.io/decommission-status")
            .or_else(|| n.annotations().get("rustic-git.io/decommission-status"))
            .and_then(|v| v.split_whitespace().next())
            .map(str::to_string)
    };
    let (was, is) = (prev.and_then(status_word), status_word(next));
    if was != is {
        if let Some(w) = is.as_deref().filter(|w| *w == "draining" || *w == "drained") {
            out.push(row(&uid, &rv, w, &format!("node.{w}"), "", &next.name_any(), region, serde_json::json!({})));
        }
    }
    out
}
```

- [ ] **Step 4: Write the watcher plumbing**

Append to `crates/workspaces/src/history/watch.rs`:

```rust
/// One reflector-shaped loop per kind: keep the previous version of each object by uid, hand both
/// to the mapper, write whatever comes out.
///
/// A HashMap of previous state, not `kube::runtime::reflector`: the mappers need the PREVIOUS
/// object and a Store gives the current one. The map is bounded by the number of objects in the
/// cluster (the same bound the Store has) and is dropped whenever the watcher restarts, which
/// re-lists everything — hence the `created` rows on restart, which the id scheme collapses.
async fn watch_kind<K>(client: kube::Client, region: String, history: Arc<History>, map: fn(Option<&K>, &K, &str) -> Vec<EventRow>)
where
    K: kube::Resource<Scope = kube::core::ClusterResourceScope, DynamicType = ()>
        + Clone + std::fmt::Debug + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let api = kube::Api::<K>::all(client);
    loop {
        let mut prev: HashMap<String, K> = HashMap::new();
        let mut stream = kube::runtime::watcher(api.clone(), kube::runtime::watcher::Config::default()).boxed();
        while let Some(ev) = stream.next().await {
            let obj = match ev {
                Ok(kube::runtime::watcher::Event::Apply(o)) => o,
                Ok(kube::runtime::watcher::Event::InitApply(o)) => o,
                // A delete drops the previous state so a recreated object with a new uid is a fresh
                // `created` rather than a phantom transition. Deletion events themselves come from
                // `/v1`'s audit rows, which name the actor — a watch cannot.
                Ok(kube::runtime::watcher::Event::Delete(o)) => {
                    if let Some(uid) = o.meta().uid.clone() {
                        prev.remove(&uid);
                    }
                    continue;
                }
                Ok(_) => continue,
                Err(e) => {
                    // Never fatal: the loop re-establishes the watch, and the ids make the re-list
                    // idempotent. Logging every blip at warn would be its own noise source.
                    tracing::debug!(error = %e, %region, "history watch interrupted; restarting");
                    break;
                }
            };
            let Some(uid) = obj.meta().uid.clone() else { continue };
            let rows = map(prev.get(&uid), &obj, &region);
            prev.insert(uid, obj);
            if rows.is_empty() {
                continue;
            }
            if let Err(e) = super::events::write_events(&history, &rows).await {
                tracing::warn!(error = %e, %region, n = rows.len(), "history watch rows not written");
            }
        }
        // A watcher that ended restarts on the next tick rather than spinning on a broken cluster.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Every watch for one cluster. Spawned once per region plus once for central (`region: "central"`,
/// where only `Region` lives).
pub async fn watch_region(client: kube::Client, region: String, history: Arc<History>) {
    let tasks = vec![
        tokio::spawn(watch_kind::<crd::Workspace>(client.clone(), region.clone(), history.clone(), workspace_events)),
        tokio::spawn(watch_kind::<crd::Environment>(client.clone(), region.clone(), history.clone(), environment_events)),
        tokio::spawn(watch_kind::<crd::Snapshot>(client.clone(), region.clone(), history.clone(), snapshot_events)),
        tokio::spawn(watch_kind::<crd::Volume>(client.clone(), region.clone(), history.clone(), volume_events)),
        tokio::spawn(watch_kind::<crd::QuotaRequest>(client.clone(), region.clone(), history.clone(), quota_request_events)),
        tokio::spawn(watch_kind::<Node>(client.clone(), region.clone(), history.clone(), node_events)),
        tokio::spawn(watch_kind::<crd::Region>(client, region, history, |p, n, _| region_events(p, n))),
    ];
    // Each task loops forever; awaiting them all only ends if the process does.
    for t in tasks {
        let _ = t.await;
    }
}
```

In `crates/workspaces/src/history/mod.rs`:

```rust
pub mod watch;
```

- [ ] **Step 5: Spawn the watches from the admin boot**

In `bins/api/src/main.rs`, in the admin arm right after the consumer spawn:

```rust
                        // One watch set per cluster the admin process can reach. `state.kube` is a
                        // region's k3s (the mounted kubeconfig) and `state.aks` is this cluster,
                        // where `Region` objects live — the same split every admin handler already
                        // makes, reused rather than a second client to keep in step.
                        if let Some(k) = state.kube.clone() {
                            let region = std::env::var("RUSTIC_GIT_REGION").unwrap_or_else(|_| "default".into());
                            let h = h.clone();
                            tokio::spawn(async move {
                                rustic_git_workspaces::history::watch::watch_region(k, region, h).await
                            });
                        }
                        if let Some(k) = state.aks.clone() {
                            let h = h.clone();
                            tokio::spawn(async move {
                                rustic_git_workspaces::history::watch::watch_region(k, "central".into(), h).await
                            });
                        }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-workspaces --test history_watch && cargo build -p rustic-git-api`
Expected: PASS, and the binary builds.

Run: `cargo clippy -p rustic-git-workspaces -p rustic-git-api --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/history/watch.rs crates/workspaces/src/history/mod.rs bins/api/src/main.rs crates/workspaces/tests/history_watch.rs
git commit -m "Turn Kubernetes watch transitions into history events"
```

---

## Task 6: The hourly usage and fleet beats

**Files:**
- Create: `crates/workspaces/src/history/beats.rs`
- Modify: `crates/workspaces/src/history/mod.rs`
- Modify: `crates/workspaces/src/api/admin/owners.rs` (`OwnerRow` fields `pub(crate)` — already are; add `pub(crate) use`)
- Modify: `crates/workspaces/src/api/admin/clusters.rs` (make `ClusterRow`'s private fields `pub(crate)`)
- Modify: `bins/api/src/main.rs`
- Test: `crates/workspaces/tests/history_beats.rs`

**Interfaces:**
- Consumes: `History::insert` (Task 1); `admin::owners::{owner_rows, OwnerRow}`; `admin::clusters::{cluster_rows, ClusterRow}`; `quota::{Usage, Dim}`.
- Produces:
  - `pub fn usage_rows(ts: chrono::DateTime<chrono::Utc>, owners: &[UsageInput]) -> Vec<serde_json::Value>`
  - `pub struct UsageInput { pub owner: String, pub is_team: bool, pub used: crate::quota::Usage, pub limit: crate::crd::QuotaSpec }`
  - `pub struct FleetInput { pub region: String, pub nodes_total: u32, pub nodes_ready: u32, pub agents_ready: u32, pub live_workspaces: u32, pub live_environments: u32, pub snapshots: u32, pub disk_gb: u64, pub cpu: u32, pub memory_gb: u32, pub pool_used_bytes: u64, pub pool_total_bytes: u64 }`
  - `pub fn fleet_rows(ts: chrono::DateTime<chrono::Utc>, fleet: &[FleetInput]) -> Vec<serde_json::Value>`
  - `pub async fn run_beats(state: Arc<crate::api::ApiState>)` — the hourly loop.

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/history_beats.rs`:

```rust
//! The hourly folds. Usage is recomputed from the CRDs on every run and never derived from an
//! earlier row (CLAUDE.md), so what is tested here is the ROW SHAPE — six dimensions per owner,
//! every one carrying both the used value and the limit it was measured against.

use rustic_git_workspaces::crd::QuotaSpec;
use rustic_git_workspaces::history::beats::{fleet_rows, usage_rows, FleetInput, UsageInput};
use rustic_git_workspaces::quota::Usage;

fn ts() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-09-04T10:00:00Z").unwrap().into()
}

#[test]
fn one_row_per_owner_per_dimension_with_its_limit() {
    let rows = usage_rows(
        ts(),
        &[UsageInput {
            owner: "acme".into(),
            is_team: true,
            used: Usage { workspaces: 3, environments: 1, snapshots: 9, disk_gb: 120, cpu: 6, memory_gb: 24 },
            limit: QuotaSpec { workspaces: 10, environments: 4, snapshots: 50, disk_gb: 500, cpu: 16, memory_gb: 64 },
        }],
    );
    assert_eq!(rows.len(), 6, "six dimensions, one row each");
    assert_eq!(rows[0]["ts"], serde_json::json!("2026-09-04 10:00:00"));
    let ws = rows.iter().find(|r| r["dimension"] == "workspaces").unwrap();
    assert_eq!(ws["owner"], serde_json::json!("acme"));
    // A team is `1`, a person `0`: the column is UInt8, not a Bool.
    assert_eq!(ws["is_team"], serde_json::json!(1));
    assert_eq!(ws["used"], serde_json::json!(3.0));
    assert_eq!(ws["limit"], serde_json::json!(10.0));
    // The dimension words are `Dim::word`'s, which the 409 message and the request form already
    // key off — a second vocabulary here would silently split every chart in two.
    let mut dims: Vec<&str> = rows.iter().map(|r| r["dimension"].as_str().unwrap()).collect();
    dims.sort_unstable();
    assert_eq!(dims, ["cpu", "disk_gb", "environments", "memory_gb", "snapshots", "workspaces"]);
}

#[test]
fn a_fleet_row_carries_every_column_the_table_declares() {
    let rows = fleet_rows(
        ts(),
        &[FleetInput {
            region: "westeurope-k3s".into(),
            nodes_total: 3, nodes_ready: 2, agents_ready: 2,
            live_workspaces: 7, live_environments: 2, snapshots: 41,
            disk_gb: 900, cpu: 24, memory_gb: 96,
            pool_used_bytes: 500_000_000_000, pool_total_bytes: 1_000_000_000_000,
        }],
    );
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    for col in ["ts", "region", "nodes_total", "nodes_ready", "agents_ready", "live_workspaces",
                "live_environments", "snapshots", "disk_gb", "cpu", "memory_gb",
                "pool_used_bytes", "pool_total_bytes"] {
        assert!(r.get(col).is_some(), "fleet row is missing {col}");
    }
    assert_eq!(r["nodes_ready"], serde_json::json!(2));
    assert_eq!(r["pool_total_bytes"], serde_json::json!(1_000_000_000_000u64));
}

/// No owners is a legitimate hour (a brand-new cluster), and it must produce no rows rather than
/// a row of zeros that a chart would draw as a cliff.
#[test]
fn an_empty_fold_writes_nothing() {
    assert!(usage_rows(ts(), &[]).is_empty());
    assert!(fleet_rows(ts(), &[]).is_empty());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test history_beats`
Expected: FAIL — `unresolved import rustic_git_workspaces::history::beats`.

- [ ] **Step 3: Write the beats**

Create `crates/workspaces/src/history/beats.rs`:

```rust
//! The hourly folds: one `usage_hourly` row per owner per dimension, one `fleet_hourly` row per
//! region.
//!
//! BOTH RECOMPUTE FROM THE CRDs EVERY RUN. Nothing here reads an earlier row and adds to it — a
//! stored counter can only be wrong in the direction that hands out allocation nobody has
//! (CLAUDE.md), and the same argument applies to a chart that would then show it.
//!
//! The row builders are pure and the loop around them is thin, so the shape is testable without a
//! cluster; `run_beats` is the only part that needs one.

use super::History;
use crate::crd::QuotaSpec;
use crate::quota::{Dim, Usage};
use std::sync::Arc;

/// ClickHouse `DateTime` over HTTP: seconds, space-separated, no zone.
const TS_FMT: &str = "%Y-%m-%d %H:%M:%S";

const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);

pub struct UsageInput {
    pub owner: String,
    pub is_team: bool,
    pub used: Usage,
    pub limit: QuotaSpec,
}

pub struct FleetInput {
    pub region: String,
    pub nodes_total: u32,
    pub nodes_ready: u32,
    pub agents_ready: u32,
    pub live_workspaces: u32,
    pub live_environments: u32,
    pub snapshots: u32,
    pub disk_gb: u64,
    pub cpu: u32,
    pub memory_gb: u32,
    pub pool_used_bytes: u64,
    pub pool_total_bytes: u64,
}

/// `(dimension word, used, limit)` for one owner. The words are `Dim::word`'s, which the 409
/// message and the request form already use — a second vocabulary here would split every chart.
fn dimensions(u: &UsageInput) -> [(&'static str, f64, f64); 6] {
    [
        (Dim::Workspaces.word(), u.used.workspaces as f64, u.limit.workspaces as f64),
        (Dim::Environments.word(), u.used.environments as f64, u.limit.environments as f64),
        (Dim::Snapshots.word(), u.used.snapshots as f64, u.limit.snapshots as f64),
        (Dim::DiskGb.word(), u.used.disk_gb as f64, u.limit.disk_gb as f64),
        (Dim::Cpu.word(), u.used.cpu as f64, u.limit.cpu as f64),
        (Dim::MemoryGb.word(), u.used.memory_gb as f64, u.limit.memory_gb as f64),
    ]
}

pub fn usage_rows(ts: chrono::DateTime<chrono::Utc>, owners: &[UsageInput]) -> Vec<serde_json::Value> {
    let ts = ts.format(TS_FMT).to_string();
    owners
        .iter()
        .flat_map(|u| {
            let (owner, is_team, ts) = (u.owner.clone(), u8::from(u.is_team), ts.clone());
            dimensions(u).into_iter().map(move |(dimension, used, limit)| {
                serde_json::json!({
                    "ts": ts, "owner": owner, "is_team": is_team,
                    "dimension": dimension, "used": used, "limit": limit,
                })
            })
        })
        .collect()
}

pub fn fleet_rows(ts: chrono::DateTime<chrono::Utc>, fleet: &[FleetInput]) -> Vec<serde_json::Value> {
    let ts = ts.format(TS_FMT).to_string();
    fleet
        .iter()
        .map(|f| {
            serde_json::json!({
                "ts": ts, "region": f.region,
                "nodes_total": f.nodes_total, "nodes_ready": f.nodes_ready,
                "agents_ready": f.agents_ready,
                "live_workspaces": f.live_workspaces, "live_environments": f.live_environments,
                "snapshots": f.snapshots,
                "disk_gb": f.disk_gb, "cpu": f.cpu, "memory_gb": f.memory_gb,
                "pool_used_bytes": f.pool_used_bytes, "pool_total_bytes": f.pool_total_bytes,
            })
        })
        .collect()
}

/// The pool gauges the agents expose, summed per region from the 5-minute rollup. Read from
/// ClickHouse rather than by scraping, because the collector is already carrying them and a second
/// path to the same number is a second thing to be wrong.
async fn pool_bytes(h: &History, region: &str) -> (u64, u64) {
    let sql = format!(
        "SELECT metric, sum(v) FROM (\
            SELECT metric, node, argMaxMerge(last_value) AS v FROM rustic.metrics_5m \
            WHERE region = '{region}' AND metric IN ('node_pool_bytes_used', 'node_pool_bytes_total') \
              AND ts > now() - INTERVAL 1 HOUR \
            GROUP BY metric, node) GROUP BY metric"
    );
    let mut used = 0u64;
    let mut total = 0u64;
    for r in h.query(&sql).await.unwrap_or_default() {
        let v = r.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
        match r.first().and_then(|m| m.as_str()) {
            Some("node_pool_bytes_used") => used = v,
            Some("node_pool_bytes_total") => total = v,
            _ => {}
        }
    }
    (used, total)
}

/// The hourly loop. Both folds are re-run from the cluster every hour; a failure logs and waits for
/// the next hour rather than retrying tightly, because the next run recomputes everything anyway.
pub async fn run_beats(state: Arc<crate::api::ApiState>) {
    let mut iv = tokio::time::interval(HOUR);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        iv.tick().await;
        let Some(h) = state.history.as_deref() else { continue };
        let ts = chrono::Utc::now();

        match crate::api::admin::owners::owner_rows(&state).await {
            Ok(rows) => {
                let inputs: Vec<UsageInput> = rows
                    .into_iter()
                    .map(|r| UsageInput { owner: r.owner, is_team: r.is_team, used: r.used, limit: r.limit })
                    .collect();
                if let Err(e) = h.insert("usage_hourly", &usage_rows(ts, &inputs)).await {
                    tracing::warn!(error = %e, "usage_hourly beat not written");
                }
            }
            Err(_) => tracing::warn!("usage_hourly beat skipped: the owners fold failed"),
        }

        match crate::api::admin::clusters::cluster_rows(&state).await {
            Ok(rows) => {
                let mut inputs = Vec::new();
                for r in rows {
                    let (pool_used_bytes, pool_total_bytes) = pool_bytes(h, &r.region).await;
                    inputs.push(FleetInput {
                        region: r.region,
                        nodes_total: r.nodes_total.max(0) as u32,
                        nodes_ready: r.nodes_ready.max(0) as u32,
                        agents_ready: r.agents_ready.max(0) as u32,
                        live_workspaces: r.working_copies.max(0) as u32,
                        live_environments: r.live_environments.max(0) as u32,
                        snapshots: r.snapshots.max(0) as u32,
                        disk_gb: r.disk_gb.max(0) as u64,
                        cpu: r.cpu.max(0) as u32,
                        memory_gb: r.memory_gb.max(0) as u32,
                        pool_used_bytes,
                        pool_total_bytes,
                    });
                }
                if let Err(e) = h.insert("fleet_hourly", &fleet_rows(ts, &inputs)).await {
                    tracing::warn!(error = %e, "fleet_hourly beat not written");
                }
            }
            Err(_) => tracing::warn!("fleet_hourly beat skipped: the clusters fold failed"),
        }
    }
}
```

- [ ] **Step 4: Widen the folds the beat reads**

In `crates/workspaces/src/api/admin/clusters.rs`, `ClusterRow` currently keeps `agents_desired`, `nodes_ready`, `nodes_total`, `draining` and `working_copies` private. Make the ones the beat reads `pub(crate)` and add the three the fleet row needs, computed in `one_row` from the same `RegionFacts` it already holds:

```rust
    pub(crate) nodes_ready: i64,
    pub(crate) nodes_total: i64,
    pub(crate) working_copies: i64,
    /// Running environments in this region, counted the same way `working_copies` counts
    /// workspaces — the fleet fold needs both halves and the page already shows only their sum.
    pub(crate) live_environments: i64,
    /// The allocation this region is actually holding: summed from the same specs the quota fold
    /// reads, so a chart and a refusal can never disagree about what is in use.
    pub(crate) snapshots: i64,
    pub(crate) disk_gb: i64,
    pub(crate) cpu: i64,
    pub(crate) memory_gb: i64,
```

In `one_row`, populate them from `RegionFacts` (`facts.workspaces` / `facts.environments`), and extend `RegionFacts` with the region's `Snapshot`s and `Volume`s so `snapshots`, `disk_gb`, `cpu` and `memory_gb` come from one list rather than a second round trip.

In `crates/workspaces/src/api/admin.rs`, change the module declarations the beat imports through:

```rust
pub(crate) mod clusters;
pub(crate) mod owners;
```

- [ ] **Step 5: Spawn the beats**

In `bins/api/src/main.rs`, after `let workspaces_router = …` where the `Arc<ApiState>` is available (the `workspaces` binding, before it is consumed by `map`), clone it and spawn:

```rust
    // The hourly folds. Spawned from the admin role only, and only with ClickHouse configured —
    // the fold itself is a cluster-wide list, and running it hourly for nowhere to write it would
    // be pure load on the API server.
    if role == "admin" {
        if let Some(ws) = workspaces.clone() {
            if ws.history.is_some() {
                tokio::spawn(rustic_git_workspaces::history::beats::run_beats(ws));
            }
        }
    }
```

Declare the module in `crates/workspaces/src/history/mod.rs`:

```rust
pub mod beats;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-workspaces --test history_beats && cargo test -p rustic-git-workspaces --test api_admin_clusters`
Expected: PASS — the second run proves the `ClusterRow` widening did not change the clusters page's own contract.

Run: `cargo clippy -p rustic-git-workspaces -p rustic-git-api --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/history/beats.rs crates/workspaces/src/history/mod.rs crates/workspaces/src/api/admin.rs crates/workspaces/src/api/admin/clusters.rs bins/api/src/main.rs crates/workspaces/tests/history_beats.rs
git commit -m "Record hourly usage and fleet folds in history"
```

---

## Task 7: Agent node gauges

**Files:**
- Create: `bins/agent/src/stats.rs`
- Modify: `bins/agent/src/lib.rs`
- Test: `bins/agent/tests/stats.rs`

**Interfaces:**
- Consumes: `rustic_git_core::metrics` (the recorder is already installed by the agent's `main`); `Engine`/`Pool` for the pool path.
- Produces:
  - `pub fn parse_btrfs_usage(text: &str) -> Option<(u64, u64)>` — `(used, total)` bytes from `btrfs filesystem usage -b`.
  - `pub fn statvfs_usage(path: &str) -> Option<(u64, u64)>` — the fallback.
  - `pub fn spawn_stats(pool: String, client: kube::Client, node: String)`

Only the two gauges nobody else can know: **`node_pool_bytes_used`**, **`node_pool_bytes_total`** and **`node_working_copies_running`**. CPU, memory and load come from `kubeletstats` and `k8s_cluster` in the collector — do not add them here.

- [ ] **Step 1: Write the failing test**

Create `bins/agent/tests/stats.rs`:

```rust
//! The btrfs usage parser. `btrfs filesystem usage` is the only thing that reports a btrfs pool
//! honestly — `df` on a btrfs filesystem reports allocation, not usage, and reads far under the
//! point at which allocations start failing (which is exactly what `PoolAlmostFull` exists to
//! catch). Untrusted text: this runs on whatever the installed btrfs-progs prints.

use rustic_git_agent::stats::{parse_btrfs_usage, statvfs_usage};

const USAGE: &str = "\
Overall:
    Device size:                1000000000000
    Device allocated:            600000000000
    Device unallocated:          400000000000
    Device missing:                        0
    Used:                        499999997952
    Free (estimated):            480000000000      (min: 280000000000)
    Data ratio:                            1.00
";

#[test]
fn parses_device_size_and_used_in_bytes() {
    let (used, total) = parse_btrfs_usage(USAGE).expect("the -b output must parse");
    assert_eq!(total, 1_000_000_000_000);
    assert_eq!(used, 499_999_997_952);
}

/// `Used:` appears again under each device section; the Overall figure is the first and the only
/// one that means the whole pool.
#[test]
fn takes_the_overall_used_not_a_per_device_one() {
    let text = format!("{USAGE}\nData,single: Size:600000000000, Used:1\n   /dev/sdb  600000000000\n");
    let (used, _) = parse_btrfs_usage(&text).unwrap();
    assert_eq!(used, 499_999_997_952);
}

/// A non-btrfs mount, a missing binary, an error message on stdout — anything unparsable must be
/// `None`, so the caller falls back rather than exporting a zero that reads as an empty pool.
#[test]
fn unparsable_output_is_none_rather_than_zero() {
    assert!(parse_btrfs_usage("").is_none());
    assert!(parse_btrfs_usage("ERROR: not a btrfs filesystem").is_none());
    assert!(parse_btrfs_usage("Overall:\n    Device size:  not-a-number\n").is_none());
}

/// The fallback has to work on the machine running the test — `/` exists everywhere, including in
/// CI and on the developer's Mac.
#[test]
fn statvfs_reports_a_plausible_root_filesystem() {
    let (used, total) = statvfs_usage("/").expect("/ must be statvfs-able");
    assert!(total > 0, "a filesystem with zero total blocks is not a filesystem");
    assert!(used <= total, "used {used} exceeds total {total}");
}

#[test]
fn statvfs_of_a_missing_path_is_none() {
    assert!(statvfs_usage("/definitely/not/a/path/here").is_none());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-agent --test stats`
Expected: FAIL — `unresolved import rustic_git_agent::stats`.

- [ ] **Step 3: Write the stats beat**

Create `bins/agent/src/stats.rs`:

```rust
//! The three gauges only this process can produce, on the agent's own `/metrics` — the collector's
//! prometheus receiver scrapes them like any other pod's.
//!
//! Deliberately just three. CPU, memory, load and per-pod usage come from the collector's
//! `kubeletstats` and `k8s_cluster` receivers, which already read them from the kubelet; exporting
//! our own would be a second number for the same fact, and the two would drift.
//!
//! `btrfs filesystem usage -b`, not `df` or `statvfs`: on btrfs, `df` reports ALLOCATION, and a
//! pool starts failing allocations while `df` still shows room — which is the exact condition
//! `PoolAlmostFull` exists to catch. statvfs is the fallback for a pool that is not btrfs (a dev
//! box), where a rough number beats none.

use std::time::Duration;

/// Fifteen seconds, matching the collector's scrape interval: a gauge refreshed slower than it is
/// scraped just repeats itself, and one refreshed faster burns IO for samples nobody reads.
const EVERY: Duration = Duration::from_secs(15);

/// `(used, total)` bytes out of `btrfs filesystem usage -b <path>`. `None` on anything unparsable —
/// a zero here would read as an empty pool and silence the disk alert.
pub fn parse_btrfs_usage(text: &str) -> Option<(u64, u64)> {
    let num = |line: &str| line.split(':').nth(1)?.split_whitespace().next()?.parse::<u64>().ok();
    let mut total = None;
    let mut used = None;
    for line in text.lines() {
        let t = line.trim();
        // `Device size` and the Overall `Used` both appear once, before the per-device sections;
        // `get_or_insert` keeps the first, which is the whole-pool figure.
        if t.starts_with("Device size:") {
            if let Some(v) = num(t) {
                total.get_or_insert(v);
            }
        } else if t.starts_with("Used:") {
            if let Some(v) = num(t) {
                used.get_or_insert(v);
            }
        }
    }
    Some((used?, total?))
}

/// `(used, total)` bytes from statvfs. Only a fallback: see the module doc on why this is wrong for
/// btrfs specifically.
pub fn statvfs_usage(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frsize = s.f_frsize as u64;
    let total = s.f_blocks as u64 * frsize;
    if total == 0 {
        return None;
    }
    Some((total - s.f_bfree as u64 * frsize, total))
}

fn pool_usage(pool: &str) -> Option<(u64, u64)> {
    let out = std::process::Command::new("btrfs")
        .args(["filesystem", "usage", "-b", pool])
        .output()
        .ok()?;
    parse_btrfs_usage(&String::from_utf8_lossy(&out.stdout)).or_else(|| statvfs_usage(pool))
}

/// The beat. Shelling out and counting pods are both blocking-ish, so the pool read goes to a
/// blocking thread — on the reactor it stalls every in-flight reconcile for as long as btrfs takes.
pub fn spawn_stats(pool: String, client: kube::Client, node: String) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(EVERY);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let p = pool.clone();
            if let Ok(Some((used, total))) = tokio::task::spawn_blocking(move || pool_usage(&p)).await {
                metrics::gauge!("node_pool_bytes_used").set(used as f64);
                metrics::gauge!("node_pool_bytes_total").set(total as f64);
            }
            // Working copies RUNNING on this node, from this node's own objects — the same
            // `status.nodeName` the controller converges on, so the gauge and the placement can
            // never disagree.
            let api = kube::Api::<rustic_git_workspaces::crd::Workspace>::all(client.clone());
            let params = kube::api::ListParams::default().fields(&format!("status.nodeName={node}"));
            if let Ok(list) = api.list(&params).await {
                let running = list
                    .items
                    .iter()
                    .filter(|w| {
                        matches!(
                            w.status.as_ref().map(|s| s.phase),
                            Some(rustic_git_workspaces::crd::Phase::Ready)
                                | Some(rustic_git_workspaces::crd::Phase::Running)
                        )
                    })
                    .count();
                metrics::gauge!("node_working_copies_running").set(running as f64);
            }
        }
    });
}
```

- [ ] **Step 4: Spawn it from the agent**

In `bins/agent/src/lib.rs`, add `pub mod stats;` alongside the other module declarations, and in `run`, after the `Ctx` is built and the client is available (right next to `spawn_settings_reflector`):

```rust
    // The gauges the collector cannot get from the kubelet: the btrfs pool is this process's
    // filesystem to read, and "working copies running here" is this node's own view.
    stats::spawn_stats(cfg.pool.clone(), client.clone(), cfg.node.clone());
```

Note: `cfg.pool` and `cfg.node` are moved into `Ctx::new` on the line below, so this call must come **before** that line, or clone them there instead.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-agent --test stats && cargo clippy -p rustic-git-agent --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/stats.rs bins/agent/src/lib.rs bins/agent/tests/stats.rs
git commit -m "Export btrfs pool and running-worktree gauges from the agent"
```

---

## Task 8: The alert evaluator, and retiring the on-request scrape

**Files:**
- Create: `crates/workspaces/src/history/alerts.rs`
- Modify: `crates/workspaces/src/history/mod.rs`
- Rewrite: `crates/workspaces/src/api/admin/monitoring.rs` (the scrape, the parser and the two-point rate window are deleted)
- Modify: `crates/workspaces/src/api/mod.rs` (drop `metrics_sample`)
- Modify: `bins/api/src/main.rs` (spawn the evaluator)
- Modify: `crates/workspaces/tests/api_admin_monitoring.rs` (rewritten around the new source)
- Test: `crates/workspaces/tests/history_alerts.rs`

**Interfaces:**
- Consumes: `History::{query, insert}` (Task 1).
- Produces:
  - `pub struct Rule { pub name: &'static str, pub why: &'static str, pub sql: fn(&str) -> String, pub for_secs: u64 }`
  - `pub const CATALOGUE: &[Rule]` — one entry per row of `deploy/alerts.md`, in its order, by its names.
  - `pub fn state_of(rows: &[Vec<serde_json::Value>], for_secs: u64, step_secs: u64) -> (&'static str, String)` — the `for`-window decision: `firing` only when every bucket in the window breached, `unknown` when the window is not fully covered, `ok` otherwise.
  - `pub fn alert_row(ts: chrono::DateTime<chrono::Utc>, region: &str, rule: &str, state: &str, detail: &str) -> serde_json::Value`
  - `pub async fn evaluate_forever(state: Arc<crate::api::ApiState>)` — the 30 s loop, writing transitions only.
  - `pub async fn current_signals(h: &History) -> Result<Vec<SignalRow>, HistoryError>` — latest row per `(region, rule)`, what `/admin/monitoring/signals` renders.
  - `pub struct SignalRow { pub alert: String, pub region: String, pub state: String, pub why: String, pub detail: Option<String> }`

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/history_alerts.rs`:

```rust
//! The `for`-window decision and the catalogue's completeness. This is the half that is a rule
//! rather than plumbing, and the property that matters most is the one the previous, scrape-based
//! evaluator could not hold: a rule whose window is not fully covered says `unknown`, never `ok`.

use rustic_git_workspaces::history::alerts::{alert_row, state_of, CATALOGUE};

/// One bucket per `step`, newest last, as `[ts, breached]` — the shape every catalogue query
/// returns so `state_of` is the single decision for all of them.
fn buckets(breaches: &[u8]) -> Vec<Vec<serde_json::Value>> {
    breaches
        .iter()
        .enumerate()
        .map(|(i, b)| vec![serde_json::json!(i), serde_json::json!(*b)])
        .collect()
}

#[test]
fn firing_needs_every_bucket_in_the_window_to_breach() {
    // 300 s of `for`, 30 s buckets: ten buckets, all breached.
    assert_eq!(state_of(&buckets(&[1; 10]), 300, 30).0, "firing");
}

/// One healthy bucket inside the window is what `for 5m` exists to tolerate — a blip must not page.
#[test]
fn one_healthy_bucket_inside_the_window_is_ok() {
    let mut b = [1u8; 10];
    b[4] = 0;
    assert_eq!(state_of(&buckets(&b), 300, 30).0, "ok");
}

/// The whole reason this evaluator replaced the scrape: a window the data does not cover cannot be
/// called healthy. A monitor that has only been up two minutes must not answer `ok` for a `for 5m`.
#[test]
fn a_window_that_is_not_covered_is_unknown_not_ok() {
    let (state, detail) = state_of(&buckets(&[1, 1, 1]), 300, 30);
    assert_eq!(state, "unknown");
    assert!(detail.contains("3 of 10"), "{detail}");
}

#[test]
fn no_data_at_all_is_unknown_with_a_reason() {
    let (state, detail) = state_of(&[], 300, 30);
    assert_eq!(state, "unknown");
    assert!(!detail.is_empty(), "an unknown must always say why");
}

/// Both evaluators read one catalogue (Global Constraints). If a rule is added to
/// `deploy/alerts.md` it must be added here, and the names must match exactly, or the console and
/// HyperDX disagree with no way to tell which is right.
#[test]
fn the_catalogue_matches_deploy_alerts_md() {
    let md = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/alerts.md"))
        .expect("deploy/alerts.md must be readable from the crate");
    for rule in CATALOGUE {
        assert!(md.contains(&format!("**{}**", rule.name)), "{} is not in deploy/alerts.md", rule.name);
        // Every rule must produce SQL for a region without panicking on the substitution.
        let sql = (rule.sql)("westeurope-k3s");
        assert!(sql.contains("westeurope-k3s"), "{} ignores its region", rule.name);
        assert!(sql.to_uppercase().starts_with("SELECT"), "{} is not a SELECT", rule.name);
    }
    // The reverse direction: every bolded alert name in the table has a rule here.
    for line in md.lines().filter(|l| l.starts_with("| **")) {
        let name = line.trim_start_matches("| **").split("**").next().unwrap();
        assert!(CATALOGUE.iter().any(|r| r.name == name), "{name} is in deploy/alerts.md but not in CATALOGUE");
    }
}

#[test]
fn an_alert_row_is_keyed_so_a_retried_write_collapses() {
    let ts = chrono::DateTime::parse_from_rfc3339("2026-09-04T10:00:00Z").unwrap().into();
    let a = alert_row(ts, "eu", "NoLeader", "firing", "sum = 0");
    let b = alert_row(ts, "eu", "NoLeader", "firing", "sum = 0");
    assert_eq!(a["id"], b["id"]);
    assert_eq!(a["ts"], serde_json::json!("2026-09-04 10:00:00"));
    assert_eq!(a["region"], serde_json::json!("eu"));
    assert_eq!(a["state"], serde_json::json!("firing"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test history_alerts`
Expected: FAIL — `unresolved import rustic_git_workspaces::history::alerts`.

- [ ] **Step 3: Write the evaluator**

Create `crates/workspaces/src/history/alerts.rs`:

```rust
//! `deploy/alerts.md`, evaluated as SQL over the collector's metric tables, with the `for` windows
//! the catalogue actually specifies.
//!
//! Two evaluators, one catalogue (see the plan's Global Constraints): HyperDX alerts page a human,
//! this one fills the console's Signals table. A difference between them is a bug in one of them,
//! which is only findable because both use these exact rule names.
//!
//! Every rule's SQL returns the SAME shape — one row per `STEP` bucket over the window, `[ts,
//! breached]` with `breached` 1 or 0 — so `state_of` below is the single decision for all of them
//! and adding a rule is adding a query, never a new code path.
//!
//! Why this replaced the on-request scrape: the old module could see one instant (or two, five
//! seconds apart) and therefore had to answer `unknown` for every `for 5m` rule in the catalogue —
//! nine of ten rules were permanently unknown. With samples in ClickHouse the window is a `WHERE`
//! clause, and the rule that stays is the important one: a window the data does not cover is
//! `unknown`, never `ok`.

use super::{History, HistoryError};
use std::collections::HashMap;
use std::sync::Arc;

const TS_FMT: &str = "%Y-%m-%d %H:%M:%S";
/// The bucket width every rule is evaluated at. Thirty seconds is the collector's scrape interval
/// doubled, so a bucket always has at least one sample in it under normal operation.
const STEP_SECS: u64 = 30;
/// How often the loop runs. Faster than the shortest `for` window (2 m) so a transition is recorded
/// within a bucket of when it happened, and slow enough that ten queries are nothing.
const EVERY: std::time::Duration = std::time::Duration::from_secs(30);

pub struct Rule {
    pub name: &'static str,
    /// The catalogue's own "Why" column — carried so the console never has to restate it and the
    /// two can never drift.
    pub why: &'static str,
    /// `region -> SQL`. Returns `[bucket_ts, breached]` rows, newest last.
    pub sql: fn(&str) -> String,
    pub for_secs: u64,
}

/// A counter's per-bucket rate out of `otel_metrics_sum`, as a fragment every rate rule shares:
/// the exporter writes cumulative values, so a rate is `max - min` inside the bucket, and a
/// negative delta (a pod restart) is clamped to zero rather than poisoning the ratio.
fn bucketed_sum(metric: &str, filter: &str, region: &str, window_secs: u64) -> String {
    format!(
        "SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                greatest(max(Value) - min(Value), 0) AS v \
         FROM default.otel_metrics_sum \
         WHERE MetricName = '{metric}' {filter} \
           AND ResourceAttributes['region'] = '{region}' \
           AND TimeUnix > now() - INTERVAL {window_secs} SECOND \
         GROUP BY b"
    )
}

/// The catalogue, in `deploy/alerts.md`'s order and by its names.
pub const CATALOGUE: &[Rule] = &[
    Rule {
        name: "NoLeader",
        why: "Zero: nobody holds the lease, so no claim in the fleet succeeds; two: the epoch check failed and the fence is all that stands between two writers.",
        for_secs: 120,
        sql: |region| format!(
            "SELECT b, toUInt8(sum_v != 1) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       sum(Value) AS sum_v \
                FROM default.otel_metrics_gauge \
                WHERE MetricName = 'ownership_is_leader' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 120 SECOND \
                GROUP BY b) ORDER BY b"
        ),
    },
    Rule {
        name: "LeaseRenewFailing",
        why: "A node that cannot renew loses its leases at the TTL; another node claims, and its warm databases must close.",
        for_secs: 180,
        sql: |region| format!(
            "SELECT b, toUInt8(v > 0) FROM ({}) ORDER BY b",
            bucketed_sum("ownership_renew_failures_total", "", region, 180)
        ),
    },
    Rule {
        name: "DbFenceDetected",
        why: "The invariant violation: two nodes opened one SlateDB. Zero is the only acceptable value.",
        // No `for` in the catalogue — any rise at all fires, so one breached bucket is enough.
        for_secs: STEP_SECS,
        sql: |region| format!(
            "SELECT b, toUInt8(v > 0) FROM ({}) ORDER BY b",
            bucketed_sum("db_fence_detected_total", "", region, 600)
        ),
    },
    Rule {
        name: "Http5xxRate",
        why: "Per listener and route class so a registry outage is not hidden by healthy git traffic.",
        for_secs: 300,
        sql: |region| format!(
            "SELECT b, toUInt8(bad / total > 0.05) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       greatest(sumIf(Value, Attributes['status'] = '5xx') - \
                                minIf(Value, Attributes['status'] = '5xx'), 0) AS bad, \
                       greatest(sum(Value) - min(Value), 0) AS total \
                FROM default.otel_metrics_sum \
                WHERE MetricName = 'http_requests_total' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b HAVING total > 0) ORDER BY b"
        ),
    },
    Rule {
        name: "MisdirectedWrites",
        why: "421s during a roll are expected; sustained ones mean the pods disagree about who holds the leader lease.",
        for_secs: 600,
        sql: |region| format!(
            "SELECT b, toUInt8(v / {STEP_SECS} > 0.1) FROM ({}) ORDER BY b",
            bucketed_sum("http_requests_total", "AND Attributes['status'] = '421'", region, 600)
        ),
    },
    Rule {
        name: "ReconcileErrors",
        why: "A controller in an error loop keeps retrying with backoff; the ratio is what shows it.",
        for_secs: 300,
        sql: |region| format!(
            "SELECT b, toUInt8(bad / total > 0.2) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       greatest(sumIf(Value, Attributes['result'] = 'error') - \
                                minIf(Value, Attributes['result'] = 'error'), 0) AS bad, \
                       greatest(sum(Value) - min(Value), 0) AS total \
                FROM default.otel_metrics_sum \
                WHERE MetricName = 'reconciles_total' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b HAVING total > 0) ORDER BY b"
        ),
    },
    Rule {
        name: "TunnelSaturation",
        why: "MAX_TUNNELS is 1000 per gateway pod; refusals start with 503 past it.",
        for_secs: 300,
        sql: |region| format!(
            "SELECT b, toUInt8(m > 800) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       max(Value) AS m \
                FROM default.otel_metrics_gauge \
                WHERE MetricName = 'gateway_open_tunnels' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b) ORDER BY b"
        ),
    },
    Rule {
        name: "WorkerHeartbeatStale",
        why: "The liveness probe only restarts; this pages when it keeps happening.",
        for_secs: 300,
        // `absent(up)` has no equivalent here, and inventing one from an empty result would fire
        // on every region that has no worker. The honest test is the catalogue's own second half:
        // the worker's restart count rising. An absent series leaves the window uncovered, so
        // `state_of` answers `unknown` — which is correct, not a gap.
        sql: |region| format!(
            "SELECT b, toUInt8(v > 3) FROM ({}) ORDER BY b",
            bucketed_sum("k8s.container.restarts", "AND ResourceAttributes['k8s.container.name'] = 'worker'", region, 3600)
        ),
    },
    Rule {
        name: "PoolAlmostFull",
        why: "btrfs past 80% starts failing allocations before df says full.",
        for_secs: 300,
        // The agent's own gauges (Task 7), which is what makes this rule evaluable at all — it was
        // permanently `unknown` while it depended on a node-exporter nobody deployed.
        sql: |region| format!(
            "SELECT b, toUInt8(used / total > 0.8) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       maxIf(Value, MetricName = 'node_pool_bytes_used') AS used, \
                       maxIf(Value, MetricName = 'node_pool_bytes_total') AS total \
                FROM default.otel_metrics_gauge \
                WHERE MetricName IN ('node_pool_bytes_used', 'node_pool_bytes_total') \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b HAVING total > 0) ORDER BY b"
        ),
    },
    Rule {
        name: "NodeDiskAlmostFull",
        why: "The worker's merge caches and the slatedb object cache live on the root disk.",
        for_secs: 300,
        // `kubeletstats`' node filesystem metric, so this needs no exporter of ours either.
        sql: |region| format!(
            "SELECT b, toUInt8(used / (used + avail) > 0.85) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       maxIf(Value, MetricName = 'k8s.node.filesystem.usage') AS used, \
                       maxIf(Value, MetricName = 'k8s.node.filesystem.available') AS avail \
                FROM default.otel_metrics_gauge \
                WHERE MetricName IN ('k8s.node.filesystem.usage', 'k8s.node.filesystem.available') \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b HAVING used + avail > 0) ORDER BY b"
        ),
    },
];

/// The one decision every rule goes through. `rows` is `[ts, breached]` newest last.
///
/// The order of the arms is the rule: a window that is not fully covered is `unknown` BEFORE it can
/// be `ok`, because "we did not look" and "we looked and it was fine" are different answers and
/// only one of them is safe to render green.
pub fn state_of(
    rows: &[Vec<serde_json::Value>],
    for_secs: u64,
    step_secs: u64,
) -> (&'static str, String) {
    let want = (for_secs / step_secs).max(1) as usize;
    if rows.is_empty() {
        return ("unknown", "no samples in the window — is a collector reporting for this region?".into());
    }
    if rows.len() < want {
        return ("unknown", format!("only {} of {want} buckets in the window have samples", rows.len()));
    }
    let recent = &rows[rows.len() - want..];
    let breached = recent
        .iter()
        .filter(|r| r.get(1).and_then(|v| v.as_u64()).unwrap_or(0) > 0)
        .count();
    if breached == want {
        ("firing", format!("breached for all {want} buckets of the {for_secs}s window"))
    } else {
        ("ok", format!("breached {breached} of {want} buckets in the {for_secs}s window"))
    }
}

/// A row for `rustic.alerts`. `id` is the coordinates of the transition, so a retried write of the
/// same transition collapses under the ReplacingMergeTree rather than doubling a count.
pub fn alert_row(
    ts: chrono::DateTime<chrono::Utc>,
    region: &str,
    rule: &str,
    state: &str,
    detail: &str,
) -> serde_json::Value {
    let ts = ts.format(TS_FMT).to_string();
    serde_json::json!({
        "ts": ts,
        "id": format!("{region}:{rule}:{state}:{ts}"),
        "region": region,
        "rule": rule,
        "state": state,
        "detail": detail,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SignalRow {
    pub alert: String,
    pub region: String,
    pub state: String,
    pub why: String,
    pub detail: Option<String>,
}

/// The latest state per `(region, rule)` — what the Signals table renders. A rule with no row at
/// all is `unknown` with the reason, added by the caller from `CATALOGUE`, so a region that has
/// never reported still shows ten rows rather than an empty table.
pub async fn current_signals(h: &History) -> Result<Vec<SignalRow>, HistoryError> {
    let rows = h
        .query(
            "SELECT region, rule, argMax(state, ts), argMax(detail, ts) \
             FROM rustic.alerts FINAL GROUP BY region, rule",
        )
        .await?;
    let why: HashMap<&str, &str> = CATALOGUE.iter().map(|r| (r.name, r.why)).collect();
    Ok(rows
        .iter()
        .map(|r| {
            let s = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let rule = s(1);
            SignalRow {
                why: why.get(rule.as_str()).copied().unwrap_or_default().to_string(),
                region: s(0),
                state: s(2),
                detail: Some(s(3)).filter(|d| !d.is_empty()),
                alert: rule,
            }
        })
        .collect())
}

/// Evaluate every rule for every region on a 30 s beat, writing ONLY transitions.
///
/// Only transitions, because `rustic.alerts` answers "when did this start", and a row per
/// evaluation would turn a 400-day retention into a hundred million rows saying nothing changed.
pub async fn evaluate_forever(state: Arc<crate::api::ApiState>) {
    let mut last: HashMap<(String, String), String> = HashMap::new();
    let mut iv = tokio::time::interval(EVERY);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        iv.tick().await;
        let Some(h) = state.history.as_deref() else { continue };
        let regions = match crate::api::admin::clusters::cluster_rows(&state).await {
            Ok(rows) => rows.into_iter().map(|r| r.region).collect::<Vec<_>>(),
            // A region list we could not read is not a reason to write "unknown" over a good
            // state: skip the beat and try again in thirty seconds.
            Err(_) => continue,
        };
        let now = chrono::Utc::now();
        let mut writes = Vec::new();
        for region in &regions {
            for rule in CATALOGUE {
                let rows = match h.query(&(rule.sql)(region)).await {
                    Ok(rows) => rows,
                    Err(e) => {
                        tracing::warn!(error = %e, rule = rule.name, %region, "alert query failed");
                        continue;
                    }
                };
                let (st, detail) = state_of(&rows, rule.for_secs, STEP_SECS);
                let key = (region.clone(), rule.name.to_string());
                if last.get(&key).map(String::as_str) != Some(st) {
                    writes.push(alert_row(now, region, rule.name, st, &detail));
                    last.insert(key, st.to_string());
                }
            }
        }
        if let Err(e) = h.insert("alerts", &writes).await {
            // The in-memory `last` already moved, so a failed write would suppress the retry.
            // Clearing it makes the next beat re-emit every transition it just computed.
            tracing::warn!(error = %e, n = writes.len(), "alert transitions not written; re-emitting next beat");
            last.clear();
        }
    }
}
```

Declare it in `crates/workspaces/src/history/mod.rs`:

```rust
pub mod alerts;
```

- [ ] **Step 4: Rewrite the signals handler and delete the scrape**

Replace the whole body of `crates/workspaces/src/api/admin/monitoring.rs` with a reader. The exposition parser (`sum_of`), `Sample`, `ScrapeSample`, `COUNTERS`, the `evaluate_*` helpers, `metrics_url`, `scrape` and the two-point rate window all go — every one of them existed only to fake a window on the request path.

```rust
//! `GET /admin/monitoring/signals`: the alert catalogue's CURRENT state, read from `rustic.alerts`.
//!
//! This used to scrape every pod on the request path and evaluate the rules from one instant, which
//! meant nine of the catalogue's ten rules were permanently `unknown` — a `for 5m` window cannot be
//! computed from a point. The evaluation moved to `history::alerts` (a 30 s beat over the
//! collector's samples, with real windows); this handler only reads what it wrote.
//!
//! The response SHAPE is unchanged — the web already consumes `signals`, `restarts` and the counts
//! — with one field added, `source`, so the page can say whether it is showing measurements
//! (`"monitor"`) or a region nothing is reporting for (`"none"`).

use crate::api::{admin::history_or_503, aks, kube_err, ApiState};
use crate::history::alerts::{current_signals, SignalRow, CATALOGUE};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, ResourceExt};
use std::sync::Arc;

#[derive(serde::Serialize)]
struct Restarts {
    workload: &'static str,
    /// ponytail: `restartCount` since each pod started, NOT a 1 h window — Kubernetes exposes no
    /// such number. The page says "since the pod started", so the field asserts no precision it
    /// does not have. Upgrade path: `k8s.container.restarts` is in the collector's tables now, so
    /// this can become a windowed query like the alert rules once the page wants one.
    restarts: i32,
}

#[derive(serde::Serialize)]
struct SignalsResponse {
    signals: Vec<SignalRow>,
    restarts: Vec<Restarts>,
    /// Kept for the web's existing rendering. Nothing is scraped on this path any more, so it is
    /// always empty — removing the field would break the page before sub-project C replaces it.
    scrape_failures: Vec<(String, String)>,
    pods_listed: usize,
    /// `"monitor"` when at least one rule has a recorded state, `"none"` when nothing is reporting.
    source: &'static str,
    /// Only when `RUSTIC_GIT_HYPERDX_URL` is set: a dead link on a monitoring page is worse than
    /// no link.
    #[serde(skip_serializing_if = "Option::is_none")]
    hyperdx_url: Option<String>,
}

pub(crate) async fn signals(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let recorded = current_signals(h).await.map_err(|e| {
        (axum::http::StatusCode::BAD_GATEWAY, format!("history: {e}")).into_response()
    })?;
    let source = if recorded.is_empty() { "none" } else { "monitor" };
    // A region nothing has reported for still shows every rule, `unknown` with the reason — an
    // empty table would read as "nothing is wrong".
    let mut signals = recorded;
    for rule in CATALOGUE {
        if !signals.iter().any(|r| r.alert == rule.name) {
            signals.push(SignalRow {
                alert: rule.name.to_string(),
                region: String::new(),
                state: "unknown".into(),
                why: rule.why.to_string(),
                detail: Some("no collector reporting for this region".into()),
            });
        }
    }

    let client = aks(&s)?;
    let pods = Api::<Pod>::namespaced(client.clone(), "rustic-git")
        .list(&ListParams::default())
        .await
        .map_err(kube_err)?;
    let restarts = crate::api::workloads::KNOWN_CENTRAL
        .iter()
        .map(|(workload, _)| Restarts {
            workload,
            restarts: pods
                .iter()
                .filter(|p| p.name_any().starts_with(&format!("{workload}-")))
                .flat_map(|p| p.status.iter())
                .flat_map(|st| st.container_statuses.iter().flatten())
                .map(|c| c.restart_count)
                .sum(),
        })
        .collect();

    Ok(Json(SignalsResponse {
        signals,
        restarts,
        scrape_failures: Vec::new(),
        pods_listed: pods.items.len(),
        source,
        hyperdx_url: std::env::var("RUSTIC_GIT_HYPERDX_URL").ok().filter(|u| !u.is_empty()),
    })
    .into_response())
}
```

Delete `metrics_sample` from `ApiState` (the struct field, the `new` initializer and its doc comment) — nothing reads it now.

- [ ] **Step 5: Rewrite the monitoring test around the new source**

Replace `crates/workspaces/tests/api_admin_monitoring.rs` entirely — its parser and rate-window tests cover deleted code. The new file asserts the handler's contract:

```rust
//! `/admin/monitoring/signals` now READS `rustic.alerts` instead of scraping. What is asserted is
//! the contract the web depends on: the response shape survives, a region nothing reports for shows
//! every rule `unknown` rather than an empty table, and no ClickHouse is a 503, never an error page.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{admin::router, ApiState};
use rustic_git_workspaces::history::alerts::CATALOGUE;
use rustic_git_workspaces::kube_test::{get, mock_client};
use serde_json::json;
use std::sync::Arc;

async fn serve(state: ApiState) -> (String, Arc<Jwt>) {
    let jwt = state.jwt.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    (format!("http://{addr}"), jwt)
}

fn jwt() -> Arc<Jwt> {
    Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap())
}

#[tokio::test]
async fn without_clickhouse_the_page_gets_a_503_not_an_error() {
    let pods = json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []});
    let (client, _rec) = mock_client(vec![get("/api/v1/namespaces/rustic-git/pods", pods)]);
    let (base, jwt) = serve(ApiState::new(jwt()).with_aks(client)).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/admin/monitoring/signals"))
        .bearer_auth(jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.text().await.unwrap(), "history unavailable");
}

/// The catalogue is still ten rules and every one of them still carries its "Why" — the console
/// renders that column straight from this response.
#[test]
fn every_catalogue_rule_carries_its_why() {
    assert_eq!(CATALOGUE.len(), 10);
    assert!(CATALOGUE.iter().all(|r| !r.why.is_empty()));
}
```

- [ ] **Step 6: Spawn the evaluator**

In `bins/api/src/main.rs`, next to the beats spawn from Task 6:

```rust
    if role == "admin" {
        if let Some(ws) = workspaces.clone() {
            if ws.history.is_some() {
                tokio::spawn(rustic_git_workspaces::history::alerts::evaluate_forever(ws.clone()));
                tokio::spawn(rustic_git_workspaces::history::beats::run_beats(ws));
            }
        }
    }
```

(Replaces the Task 6 spawn block; keep one, not two.)

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-workspaces --test history_alerts --test api_admin_monitoring`
Expected: PASS.

Run: `cargo clippy -p rustic-git-workspaces -p rustic-git-api --all-targets -- -D warnings`
Expected: no warnings — in particular no `unused` left over from the deleted scrape.

- [ ] **Step 8: Commit**

```bash
git add crates/workspaces/src/history/alerts.rs crates/workspaces/src/history/mod.rs crates/workspaces/src/api/admin/monitoring.rs crates/workspaces/src/api/mod.rs bins/api/src/main.rs crates/workspaces/tests/history_alerts.rs crates/workspaces/tests/api_admin_monitoring.rs
git commit -m "Evaluate the alert catalogue with real windows and retire the on-request scrape"
```

---

## Task 9: The history API

**Files:**
- Create: `crates/workspaces/src/history/series.rs`
- Create: `crates/workspaces/src/api/admin/history.rs`
- Modify: `crates/workspaces/src/history/mod.rs`, `crates/workspaces/src/api/admin.rs` (routes)
- Test: `crates/workspaces/tests/history_series.rs`

**Interfaces:**
- Consumes: `History::query` (Task 1), `history_or_503` (Task 2).
- Produces:
  - `pub struct SeriesQuery { pub range: String, pub step: String, pub region: Option<String>, pub owner: Option<String>, pub dimension: Option<String> }`
  - `pub fn sql_for(series: &str, q: &SeriesQuery) -> Option<String>` — `None` for an unknown series (the 404).
  - `pub fn summarize(points: &[(String, f64)]) -> Summary` with `pub struct Summary { pub last: f64, pub delta: f64, pub min: f64, pub max: f64 }`
  - `pub fn parse_range(range: &str) -> Option<u32>` / `pub fn parse_step(step: &str) -> Option<&'static str>` — the allow-lists.
  - Routes: `GET /admin/history/{series}`, `GET /admin/history/events`.

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/history_series.rs`:

```rust
//! The series catalogue. Each entry is one SQL statement, and what is asserted here is that the
//! statements are built from an ALLOW-LIST — a range, a step and a series name that came off the
//! wire must never reach a query as text.

use rustic_git_workspaces::history::series::{parse_range, parse_step, sql_for, summarize, SeriesQuery};

fn q() -> SeriesQuery {
    SeriesQuery { range: "7d".into(), step: "1h".into(), region: None, owner: None, dimension: None }
}

#[test]
fn every_named_series_the_console_asks_for_has_a_statement() {
    for name in [
        "pending_requests", "firing_signals", "owners_over_80", "live_workspaces",
        "live_environments", "decided_requests", "time_to_decide_p50", "pool_used",
        "cpu_used", "memory_used", "restarts", "audit_events",
    ] {
        let sql = sql_for(name, &q()).unwrap_or_else(|| panic!("{name} has no statement"));
        assert!(sql.to_uppercase().starts_with("SELECT"), "{name}: {sql}");
        // Every series is a time series: two columns, ts first.
        assert!(sql.contains("ORDER BY"), "{name} must order its buckets: {sql}");
    }
}

/// `usage` needs an owner and a dimension; without them it is a 404-shaped miss rather than a query
/// over every owner at once.
#[test]
fn the_usage_series_requires_an_owner_and_a_dimension() {
    assert!(sql_for("usage", &q()).is_none());
    let with = SeriesQuery { owner: Some("acme".into()), dimension: Some("cpu".into()), ..q() };
    let sql = sql_for("usage", &with).unwrap();
    assert!(sql.contains("'acme'") && sql.contains("'cpu'"), "{sql}");
}

#[test]
fn an_unknown_series_has_no_statement() {
    assert!(sql_for("../../etc/passwd", &q()).is_none());
    assert!(sql_for("drop_table", &q()).is_none());
}

/// The one injection surface: `owner` and `region` are caller-supplied. They are quoted into SQL,
/// so a quote in them must be rejected outright rather than escaped — an owner slug never contains
/// one, and rejecting is the arm that cannot be got subtly wrong.
#[test]
fn a_quote_in_an_owner_or_region_is_refused_not_escaped() {
    let bad = SeriesQuery { owner: Some("a' OR '1'='1".into()), dimension: Some("cpu".into()), ..q() };
    assert!(sql_for("usage", &bad).is_none());
    let bad = SeriesQuery { region: Some("eu'; DROP TABLE rustic.events; --".into()), ..q() };
    assert!(sql_for("pool_used", &bad).is_none());
}

#[test]
fn range_and_step_are_allow_lists() {
    assert_eq!(parse_range("7d"), Some(7));
    assert_eq!(parse_range("30d"), Some(30));
    assert_eq!(parse_range("90d"), Some(90));
    assert_eq!(parse_range("9999d"), None);
    assert_eq!(parse_range("7d; DROP"), None);
    assert!(parse_step("1h").is_some());
    assert!(parse_step("1d").is_some());
    assert!(parse_step("1s").is_none());
}

#[test]
fn the_summary_is_last_delta_min_and_max() {
    let s = summarize(&[("a".into(), 3.0), ("b".into(), 9.0), ("c".into(), 5.0)]);
    assert_eq!(s.last, 5.0);
    // Delta is against the FIRST point in the range — "how much has this moved over the window".
    assert_eq!(s.delta, 2.0);
    assert_eq!(s.min, 3.0);
    assert_eq!(s.max, 9.0);
}

/// An empty series is the normal state of a fresh cluster and must summarize to zeros rather than
/// NaN, which serializes as `null` and renders as a broken chart.
#[test]
fn an_empty_series_summarizes_to_zeros() {
    let s = summarize(&[]);
    assert_eq!((s.last, s.delta, s.min, s.max), (0.0, 0.0, 0.0, 0.0));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test history_series`
Expected: FAIL — `unresolved import rustic_git_workspaces::history::series`.

- [ ] **Step 3: Write the series catalogue**

Create `crates/workspaces/src/history/series.rs`:

```rust
//! The named series the console charts, one SQL statement each.
//!
//! A fixed catalogue, not a query language: the caller names a series and this module decides what
//! that means. Everything variable — the range, the step, the region, the owner — goes through an
//! allow-list or an identifier check before it is anywhere near a statement, because ClickHouse's
//! HTTP interface has no bound parameters on this path and a caller-supplied string in a query is
//! the only injection surface this crate has.

/// The allow-listed ranges, in days.
pub fn parse_range(range: &str) -> Option<u32> {
    match range {
        "7d" => Some(7),
        "30d" => Some(30),
        "90d" => Some(90),
        _ => None,
    }
}

/// The allow-listed steps, as the ClickHouse bucketing function each one means.
pub fn parse_step(step: &str) -> Option<&'static str> {
    match step {
        "1h" => Some("toStartOfHour"),
        "1d" => Some("toStartOfDay"),
        _ => None,
    }
}

/// An owner slug, a region id and a dimension word are all `[a-z0-9-_]`. Anything else is refused
/// rather than escaped: escaping is a thing to get subtly wrong, and no legitimate value here has
/// ever needed it.
fn ident(s: &str) -> Option<&str> {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        .then_some(s)
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct SeriesQuery {
    #[serde(default = "default_range")]
    pub range: String,
    #[serde(default = "default_step")]
    pub step: String,
    pub region: Option<String>,
    pub owner: Option<String>,
    pub dimension: Option<String>,
}

fn default_range() -> String {
    "7d".into()
}
fn default_step() -> String {
    "1h".into()
}

#[derive(Debug, serde::Serialize)]
pub struct Summary {
    pub last: f64,
    pub delta: f64,
    pub min: f64,
    pub max: f64,
}

/// Zeros, not NaN, on an empty series: NaN serializes as `null` and renders as a broken chart on a
/// cluster whose only fault is being new.
pub fn summarize(points: &[(String, f64)]) -> Summary {
    let values: Vec<f64> = points.iter().map(|(_, v)| *v).collect();
    match (values.first(), values.last()) {
        (Some(first), Some(last)) => Summary {
            last: *last,
            delta: last - first,
            min: values.iter().cloned().fold(f64::INFINITY, f64::min),
            max: values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        },
        _ => Summary { last: 0.0, delta: 0.0, min: 0.0, max: 0.0 },
    }
}

/// `None` means "no such series" — the route's 404 — and also covers a series whose required
/// parameter is missing or malformed, since both are "this request names nothing we can answer".
pub fn sql_for(series: &str, q: &SeriesQuery) -> Option<String> {
    let days = parse_range(&q.range)?;
    let bucket = parse_step(&q.step)?;
    // Validated once, here, so no statement below has to remember to.
    let region = match q.region.as_deref() {
        Some(r) => Some(ident(r)?),
        None => None,
    };
    let region_filter = region.map(|r| format!("AND region = '{r}'")).unwrap_or_default();
    let metric_series = |metric: &str| {
        format!(
            "SELECT {bucket}(ts) AS b, avg(argMaxMerge(last_value)) AS v \
             FROM rustic.metrics_5m \
             WHERE metric = '{metric}' AND ts > now() - INTERVAL {days} DAY {region_filter} \
             GROUP BY b ORDER BY b"
        )
    };
    let event_count = |kinds: &str| {
        format!(
            "SELECT {bucket}(ts) AS b, count() AS v FROM rustic.events FINAL \
             WHERE kind IN ({kinds}) AND ts > now() - INTERVAL {days} DAY {region_filter} \
             GROUP BY b ORDER BY b"
        )
    };
    Some(match series {
        // A request is pending from the hour it opened until the hour it was decided; counting the
        // running difference of the two event kinds is what makes that a series rather than a
        // point-in-time number.
        "pending_requests" => format!(
            "SELECT b, sum(v) OVER (ORDER BY b) AS running FROM (\
                SELECT {bucket}(ts) AS b, \
                       countIf(kind = 'request.opened') - \
                       countIf(kind IN ('request.approved', 'request.denied')) AS v \
                FROM rustic.events FINAL \
                WHERE kind LIKE 'request.%' AND ts > now() - INTERVAL {days} DAY \
                GROUP BY b ORDER BY b)"
        ),
        "decided_requests" => event_count("'request.approved', 'request.denied'"),
        "audit_events" => event_count("'admin.drain', 'admin.undrain', 'admin.decommission', \
                                      'admin.approve', 'admin.deny', 'admin.quota', 'admin.region'"),
        "live_workspaces" => format!(
            "SELECT {bucket}(ts) AS b, max(live_workspaces) AS v FROM rustic.fleet_hourly \
             WHERE ts > now() - INTERVAL {days} DAY {region_filter} GROUP BY b ORDER BY b"
        ),
        "live_environments" => format!(
            "SELECT {bucket}(ts) AS b, max(live_environments) AS v FROM rustic.fleet_hourly \
             WHERE ts > now() - INTERVAL {days} DAY {region_filter} GROUP BY b ORDER BY b"
        ),
        "pool_used" => format!(
            "SELECT {bucket}(ts) AS b, max(pool_used_bytes) / nullIf(max(pool_total_bytes), 0) AS v \
             FROM rustic.fleet_hourly \
             WHERE ts > now() - INTERVAL {days} DAY {region_filter} GROUP BY b ORDER BY b"
        ),
        // Every rule that was firing at the end of each bucket, from the transition log.
        "firing_signals" => format!(
            "SELECT b, countIf(s = 'firing') AS v FROM (\
                SELECT {bucket}(ts) AS b, region, rule, argMax(state, ts) AS s \
                FROM rustic.alerts FINAL \
                WHERE ts > now() - INTERVAL {days} DAY {region_filter} \
                GROUP BY b, region, rule) GROUP BY b ORDER BY b"
        ),
        // An owner counts once per bucket if ANY dimension is past 80% — the number the Overview
        // strip shows is "owners who need attention", not "owner-dimension pairs".
        "owners_over_80" => format!(
            "SELECT b, uniqExact(owner) AS v FROM (\
                SELECT {bucket}(ts) AS b, owner FROM rustic.usage_hourly \
                WHERE ts > now() - INTERVAL {days} DAY AND `limit` > 0 AND used / `limit` > 0.8) \
             GROUP BY b ORDER BY b"
        ),
        "time_to_decide_p50" => format!(
            "SELECT b, quantile(0.5)(secs) AS v FROM (\
                SELECT {bucket}(decided) AS b, \
                       dateDiff('second', opened, decided) AS secs FROM (\
                    SELECT target, \
                           minIf(ts, kind = 'request.opened') AS opened, \
                           maxIf(ts, kind IN ('request.approved', 'request.denied')) AS decided \
                    FROM rustic.events FINAL \
                    WHERE kind LIKE 'request.%' AND ts > now() - INTERVAL {days} DAY \
                    GROUP BY target \
                    HAVING opened > 0 AND decided > opened)) \
             GROUP BY b ORDER BY b"
        ),
        "cpu_used" => metric_series("k8s.node.cpu.utilization"),
        "memory_used" => metric_series("k8s.node.memory.utilization"),
        "restarts" => metric_series("k8s.container.restarts"),
        "usage" => {
            let owner = ident(q.owner.as_deref()?)?;
            let dimension = ident(q.dimension.as_deref()?)?;
            format!(
                "SELECT {bucket}(ts) AS b, max(used) AS v FROM rustic.usage_hourly \
                 WHERE owner = '{owner}' AND dimension = '{dimension}' \
                   AND ts > now() - INTERVAL {days} DAY GROUP BY b ORDER BY b"
            )
        }
        _ => return None,
    })
}
```

- [ ] **Step 4: Write the routes**

Create `crates/workspaces/src/api/admin/history.rs`:

```rust
//! `GET /admin/history/{series}` and `GET /admin/history/events`.
//!
//! Both are read-only and both live behind `refuse_without_claim` like every other admin route. A
//! missing ClickHouse is 503 with the sentence the web keys its flat placeholder off — never a 500
//! and never an error page, because "we have no history yet" is a normal state of a new cluster.

use super::history_or_503;
use crate::api::ApiState;
use crate::history::series::{sql_for, summarize, SeriesQuery, Summary};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

#[derive(serde::Serialize)]
struct Point {
    ts: String,
    value: f64,
}

#[derive(serde::Serialize)]
struct SeriesResponse {
    series: Vec<Point>,
    summary: Summary,
}

fn bad_gateway(e: crate::history::HistoryError) -> Response {
    // A ClickHouse that answered an error is upstream trouble, distinct from the 503 that means
    // "no ClickHouse configured" — an operator must be able to tell those apart from the status.
    (StatusCode::BAD_GATEWAY, format!("history: {e}")).into_response()
}

pub(crate) async fn series(
    State(s): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Query(q): Query<SeriesQuery>,
) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let sql = sql_for(&name, &q)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no such series").into_response())?;
    let rows = h.query(&sql).await.map_err(bad_gateway)?;
    let points: Vec<(String, f64)> = rows
        .iter()
        .map(|r| {
            (
                r.first().and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                // A null bucket (a division by zero guarded with nullIf) is a hole in the series,
                // and 0.0 draws it as a dip — but a chart with a dip beats a 500.
                r.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
            )
        })
        .collect();
    let summary = summarize(&points);
    Ok(Json(SeriesResponse {
        series: points.into_iter().map(|(ts, value)| Point { ts, value }).collect(),
        summary,
    })
    .into_response())
}

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct EventsQuery {
    kind: Option<String>,
    owner: Option<String>,
    region: Option<String>,
    from: Option<String>,
    to: Option<String>,
    /// The `ts` of the last row of the previous page, RFC 3339. A timestamp cursor, not an offset:
    /// a page boundary that shifts as rows arrive is how a timeline silently skips an event.
    cursor: Option<String>,
}

#[derive(serde::Serialize)]
struct EventOut {
    ts: String,
    kind: String,
    actor: String,
    owner: String,
    target: String,
    region: String,
    attrs: serde_json::Value,
}

#[derive(serde::Serialize)]
struct EventsPage {
    rows: Vec<EventOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

const PAGE: usize = 100;

/// Every filter is a literal in the statement, so each one is validated the same way the series
/// module validates an owner: a value that is not an identifier (or, for a timestamp, not a
/// timestamp) is a 422 naming the field, never an escaped string.
fn literal(field: &str, v: &str) -> Result<String, Response> {
    let ok = v.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:+".contains(c));
    if !ok || v.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{field} is not a valid value")).into_response());
    }
    Ok(v.to_string())
}

pub(crate) async fn events(
    State(s): State<Arc<ApiState>>,
    Query(q): Query<EventsQuery>,
) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let mut wheres = Vec::new();
    for (field, value) in [
        ("kind", &q.kind),
        ("owner", &q.owner),
        ("region", &q.region),
    ] {
        if let Some(v) = value.as_deref() {
            wheres.push(format!("{field} = '{}'", literal(field, v)?));
        }
    }
    for (field, op, value) in [("from", ">=", &q.from), ("to", "<=", &q.to)] {
        if let Some(v) = value.as_deref() {
            wheres.push(format!("ts {op} parseDateTimeBestEffort('{}')", literal(field, v)?));
        }
    }
    if let Some(c) = q.cursor.as_deref() {
        wheres.push(format!("ts < parseDateTimeBestEffort('{}')", literal("cursor", c)?));
    }
    let filter = if wheres.is_empty() { String::new() } else { format!("WHERE {}", wheres.join(" AND ")) };
    let sql = format!(
        "SELECT toString(ts), kind, actor, owner, target, region, attrs \
         FROM rustic.events FINAL {filter} ORDER BY ts DESC LIMIT {PAGE}"
    );
    let rows = h.query(&sql).await.map_err(bad_gateway)?;
    let out: Vec<EventOut> = rows
        .iter()
        .map(|r| {
            let s = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
            EventOut {
                ts: s(0),
                kind: s(1),
                actor: s(2),
                owner: s(3),
                target: s(4),
                region: s(5),
                // Stored as text; handed back parsed so the web does not have to parse it twice.
                attrs: serde_json::from_str(&s(6)).unwrap_or(serde_json::Value::Null),
            }
        })
        .collect();
    // A short page is the last page — offering a cursor there would make the client fetch one more
    // empty page every time it reached the end of a quiet timeline.
    let next_cursor = (out.len() == PAGE).then(|| out.last().map(|r| r.ts.clone())).flatten();
    Ok(Json(EventsPage { rows: out, next_cursor }).into_response())
}
```

Declare and route. In `crates/workspaces/src/history/mod.rs`:

```rust
pub mod series;
```

In `crates/workspaces/src/api/admin.rs`, add `mod history;` and, in `router`, next to the monitoring route:

```rust
        .route("/admin/history/events", get(history::events))
        // AFTER `/admin/history/events`, so the literal path wins over the `{series}` capture.
        .route("/admin/history/{series}", get(history::series))
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rustic-git-workspaces --test history_series`
Expected: PASS.

Run: `cargo test -p rustic-git-workspaces --test api_admin`
Expected: PASS — `every_admin_path_refuses_without_the_claim` must still hold with the two new routes, proving they are behind the gate.

Run: `cargo clippy -p rustic-git-workspaces --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/history/series.rs crates/workspaces/src/history/mod.rs crates/workspaces/src/api/admin/history.rs crates/workspaces/src/api/admin.rs crates/workspaces/tests/history_series.rs
git commit -m "Serve the console's history series and event pages"
```

---

## Task 10: Deploy ClickStack, the collectors, and the docs

**Files:**
- Create: `deploy/clickstack/README.md`, `deploy/clickstack/operators-values.yaml`, `deploy/clickstack/clickstack-values.yaml`
- Create: `deploy/k3s/otel-agent.yaml`
- Modify: `deploy/rustic-git.yaml`, `deploy/k3s/agent-peer.yaml`, `deploy/k3s/README.md`, `deploy/alerts.md`, `CLAUDE.md`, `tests/ws_e2e.sh`

**Interfaces:**
- Consumes: everything above. `RUSTIC_GIT_CLICKHOUSE_URL`, `RUSTIC_GIT_CLICKHOUSE_USER`, `RUSTIC_GIT_CLICKHOUSE_PASSWORD`, `RUSTIC_GIT_HYPERDX_URL` on the admin Deployment; `RUSTIC_GIT_REGION` for the watch's region label.
- Produces: a deployable stack. No Rust changes, so this task's gate is the e2e script and a real apply.

- [ ] **Step 1: Write the Helm value files**

Create `deploy/clickstack/operators-values.yaml`:

```yaml
# ClickStack, step 1 of 2: the operators the stack's CRs need (ClickHouse and the OTel collector).
# Installed once per cluster, before `clickstack` below, and it owns no data — a reinstall is safe.
#
# Chart: clickhouse/clickstack-operators. Pin the chart version in the install command
# (deploy/clickstack/README.md), never `latest`: an operator that upgrades itself under a running
# ClickHouse is exactly the kind of surprise this repo pins image digests to avoid.
clickhouse-operator:
  enabled: true
opentelemetry-operator:
  enabled: true
```

Create `deploy/clickstack/clickstack-values.yaml`:

```yaml
# ClickStack, step 2 of 2: ClickHouse, the gateway OTel collector, HyperDX and HyperDX's MongoDB.
#
# ONE ClickHouse replica, deliberately (design §"Not doing": no multi-node ClickHouse). This is
# telemetry, not the platform's record of anything a user owns — the git repos and the workspaces
# survive its loss entirely, and the console degrades to "history unavailable". A second replica
# would double the cost of the one component here whose outage is survivable.
clickhouse:
  replicaCount: 1
  persistence:
    enabled: true
    storageClass: managed-csi
    size: 100Gi
  service:
    # ClusterIP only. Nothing outside the cluster speaks the native protocol to this, and the
    # collector's ingress below is the one door in.
    type: ClusterIP
  # 30 days of raw metrics, which is what `rustic.metrics_5m` (400 d) exists to outlive.
  ttl: 720h

otel-collector:
  # The GATEWAY collector: it receives OTLP from every cluster's agent collector and writes
  # ClickHouse. The agents are ours (deploy/k3s/otel-agent.yaml), not this chart's.
  enabled: true
  ingress:
    enabled: true
    className: nginx
    annotations:
      # OTLP/gRPC is HTTP/2 end to end; without this nginx downgrades it and every export fails
      # with a protocol error that reads like a TLS problem.
      nginx.ingress.kubernetes.io/backend-protocol: GRPC
      cert-manager.io/cluster-issuer: letsencrypt
    hosts:
      - host: otel-dev.kloudlite.io
        paths: [{ path: /, pathType: Prefix }]
    tls:
      - secretName: otel-tls
        hosts: [otel-dev.kloudlite.io]

hyperdx:
  enabled: true
  ingress:
    enabled: true
    className: nginx
    annotations:
      cert-manager.io/cluster-issuer: letsencrypt
      # HyperDX has its own login, but this area is superadmin-only and a second gate costs one
      # annotation. The Secret is created by hand — see the README.
      nginx.ingress.kubernetes.io/auth-type: basic
      nginx.ingress.kubernetes.io/auth-secret: hyperdx-basic-auth
    hosts:
      - host: hyperdx-dev.kloudlite.io
        paths: [{ path: /, pathType: Prefix }]
    tls:
      - secretName: hyperdx-tls
        hosts: [hyperdx-dev.kloudlite.io]

mongodb:
  # HyperDX's own state (saved searches, dashboards, alert definitions). Small, and unrelated to
  # the platform's Mongo — do NOT point it at `rustic-git-mongo`.
  enabled: true
  persistence:
    enabled: true
    storageClass: managed-csi
    size: 10Gi
```

- [ ] **Step 2: Write the ClickStack README**

Create `deploy/clickstack/README.md`:

```markdown
# ClickStack

ClickHouse + an OpenTelemetry gateway collector + HyperDX, from the official charts. This is the
history layer's substrate: the collectors write `default.otel_*`, and the admin process owns a
second database, `rustic`, which it migrates at boot.

Applied by hand, like the k3s side — not by `deploy/roll.sh`, which only rolls our own images.

## Install

```sh
helm repo add clickstack https://clickhouse.github.io/ClickStack-helm-charts
helm repo update

# 1. The operators. Pin the version; never install unpinned.
helm upgrade --install clickstack-operators clickstack/clickstack-operators \
  --version <pinned> --namespace clickstack --create-namespace \
  -f deploy/clickstack/operators-values.yaml

# 2. The stack.
helm upgrade --install clickstack clickstack/clickstack \
  --version <pinned> --namespace clickstack \
  -f deploy/clickstack/clickstack-values.yaml
```

## The one manual step: the ingestion API key

HyperDX mints the key the collectors authenticate with; nothing in a values file can create it.

1. Open `https://hyperdx-dev.kloudlite.io`, create the first account (do this immediately after
   the install — the first account is unauthenticated by design).
2. Team Settings → API Keys → copy the **ingestion** key.
3. Put it where the agent collectors read it, in **every** cluster:

```sh
kubectl -n kube-system create secret generic rustic-git-otel \
  --from-literal=key='<ingestion key>'          # each k3s region
kubectl -n rustic-git create secret generic rustic-git-otel \
  --from-literal=key='<ingestion key>'          # AKS
```

4. The basic-auth Secret in front of HyperDX's ingress:

```sh
htpasswd -c auth <superadmin-username>
kubectl -n clickstack create secret generic hyperdx-basic-auth --from-file=auth
rm auth
```

## Wiring the admin process

`deploy/rustic-git.yaml` reads the chart's ClickHouse Secret. Confirm the names before rolling:

```sh
kubectl -n clickstack get secret -l app.kubernetes.io/name=clickhouse
```

`RUSTIC_GIT_CLICKHOUSE_URL` is `http://clickstack-clickhouse.clickstack.svc:8123`. The user needs
`CREATE`/`INSERT`/`SELECT` on `rustic` and `SELECT` on `default`:

```sql
CREATE USER IF NOT EXISTS rustic IDENTIFIED BY '<password>';
GRANT SELECT ON default.* TO rustic;
GRANT CREATE, INSERT, SELECT, ALTER ON rustic.* TO rustic;
```

The admin process migrates `rustic` itself on its next start; `kubectl logs` shows
`clickhouse migrations applied` once.

## Alerts

`deploy/alerts.md` is the catalogue, and it is evaluated TWICE on purpose: HyperDX alerts page a
human, and the admin process evaluates the same rules in SQL for the console's Signals table. The
HyperDX definitions live in that file, next to each rule. Create them once from there; a rule added
to the file must be added to both, or the two disagree with no way to tell which is right.

## Recovery

Losing ClickHouse loses history and nothing else — no repository, workspace, snapshot or quota
lives here. Reinstall the chart, let the admin process re-migrate, and accept the gap. That is why
one replica is enough.
```

- [ ] **Step 3: Write the per-region collector**

Create `deploy/k3s/otel-agent.yaml`:

```yaml
# The region's OpenTelemetry agent collector: it scrapes what this cluster exposes and exports it to
# the ClickStack gateway on AKS. One replica — this is a scraper, not a sidecar, and two would
# double every sample.
#
# What `rustic-git-otel-agent` may do in this cluster (the header table IS the role):
#
# | Resource                | Verbs            | Why                                                 |
# |-------------------------|------------------|-----------------------------------------------------|
# | pods                    | get,list,watch   | the prometheus receiver's service discovery, and     |
# |                         |                  | k8sattributes' pod → owner/namespace enrichment      |
# | nodes, nodes/proxy      | get,list,watch   | kubeletstats reads each node's /stats/summary        |
# | nodes/metrics           | get              | the same, on clusters that gate it separately        |
# | namespaces, replicasets | get,list,watch   | k8sattributes resolves a pod's workload through them |
# | services, endpoints     | get,list,watch   | prometheus service discovery's other half            |
#
# NOT granted, deliberately: anything in `rustic-git.io` (the CRDs are the API's and the agent's,
# and telemetry has no business reading a workspace's spec), any write verb at all, and secrets.
apiVersion: v1
kind: ServiceAccount
metadata:
  name: rustic-git-otel-agent
  namespace: kube-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: rustic-git-otel-agent
rules:
  - apiGroups: [""]
    resources: ["pods", "nodes", "namespaces", "services", "endpoints"]
    verbs: ["get", "list", "watch"]
  - apiGroups: [""]
    resources: ["nodes/proxy"]
    verbs: ["get", "list", "watch"]
  - apiGroups: [""]
    resources: ["nodes/metrics"]
    verbs: ["get"]
  - apiGroups: ["apps"]
    resources: ["replicasets"]
    verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: rustic-git-otel-agent
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: rustic-git-otel-agent
subjects:
  - kind: ServiceAccount
    name: rustic-git-otel-agent
    namespace: kube-system
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: rustic-git-otel-agent
  namespace: kube-system
data:
  config.yaml: |
    receivers:
      # Every pod already annotated `prometheus.io/scrape` — the same annotation the retired
      # on-request scrape used, so nothing had to be re-annotated for this.
      prometheus:
        config:
          scrape_configs:
            - job_name: kubernetes-pods
              scrape_interval: 15s
              kubernetes_sd_configs: [{ role: pod }]
              relabel_configs:
                - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_scrape]
                  action: keep
                  regex: "true"
                - source_labels: [__address__, __meta_kubernetes_pod_annotation_prometheus_io_port]
                  action: replace
                  regex: ([^:]+)(?::\d+)?;(\d+)
                  replacement: $$1:$$2
                  target_label: __address__
                - source_labels: [__meta_kubernetes_pod_name]
                  target_label: k8s_pod_name
                - source_labels: [__meta_kubernetes_namespace]
                  target_label: k8s_namespace_name
                - source_labels: [__meta_kubernetes_pod_node_name]
                  target_label: k8s_node_name
      # Node and pod CPU/memory/filesystem, straight from each kubelet. This is why the agent
      # exports only the two gauges the kubelet cannot know (btrfs pool, running worktrees).
      kubeletstats:
        collection_interval: 15s
        auth_type: serviceAccount
        endpoint: https://${env:K8S_NODE_NAME}:10250
        insecure_skip_verify: true
        metric_groups: [node, pod, container]
      k8s_cluster:
        collection_interval: 30s
      # Pod logs, so HyperDX has logs beside metrics. Our binaries keep plain tracing output;
      # `RUSTIC_GIT_LOG_FORMAT=json` makes them structured here without changing the code.
      filelog:
        include: [/var/log/pods/*/*/*.log]
        include_file_path: true
        operators:
          - type: container
            id: container-parser

    processors:
      k8sattributes:
        auth_type: serviceAccount
        passthrough: false
        extract:
          metadata: [k8s.namespace.name, k8s.pod.name, k8s.node.name, k8s.deployment.name, k8s.container.name]
      # The one label every query in `history/series.rs` and `history/alerts.rs` filters on. Set
      # here, per cluster, because nothing in the telemetry itself knows which region it is in.
      resource:
        attributes:
          - { key: region, value: "${env:RUSTIC_GIT_REGION}", action: upsert }
      batch:
        timeout: 10s
        send_batch_size: 1024

    exporters:
      otlphttp:
        endpoint: https://otel-dev.kloudlite.io
        headers:
          authorization: "${env:OTEL_INGESTION_KEY}"
        # The collector's own queue is the buffer. Nothing of ours buffers telemetry — a dropped
        # sample is a gap in a chart, and building a durable queue for that would be the most
        # over-engineered thing in this repo.
        retry_on_failure: { enabled: true }
        sending_queue: { enabled: true, queue_size: 5000 }

    service:
      pipelines:
        metrics:
          receivers: [prometheus, kubeletstats, k8s_cluster]
          processors: [k8sattributes, resource, batch]
          exporters: [otlphttp]
        logs:
          receivers: [filelog]
          processors: [k8sattributes, resource, batch]
          exporters: [otlphttp]
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rustic-git-otel-agent
  namespace: kube-system
spec:
  replicas: 1
  selector:
    matchLabels: { app: rustic-git-otel-agent }
  template:
    metadata:
      labels: { app: rustic-git-otel-agent }
    spec:
      serviceAccountName: rustic-git-otel-agent
      containers:
        - name: collector
          image: otel/opentelemetry-collector-contrib:0.109.0
          args: ["--config=/conf/config.yaml"]
          env:
            - name: K8S_NODE_NAME
              valueFrom: { fieldRef: { fieldPath: spec.nodeName } }
            - name: RUSTIC_GIT_REGION
              # Edit per region before applying — this is the value every console query filters on.
              value: westeurope-k3s
            - name: OTEL_INGESTION_KEY
              valueFrom: { secretKeyRef: { name: rustic-git-otel, key: key } }
          volumeMounts:
            - { name: conf, mountPath: /conf }
            - { name: varlogpods, mountPath: /var/log/pods, readOnly: true }
          resources:
            requests: { cpu: 100m, memory: 256Mi }
            limits: { memory: 512Mi }
      volumes:
        - name: conf
          configMap: { name: rustic-git-otel-agent }
        - name: varlogpods
          hostPath: { path: /var/log/pods }
```

- [ ] **Step 4: Widen the metrics NetworkPolicy**

In `deploy/k3s/agent-peer.yaml`, the metrics rule currently admits a namespace literally named
`monitoring`, which does not exist. Replace it (keeping the surrounding comment's warning about
`policyTypes: [Ingress]` denying everything it does not name):

```yaml
    # The OTel agent collector (deploy/k3s/otel-agent.yaml) scrapes the agent's 9464. It runs in
    # kube-system, so this selects that namespace rather than the `monitoring` one that never
    # existed — while that rule stood, nothing could reach 9464 and the agent's gauges went
    # nowhere.
    - from:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
          podSelector:
            matchLabels:
              app: rustic-git-otel-agent
      ports:
        - protocol: TCP
          port: 9464
```

- [ ] **Step 5: Wire the admin Deployment and the AKS collector**

In `deploy/rustic-git.yaml`, on the `rustic-git-admin` Deployment's `env` list:

```yaml
            # ClickStack. Optional by design: without these the admin process runs exactly as
            # before and every /admin/history route answers 503 (crates/workspaces/src/history).
            - name: RUSTIC_GIT_CLICKHOUSE_URL
              value: http://clickstack-clickhouse.clickstack.svc:8123
            - name: RUSTIC_GIT_CLICKHOUSE_USER
              valueFrom: { secretKeyRef: { name: rustic-git-clickhouse, key: user, optional: true } }
            - name: RUSTIC_GIT_CLICKHOUSE_PASSWORD
              valueFrom: { secretKeyRef: { name: rustic-git-clickhouse, key: password, optional: true } }
            # Only for the "Open in HyperDX" link — unset means no link, never a dead one.
            - name: RUSTIC_GIT_HYPERDX_URL
              value: https://hyperdx-dev.kloudlite.io
            # The region label the per-region watches stamp on their event rows.
            - name: RUSTIC_GIT_REGION
              value: westeurope-k3s
```

Then copy `deploy/k3s/otel-agent.yaml` into `deploy/rustic-git.yaml` as the AKS collector, changed in exactly three ways, each called out in a comment above the copy: namespace `rustic-git`, `RUSTIC_GIT_REGION: central`, and `endpoint: http://clickstack-otel-collector.clickstack.svc:4318` (in-cluster, so it does not leave through the ingress).

- [ ] **Step 6: Document the catalogue's second evaluator**

In `deploy/alerts.md`, replace the paragraph about node-exporter (the last two rules no longer need it — `PoolAlmostFull` reads the agent's gauges and `NodeDiskAlmostFull` reads `kubeletstats`) and add a column to the table:

```markdown
Every rule below is evaluated TWICE, from this one table: HyperDX pages a human, and the admin
process evaluates the same rule as SQL over the collector's `otel_metrics_*` tables every 30 s and
writes state transitions to `rustic.alerts`, which is what the console's Signals table reads
(`crates/workspaces/src/history/alerts.rs`). Adding a rule here means adding it in both places —
`the_catalogue_matches_deploy_alerts_md` fails the build if the Rust half is missed, and the
HyperDX half is the "HyperDX alert" column. Node/disk signals come from the collector's
`kubeletstats` receiver and the agent's own `node_pool_bytes_*` gauges; node-exporter is not
deployed and is no longer needed.

| Alert | PromQL (for 5m unless noted) | HyperDX alert | Why |
```

Fill the new column per row with the HyperDX saved-search + threshold to create (for example, for `NoLeader`: *saved search `MetricName:ownership_is_leader`, chart `sum(Value)`, alert when `!= 1` for 2 m, notify the ops webhook*).

- [ ] **Step 7: Document the deploy order**

In `deploy/k3s/README.md`, add `otel-agent.yaml` to the file table and to the fresh-cluster apply line, and add a release note:

```markdown
## Release: the history layer on ClickStack

ClickStack goes up FIRST, on AKS (`deploy/clickstack/README.md`), because the ingestion API key it
mints is what every region's collector needs; a collector applied before the Secret exists
CrashLoopBackOffs on a missing env var rather than starting and silently sending nothing.

```sh
# 1. AKS: the charts, then the API key (see deploy/clickstack/README.md), then the Secret in
#    every cluster.
# 2. Per region: the collector's Secret, then the collector, then the widened metrics policy.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/otel-agent.yaml
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-peer.yaml
# 3. AKS: the admin Deployment's new env, after the ClickHouse user exists — the process logs
#    `clickhouse migrations applied` once and then `clickhouse schema up to date` on every restart.
kubectl -n rustic-git apply -f deploy/rustic-git.yaml
```

`agent-peer.yaml`'s metrics rule previously admitted a namespace named `monitoring` that never
existed, so the agent's 9464 was unreachable; applying the new one is what makes the node gauges
arrive at all.
```

- [ ] **Step 8: Add the CLAUDE.md paragraph**

Add a section after "Live settings":

```markdown
## History and telemetry

Telemetry is **ClickStack's**, not ours: the official charts run ClickHouse, a gateway OTel
collector and HyperDX on AKS (`deploy/clickstack/`), and an `opentelemetry-collector-contrib`
agent in every cluster (`deploy/k3s/otel-agent.yaml`) scrapes the pods already annotated
`prometheus.io/scrape`, reads the kubelet for node and pod resource usage, ships pod logs, stamps
`region`, and exports OTLP to the gateway — which writes the exporter's own tables in the `default`
database. **We write no metrics pipeline**; if a number is missing the fix is collector config.

What IS ours is the `rustic` database, and **the admin process is its only writer** (`bins/api`
with `RUSTIC_GIT_API_ROLE=admin`, `crates/workspaces/src/history/`): `events` (the record, no TTL),
`usage_hourly` and `fleet_hourly` (2 y), `alerts` (400 d) and the `metrics_5m` rollup over the
collector's tables (400 d, because the exporter drops raw metrics at 30). It migrates the schema at
boot, consumes the Redis `events` stream in a **second** consumer group (`history`) that acks only
after the insert — the stream stays a nudge, never the record, so with Redis down the consumer
idles and everything else keeps writing — turns per-region Kubernetes watch transitions into rows
keyed `{uid}:{resourceVersion}:{transition}` so a replayed watch is idempotent, dual-writes every
audit row as `admin.<action>` (the object-store log stays the legal record), and runs hourly folds
recomputed from the CRDs every time, never from an earlier row. `events` and `alerts` are
`ReplacingMergeTree` on `id`; every reader queries `FINAL`.

`deploy/alerts.md` is evaluated **twice from one catalogue**: HyperDX pages a human, and
`history::alerts` evaluates the same rules as SQL every 30 s with the catalogue's real `for`
windows, writing only state TRANSITIONS to `rustic.alerts`. A window the samples do not cover is
`unknown`, never `ok` — that rule is why the old on-request scrape was retired, since a point-in-
time scrape could not compute a `for 5m` and left nine of ten rules permanently unknown.
`GET /admin/monitoring/signals` now only reads that table. `RUSTIC_GIT_CLICKHOUSE_URL` is optional
everywhere: without it every process runs exactly as today and `/admin/history/*` answers
`503 history unavailable`, which the web renders as a flat placeholder.
```

- [ ] **Step 9: Add the e2e assertions**

In `tests/ws_e2e.sh`, after the existing superadmin console block (the audit-row assertion around line 1491), add:

```sh
# --- history layer -----------------------------------------------------------------------------
# Skipped rather than failed when the admin process has no ClickHouse: `RUSTIC_GIT_CLICKHOUSE_URL`
# is optional by design, and a laptop run without ClickStack must still pass the rest of this file.
HISTORY_STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$ADMIN_BASE/admin/history/live_workspaces" \
  -H "Authorization: Bearer $ADMIN_TOKEN")
if [ "$HISTORY_STATUS" = "503" ]; then
  log "history: no ClickHouse configured on the admin process, skipping the history assertions"
else
  log "history: asserting every console series answers with a series and a summary"
  for series in pending_requests firing_signals owners_over_80 live_workspaces live_environments \
                decided_requests time_to_decide_p50 pool_used cpu_used memory_used restarts \
                audit_events; do
    BODY=$(curl -fsS "$ADMIN_BASE/admin/history/$series?range=7d&step=1h" \
      -H "Authorization: Bearer $ADMIN_TOKEN") || fail "history series $series did not answer"
    echo "$BODY" | grep -q '"summary"' || fail "history series $series has no summary: $BODY"
    echo "$BODY" | grep -q '"series"' || fail "history series $series has no series: $BODY"
  done

  # An unknown series is a 404, not an empty chart — a typo in the web must be visible.
  UNKNOWN=$(curl -s -o /dev/null -w '%{http_code}' "$ADMIN_BASE/admin/history/not_a_series" \
    -H "Authorization: Bearer $ADMIN_TOKEN")
  [ "$UNKNOWN" = "404" ] || fail "an unknown history series answered $UNKNOWN, expected 404"

  # The audit dual write: the drain/undrain above wrote audit rows, so the events table must carry
  # them as `admin.drain`. This is the one end-to-end proof that a write reached ClickHouse.
  log "history: asserting the drain audit row reached the events table"
  for i in $(seq 1 30); do
    curl -fsS "$ADMIN_BASE/admin/history/events?kind=admin.drain" -H "Authorization: Bearer $ADMIN_TOKEN" \
      | grep -q "$REGION_ID/$E2E_NODE" && break
    sleep 2
    [ "$i" -eq 30 ] && fail "the drain audit row never appeared in history events"
  done

  # Signals read the alerts table now, and say so. `source` is `none` until the evaluator has run
  # once against a region with samples, which is a legitimate outcome on a fresh stack.
  SIGNALS=$(curl -fsS "$ADMIN_BASE/admin/monitoring/signals" -H "Authorization: Bearer $ADMIN_TOKEN")
  echo "$SIGNALS" | grep -qE '"source":"(monitor|none)"' || fail "signals carries no source: $SIGNALS"
  echo "$SIGNALS" | grep -q '"NoLeader"' || fail "signals lost the catalogue: $SIGNALS"
fi
```

Add the history clause to the final `echo "OK: …"` line: `, history layer (twelve series, unknown series 404s, audit dual write lands, signals read from the alerts table)`.

- [ ] **Step 10: Verify**

Run: `cargo test --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: PASS, no warnings.

Run: `bash -n tests/ws_e2e.sh`
Expected: no syntax errors.

Run (on the k3s VM, once ClickStack is up per the README): `./tests/ws_e2e.sh`
Expected: the history block runs rather than skipping, and the summary line ends with the history clause. Exit 77 means a prerequisite was missing, not a pass.

- [ ] **Step 11: Commit**

```bash
git add deploy/clickstack deploy/k3s/otel-agent.yaml deploy/k3s/agent-peer.yaml deploy/k3s/README.md deploy/rustic-git.yaml deploy/alerts.md CLAUDE.md tests/ws_e2e.sh
git commit -m "Deploy ClickStack and the region collectors for the history layer"
```

---

## Self-Review

**Spec coverage (§A):**

| Spec section | Task |
|---|---|
| A1 store: ClickHouse from the chart, one replica, 100 Gi `managed-csi`, ClusterIP | 10 |
| A1: `default` = OTel tables, `rustic` = ours, admin process the only writer | 1, 2 |
| A1: `rustic` tables `events` / `usage_hourly` / `fleet_hourly` / `alerts` with their TTLs | 1 |
| A1: `rustic.metrics_5m` rollup, 400 d | 1 (migrations 5–7) |
| A1: `RUSTIC_GIT_CLICKHOUSE_URL` optional everywhere; reqwest, JSONEachRow/JSONCompact, no new crate | 1, 2 |
| A2: gateway collector, ingestion key in Secret `rustic-git-otel`, `otel-dev.kloudlite.io` | 10 |
| A2: agent collectors per cluster — prometheus, k8s_cluster, kubeletstats, k8sattributes, resource(`region`), batch, otlphttp; RBAC header table | 10 |
| A2: logs via `filelog` | 10 |
| A2: agent node gauges (pool bytes, running copies) | 7 |
| A3: HyperDX alerts documented beside the catalogue; admin process evaluates the same rules every 30 s with real `for` windows into `rustic.alerts` | 8, 10 |
| A3: `/admin/monitoring/signals` keeps its shape, reads `rustic.alerts`, region with no collector = `unknown` | 8 |
| A4: Redis `history` consumer group, XREADGROUP/XAUTOCLAIM, XACK after insert | 4 |
| A4: kube watches → events with `{uid}:{resourceVersion}:{transition}` | 5 |
| A4: audit rows dual-written as `admin.<action>` | 3 |
| A4: hourly `usage_hourly` / `fleet_hourly` from CRDs every run | 6 |
| A5: `GET /admin/history/{series}` with the twelve names + `usage`, one SQL each, 404 unknown, 503 no ClickHouse | 9 |
| A5: `GET /admin/history/events` paged | 9 |
| A5: "Open in HyperDX" via `RUSTIC_GIT_HYPERDX_URL` | 8 (field), 10 (env) |
| A6: `deploy/clickstack/`, `deploy/rustic-git.yaml`, `deploy/k3s/otel-agent.yaml`, NetworkPolicy, `deploy/alerts.md` | 10 |
| §Not doing: no Prometheus/Grafana, no multi-node ClickHouse, no retention below the TTLs | honoured throughout |

**Placeholder scan:** every code step carries real code; no "TBD", no "similar to Task N", no "add error handling". Two steps are deliberately prose because they are documentation, not code (Task 10 steps 6 and 8), and both quote the exact text to write. Task 1 step 7 and Task 10 step 10 are verification steps with the exact command and expected output.

**Type consistency:** `History`, `HistoryError`, `EventRow`, `SignalRow`, `Rule`, `SeriesQuery`, `Summary`, `UsageInput`, `FleetInput` are each defined once and referred to by the same name and field names everywhere after. `history_or_503` is defined in Task 2 and used in Tasks 8 and 9. `ApiState::with_history` / `with_cache` are defined in Tasks 2 and 4 and called in Task 4's boot wiring. `alerts::CATALOGUE`'s `Rule.why` is `&'static str` and `SignalRow.why` is `String` — the conversion is explicit at the one place they meet (`current_signals`, and the `unknown` filler in `monitoring.rs`). `state_of` takes `(rows, for_secs, step_secs)` in Task 8's test and its implementation. `cluster_rows` returns `ClusterRow` with the fields Task 6 widened and Task 8 reads (`region` only).

**One thing to watch during execution:** Task 6's `ClusterRow` widening adds four fields (`snapshots`, `disk_gb`, `cpu`, `memory_gb`) that `one_row` does not compute today. Extending `RegionFacts` with the region's `Snapshot`s and `Volume`s is part of that step; if it turns out the clusters page's existing tests pin `RegionFacts`'s shape, add the counts as a separate fold rather than reshaping it, and keep the beat reading one type.
