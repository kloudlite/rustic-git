use super::{events::EventRow, History, HistoryError};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use slatedb::object_store::{path::Path, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use std::sync::Arc;

const OUTBOX_PREFIX: &str = "history-outbox/";
const DRAIN_BATCH: usize = 64;

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
    let mut oldest = None;
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
    let mut listing = match cursor.as_ref() {
        Some(cursor) => os.list_with_offset(Some(&prefix), cursor),
        None => os.list(Some(&prefix)),
    };
    while paths.len() < DRAIN_BATCH {
        let Some(meta) = listing.next().await else { break };
        paths.push(meta.map_err(|e| HistoryError::Outbox(e.to_string()))?.location);
    }
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
    if let Some(last) = paths.last() {
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

pub async fn drain_outbox_forever(history: Arc<History>) {
    const IDLE: std::time::Duration = std::time::Duration::from_secs(2);
    const STATUS_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
    let mut last_status = std::time::Instant::now().checked_sub(STATUS_EVERY).unwrap_or_else(std::time::Instant::now);
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
            Ok(0) => tokio::time::sleep(IDLE).await,
            Ok(_) => {}
            Err(e) => { tracing::warn!(error = %e, "history.outbox.drain.failed"); tokio::time::sleep(IDLE).await; }
        }
    }
}
