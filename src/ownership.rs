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
/// How long a node keeps serving a repo it has decided to give up, before it closes the database.
/// The entry is untouched for the whole window — this node is still the owner, on the record and
/// in fact — so a request routed here mid-drain is served, not fenced. The release comes after the
/// close; see `Pool::retire`.
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

/// The nodes that may hold repositories: every ordinal except zero.
///
/// Pod zero writes the ownership map and nothing else. It was the only node that could grant a
/// claim, so when it also held repositories a restart took both away at once — the repo lost its
/// owner and its only possible granter in the same instant, and no other node could take over
/// until it came back. Measured on a rolling restart, that cost 21 failures in 100 requests
/// against 3 for the design this replaced. Excluding it costs one node of serving capacity and
/// removes the compound failure entirely: repos live on nodes that are never the leader, so a
/// leader restart leaves them serving.
///
/// With fewer than two replicas there is no one else, so the leader serves — that keeps
/// single-node and two-node deployments working rather than refusing every request.
pub fn servers(leader: &str, replicas: u32) -> Vec<String> {
    let prefix = leader.rsplit_once('-').map(|(p, _)| p).unwrap_or(leader);
    if replicas < 2 {
        return vec![leader.to_string()];
    }
    (1..replicas).map(|i| format!("{prefix}-{i}")).collect()
}

/// Which server should take a repo nobody holds: the one holding the fewest, ties to the lowest
/// ordinal. Deterministic, so two claims racing through the leader agree.
///
/// Nodes that have announced they are draining are skipped. Releasing every lease is exactly what a
/// node does at SIGTERM, which leaves it holding zero — so without this the departing pod is the
/// MOST attractive candidate at the moment it is least able to serve, and ties to the lowest
/// ordinal make it win more often still. Every node then forwards into a draining pod until the
/// lease lapses, which is a burst of 502s in the middle of a roll.
///
/// If every server is draining the filter is ignored rather than naming nobody: a grant to a node
/// that is going away still beats refusing to answer.
pub fn least_loaded(
    servers: &[String],
    held: &[(String, Entry)],
    draining: &[String],
    now_ms: u64,
) -> Option<String> {
    let load = |s: &&String| {
        held.iter()
            .filter(|(_, e)| &e.node == *s && !is_expired(e, now_ms))
            .count()
    };
    servers
        .iter()
        .filter(|s| !draining.contains(s))
        .min_by_key(load)
        .or_else(|| servers.iter().min_by_key(load))
        .cloned()
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

/// Nodes that have announced they are shutting down. Outside `own/` so ownership scans skip it.
pub const DRAIN_PREFIX: &str = "cluster/draining/";

fn key(repo: &str) -> String {
    format!("own/{repo}")
}

/// Where the ownership map lives, alongside every repo database in the same object store.
const PATH: &str = "cluster/ownership";

/// The ownership map: one SlateDB database, opened for writing by the leader and for reading
/// (via a `FollowLatest` reader) by everyone else.
pub enum OwnershipStore {
    Writer(std::sync::Arc<slatedb::Db>),
    /// Follower. The reader is acquired lazily: only the leader's `Db::builder` creates the
    /// database, and a StatefulSet rolls in reverse ordinal order, so on a fresh cluster every
    /// follower starts before the map exists. Until the reader opens, the map reads as empty —
    /// exactly like `Solo` — which means "nothing is known to be owned" and sends every request
    /// down the claim path to the leader, the only thing that can grant ownership anyway.
    Reader(std::sync::Arc<tokio::sync::RwLock<Option<std::sync::Arc<slatedb::DbReader>>>>),
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
            let slot = std::sync::Arc::new(tokio::sync::RwLock::new(None));
            let cell = slot.clone();
            tokio::spawn(async move {
                let mut logged = false;
                loop {
                    match slatedb::DbReader::open(
                        PATH,
                        os.clone(),
                        slatedb::DbReaderMode::FollowLatest,
                        slatedb::config::DbReaderOptions {
                            manifest_poll_interval: std::time::Duration::from_millis(200),
                            ..Default::default()
                        },
                    )
                    .await
                    {
                        Ok(r) => {
                            *cell.write().await = Some(std::sync::Arc::new(r));
                            if logged {
                                eprintln!("ownership map opened"); // ponytail: eprintln
                            }
                            return;
                        }
                        Err(e) => {
                            // First failure only: the leader may not have created the map yet, and
                            // one line a second forever is noise, not signal.
                            if !logged {
                                eprintln!("ownership map not readable yet ({e}); retrying"); // ponytail: eprintln
                                logged = true;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
            });
            Ok(OwnershipStore::Reader(slot))
        }
    }

    pub async fn get(&self, repo: &str) -> crate::Result<Option<Entry>> {
        let bytes = match self {
            OwnershipStore::Writer(db) => db.get(key(repo)).await?,
            OwnershipStore::Reader(slot) => match slot.read().await.clone() {
                Some(r) => r.get(key(repo)).await?,
                // No reader yet: same answer as `Solo`, and safe for the same reason.
                None => return Ok(None),
            },
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

    /// Flush and close the map's database. Shutdown only: the leader writes with a 10ms flush
    /// interval, so its last few decisions are still in memory when the process ends.
    pub async fn close(&self) -> crate::Result<()> {
        if let OwnershipStore::Writer(db) = self {
            db.close().await?;
        }
        Ok(())
    }

    /// Every entry currently in the map, for pruning and for `/healthz` diagnostics.
    /// Announce, or withdraw, that a node is on its way out. Leader-only, like every other write.
    pub async fn set_draining(&self, node: &str, draining: bool) -> crate::Result<()> {
        let key = format!("{DRAIN_PREFIX}{node}");
        match self {
            OwnershipStore::Writer(db) => {
                if draining {
                    db.put(key, b"1".as_slice()).await?;
                } else {
                    db.delete(key).await?;
                }
                Ok(())
            }
            OwnershipStore::Reader(_) => Err(crate::err("ownership: put on a follower")),
            OwnershipStore::Solo => Ok(()),
        }
    }

    /// The nodes that have said they are shutting down.
    pub async fn draining(&self) -> crate::Result<Vec<String>> {
        let mut iter = match self {
            OwnershipStore::Writer(db) => db.scan_prefix(DRAIN_PREFIX, ..).await?,
            OwnershipStore::Reader(slot) => match slot.read().await.clone() {
                Some(r) => r.scan_prefix(DRAIN_PREFIX, ..).await?,
                None => return Ok(Vec::new()),
            },
            OwnershipStore::Solo => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        while let Some(kv) = iter.next().await? {
            if let Ok(k) = std::str::from_utf8(&kv.key) {
                if let Some(n) = k.strip_prefix(DRAIN_PREFIX) {
                    out.push(n.to_string());
                }
            }
        }
        Ok(out)
    }

    pub async fn all(&self) -> crate::Result<Vec<(String, Entry)>> {
        let prefix = "own/";
        let mut iter = match self {
            OwnershipStore::Writer(db) => db.scan_prefix(prefix, ..).await?,
            OwnershipStore::Reader(slot) => match slot.read().await.clone() {
                Some(r) => r.scan_prefix(prefix, ..).await?,
                None => return Ok(Vec::new()),
            },
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

/// Extend the lease unless the map names somebody else. `None` means the asker has genuinely lost
/// the repo — another node holds it — and the caller must close its database rather than keep
/// serving.
///
/// A lapsed clock is deliberately NOT a reason to decline. The asker is telling us, right now, that
/// it still holds this database open; if nobody else has taken it, the lease should follow the
/// handle rather than the other way round. The leader is the only node that can renew, so its own
/// downtime is exactly when leases lapse through no fault of their holders — and declining then
/// destroys healthy, actively-serving state. Measured on a rolling restart: the leader was
/// unreachable for about twenty seconds against a ten second TTL, so a node that had done nothing
/// wrong was told to close a database it was serving.
///
/// An absent entry is regranted for the same reason: the prune loop may have reaped it while the
/// leader was away, and the holder is still holding it. Safety is unchanged — if another node did
/// claim it in the meantime the map names that node, and this returns `None`.
pub fn decide_renew(current: Option<&Entry>, asker: &str, now_ms: u64) -> Option<Entry> {
    match current {
        Some(e) if e.node != asker => None,
        _ => Some(Entry {
            node: asker.to_string(),
            expires_ms: now_ms + LEASE_TTL.as_millis() as u64,
        }),
    }
}

/// Whether `asker` may drop this entry outright. Release is a plain delete — it runs only after
/// the database is already closed, so there is nothing left for a successor to fence, and no
/// reason to leave a tombstone behind. A node may only release what it still holds: a stale
/// release from a node that already lost the lease must not delete the new owner's entry.
pub fn may_release(current: Option<&Entry>, asker: &str) -> bool {
    current.is_some_and(|e| e.node == asker)
}

#[cfg(test)]
mod tests;
