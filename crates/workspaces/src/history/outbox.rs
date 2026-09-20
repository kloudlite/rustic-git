use super::{events::EventRow, History, HistoryError};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use slatedb::object_store::{path::Path, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use std::sync::Arc;

const OUTBOX_PREFIX: &str = "history-outbox/";
const DRAIN_BATCH: usize = 64;

fn retain_candidate(paths: &mut Vec<Path>, cursor: Option<&Path>, path: Path) {
    if cursor.is_some_and(|current| path.as_ref() <= current.as_ref()) {
        return;
    }
    paths.push(path);
    paths.sort_by(|left, right| left.as_ref().cmp(right.as_ref()));
    if paths.len() > DRAIN_BATCH {
        paths.pop();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxStatus { pub count: usize, pub oldest: Option<chrono::DateTime<chrono::Utc>> }

fn outbox_path(value: &serde_json::Value, payload: &[u8]) -> Path {
    let mut hash = Sha256::new();
    for field in ["kind", "ts", "id"] {
        hash.update(value.get(field).and_then(serde_json::Value::as_str).unwrap_or_default().as_bytes());
        hash.update([0]);
    }
    hash.update(payload);
    Path::from(format!("{OUTBOX_PREFIX}{}.json", hex::encode(hash.finalize())))
}

async fn persist_row(os: &dyn slatedb::object_store::ObjectStore, row: &EventRow) -> Result<(), HistoryError> {
    let value = row.to_json();
    let payload = serde_json::to_vec(&value).map_err(|e| HistoryError::Outbox(e.to_string()))?;
    match os.put_opts(&outbox_path(&value, &payload), PutPayload::from(payload), PutOptions { mode: PutMode::Create, ..Default::default() }).await {
        Ok(_) | Err(slatedb::object_store::Error::AlreadyExists { .. }) => Ok(()),
        Err(e) => Err(HistoryError::Outbox(e.to_string())),
    }
}

pub async fn enqueue_events(h: &History, rows: &[EventRow]) -> Result<(), HistoryError> {
    let Some(os) = h.outbox() else { return super::events::write_events(h, rows).await };
    for row in rows { persist_row(os.as_ref(), row).await?; }
    Ok(())
}

pub async fn outbox_status(h: &History) -> Result<OutboxStatus, HistoryError> {
    let Some(os) = h.outbox() else { return Ok(OutboxStatus { count: 0, oldest: None }) };
    let mut count = 0;
    let mut oldest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut listing = os.list(Some(&Path::from(OUTBOX_PREFIX)));
    while let Some(meta) = listing.next().await {
        let meta = meta.map_err(|e| HistoryError::Outbox(e.to_string()))?;
        count += 1;
        oldest = Some(oldest.map_or(meta.last_modified, |current| current.min(meta.last_modified)));
    }
    Ok(OutboxStatus { count, oldest })
}

pub async fn drain_outbox_once(h: &History) -> Result<usize, HistoryError> {
    let Some(os) = h.outbox() else { return Ok(0) };
    let mut paths = Vec::with_capacity(DRAIN_BATCH);
    let prefix = Path::from(OUTBOX_PREFIX);
    let cursor = h.outbox_cursor().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    // `list_with_offset` when a cursor exists: production runs on S3/Azure, both of which push the
    // offset down server-side, so a pass with nothing new to drain costs one bounded listing
    // instead of the WHOLE `history-outbox/` prefix (unbounded with ClickHouse down and the outbox
    // growing). `list` (unfiltered) on the very first pass, when there is no cursor yet.
    // `retain_candidate` and the sort below are UNCHANGED: this must not assume the listing is
    // ordered — the local filesystem store (used by `env_file`-shaped tests) is not.
    let mut listing = match &cursor {
        Some(c) => os.list_with_offset(Some(&prefix), c),
        None => os.list(Some(&prefix)),
    };
    while let Some(meta) = listing.next().await {
        let path = meta.map_err(|e| HistoryError::Outbox(e.to_string()))?.location;
        retain_candidate(&mut paths, cursor.as_ref(), path);
    }
    paths.sort_by(|left, right| left.as_ref().cmp(right.as_ref()));
    let last_path = paths.last().cloned();
    let mut rows = Vec::with_capacity(paths.len());
    let mut first_error = None;
    for path in paths {
        let result = match os.get(&path).await { Ok(result) => result, Err(e) => { first_error.get_or_insert(HistoryError::Outbox(e.to_string())); continue } };
        let payload = match result.bytes().await { Ok(payload) => payload, Err(e) => { first_error.get_or_insert(HistoryError::Outbox(e.to_string())); continue } };
        let row: serde_json::Value = match serde_json::from_slice(&payload) { Ok(row) => row, Err(e) => { first_error.get_or_insert(HistoryError::Outbox(e.to_string())); continue } };
        if outbox_path(&row, &payload) != path {
            first_error.get_or_insert(HistoryError::Outbox(format!("outbox key does not match its payload: {path}")));
            continue;
        }
        rows.push((path, row));
    }
    if let Some(last) = last_path {
        *h.outbox_cursor().lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(last.clone());
    } else {
        *h.outbox_cursor().lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
    if rows.is_empty() { return first_error.map_or(Ok(0), Err) }
    let mut drained = 0;
    if let Err(e) = h.insert("events", &rows.iter().map(|(_, row)| row.clone()).collect::<Vec<_>>()).await {
        first_error.get_or_insert(e);
        for (path, row) in rows {
            if let Err(e) = h.insert("events", &[row]).await { first_error.get_or_insert(e); continue }
            match os.delete(&path).await {
                Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => drained += 1,
                Err(e) => { first_error.get_or_insert(HistoryError::Outbox(e.to_string())); }
            }
        }
    } else {
        for (path, _) in rows {
            match os.delete(&path).await {
                Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => drained += 1,
                Err(e) => { first_error.get_or_insert(HistoryError::Outbox(e.to_string())); }
            }
        }
    }
    first_error.map_or(Ok(drained), Err)
}

/// The delay before the next pass: doubles every time a pass drained nothing (`Ok(0)`, or an
/// error — the outbox isn't shrinking either way), capped at 60 s so a long ClickHouse outage
/// doesn't stretch the wait past a minute; reset to the 2 s floor by ANY drained row, because a
/// pass that found work means there is likely more right behind it.
fn next_idle(current: std::time::Duration, drained: Option<usize>) -> std::time::Duration {
    const FLOOR: std::time::Duration = std::time::Duration::from_secs(2);
    const CAP: std::time::Duration = std::time::Duration::from_secs(60);
    match drained {
        Some(n) if n > 0 => FLOOR,
        _ => (current * 2).min(CAP),
    }
}

pub async fn drain_outbox_forever(history: Arc<History>) {
    const IDLE: std::time::Duration = std::time::Duration::from_secs(2);
    const STATUS_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
    let mut last_status = std::time::Instant::now().checked_sub(STATUS_EVERY).unwrap_or_else(std::time::Instant::now);
    let mut idle = IDLE;
    loop {
        if last_status.elapsed() >= STATUS_EVERY {
            last_status = std::time::Instant::now();
            match outbox_status(&history).await {
                Ok(status) => {
                    kloudlite_core::metrics::set_gauge("history_outbox_count", status.count as f64);
                    let age = status.oldest.map(|oldest| (chrono::Utc::now() - oldest).num_seconds().max(0) as f64).unwrap_or(0.0);
                    kloudlite_core::metrics::set_gauge("history_outbox_oldest_age_seconds", age);
                }
                Err(error) => tracing::warn!(error = %error, "history.outbox.status.failed"),
            }
        }
        match drain_outbox_once(&history).await {
            Ok(n) => {
                idle = next_idle(idle, Some(n));
                if n == 0 { tokio::time::sleep(idle).await; }
            }
            Err(e) => {
                tracing::warn!(error = %e, "history.outbox.drain.failed");
                idle = next_idle(idle, None);
                tokio::time::sleep(idle).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unordered_listing_keeps_smallest_candidates_after_cursor() {
        let mut paths = Vec::new();
        for index in (0..65).rev() {
            retain_candidate(&mut paths, None, Path::from(format!("{OUTBOX_PREFIX}{index:03}.json")));
        }
        assert_eq!(paths.len(), DRAIN_BATCH);
        assert_eq!(paths.first().unwrap().as_ref(), "history-outbox/000.json");
        assert_eq!(paths.last().unwrap().as_ref(), "history-outbox/063.json");
        let cursor = paths.last().cloned();
        paths.clear();
        retain_candidate(&mut paths, cursor.as_ref(), Path::from("history-outbox/999.json"));
        assert_eq!(paths.last().unwrap().as_ref(), "history-outbox/999.json");
    }

    #[test]
    fn next_idle_doubles_on_empty_or_error_and_resets_on_any_drain() {
        use std::time::Duration;
        let mut delay = Duration::from_secs(2);
        for expected in [4, 8, 16, 32, 60, 60] {
            delay = next_idle(delay, None);
            assert_eq!(delay, Duration::from_secs(expected), "doubling toward the 60 s cap");
        }
        assert_eq!(next_idle(delay, Some(0)), Duration::from_secs(120).min(Duration::from_secs(60)), "Ok(0) still doubles (capped)");
        assert_eq!(next_idle(Duration::from_secs(32), Some(1)), Duration::from_secs(2), "any drained row resets to the floor");
        assert_eq!(next_idle(Duration::from_secs(60), Some(5)), Duration::from_secs(2), "draining resets even from the cap");
    }
}
