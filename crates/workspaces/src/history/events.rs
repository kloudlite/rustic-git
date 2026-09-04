//! `rustic.events` rows: the shape, the id scheme that makes at-least-once delivery safe, and the
//! writers that feed the table (the audit dual write here; the Redis consumer and the kube watches
//! in the tasks that follow).
//!
//! Every producer must compute the SAME id for the same fact, because `events` is a
//! ReplacingMergeTree on `id` and that is the entire deduplication story: a replayed watch, a
//! redelivered Redis entry and a retried insert all collapse to one row.

use std::sync::Arc;

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
        // One namespace convention across the table (`admin.<action>`, `workspace.created`): the
        // git tier's own words are kept verbatim, only prefixed with the tier they came from.
        kind: format!("git.{}", e.kind.as_str()),
        actor: e.actor,
        owner,
        // `HeadMoved` is repo-wide and carries no PR, so its `number` is a 0 marker rather than a
        // pull number — naming `repo#0` would invent a pull request that does not exist.
        target: match e.kind {
            rustic_git_storage::events::Kind::HeadMoved => e.repo.clone(),
            _ => format!("{}#{}", e.repo, e.number),
        },
        // The stream carries repo events, which belong to the git tier, not to a workspace region.
        region: "central".to_string(),
        attrs: serde_json::json!({ "title": e.title, "base": e.base, "head": e.head }),
    })
}

/// The `history` consumer group: read, insert, THEN ack.
///
/// The ack order is the opposite of the merge worker's, deliberately. The worker acks first
/// because a redelivery would cost it a redundant merge check; here the destination dedupes for us
/// (ReplacingMergeTree on `id`), so acking only after the insert returns OK means a ClickHouse
/// outage costs redelivery rather than a lost row.
///
/// The stream stays a nudge, never the record (CLAUDE.md): with Redis absent `xreadgroup` answers
/// empty and this loop simply idles — nothing anywhere depends on an entry having arrived.
pub async fn consume_forever(cache: Arc<rustic_git_storage::cache::Cache>, history: Arc<History>) {
    if !cache.connected() {
        // Loud once, at startup: "the activity feed stopped filling in" is much harder to diagnose
        // than a missing RUSTIC_GIT_REDIS_URL named in the logs.
        tracing::warn!("no Redis: the history consumer will idle; nothing else stops recording");
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
