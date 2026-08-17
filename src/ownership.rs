//! Who owns what: the leader's decisions, as pure functions over an optional current entry and
//! an explicit clock. No I/O here — the database and the clock belong to the caller (Task 2), so
//! this module is exhaustively testable without either.

use std::time::Duration;

/// A repo's current owner and when that ownership lapses. `expires_ms` is Unix epoch
/// milliseconds, not an `Instant`: it has to survive a round trip through SlateDB and mean the
/// same thing on whichever node reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub node: String,
    pub expires_ms: u64,
}

/// How long a fresh claim lasts before it must be renewed or is up for grabs.
pub const LEASE_TTL: Duration = Duration::from_secs(10);
/// How often a holder renews, well inside `LEASE_TTL` so a missed beat or two is not fatal.
pub const RENEW_EVERY: Duration = Duration::from_secs(3);
/// How long a released entry stays valid after release, so a database that is still closing
/// keeps its owner on record. See `decide_release`.
pub const DRAIN: Duration = Duration::from_millis(500);

/// The reply to a claim: either the asker now owns it, or someone else already does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    Granted(Entry),
    HeldBy(Entry),
}

/// The leader for a node's own repos: `rustic-git-N` always answers to the pod at ordinal 0.
/// Errors if the name has no `-{ordinal}` suffix to replace.
pub fn leader_of(self_name: &str) -> crate::Result<String> {
    let (prefix, ordinal) = self_name
        .rsplit_once('-')
        .ok_or_else(|| crate::err(format!("{self_name}: no -<ordinal> suffix")))?;
    ordinal
        .parse::<u32>()
        .map_err(|_| crate::err(format!("{self_name}: {ordinal} is not an ordinal")))?;
    Ok(format!("{prefix}-0"))
}

pub fn is_expired(e: &Entry, now_ms: u64) -> bool {
    now_ms >= e.expires_ms
}

/// Grant if nobody holds it, the holder's lease has lapsed, or the asker already holds it
/// (idempotent re-claim — a restarted node re-claiming what it already has must not be told
/// someone else has it). Otherwise report who does.
pub fn decide_claim(current: Option<&Entry>, asker: &str, now_ms: u64) -> Grant {
    match current {
        Some(e) if !is_expired(e, now_ms) && e.node != asker => Grant::HeldBy(e.clone()),
        _ => Grant::Granted(Entry {
            node: asker.to_string(),
            expires_ms: now_ms + LEASE_TTL.as_millis() as u64,
        }),
    }
}

/// Extend the lease only if the asker still holds it and it has not already lapsed. `None` means
/// the asker has lost it — the caller must close its database rather than keep serving.
pub fn decide_renew(current: Option<&Entry>, asker: &str, now_ms: u64) -> Option<Entry> {
    let e = current?;
    if e.node == asker && !is_expired(e, now_ms) {
        Some(Entry { node: asker.to_string(), expires_ms: now_ms + LEASE_TTL.as_millis() as u64 })
    } else {
        None
    }
}

/// Release is not a delete: the entry stays valid for `DRAIN` more so a claim that lands while
/// the holder is still closing its database is told who holds it, not granted. Granting it would
/// let a new opener race the old one's close and get fenced by it.
pub fn decide_release(current: Option<&Entry>, asker: &str, now_ms: u64) -> Option<Entry> {
    let e = current?;
    if e.node == asker {
        Some(Entry { node: asker.to_string(), expires_ms: now_ms + DRAIN.as_millis() as u64 })
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
