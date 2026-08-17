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

/// Wall-clock milliseconds since the epoch. Entries cross nodes, so the clock has to be the one
/// thing every node already agrees on well enough; the leases are seconds long and NTP skew is
/// milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as u64
}

/// A node of the fleet, as routing needs it: the stable pod name (what the map stores) and where
/// its peer listener is (what a forward needs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub addr: String,
}

/// Where a request for a repo belongs. The map answers this; nothing is derived or probed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// This node owns it and serves it.
    Local,
    /// Another node owns it. Forward there.
    Peer(Peer),
    /// Nobody may safely serve it right now — 503, and let the client retry. The leader being
    /// unreachable lands here, deliberately: an unclaimable repo is not served by whoever asked.
    Unavailable,
}

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

impl Entry {
    /// `"{node}\n{expires_ms}"` — two fields, not worth a serde_json dependency this project
    /// doesn't otherwise have. A node name is validated elsewhere and cannot contain a newline,
    /// but this asserts rather than trusts that.
    fn encode(&self) -> Vec<u8> {
        assert!(!self.node.contains('\n'), "node name must not contain a newline: {}", self.node);
        format!("{}\n{}", self.node, self.expires_ms).into_bytes()
    }

    fn decode(bytes: &[u8]) -> crate::Result<Entry> {
        let s = std::str::from_utf8(bytes).map_err(|e| crate::err(format!("ownership entry: {e}")))?;
        let (node, expires_ms) = s
            .split_once('\n')
            .ok_or_else(|| crate::err(format!("ownership entry: malformed: {s:?}")))?;
        let expires_ms = expires_ms
            .parse::<u64>()
            .map_err(|e| crate::err(format!("ownership entry: bad expires_ms: {e}")))?;
        Ok(Entry { node: node.to_string(), expires_ms })
    }
}

fn key(repo: &str) -> String {
    format!("own/{repo}")
}

/// Where the ownership map lives, alongside every repo database in the same object store.
const PATH: &str = "cluster/ownership";

/// The ownership map: one SlateDB database, opened for writing by the leader and for reading
/// (via a `FollowLatest` reader) by everyone else.
pub enum OwnershipStore {
    Writer(std::sync::Arc<slatedb::Db>),
    Reader(std::sync::Arc<slatedb::DbReader>),
    /// Single node: there is nothing to coordinate, so there is no database. The map is always
    /// empty, which makes every repo unowned, which makes this node claim it and own it. No
    /// object-store traffic, no leader, no renewal.
    Solo,
}

impl OwnershipStore {
    /// Leader: opens for writing with background compaction off. A follower's `FollowLatest`
    /// reader has no protection from garbage collection — SlateDB's own docs warn that reads
    /// using an older manifest can fail once the objects they reference are deleted. The map is a
    /// few dozen tiny keys, so compaction buys nothing here and risks breaking every follower's
    /// read; leave it off.
    ///
    /// Follower: opens read-only, polling the manifest so its view of the map catches up on its
    /// own schedule rather than the request path.
    pub async fn open(os: std::sync::Arc<dyn slatedb::object_store::ObjectStore>, is_leader: bool) -> crate::Result<OwnershipStore> {
        if is_leader {
            let settings = slatedb::config::Settings {
                flush_interval: Some(std::time::Duration::from_millis(10)),
                compactor_options: None,
                garbage_collector_options: None,
                ..Default::default()
            };
            let db = slatedb::Db::builder(PATH, os).with_settings(settings).build().await?;
            Ok(OwnershipStore::Writer(std::sync::Arc::new(db)))
        } else {
            let reader = slatedb::DbReader::open(
                PATH,
                os,
                slatedb::DbReaderMode::FollowLatest,
                slatedb::config::DbReaderOptions {
                    manifest_poll_interval: std::time::Duration::from_millis(200),
                    ..Default::default()
                },
            )
            .await?;
            Ok(OwnershipStore::Reader(std::sync::Arc::new(reader)))
        }
    }

    pub async fn get(&self, repo: &str) -> crate::Result<Option<Entry>> {
        let bytes = match self {
            OwnershipStore::Writer(db) => db.get(key(repo)).await?,
            OwnershipStore::Reader(r) => r.get(key(repo)).await?,
            OwnershipStore::Solo => return Ok(None),
        };
        bytes.as_deref().map(Entry::decode).transpose()
    }

    /// Leader only. A follower writing is a bug, not a fallback — this errors rather than
    /// silently opening a writer or dropping the write.
    pub async fn put(&self, repo: &str, e: &Entry) -> crate::Result<()> {
        match self {
            OwnershipStore::Writer(db) => {
                db.put(key(repo), e.encode()).await?;
                Ok(())
            }
            OwnershipStore::Reader(_) => Err(crate::err("ownership: put on a follower")),
            OwnershipStore::Solo => Ok(()),
        }
    }

    /// Drop an entry. Leader only, and only for entries that have already expired — the prune
    /// task. A live entry is shortened by `decide_release`, never deleted; see its comment.
    pub async fn delete(&self, repo: &str) -> crate::Result<()> {
        match self {
            OwnershipStore::Writer(db) => {
                db.delete(key(repo)).await?;
                Ok(())
            }
            OwnershipStore::Reader(_) => Err(crate::err("ownership: delete on a follower")),
            OwnershipStore::Solo => Ok(()),
        }
    }

    /// Every entry currently in the map, for pruning and for `/healthz` diagnostics.
    pub async fn all(&self) -> crate::Result<Vec<(String, Entry)>> {
        let prefix = "own/";
        let mut iter = match self {
            OwnershipStore::Writer(db) => db.scan_prefix(prefix, ..).await?,
            OwnershipStore::Reader(r) => r.scan_prefix(prefix, ..).await?,
            OwnershipStore::Solo => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        while let Some(kv) = iter.next().await? {
            let repo = std::str::from_utf8(&kv.key)
                .map_err(|e| crate::err(format!("ownership key: {e}")))?
                .strip_prefix(prefix)
                .ok_or_else(|| crate::err("ownership key: missing own/ prefix"))?
                .to_string();
            out.push((repo, Entry::decode(&kv.value)?));
        }
        Ok(out)
    }
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
