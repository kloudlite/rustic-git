//! The superadmin audit log: an append-only row per admin write, `audit/{yyyy-mm}/{ts}-{rand}.json`
//! in the object store — never SlateDB, because this must survive even when a repo/image database
//! is unreachable, and it is read across every owner at once, which a per-repo database cannot do.
//!
//! `record` is called from each write handler's own success path (see `api::admin`), not a
//! middleware: the action word and target differ per route, and a generic wrapper would have to
//! parse them back out of the response — more code for the same line count (ladder rung 2).

use rand::RngCore;
use serde::{Deserialize, Serialize};
use slatedb::object_store::{path::Path as OsPath, ObjectStore, ObjectStoreExt, PutPayload};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub reason: Option<String>,
    /// `"ok"` or `"error:<code>"` — every writer passes a `&'static str` literal (`Cow` lets the
    /// field stay borrowed for that common case, no allocation on the write path), while `list`
    /// below reads a row back as an owned `String` (`serde` cannot hand back a borrow that outlives
    /// the bytes it parsed) — the same field either way, since `Cow` round-trips through `serde`
    /// as plain string content, not as a tagged enum.
    pub result: std::borrow::Cow<'static, str>,
}

/// `audit/{yyyy-mm}/{ts}-{rand}.json`. Lexicographic within a month because `ts` is RFC 3339: a
/// plain sorted listing of one month's prefix is already time order, no separate index to keep.
/// The random suffix (same 16-byte-hex shape `credentials.rs`'s poll id uses) is only there to
/// keep two rows in the same instant from colliding — it carries no meaning of its own.
fn object_key(ts: &str, rand_suffix: &str) -> String {
    let month = ts.get(0..7).unwrap_or(ts); // "2026-09-04T..." -> "2026-09"
    format!("audit/{month}/{ts}-{rand_suffix}.json")
}

/// One `put`, no batching, no queue. A lost row on a transient object-store error is a real gap,
/// but audit is evidence, not a gate: the caller logs the error and returns the write's own
/// response regardless (see `api::admin`'s call sites) — refusing an already-successful write
/// because this `put` failed would make the platform less reliable, not its audit trail more so.
pub async fn record(os: &Arc<dyn ObjectStore>, entry: &AuditEntry) -> Result<(), slatedb::object_store::Error> {
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    let key = OsPath::from(object_key(&entry.ts, &rustic_git_core::hex(&buf)));
    let bytes = serde_json::to_vec(entry).expect("AuditEntry has no non-serializable field");
    os.put(&key, PutPayload::from(bytes)).await?;
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
pub struct AuditFilter {
    pub actor: Option<String>,
    pub action: Option<String>,
    pub target: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuditPage {
    pub rows: Vec<AuditEntry>,
    pub next_cursor: Option<String>,
}

/// `yyyy-mm` prefixes to walk, newest first: `from`/`to` narrow the window (both are RFC 3339
/// timestamps or bare `yyyy-mm`; only the year-month part matters for picking prefixes), and with
/// neither given this defaults to the last 3 months — "the last quarter" is the common case, and a
/// truly historical query is what `from` is for.
fn months(filter: &AuditFilter) -> Vec<String> {
    use chrono::Datelike;
    let month_of = |s: &str| s.get(0..7).unwrap_or(s).to_string();
    let now = chrono::Utc::now();
    let to = filter.to.as_deref().map(month_of).unwrap_or_else(|| now.format("%Y-%m").to_string());
    let from = filter
        .from
        .as_deref()
        .map(month_of)
        .unwrap_or_else(|| (now - chrono::Duration::days(90)).format("%Y-%m").to_string());

    let mut parts = to.splitn(2, '-');
    let mut y: i32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| now.year());
    let mut m: u32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(1);

    let mut out = Vec::new();
    loop {
        let cur = format!("{y:04}-{m:02}");
        out.push(cur.clone());
        if cur <= from || out.len() > 240 {
            // The length cap is a hard stop against a malformed/ancient `from` walking this loop
            // back to year zero one month at a time.
            break;
        }
        if m == 1 {
            m = 12;
            y -= 1;
        } else {
            m -= 1;
        }
    }
    out
}

fn parse_entry(bytes: &[u8]) -> Option<AuditEntry> {
    serde_json::from_slice(bytes).ok()
}

/// Reads the requested month prefixes newest-first, filters `actor`/`action`/`target` in memory,
/// and pages by `limit` with a cursor that is just the last object key read.
///
/// ponytail: a full-month scan per query, no per-field index — add one if a fleet's monthly audit
/// volume ever makes this slow in practice, not ahead of evidence it does.
pub async fn list(
    os: &Arc<dyn ObjectStore>,
    filter: AuditFilter,
    cursor: Option<String>,
    limit: usize,
) -> Result<AuditPage, slatedb::object_store::Error> {
    let mut keys: Vec<String> = Vec::new();
    for month in months(&filter) {
        let prefix = OsPath::from(format!("audit/{month}"));
        let mut listing = os.list(Some(&prefix));
        while let Some(m) = futures::StreamExt::next(&mut listing).await {
            keys.push(m?.location.to_string());
        }
    }
    // Newest first: the key embeds `ts` right after the month prefix, so a reverse lexicographic
    // sort is reverse time order within and across the walked months.
    keys.sort_unstable_by(|a, b| b.cmp(a));
    let start = match &cursor {
        Some(c) => keys.iter().position(|k| k == c).map(|i| i + 1).unwrap_or(0),
        None => 0,
    };

    let mut rows = Vec::new();
    let mut next_cursor = None;
    for key in &keys[start.min(keys.len())..] {
        if rows.len() >= limit {
            next_cursor = Some(key.clone());
            break;
        }
        let bytes = os.get(&OsPath::from(key.as_str())).await?.bytes().await?.to_vec();
        let Some(entry) = parse_entry(&bytes) else { continue };
        if filter.actor.as_deref().is_some_and(|a| entry.actor != a) {
            continue;
        }
        if filter.action.as_deref().is_some_and(|a| entry.action != a) {
            continue;
        }
        if filter.target.as_deref().is_some_and(|t| entry.target != t) {
            continue;
        }
        rows.push(entry);
    }
    Ok(AuditPage { rows, next_cursor })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key shape is the contract with everything downstream: `{yyyy-mm}/{ts}-{ulid}.json`
    /// under `audit/`, lexicographically sortable within a month because `ts` is RFC 3339.
    #[test]
    fn the_object_key_sorts_by_time_within_a_month() {
        let a = object_key("2026-09-04T10:00:00Z", "01J...A");
        let b = object_key("2026-09-04T11:00:00Z", "01J...B");
        assert!(a.starts_with("audit/2026-09/"));
        assert!(a < b);
    }

    /// Every field the spec's Audit page needs, round-tripped through JSON exactly as it will be
    /// read back by `list`.
    #[test]
    fn an_entry_round_trips() {
        let e = AuditEntry {
            ts: "2026-09-04T10:00:00Z".into(),
            actor: "op@example.com".into(),
            action: "deny".into(),
            target: "acme".into(),
            reason: Some("over budget".into()),
            result: "ok".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: AuditEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.actor, "op@example.com");
        assert_eq!(back.result, "ok");
    }
}
