//! `kloudlite.slo_exclusions`: acknowledged incident windows the SLO budget maths leaves out.
//!
//! An exclusion never deletes a sample. The raw rows stay in `slo_results`, the console states how
//! many samples a window took out (`SloStatus.excluded`), and the whole effect lives in one
//! predicate the readers in `slo.rs` share (`EXCL_CTE`). A delete is a TOMBSTONE row rather than a
//! mutation — ClickHouse mutations are asynchronous and this table is read on every console poll,
//! so "it will be gone shortly" is not an answer a superadmin can act on.

use chrono::{DateTime, Utc};

use super::{History, HistoryError};

/// Seconds, because a window is a decision a person made, not a measurement.
const TS_FMT: &str = "%Y-%m-%d %H:%M:%S";
/// Milliseconds, so a create and an immediate undo still order under `ReplacingMergeTree(created)`.
const MS_FMT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// The longest window one exclusion may cover. Past this it is not an incident any more, it is a
/// policy, and a policy belongs in the target rather than in a row nobody re-reads.
pub const MAX_WINDOW_DAYS: i64 = 7;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Exclusion {
    pub id: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Empty means every SLO — the shape an infrastructure incident has.
    pub slo_ids: Vec<String>,
    pub note: String,
    /// The superadmin who excluded the window.
    pub by: String,
    pub created: DateTime<Utc>,
}

/// Newest window first. `FINAL` because a delete is a second row until the parts merge, and the
/// tombstone filter only means anything once the newer row has won.
pub async fn exclusions(h: &History) -> Result<Vec<Exclusion>, HistoryError> {
    let rows = h
        .query(
            "SELECT id, toString(`from`), toString(`to`), slo_ids, note, `by`, toString(created) \
             FROM kloudlite.slo_exclusions FINAL WHERE deleted = toDateTime64(0, 3) \
             ORDER BY `from` DESC LIMIT 500",
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let s = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            Exclusion {
                id: s(0),
                from: ts(&s(1)),
                to: ts(&s(2)),
                slo_ids: r
                    .get(3)
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).map(str::to_string).collect())
                    .unwrap_or_default(),
                note: s(4),
                by: s(5),
                created: ts(&s(6)),
            }
        })
        .collect())
}

pub async fn put_exclusion(h: &History, e: &Exclusion) -> Result<(), HistoryError> {
    h.insert(
        "slo_exclusions",
        &[serde_json::json!({
            "id": e.id,
            "from": e.from.format(TS_FMT).to_string(),
            "to": e.to.format(TS_FMT).to_string(),
            "slo_ids": e.slo_ids,
            "note": e.note,
            "by": e.by,
            "created": e.created.format(MS_FMT).to_string(),
            "deleted": DateTime::UNIX_EPOCH.format(MS_FMT).to_string(),
        })],
    )
    .await
}

/// The tombstone. `created` is stamped now as well, because it IS the row's version — the delete
/// has to be newer than the create it undoes, and a deleted window is never shown again anyway.
pub async fn delete_exclusion(h: &History, id: &str, by: &str) -> Result<(), HistoryError> {
    let now = Utc::now().format(MS_FMT).to_string();
    h.insert(
        "slo_exclusions",
        &[serde_json::json!({
            "id": id,
            "from": DateTime::UNIX_EPOCH.format(TS_FMT).to_string(),
            "to": DateTime::UNIX_EPOCH.format(TS_FMT).to_string(),
            "slo_ids": Vec::<String>::new(),
            "note": "",
            "by": by,
            "created": now,
            "deleted": now,
        })],
    )
    .await
}

/// An unparsable timestamp reads as the epoch rather than dropping the row, the same rule
/// `slo::ts` holds: a visibly wrong date is easier to notice than a missing window.
fn ts(s: &str) -> DateTime<Utc> {
    chrono::NaiveDateTime::parse_from_str(s, MS_FMT)
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, TS_FMT))
        .map(|t| t.and_utc())
        .unwrap_or(DateTime::UNIX_EPOCH)
}
