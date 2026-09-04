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

/// A bad `from`/`to` is the caller's mistake (422, names the field); anything else reading the
/// object store is ours (500, logged) — the two must not collapse into one status.
fn list_err(e: crate::audit::ListError) -> Response {
    match e {
        crate::audit::ListError::InvalidFilter(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg).into_response(),
        crate::audit::ListError::Store(e) => {
            tracing::error!(error = %e, "admin/audit");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

pub(crate) async fn list_audit(State(s): State<Arc<ApiState>>, Query(q): Query<AuditQuery>) -> Result<Response, Response> {
    let os = object_store(&s)?;
    // Default 50, capped at 200 — same shape every other paged admin list uses.
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let page = crate::audit::list(&os, q.filter(), q.cursor.clone(), limit).await.map_err(list_err)?;
    Ok(Json(page).into_response())
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
///
/// ponytail: every row is held in memory and re-fetched one `GET` at a time through `list`'s own
/// paging (1000 rows/page) rather than a bulk export path — fine at the volumes one admin's
/// `from`/`to` window produces; if a truly large export ever needs this, narrow the window first,
/// and only add streaming if that stops being enough.
pub(crate) async fn audit_csv(State(s): State<Arc<ApiState>>, Query(q): Query<AuditQuery>) -> Result<Response, Response> {
    let os = object_store(&s)?;
    let mut rows = Vec::new();
    let mut cursor = None;
    loop {
        let page = crate::audit::list(&os, q.filter(), cursor.clone(), 1000).await.map_err(list_err)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A comma and a quote both need escaping, and in the same field must not fight each other —
    /// the quote doubles, the whole thing gets wrapped once.
    #[test]
    fn csv_field_escapes_a_comma_and_a_quote() {
        assert_eq!(csv_field(r#"over budget, said "ops""#), r#""over budget, said ""ops""""#);
        assert_eq!(csv_field("plain"), "plain");
    }
}
