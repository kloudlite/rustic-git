//! Lease-taking: opening a repo's database, single-flighted, through fencing detection.

use super::{path, Entry, FencedError, Pool};
use crate::Result;
use slatedb::object_store::ObjectStoreExt;
use slatedb::Db;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

impl Pool {
    /// The database for a repo, opening it if this node does not already hold it warm.
    ///
    /// A closed handle is evicted and reported, NOT reopened. Under routing, "closed" almost always
    /// means "fenced": another node opened this repo because it believes it owns it. Reopening here
    /// would take it straight back and turn any disagreement into a flap. The caller decides — via
    /// the routing rule — whether this node should hold the repo, and only then reopens.
    pub async fn get(self: &Arc<Self>, owner: &str, name: &str) -> Result<Arc<Db>> {
        let h = self.get_once(owner, name).await?;
        match h.status().close_reason {
            None => Ok(h),
            Some(slatedb::CloseReason::Fenced) => {
                self.evict_if_same(owner, name, &h).await;
                drop(h);
                Err(FencedError { repo: format!("{owner}/{name}") }.into())
            }
            // Closed clean (a shutdown racing this request) or by a panicked background task:
            // nobody else holds the epoch, so this is not a routing question and must not be
            // answered as one — a fence here sends the caller off to force-claim a repo nobody
            // took. Drop the dead handle; the next call reopens in place.
            Some(_) => {
                self.evict_if_same(owner, name, &h).await;
                drop(h);
                Err(crate::err(format!("{owner}/{name}: database was closed; retry")))
            }
        }
    }

    async fn get_once(self: &Arc<Self>, owner: &str, name: &str) -> Result<Arc<Db>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(crate::err(format!("{owner}/{name}: pool is closed")));
        }
        let key = format!("{owner}/{name}");
        let entry = {
            let mut map = self.entries.lock().unwrap();
            let e = map
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(Entry {
                        db: tokio::sync::OnceCell::new(),
                        last_used: std::sync::Mutex::new(Instant::now()),
                        last_flush: std::sync::Mutex::new(Instant::now()),
                        releasing: AtomicBool::new(false),
                        closed: Default::default(),
                    })
                })
                .clone();
            *e.last_used.lock().unwrap() = Instant::now();
            e
        };
        // Outside the map lock: opening is slow, and holding the lock across it would serialise
        // every repo behind whichever one is currently opening.
        let handle = entry
            .db
            .get_or_try_init(|| async {
                // Decided here, inside the single-flight, not at the top of `get_once`: a check
                // before the slot was inserted can be answered by a `delete` that starts after it.
                // And the slot must still be OURS — an evict (a lost lease, a delete) that ran
                // while this task sat between taking the entry and opening it has already
                // removed it, and opening now would CREATE the database that delete just walked.
                // `adopt` would close it again, but the manifest write has happened by then.
                if self.deleting.lock().unwrap().contains(&key) {
                    return Err(crate::err(format!("{key}: repository is being deleted")));
                }
                let ours = self.entries.lock().unwrap().get(&key).is_some_and(|e| Arc::ptr_eq(e, &entry));
                if !ours {
                    return Err(FencedError { repo: key.clone() }.into());
                }
                self.open(owner, name).await
            })
            .await
            // A failed open leaves an empty cell, so the next caller retries rather than
            // inheriting the error. Drop the slot so a poisoned key cannot accumulate — but only
            // if it is still OUR slot: an evict and a reopen may have replaced it while we were
            // failing, and removing the successor's entry would make `adopt` close its healthy
            // database and report a fence that only a local open error caused.
            .inspect_err(|_| {
                let mut map = self.entries.lock().unwrap();
                if map.get(&key).is_some_and(|e| Arc::ptr_eq(e, &entry)) {
                    map.remove(&key);
                }
            })?
            .clone();
        let handle = self.adopt(&key, &entry, handle).await?;
        self.enforce_bound().await;
        Ok(handle)
    }

    /// The last step of an open: keep the handle only if the map still names this slot.
    ///
    /// An evict that ran DURING the open (a lost lease, a fence) found no handle to close and
    /// removed the slot. Adopting the handle now would leave a database open that nothing names
    /// and no sweep can reach — holding the writer epoch until the process dies, which is the
    /// fence the next owner will hit. Close it and report a fence: the caller re-routes.
    // ponytail: `App::on_fenced` evicts blindly by key, so this synthesized fence can close a
    // sibling's fresh healthy entry — one extra flap, self-healing. Upgrade: have `on_fenced`
    // evict only when the slot's own handle actually reports closed.
    pub(super) async fn adopt(&self, key: &str, entry: &Arc<Entry>, handle: Arc<Db>) -> Result<Arc<Db>> {
        let current = self.entries.lock().unwrap().get(key).is_some_and(|e| Arc::ptr_eq(e, entry));
        if current {
            return Ok(handle);
        }
        drop(handle);
        entry.close().await;
        Err(FencedError { repo: key.to_string() }.into())
    }

    pub(super) async fn open(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        Ok(Arc::new(
            Db::builder(path(owner, name), self.os.clone())
                .with_settings(self.settings.clone())
                .with_db_cache(self.db_cache.clone())
                .build()
                .await?,
        ))
    }

    /// Drop a repo from the pool and close it, now, with no release and no drain. Two callers:
    /// a write that came back fenced (another node already has the epoch), and the renewal task
    /// finding this node has lost the lease. Both mean the map no longer names us, so there is
    /// nothing to give back and nothing to drain for — holding the handle any longer is the
    /// lifecycle invariant's other half broken.
    pub async fn evict(&self, owner: &str, name: &str) {
        let entry = self
            .entries
            .lock()
            .unwrap()
            .remove(&format!("{owner}/{name}"));
        // Dropping the slot without closing would leave a database open on a lease we were just
        // told we had lost (the `renew_once` caller), which is the invariant broken the other way.
        if let Some(e) = entry {
            Self::close_bounded(e).await;
        }
    }

    /// Delete a repo's database files, closing this node's handle first.
    ///
    /// `evict` alone was not enough: an open in flight when the slot is removed still finishes,
    /// and its manifest and WAL writes race the file deletes below — the ghost repo that
    /// `admin purge-ghost-repo` exists to clean up. `Entry::close` now waits for that open and
    /// closes its handle before the first delete, and `deleting` turns every open that starts
    /// later into an error rather than a fresh database.
    pub async fn delete(&self, owner: &str, name: &str) -> Result<()> {
        let key = format!("{owner}/{name}");
        self.deleting.lock().unwrap().insert(key.clone());
        let entry = self.entries.lock().unwrap().remove(&key);
        if let Some(e) = entry {
            Self::close_bounded(e).await;
        }
        // ponytail: a close that outlives `FLUSH_PATIENCE` may still be flushing while these
        // deletes run; the same ceiling `evict` accepts. Upgrade: refuse the delete instead.
        let prefix = slatedb::object_store::path::Path::from(path(owner, name));
        let deleted = async {
            let locs: Vec<_> = futures::TryStreamExt::try_collect(futures::TryStreamExt::map_ok(
                self.os.list(Some(&prefix)),
                |m| m.location,
            ))
            .await?;
            for loc in locs {
                self.os.delete(&loc).await?;
            }
            Ok(())
        }
        .await;
        self.deleting.lock().unwrap().remove(&key);
        deleted
    }

    /// Close a handle taken out of the map, waiting at most `FLUSH_PATIENCE` for it. Both evicts
    /// run on the renewal task, so an unbounded close (an S3 flush that hangs) would stop every
    /// other lease on the node from renewing — the same failure the leader's checkpoint got a
    /// deadline for. The close is SPAWNED rather than merely timed out: dropping it mid-flush
    /// would leave the database open with nobody left to close it, whereas a detached close still
    /// finishes on its own; the handle is already out of the map either way, so no new writer can
    /// reach it. Closing flushes, which a fenced database cannot do; that error is expected and
    /// ignored.
    async fn close_bounded(e: Arc<Entry>) {
        let close = tokio::spawn(async move { e.close().await });
        if tokio::time::timeout(Self::FLUSH_PATIENCE, close).await.is_err() {
            tracing::warn!(timeout_ms = Self::FLUSH_PATIENCE.as_millis() as u64, "pool.close.stalled");
        }
    }

    /// Evict only if the map still holds the exact handle the caller saw as closed. A blind evict
    /// races a concurrent reopen: two requests observing the same fenced handle would otherwise
    /// have the second one close the first's fresh, healthy database.
    pub async fn evict_if_same(&self, owner: &str, name: &str, observed: &Arc<Db>) {
        let key = format!("{owner}/{name}");
        let entry = {
            let mut map = self.entries.lock().unwrap();
            match map.get(&key) {
                Some(e) if e.db.get().is_some_and(|cur| Arc::ptr_eq(cur, observed)) => map.remove(&key),
                _ => None,
            }
        };
        if let Some(e) = entry {
            Self::close_bounded(e).await;
        }
    }
}
