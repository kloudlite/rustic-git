//! `GET /admin/audit` and `GET /admin/audit.csv` — the read side of `crate::audit`. Both share
//! one filter (`?actor=&action=&target=&from=&to=`); the JSON route also pages
//! (`&cursor=&limit=`), the CSV route does not (spec: a truly large export is an operator problem
//! to solve with a narrower `from`/`to`, not this route's to solve with streaming).

use super::*;
use std::fmt::Write as _;

fn object_store(s: &ApiState) -> Result<std::sync::Arc<dyn slatedb::object_store::ObjectStore>, Response> {
    s.keys
        .as_ref()
        .map(|store| store.os.clone())
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "object store not configured").into_response())
}

#[derive(serde::Deserialize)]
pub(crate) struct AuditQuery {
    actor: Option<String>,
    action: Option<String>,
    target: Option<String>,
    from: Option<String>,
    to: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

impl AuditQuery {
    fn filter(&self) -> crate::audit::AuditFilter {
        crate::audit::AuditFilter {
            actor: self.actor.clone(),
            action: self.action.clone(),
            target: self.target.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
        }
    }
}

pub(crate) async fn list_audit(State(s): State<Arc<ApiState>>, Query(q): Query<AuditQuery>) -> Result<Response, Response> {
    let os = object_store(&s)?;
    // Default 50, capped at 200 — same shape every other paged admin list uses.
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let page = crate::audit::list(&os, q.filter(), q.cursor.clone(), limit).await.map_err(internal)?;
    Ok(Json(page).into_response())
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "admin/audit");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// A comma or quote in a field must not shift the columns after it in a spreadsheet — the
/// standard CSV escape (wrap in quotes, double any quote inside) covers both.
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// No pagination: bounded by the same `from`/`to` window as the JSON route, walked once in full.
pub(crate) async fn audit_csv(State(s): State<Arc<ApiState>>, Query(q): Query<AuditQuery>) -> Result<Response, Response> {
    let os = object_store(&s)?;
    let mut rows = Vec::new();
    let mut cursor = None;
    loop {
        let page = crate::audit::list(&os, q.filter(), cursor.clone(), 1000).await.map_err(internal)?;
        let done = page.next_cursor.is_none();
        cursor = page.next_cursor.clone();
        rows.extend(page.rows);
        if done {
            break;
        }
    }
    let mut out = String::from("ts,actor,action,target,reason,result\n");
    for r in &rows {
        let _ = writeln!(
            out,
            "{},{},{},{},{},{}",
            csv_field(&r.ts),
            csv_field(&r.actor),
            csv_field(&r.action),
            csv_field(&r.target),
            csv_field(r.reason.as_deref().unwrap_or("")),
            csv_field(&r.result),
        );
    }
    Ok(([("content-type", "text/csv")], out).into_response())
}
