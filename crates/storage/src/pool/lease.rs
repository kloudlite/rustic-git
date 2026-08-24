//! Lease-taking: opening a repo's database, single-flighted, through fencing detection.

use super::{path, Entry, FencedError, Pool};
use crate::Result;
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
            .get_or_try_init(|| self.open(owner, name))
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
        let _ = handle.close().await;
        Err(FencedError { repo: key.to_string() }.into())
    }

    pub(super) async fn open(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        Ok(Arc::new(
            Db::builder(path(owner, name), self.os.clone())
                .with_settings(self.settings.clone())
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
        // `Arc::into_inner` fails whenever another task still holds the entry — which `get_once`
        // does across its whole open — so take the handle out of the shared entry instead. Dropping
        // the slot without closing would leave a database open on a lease we were just told we had
        // lost (the `renew_once` caller), which is the invariant broken the other way round.
        let handle = match entry {
            Some(e) => match Arc::try_unwrap(e) {
                Ok(e) => e.db.into_inner(),
                Err(shared) => shared.db.get().cloned(),
            },
            None => None,
        };
        // Closing flushes, which a fenced database cannot do; the error is expected and ignored.
        if let Some(h) = handle {
            let _ = h.close().await;
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
        // Same fallback as `evict`: another task (e.g. this call's own caller, still holding
        // `observed`) may keep the `Entry` Arc alive, so `try_unwrap` can fail even though we
        // decided to evict — take a clone of the handle to close instead of losing it.
        let handle = match entry {
            Some(e) => match Arc::try_unwrap(e) {
                Ok(e) => e.db.into_inner(),
                Err(shared) => shared.db.get().cloned(),
            },
            None => None,
        };
        // Closing flushes, which a fenced database cannot do; the error is expected and ignored.
        if let Some(h) = handle {
            let _ = h.close().await;
        }
    }
}
