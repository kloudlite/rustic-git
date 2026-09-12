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
    let key = OsPath::from(object_key(&entry.ts, &kloudlite_core::hex(&buf)));
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

/// How far back one read may walk, in day prefixes. 400 is the audit retention — the rows are
/// kept forever, but nothing older than that window is what an operator is paging through, and
/// without a bound a sparse or empty log would list one prefix per day back to the epoch (a
/// `from` of `1970-01` used to be a 240-month scan; this is the same guard one granularity down).
const MAX_DAYS_WALKED: usize = 400;

/// `from`/`to` as `yyyy-mm` or `yyyy-mm-dd` (a full RFC 3339 timestamp works too — only its first
/// 10 characters are read), named in the error so a 422 can point at the field. A bare `yyyy-mm`
/// means the WHOLE month, so it resolves to the first day for `from` and the last for `to`.
fn parse_day_of(field: &str, s: &str, end_of_month: bool) -> Result<chrono::NaiveDate, ListError> {
    let bad = || ListError::InvalidFilter(format!("{field} must be yyyy-mm or yyyy-mm-dd"));
    match s.len() {
        7 => {
            let first = chrono::NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d").map_err(|_| bad())?;
            Ok(if end_of_month {
                first.checked_add_months(chrono::Months::new(1)).and_then(|d| d.pred_opt()).ok_or_else(bad)?
            } else {
                first
            })
        }
        n if n >= 10 => chrono::NaiveDate::parse_from_str(&s[0..10], "%Y-%m-%d").map_err(|_| bad()),
        _ => Err(bad()),
    }
}

/// The day prefixes to walk, newest first: `{yyyy-mm}/{yyyy-mm-dd}`, which is a key prefix because
/// the key embeds an RFC 3339 `ts` right after the month. Walking days rather than whole months is
/// what lets `list` stop after one listing when the caller asked for one screenful (2026-09-12:
/// `GET /admin/audit` was 3–7 s, one full-month listing per query, growing with the log).
/// `from` after `to` is a valid, empty window, not an error and not a walk back to the beginning
/// of time — the caller asked for nothing.
fn days(filter: &AuditFilter) -> Result<Vec<String>, ListError> {
    let today = chrono::Utc::now().date_naive();
    let to = match &filter.to {
        Some(s) => parse_day_of("to", s, true)?,
        None => today,
    };
    let from = match &filter.from {
        Some(s) => parse_day_of("from", s, false)?,
        // No window given: the last quarter, the common case — a truly historical query is what
        // `from` is for.
        None => today - chrono::Duration::days(90),
    };
    let mut out = Vec::new();
    let mut d = to;
    while d >= from && out.len() < MAX_DAYS_WALKED {
        out.push(d.format("%Y-%m/%Y-%m-%d").to_string());
        let Some(prev) = d.pred_opt() else { break };
        d = prev;
    }
    Ok(out)
}

fn parse_entry(bytes: &[u8]) -> Option<AuditEntry> {
    serde_json::from_slice(bytes).ok()
}

/// The `{yyyy-mm}/{yyyy-mm-dd}` a cursor key sits in, so a resume can skip the newer days without
/// listing them at all.
fn day_of_key(key: &str) -> Option<&str> {
    key.strip_prefix("audit/")?.get(0..18)
}

/// Walks day prefixes newest-first, filtering `actor`/`action`/`target` in memory, and stops as
/// soon as `limit` rows past the cursor are in hand — so a screenful costs one day's listing, not
/// a month's. A filter that matches nothing recent simply keeps walking, bounded by
/// `MAX_DAYS_WALKED` and by the window's own `from`.
///
/// Pages by a cursor that is the last object key this page CONSUMED — the next page resumes at the
/// key after it. Naming the first unread key instead would drop exactly one row per page boundary,
/// since the resume skips past whatever the cursor names. An unrecognised `cursor` (the row it
/// named is gone, or it never existed) answers with an empty page rather than silently restarting
/// at page 1 — a caller paging forward must not loop.
pub async fn list(
    os: &Arc<dyn ObjectStore>,
    filter: AuditFilter,
    cursor: Option<String>,
    limit: usize,
) -> Result<AuditPage, ListError> {
    let cursor_day = cursor.as_deref().and_then(day_of_key).map(str::to_string);
    let empty = || AuditPage { rows: Vec::new(), next_cursor: None };
    let mut rows = Vec::new();
    let mut next_cursor = None;
    // The last key CONSUMED, filtered-out rows included: a resume must skip them too, or every
    // page would re-walk the same non-matching keys.
    let mut last_consumed: Option<String> = None;
    let mut resumed = cursor.is_none();

    'days: for day in days(&filter)? {
        // Everything newer than the cursor was already paged; not listing those prefixes at all is
        // the point of resuming at the cursor's own day.
        if cursor_day.as_deref().is_some_and(|c| day.as_str() > c) {
            continue;
        }
        // object_store matches a `list` prefix on SEGMENT boundaries, so `audit/{month}/{day}` is
        // not a prefix it would accept — the day is only part of the key's last segment. The day
        // is still a byte prefix, so it is reached as an OFFSET into the month instead: S3 sends
        // that as `start-after` server-side, and the listing is ascending, so the first key that
        // no longer carries the day's prefix ends the day and the rest of the month is never read.
        let month = OsPath::from(format!("audit/{}", &day[0..7]));
        let day_prefix = format!("audit/{day}");
        let mut keys: Vec<String> = Vec::new();
        let mut listing = os.list_with_offset(Some(&month), &OsPath::from(day_prefix.as_str()));
        while let Some(m) = futures::StreamExt::next(&mut listing).await {
            let key = m?.location.to_string();
            if !key.starts_with(&day_prefix) {
                break;
            }
            keys.push(key);
        }
        // Newest first: the key embeds `ts` right after the month prefix, so a reverse
        // lexicographic sort within a day, walked newest day first, is reverse time order overall.
        keys.sort_unstable_by(|a, b| b.cmp(a));

        let start = if resumed {
            0
        } else {
            match keys.iter().position(|k| Some(k.as_str()) == cursor.as_deref()) {
                Some(i) => {
                    resumed = true;
                    i + 1
                }
                None => return Ok(empty()),
            }
        };
        if start >= keys.len() {
            continue;
        }

        // One object per row, so the fetch is what a page costs: `buffered` keeps the order the
        // keys are in (newest first) while holding a batch of GETs in flight — one at a time was
        // 4 ms a row against S3, 17 s for a quarter's export.
        let tail: Vec<String> = keys.split_off(start);
        let mut fetched = futures::StreamExt::buffered(
            futures::stream::iter(tail.into_iter().map(|key| {
                let os = os.clone();
                async move {
                    let bytes = os.get(&OsPath::from(key.as_str())).await?.bytes().await?.to_vec();
                    Ok::<_, ListError>((key, bytes))
                }
            })),
            32,
        );
        while let Some(next) = futures::StreamExt::next(&mut fetched).await {
            if rows.len() >= limit {
                next_cursor = last_consumed.clone();
                break 'days;
            }
            let (key, bytes) = next?;
            let Some(entry) = parse_entry(&bytes) else {
                tracing::warn!(name = %key, "audit.read.failed");
                last_consumed = Some(key);
                continue;
            };
            last_consumed = Some(key);
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
    }
    // A cursor whose day fell outside the window is as unknown as one whose key is gone.
    if !resumed {
        return Ok(empty());
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

    /// An explicit `from`/`to` walks exactly that span in day prefixes, newest first, both
    /// boundaries included — and a bare `yyyy-mm` `to` means the whole month, not its first day.
    #[test]
    fn days_walks_the_explicit_span() {
        let f = AuditFilter { from: Some("2026-08-30".into()), to: Some("2026-09-02".into()), ..Default::default() };
        assert_eq!(
            days(&f).unwrap(),
            vec!["2026-09/2026-09-02", "2026-09/2026-09-01", "2026-08/2026-08-31", "2026-08/2026-08-30"]
        );
        let m = AuditFilter { from: Some("2026-08".into()), to: Some("2026-08".into()), ..Default::default() };
        assert_eq!(days(&m).unwrap().len(), 31, "a bare month means every one of its days");
    }

    /// `from` after `to` is a valid, empty window — not a 422 and not a walk to year zero.
    #[test]
    fn days_with_from_after_to_is_empty() {
        let f = AuditFilter { from: Some("2026-08".into()), to: Some("2026-06".into()), ..Default::default() };
        assert_eq!(days(&f).unwrap(), Vec::<String>::new());
    }

    /// The bound, not the window, is what stops an ancient `from` — otherwise an empty log would
    /// be one listing per day back to the epoch.
    #[test]
    fn days_stops_at_the_bound() {
        let f = AuditFilter { from: Some("1970-01".into()), ..Default::default() };
        assert_eq!(days(&f).unwrap().len(), MAX_DAYS_WALKED);
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
    fn days_rejects_a_malformed_date() {
        let f = AuditFilter { from: Some("not-a-date".into()), ..Default::default() };
        assert!(matches!(days(&f), Err(ListError::InvalidFilter(msg)) if msg.contains("from")));
    }

    /// A store that remembers which prefixes were listed: the whole point of the day walk is the
    /// listings it DOESN'T make, which no assertion on the rows can see.
    #[derive(Debug)]
    struct Recorder {
        inner: slatedb::object_store::memory::InMemory,
        listed: std::sync::Mutex<Vec<String>>,
    }

    impl std::fmt::Display for Recorder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Recorder")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Recorder {
        async fn put_opts(
            &self,
            location: &OsPath,
            payload: PutPayload,
            opts: slatedb::object_store::PutOptions,
        ) -> slatedb::object_store::Result<slatedb::object_store::PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }
        async fn put_multipart_opts(
            &self,
            location: &OsPath,
            opts: slatedb::object_store::PutMultipartOptions,
        ) -> slatedb::object_store::Result<Box<dyn slatedb::object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }
        async fn get_opts(
            &self,
            location: &OsPath,
            options: slatedb::object_store::GetOptions,
        ) -> slatedb::object_store::Result<slatedb::object_store::GetResult> {
            self.inner.get_opts(location, options).await
        }
        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<'static, slatedb::object_store::Result<OsPath>>,
        ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<OsPath>> {
            self.inner.delete_stream(locations)
        }
        fn list(
            &self,
            prefix: Option<&OsPath>,
        ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::ObjectMeta>> {
            self.inner.list(prefix)
        }
        /// The day walk reaches a day as an offset into its month, so the OFFSET is what says which
        /// days were touched — recording the prefix alone would show only the month.
        fn list_with_offset(
            &self,
            prefix: Option<&OsPath>,
            offset: &OsPath,
        ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::ObjectMeta>> {
            self.listed.lock().unwrap().push(offset.to_string());
            self.inner.list_with_offset(prefix, offset)
        }
        async fn list_with_delimiter(
            &self,
            prefix: Option<&OsPath>,
        ) -> slatedb::object_store::Result<slatedb::object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &OsPath,
            to: &OsPath,
            options: slatedb::object_store::CopyOptions,
        ) -> slatedb::object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// 3 days x 5 rows, newest day first. Returns the store and the day prefixes written.
    async fn three_days(actor_3_days_ago: &str) -> (Arc<dyn ObjectStore>, Arc<Recorder>, Vec<String>) {
        let rec = Arc::new(Recorder {
            inner: slatedb::object_store::memory::InMemory::new(),
            listed: std::sync::Mutex::new(Vec::new()),
        });
        let os: Arc<dyn ObjectStore> = rec.clone();
        let today = chrono::Utc::now().date_naive();
        let mut prefixes = Vec::new();
        for back in 0..3i64 {
            let d = today - chrono::Duration::days(back);
            prefixes.push(format!("audit/{}", d.format("%Y-%m/%Y-%m-%d")));
            for i in 0..5 {
                let entry = AuditEntry {
                    ts: format!("{}T10:0{i}:00Z", d.format("%Y-%m-%d")),
                    actor: if back == 2 { actor_3_days_ago.to_string() } else { "op@example.com".into() },
                    action: "deny".into(),
                    target: format!("d{back}r{i}"),
                    reason: None,
                    result: "ok".into(),
                };
                record(&os, &entry).await.unwrap();
            }
        }
        (os, rec, prefixes)
    }

    fn listed(rec: &Recorder) -> Vec<String> {
        rec.listed.lock().unwrap().clone()
    }

    /// One screenful must cost one day's listing. This is the whole fix (2026-09-12): the old walk
    /// listed the entire month before it knew it had enough.
    #[tokio::test]
    async fn a_short_limit_lists_only_todays_prefix() {
        let (os, rec, prefixes) = three_days("op@example.com").await;
        rec.listed.lock().unwrap().clear();
        let page = list(&os, AuditFilter::default(), None, 4).await.unwrap();
        assert_eq!(page.rows.len(), 4);
        assert_eq!(listed(&rec), vec![prefixes[0].clone()]);
    }

    /// Rows come back newest-first ACROSS a day boundary, not just within one day — the day walk
    /// is what has to preserve that now, since each day is sorted on its own.
    #[tokio::test]
    async fn order_is_reverse_lexicographic_across_a_day_boundary() {
        let (os, _rec, _) = three_days("op@example.com").await;
        let page = list(&os, AuditFilter::default(), None, 100).await.unwrap();
        assert_eq!(page.rows.len(), 15);
        let ts: Vec<&str> = page.rows.iter().map(|r| r.ts.as_str()).collect();
        let mut sorted = ts.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(ts, sorted);
    }

    /// A cursor resumes in its OWN day and never re-lists the newer ones.
    #[tokio::test]
    async fn a_cursor_resumes_in_its_own_day() {
        let (os, rec, prefixes) = three_days("op@example.com").await;
        let first = list(&os, AuditFilter::default(), None, 7).await.unwrap();
        let cursor = first.next_cursor.clone().expect("more rows remain");
        assert!(day_of_key(&cursor).unwrap().ends_with(&prefixes[1][6..]), "the 8th row is yesterday's");
        rec.listed.lock().unwrap().clear();
        // Two of yesterday's three remaining rows: the page fills inside that one day, so the walk
        // never reaches the day before it either.
        let page = list(&os, AuditFilter::default(), Some(cursor), 2).await.unwrap();
        assert_eq!(page.rows.len(), 2);
        assert!(!listed(&rec).contains(&prefixes[0]), "today was already paged");
        assert_eq!(listed(&rec), vec![prefixes[1].clone()]);
    }

    /// A filter that only matches old rows keeps walking day by day until it has its matches —
    /// exactly three prefixes here, never the whole window.
    #[tokio::test]
    async fn a_filter_walks_day_by_day_until_it_matches() {
        let (os, rec, prefixes) = three_days("audit@example.com").await;
        rec.listed.lock().unwrap().clear();
        let f = AuditFilter { actor: Some("audit@example.com".into()), ..Default::default() };
        let page = list(&os, f, None, 4).await.unwrap();
        assert_eq!(page.rows.len(), 4);
        assert_eq!(listed(&rec), prefixes);
    }

    /// An empty log walks the bound and stops — never one listing per day back to the epoch.
    #[tokio::test]
    async fn the_bound_stops_an_empty_walk() {
        let rec = Arc::new(Recorder {
            inner: slatedb::object_store::memory::InMemory::new(),
            listed: std::sync::Mutex::new(Vec::new()),
        });
        let os: Arc<dyn ObjectStore> = rec.clone();
        let f = AuditFilter { from: Some("1970-01".into()), ..Default::default() };
        let page = list(&os, f, None, 10).await.unwrap();
        assert!(page.rows.is_empty());
        assert_eq!(listed(&rec).len(), MAX_DAYS_WALKED);
    }
}
