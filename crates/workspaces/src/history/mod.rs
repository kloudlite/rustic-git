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

pub mod alerts;
pub mod beats;
pub mod events;
pub mod schema;
pub mod watch;

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
            HistoryError::Server { status, body } => {
                write!(f, "clickhouse {status}: {}", body.trim())
            }
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
            // A default client would silently have NO timeout, which is the one property this
            // client exists to guarantee; a TLS backend that cannot initialise is a boot failure.
            client: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .expect("reqwest client"),
        }
    }

    /// `None` is a supported configuration, not a failure: see the module doc. The credentials come
    /// from the ClickStack chart's own ClickHouse Secret.
    pub fn from_env() -> Option<History> {
        let url = std::env::var("RUSTIC_GIT_CLICKHOUSE_URL")
            .ok()
            .filter(|u| !u.is_empty())?;
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
    ///
    /// `table` is an identifier interpolated into SQL, so it is refused unless it is a plain
    /// lowercase identifier — never escaped, and never taken from a request.
    pub async fn insert(
        &self,
        table: &str,
        rows: &[serde_json::Value],
    ) -> Result<(), HistoryError> {
        // Checked before the empty-batch shortcut: a bad table name is a bug in the caller, and
        // an empty batch must not be the reason it goes unnoticed until the first non-empty one.
        if table.is_empty()
            || !table
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(HistoryError::Server {
                status: 400,
                body: format!("refusing to insert into an unsafe table name: {table:?}"),
            });
        }
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
        // DDL answers 200 with an empty body — the migrations go through this same verb, and an
        // empty body is "no rows", not a malformed response.
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        // A ClickHouse response is untrusted input: a 200 that is not JSON (a proxy's error page)
        // must be an error, never a panic and never an empty result.
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| HistoryError::Server {
            status: 200,
            body: format!("{e}: {text}"),
        })?;
        Ok(v.get("data")
            .and_then(|d| d.as_array())
            .map(|rows| {
                rows.iter()
                    .map(|r| r.as_array().cloned().unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// A cheap liveness probe for the boot path and the history routes' 503 decision.
    pub async fn healthy(&self) -> bool {
        self.query("SELECT 1").await.is_ok()
    }
}
