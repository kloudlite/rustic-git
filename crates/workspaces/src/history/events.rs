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
