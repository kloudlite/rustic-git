//! `GET /admin/history/{series}` and `GET /admin/history/events`.
//!
//! Both are read-only and both live behind `refuse_without_claim` like every other admin route. A
//! missing ClickHouse is 503 with the sentence the web keys its flat placeholder off — never a 500
//! and never an error page, because "we have no history yet" is a normal state of a new cluster.

use super::history_or_503;
use crate::api::ApiState;
use crate::history::series::{ident, sql_for, summarize, SeriesQuery, Summary};
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

/// ClickHouse hands back `2026-09-04 10:00:00`, which `new Date()` parses inconsistently across
/// browsers. Every timestamp we return is normalised here rather than in twelve statements.
fn rfc3339(ts: &str) -> String {
    match ts.split_once(' ') {
        Some((d, t)) => format!("{d}T{t}Z"),
        None => ts.to_string(),
    }
}

pub(crate) async fn series(
    State(s): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Query(q): Query<SeriesQuery>,
) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let sql =
        sql_for(&name, &q).ok_or_else(|| (StatusCode::NOT_FOUND, "no such series").into_response())?;
    let rows = h.query(&sql).await.map_err(bad_gateway)?;
    let points: Vec<(String, f64)> = rows
        .iter()
        .map(|r| {
            (
                rfc3339(r.first().and_then(|v| v.as_str()).unwrap_or_default()),
                // A null bucket (a division by zero guarded with nullIf) is a hole in the series,
                // and 0.0 draws it as a dip — but a chart with a dip beats a 500.
                r.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
            )
        })
        .collect();
    let summary = summarize(&points);
    Ok(Json(SeriesResponse {
        series: points
            .into_iter()
            .map(|(ts, value)| Point { ts, value })
            .collect(),
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
    limit: Option<usize>,
}

#[derive(serde::Serialize)]
struct EventOut {
    id: String,
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
    events: Vec<EventOut>,
    /// `null` on the last page. Offering a cursor there would make the client fetch one more empty
    /// page every time it reached the end of a quiet timeline.
    cursor: Option<String>,
}

const PAGE: usize = 100;
const MAX_PAGE: usize = 500;

/// Every filter is a literal in the statement, so each one is validated the same way the series
/// module validates an owner: a value that is not an identifier (or, for a timestamp, not made of
/// timestamp characters) is a 404, never an escaped string. 404 rather than 422 because the web
/// treats a malformed filter the same as a series that does not exist — there is nothing to show
/// either way, and a distinct status would only invite a caller to probe with it.
fn literal(v: &str, timestamp: bool) -> Result<&str, Response> {
    let ok = if timestamp {
        !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "-:.+ ".contains(c))
    } else {
        // Kinds carry dots (`admin.drain`), so they are identifiers plus that one character.
        !v.is_empty()
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    ok.then_some(v)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "unusable filter").into_response())
}

pub(crate) async fn events(
    State(s): State<Arc<ApiState>>,
    Query(q): Query<EventsQuery>,
) -> Result<Response, Response> {
    let h = history_or_503(&s)?;
    let mut wheres = Vec::new();
    if let Some(v) = q.kind.as_deref() {
        wheres.push(format!("kind = '{}'", literal(v, false)?));
    }
    // An owner and a region are slugs, held to the same shape the series catalogue holds them to.
    for (field, value) in [("owner", &q.owner), ("region", &q.region)] {
        if let Some(v) = value.as_deref() {
            let v = ident(v).ok_or_else(|| {
                (StatusCode::NOT_FOUND, format!("unusable {field}")).into_response()
            })?;
            wheres.push(format!("{field} = '{v}'"));
        }
    }
    for (op, value) in [(">=", &q.from), ("<=", &q.to)] {
        if let Some(v) = value.as_deref() {
            wheres.push(format!(
                "ts {op} parseDateTimeBestEffort('{}')",
                literal(v, true)?
            ));
        }
    }
    if let Some(c) = q.cursor.as_deref() {
        wheres.push(format!(
            "ts < parseDateTimeBestEffort('{}')",
            literal(c, true)?
        ));
    }
    let filter = if wheres.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", wheres.join(" AND "))
    };
    let limit = q.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE);
    let sql = format!(
        "SELECT id, toString(ts), kind, actor, owner, target, region, attrs \
         FROM rustic.events FINAL {filter} ORDER BY ts DESC LIMIT {limit}"
    );
    let rows = h.query(&sql).await.map_err(bad_gateway)?;
    let out: Vec<EventOut> = rows
        .iter()
        .map(|r| {
            let s = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
            EventOut {
                id: s(0),
                ts: rfc3339(&s(1)),
                kind: s(2),
                actor: s(3),
                owner: s(4),
                target: s(5),
                region: s(6),
                // Stored as text; handed back parsed so the web does not have to parse it twice.
                // An unparsable or non-object value becomes `{}` rather than null, because the
                // console reads fields off it directly.
                attrs: serde_json::from_str(&s(7))
                    .ok()
                    .filter(serde_json::Value::is_object)
                    .unwrap_or_else(|| serde_json::json!({})),
            }
        })
        .collect();
    let cursor = (out.len() == limit)
        .then(|| out.last().map(|r| r.ts.clone()))
        .flatten();
    Ok(Json(EventsPage {
        events: out,
        cursor,
    })
    .into_response())
}
