//! `/admin/slo*`: the probe's report in, and everything the console and the probe read back.
//!
//! The probe is a superadmin caller like any other admin client, so `refuse_without_claim` on the
//! parent router is the whole authorization story here — there is no probe-specific credential and
//! no second check inside a handler.
//!
//! No ClickHouse is `503 history unavailable` on every route, the same sentence `/admin/history/*`
//! answers with: the console renders its flat placeholder, and the probe retries and then exits
//! non-zero rather than reporting a run nobody stored.

use super::history_or_503;
use crate::api::ApiState;
use crate::history::{series::ident, slo, HistoryError};
use crate::slo::catalogue::{self, Suite};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

pub(crate) fn bad_gateway(e: HistoryError) -> Response {
    (StatusCode::BAD_GATEWAY, format!("history: {e}")).into_response()
}

/// `PUT /admin/slo/runs/{id}`: the probe's whole report, upserted into both tables.
///
/// The path id and the body's `run_id` must agree — they are the same fact twice, and a mismatch
/// means a report is about to be filed under a run it does not describe.
pub(crate) async fn put_run(
    State(s): State<Arc<ApiState>>,
    Path(id): Path<String>,
    Json(report): Json<slo::RunReport>,
) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    if id != report.run_id {
        return Err((StatusCode::BAD_REQUEST, format!("path id {id:?} is not the body's run id {:?}", report.run_id)).into_response());
    }
    slo::validate(&report).map_err(|e| (StatusCode::BAD_REQUEST, e).into_response())?;
    slo::upsert(h, &report).await.map_err(bad_gateway)?;
    metrics::counter!("slo_reports_total", "state" => report.state.as_str()).increment(1);
    // The row is already stored, so a webhook nobody answers must not fail the report: `post`
    // logs and counts its own failure and returns.
    if report.state == slo::RunState::Failed {
        if let Some(url) = s.slo_webhook.as_deref() {
            let failed = report.steps.iter().find(|st| !st.ok && !st.skipped);
            crate::history::notify::post(
                url,
                &crate::history::notify::body(
                    "slo.run.failed",
                    &report.run_id,
                    &report.suite,
                    failed.map(|st| st.slo_id.as_str()).unwrap_or_default(),
                    failed.map(|st| st.detail.as_str()).unwrap_or_default(),
                ),
            )
            .await;
        }
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// One stage of the journey as the console renders it. `ids` may be empty (Boot, Teardown).
#[derive(serde::Serialize)]
struct JourneyStage {
    name: &'static str,
    ids: Vec<&'static str>,
}

fn journey_of(suite: Suite) -> Vec<JourneyStage> {
    catalogue::journey(suite).into_iter().map(|(name, ids)| JourneyStage { name, ids }).collect()
}

#[derive(serde::Serialize)]
struct Journeys {
    fast: Vec<JourneyStage>,
    weekly: Vec<JourneyStage>,
    monthly: Vec<JourneyStage>,
}

#[derive(serde::Serialize)]
struct Overview {
    slos: Vec<slo::SloStatus>,
    running: Option<slo::Run>,
    runs: Vec<slo::Run>,
    /// The walk each suite makes, served with the page rather than looked up per run: it is
    /// compiled into the binary and the console needs it to render a run that has no steps yet.
    journey: Journeys,
    generated: String,
}

/// `GET /admin/slo`: the whole page in one call — every SLO's state, the run in flight, and the
/// twenty newest runs. One request rather than three, because the console polls it.
pub(crate) async fn overview(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    Ok(Json(Overview {
        slos: slo::statuses(h).await.map_err(bad_gateway)?,
        running: slo::running(h).await.map_err(bad_gateway)?,
        runs: slo::runs(h, None, 20).await.map_err(bad_gateway)?,
        journey: Journeys {
            fast: journey_of(Suite::Fast),
            weekly: journey_of(Suite::Weekly),
            monthly: journey_of(Suite::Monthly),
        },
        generated: chrono::Utc::now().to_rfc3339(),
    })
    .into_response())
}

#[derive(serde::Deserialize)]
pub(crate) struct RunsQuery {
    suite: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

pub(crate) async fn list_runs(
    State(s): State<Arc<ApiState>>,
    Query(q): Query<RunsQuery>,
) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let runs = slo::runs(h, q.suite.as_deref(), q.limit).await.map_err(bad_gateway)?;
    Ok(Json(runs).into_response())
}

#[derive(serde::Serialize)]
struct RunDetail {
    #[serde(flatten)]
    run: slo::Run,
    steps: Vec<slo::StepReport>,
    /// This run's own suite. A suite the catalogue does not name falls back to the fast journey,
    /// which is every suite's prefix — never an empty list, which would render as no journey.
    journey: Vec<JourneyStage>,
}

pub(crate) async fn run_detail(
    State(s): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let (run, steps) = slo::run_steps(h, &id)
        .await
        .map_err(bad_gateway)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no such run").into_response())?;
    let suite = Suite::parse(&run.suite).unwrap_or(Suite::Fast);
    Ok(Json(RunDetail { run, steps, journey: journey_of(suite) }).into_response())
}

// ── the three reads the probe itself makes (stage 10, "edge and pipeline") ───

/// ClickHouse quotes 64-bit integers in JSON, so every number can arrive as a string.
fn num(v: Option<&serde_json::Value>) -> Option<f64> {
    match v? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[derive(serde::Serialize)]
struct Marker {
    found: bool,
    ts: String,
}

/// `GET /admin/slo/marker/{run_id}`: did the log line the probe emitted reach the collector's
/// tables? `tel.log.latency` is exactly this question, and the probe cannot ask ClickHouse itself
/// (it holds no ClickHouse credential — only the admin process does).
pub(crate) async fn marker(
    State(s): State<Arc<ApiState>>,
    Path(run_id): Path<String>,
) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    // The one caller-shaped value on this path, checked rather than escaped like every other id.
    let id = ident(&run_id)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "run id is not an identifier").into_response())?;
    // `max()` over no rows is the zero date, not a missing row, so the SQL answers "found"
    // itself rather than the handler pattern-matching a date string.
    let rows = h
        .query(&format!(
            "SELECT max(Timestamp) > toDateTime(0) AS found, toString(max(Timestamp)) \
             FROM default.otel_logs \
             WHERE LogAttributes['run_id'] = '{id}' AND Timestamp > now() - INTERVAL 1 HOUR"
        ))
        .await
        .map_err(bad_gateway)?;
    let row = rows.first();
    let found = num(row.and_then(|r| r.first())).unwrap_or(0.0) > 0.0;
    let ts = row
        .and_then(|r| r.get(1))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(Json(Marker { found, ts }).into_response())
}

#[derive(serde::Serialize)]
struct Coverage {
    instances: Vec<String>,
}

/// `GET /admin/slo/coverage`: which pods the region's collector actually scraped in the last two
/// minutes — `tel.pod.coverage` compares that list against the pods it expects.
pub(crate) async fn coverage(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let rows = h
        .query(
            "SELECT DISTINCT ResourceAttributes['service.instance.id'] \
             FROM default.otel_metrics_gauge WHERE TimeUnix > now() - INTERVAL 2 MINUTE",
        )
        .await
        .map_err(bad_gateway)?;
    let instances = rows
        .iter()
        .filter_map(|r| r.first()?.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    Ok(Json(Coverage { instances }).into_response())
}

#[derive(serde::Serialize)]
struct Pipeline {
    /// The `history` consumer group's backlog on the Redis `events` stream (`tel.stream.lag`).
    stream_pending: Option<u64>,
    ch_disk_free_pct: Option<f64>,
    leader_pod: Option<String>,
}

/// `GET /admin/slo/pipeline`: the three fleet-internal numbers stage 10 checks. Each is
/// independently `None` when nothing has reported it — an absent number is a failed step for the
/// probe, and inventing a zero here would make it pass.
pub(crate) async fn pipeline(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let one = |sql: String| async move { h.query(&sql).await.map_err(bad_gateway) };
    let pending = one(
        "SELECT argMax(Value, TimeUnix) FROM default.otel_metrics_gauge \
         WHERE MetricName = 'history_stream_pending' AND TimeUnix > now() - INTERVAL 10 MINUTE"
            .into(),
    )
    .await?;
    let disk = one(
        "SELECT free_space / total_space * 100 FROM system.disks WHERE total_space > 0 \
         ORDER BY free_space / total_space LIMIT 1"
            .into(),
    )
    .await?;
    let leader = one(
        "SELECT argMax(ResourceAttributes['k8s.pod.name'], TimeUnix) \
         FROM default.otel_metrics_gauge \
         WHERE MetricName = 'ownership_is_leader' AND Value = 1 \
           AND TimeUnix > now() - INTERVAL 10 MINUTE"
            .into(),
    )
    .await?;
    Ok(Json(Pipeline {
        stream_pending: num(pending.first().and_then(|r| r.first())).map(|v| v as u64),
        ch_disk_free_pct: num(disk.first().and_then(|r| r.first())),
        leader_pod: leader
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
            .map(str::to_string),
    })
    .into_response())
}
