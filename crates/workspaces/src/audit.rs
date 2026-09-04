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

/// The read side's one error: a bad `from`/`to` (422, names the field) versus a real
/// object-store failure (500) — the two must not be conflated into one opaque error, since the
/// HTTP route needs to answer them differently.
#[derive(Debug)]
pub enum ListError {
    InvalidFilter(String),
    Store(slatedb::object_store::Error),
}

impl From<slatedb::object_store::Error> for ListError {
    fn from(e: slatedb::object_store::Error) -> Self {
        ListError::Store(e)
    }
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListError::InvalidFilter(msg) => write!(f, "{msg}"),
            ListError::Store(e) => write!(f, "{e}"),
        }
    }
}

/// `from`/`to` as `yyyy-mm` or `yyyy-mm-dd` (a full RFC 3339 timestamp works too — only its first
/// 10 characters are read), named in the error so a 422 can point at the field.
fn parse_month_of(field: &str, s: &str) -> Result<(i32, u32), ListError> {
    use chrono::Datelike;
    let bad = || ListError::InvalidFilter(format!("{field} must be yyyy-mm or yyyy-mm-dd"));
    let head = match s.len() {
        7 => format!("{s}-01"),
        n if n >= 10 => s[0..10].to_string(),
        _ => return Err(bad()),
    };
    let d = chrono::NaiveDate::parse_from_str(&head, "%Y-%m-%d").map_err(|_| bad())?;
    Ok((d.year(), d.month()))
}

/// `yyyy-mm` prefixes to walk, newest first, between the validated `from`/`to` window (defaulting
/// to the last 3 months when neither is given — "the last quarter" is the common case, and a
/// truly historical query is what `from` is for). `from` after `to` is a valid, empty window, not
/// an error and not a walk back to the beginning of time — the caller asked for nothing.
fn months(filter: &AuditFilter) -> Result<Vec<String>, ListError> {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    let (to_y, to_m) = match &filter.to {
        Some(s) => parse_month_of("to", s)?,
        None => (now.year(), now.month()),
    };
    let (from_y, from_m) = match &filter.from {
        Some(s) => parse_month_of("from", s)?,
        None => {
            let d = now - chrono::Duration::days(90);
            (d.year(), d.month())
        }
    };
    let to = format!("{to_y:04}-{to_m:02}");
    let from = format!("{from_y:04}-{from_m:02}");
    if from > to {
        return Ok(Vec::new());
    }

    let (mut y, mut m) = (to_y, to_m);
    let mut out = Vec::new();
    loop {
        let cur = format!("{y:04}-{m:02}");
        out.push(cur.clone());
        if cur <= from || out.len() > 240 {
            // The length cap is a hard stop against a malformed/ancient `from` walking this loop
            // back to year zero one month at a time — `from > to` above already handles the
            // common "swapped the two" mistake, this is only for a truly stale `from`.
            break;
        }
        if m == 1 {
            m = 12;
            y -= 1;
        } else {
            m -= 1;
        }
    }
    Ok(out)
}

fn parse_entry(bytes: &[u8]) -> Option<AuditEntry> {
    serde_json::from_slice(bytes).ok()
}

/// Reads the requested month prefixes newest-first, filters `actor`/`action`/`target` in memory,
/// and pages by `limit` with a cursor that is the last object key this page CONSUMED — the next
/// page resumes at the key after it. Naming the first unread key instead would drop exactly one
/// row per page boundary, since the resume skips past whatever the cursor names. An unrecognised
/// `cursor` (the row it named is gone, or it never existed) answers with an empty page rather than
/// silently restarting at page 1 — a caller paging forward must not loop.
///
/// ponytail: a full-month scan per query, no per-field index — add one if a fleet's monthly audit
/// volume ever makes this slow in practice, not ahead of evidence it does.
pub async fn list(
    os: &Arc<dyn ObjectStore>,
    filter: AuditFilter,
    cursor: Option<String>,
    limit: usize,
) -> Result<AuditPage, ListError> {
    let mut keys: Vec<String> = Vec::new();
    for month in months(&filter)? {
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
        None => 0,
        Some(c) => match keys.iter().position(|k| k == c) {
            Some(i) => i + 1,
            // Unknown cursor: nothing to resume from, so the page is empty rather than page 1 —
            // silently restarting would look to a paging client like the list looped.
            None => keys.len(),
        },
    };

    let mut rows = Vec::new();
    let mut next_cursor = None;
    // The last key CONSUMED, filtered-out rows included: a resume must skip them too, or every
    // page would re-walk the same non-matching keys.
    let mut last_consumed: Option<&String> = None;
    for key in &keys[start.min(keys.len())..] {
        if rows.len() >= limit {
            next_cursor = last_consumed.cloned();
            break;
        }
        last_consumed = Some(key);
        let bytes = os.get(&OsPath::from(key.as_str())).await?.bytes().await?.to_vec();
        let Some(entry) = parse_entry(&bytes) else {
            tracing::warn!(key, "unreadable audit row");
            continue;
        };
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

    /// An explicit `from`/`to` walks exactly that span, oldest boundary included.
    #[test]
    fn months_walks_the_explicit_span() {
        let f = AuditFilter { from: Some("2026-06".into()), to: Some("2026-08-15".into()), ..Default::default() };
        assert_eq!(months(&f).unwrap(), vec!["2026-08", "2026-07", "2026-06"]);
    }

    /// `from` after `to` is a valid, empty window — not a 422 and not a walk to year zero.
    #[test]
    fn months_with_from_after_to_is_empty() {
        let f = AuditFilter { from: Some("2026-08".into()), to: Some("2026-06".into()), ..Default::default() };
        assert_eq!(months(&f).unwrap(), Vec::<String>::new());
    }

    /// The cursor contract: walking the log a page at a time must yield exactly the unpaged
    /// walk. A cursor naming the first UNREAD key instead of the last read one silently drops one
    /// row per page boundary, which is invisible in any single-page test.
    #[tokio::test]
    async fn paging_yields_the_same_rows_as_one_unpaged_read() {
        let os: Arc<dyn ObjectStore> = Arc::new(slatedb::object_store::memory::InMemory::new());
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        for i in 0..5 {
            let entry = AuditEntry {
                ts: format!("{month}-04T10:0{i}:00Z"),
                actor: "op@example.com".into(),
                action: "deny".into(),
                target: format!("owner{i}"),
                reason: None,
                result: "ok".into(),
            };
            record(&os, &entry).await.unwrap();
        }
        let all = list(&os, AuditFilter::default(), None, 100).await.unwrap();
        assert_eq!(all.rows.len(), 5);
        assert!(all.next_cursor.is_none());

        let mut paged = Vec::new();
        let mut cursor = None;
        loop {
            let page = list(&os, AuditFilter::default(), cursor, 2).await.unwrap();
            paged.extend(page.rows);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        let names = |rows: &[AuditEntry]| rows.iter().map(|r| r.target.clone()).collect::<Vec<_>>();
        assert_eq!(names(&paged), names(&all.rows));
    }

    /// A malformed date names the field in the 422 the caller turns this into.
    #[test]
    fn months_rejects_a_malformed_date() {
        let f = AuditFilter { from: Some("not-a-date".into()), ..Default::default() };
        assert!(matches!(months(&f), Err(ListError::InvalidFilter(msg)) if msg.contains("from")));
    }
}
