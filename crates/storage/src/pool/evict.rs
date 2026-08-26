//! Eviction, `max_warm` pressure, and release-on-close.

use super::Pool;
use slatedb::Db;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

impl Pool {
    /// Close databases nobody is using: idle past the TTL, or the least recently used once the
    /// pool is over `max_warm`.
    pub async fn sweep(self: &Arc<Self>) {
        let picked = self.evictable(Instant::now());
        self.retire(picked).await
    }

    pub(super) async fn enforce_bound(self: &Arc<Self>) {
        if self.entries.lock().unwrap().len() > self.max_warm() {
            let picked = self.evictable(Instant::now());
            self.retire(picked).await
        }
    }

    /// Pick what may be closed, under one lock, and mark it releasing. An entry still referenced
    /// outside the pool is skipped — that is a request in flight, and closing under it would fail
    /// the request. It becomes evictable on a later sweep.
    ///
    /// The entry is deliberately LEFT IN THE MAP: this node still holds the lease until the drain
    /// is over, so a request arriving meanwhile must find the handle it is still being routed to.
    /// Removing it here would make the pool re-open a database it is about to close — two handles
    /// on one repo, and a fence.
    pub(super) fn evictable(&self, now: Instant) -> Vec<(String, Arc<Db>)> {
        let map = self.entries.lock().unwrap();
        let mut idle: Vec<(Instant, String)> = map
            .iter()
            .filter(|(_, e)| !e.releasing.load(Ordering::SeqCst))
            .filter(|(_, e)| e.db.get().is_none_or(|db| Arc::strong_count(db) == 1))
            .map(|(k, e)| (*e.last_used.lock().unwrap(), k.clone()))
            .collect();
        idle.sort_by_key(|(t, _)| *t); // oldest first
        let over = map.len().saturating_sub(self.max_warm());
        let mut out = Vec::new();
        for (i, (last, key)) in idle.into_iter().enumerate() {
            if i >= over && now.duration_since(last) < self.idle_ttl() {
                continue; // young enough, and we are not over the bound
            }
            // The handle comes FIRST, and the flag only with it. An entry whose open is still in
            // flight (inserted by `get_once`, `OnceCell` not yet filled) has no handle to release
            // or close; flagging it would strand it — never released, never closed, and skipped by
            // `warm_repos`, so its lease would lapse under a database this node still holds open.
            // Skip it; the next sweep picks it up once the open has finished.
            if let Some(db) = map.get(&key).and_then(|e| e.db.get()).cloned() {
                map[&key].releasing.store(true, Ordering::SeqCst);
                out.push((key, db));
            }
        }
        out
    }

    /// Drain, close, THEN give the leases back. Spawned, because the drain is half a second and
    /// the sweeper must not block for it. With no hook there is no lease to give back and nothing
    /// to wait for.
    ///
    /// The order is the whole point. Through the drain this node is still the owner on the record
    /// AND still holds the handle, so a request routed here by a follower whose map is behind is
    /// served rather than fenced. The entry only disappears once the database is shut, so the next
    /// claimer opens a repo nobody holds — there is nothing left to fence.
    async fn retire(self: &Arc<Self>, picked: Vec<(String, Arc<Db>)>) {
        if picked.is_empty() {
            return;
        }
        let Some(hook) = self.hook() else {
            // Should not happen with a hook set: `serve()` holds the `App` for the process's whole
            // life. Closing is still better than leaking handles, but say so — this closes without
            // releasing, which is the ordering the design forbids.
            tracing::error!(count = picked.len(), "release hook unavailable: closing database(s) WITHOUT releasing; the lease may outlive the handle");
            self.close_all(picked).await;
            return;
        };
        let pool = self.clone();
        let h = tokio::spawn(async move {
            // Still the owner, still serving, for exactly as long as a follower's stale copy of
            // the map can still send us traffic.
            tokio::time::sleep(crate::ownership::DRAIN).await;
            let keys: Vec<String> = picked.iter().map(|(k, _)| k.clone()).collect();
            let skipped = pool.close_all(picked).await;
            // Release ONLY what actually closed. A handle skipped for being in use went warm again
            // during the drain: it keeps its lease, keeps serving, and a later sweep retries it.
            // Deleting its entry here would leave this node holding an open database that the map
            // says nobody owns — the lifecycle invariant broken the other way round.
            for repo in keys.iter().filter(|k| !skipped.iter().any(|(s, _)| s == *k)) {
                hook.release(repo.clone()).await;
            }
        });
        // Tracked so shutdown can wait for it: a retire dropped mid-sleep would never close its
        // databases (WAL replay on the next open), and a `close()` running alongside one would
        // release and close the same entries twice.
        let mut v = self.retires.lock().unwrap();
        v.retain(|h| !h.is_finished());
        v.push(h);
    }

    /// Returns the handles it skipped for being in use, so `close()` can deal with them; a sweep
    /// ignores the return value because a later sweep picks them up again.
    async fn close_all(&self, picked: Vec<(String, Arc<Db>)>) -> Vec<(String, Arc<Db>)> {
        let mut skipped = Vec::new();
        for (key, h) in picked {
            {
                let mut map = self.entries.lock().unwrap();
                // Two references are expected: the map's and our own clone in `picked`. A third is
                // a request that arrived DURING the drain — which is the whole point of the drain,
                // so let it finish. Un-flag the entry and leave it warm for a later sweep.
                if Arc::strong_count(&h) > 2 {
                    if let Some(e) = map.get(&key) {
                        e.releasing.store(false, Ordering::SeqCst);
                    }
                    skipped.push((key, h));
                    continue;
                }
                map.remove(&key);
            }
            if let Err(e) = h.close().await {
                tracing::error!(repo = %key, error = %e, "closing warm database failed");
            }
        }
        skipped
    }

    /// Wait for every drain-close-release already in flight. A test that has just swept wants
    /// the state AFTER the drain, and the only other way to get it is to sleep `DRAIN` plus a
    /// guess — which is a flake with a margin. Takes the handles, so a caller that races `close()`
    /// simply finds nothing to wait for there.
    pub async fn await_retires(&self) {
        let in_flight: Vec<_> = std::mem::take(&mut *self.retires.lock().unwrap());
        for h in in_flight {
            // Bounded like `close()`: a stuck close must fail the assert, not hang the test.
            let _ = tokio::time::timeout(crate::ownership::DRAIN * 3, h).await;
        }
    }

    /// Close every database. Used on shutdown, so the next node to open them replays no WAL — and
    /// the leases go back LAST, once nothing is open, or the peer that takes a repo over fences a
    /// node still holding it. Same drain-close-release order as eviction.
    pub async fn close(self: &Arc<Self>) {
        self.closed.store(true, Ordering::SeqCst);
        // Let any drain already in flight finish first, so it is not dropped mid-sleep and cannot
        // race this pass into a double release. Bounded: shutdown must not hang on a stuck close.
        let in_flight: Vec<_> = std::mem::take(&mut *self.retires.lock().unwrap());
        for h in in_flight {
            let _ = tokio::time::timeout(crate::ownership::DRAIN * 3, h).await;
        }
        let all: Vec<(String, Arc<Db>)> = {
            let map = self.entries.lock().unwrap();
            map.iter()
                // Same rule as `evictable`: only an entry with a handle may be flagged. One whose
                // open is still in flight would otherwise be flagged, skipped here, and then
                // dropped by the `clear()` below with its database left open.
                .filter_map(|(k, e)| {
                    let db = e.db.get()?;
                    e.releasing.store(true, Ordering::SeqCst);
                    Some((k.clone(), db.clone()))
                })
                .collect()
        };
        if self.hook().is_some() {
            tokio::time::sleep(crate::ownership::DRAIN).await;
        }
        let keys: Vec<String> = all.iter().map(|(k, _)| k.clone()).collect();
        let skipped = self.close_all(all).await;
        // A handle skipped for being in use survives inside its request task, holding the writer
        // epoch on a lease already shortened to the drain — so the successor claims half a second
        // later and fences a database this dying pod is still writing through. Only here, never in
        // the sweep path (a later sweep retries those): at shutdown, cutting one in-flight request
        // is strictly better than fencing the new owner.
        for (_, h) in skipped {
            if let Err(e) = h.close().await {
                tracing::error!(error = %e, "closing an in-use database at shutdown failed");
            }
        }
        // Everything is shut now, in-use handles included, so every lease can go back — and only
        // now. Releasing before the close is what lets the successor fence a dying pod that is
        // still writing.
        if let Some(hook) = self.hook() {
            for repo in &keys {
                hook.release(repo.clone()).await;
            }
        }
        self.entries.lock().unwrap().clear(); // slots whose open never completed
    }

    /// A flush shares the sweeper's task, so it gets a deadline for the same reason the leader's
    /// checkpoint does: housekeeping must never be able to stop the eviction it rides on.
    const FLUSH_PATIENCE: Duration = Duration::from_secs(30);

    /// Flush warm databases that have gone `FLUSH_EVERY` without one.
    ///
    /// `close()` flushes the memtable, which moves `replay_after_wal_id` and is what makes a repo's
    /// WAL collectable at all. Databases normally get that for free by being evicted when idle and
    /// closed, over and over — but a repo busy enough never to go idle is never closed, so its
    /// pointer never moves and its WAL grows without bound. That is precisely how the ownership map
    /// reached 23,083 objects and stopped being able to start: it was opened once and never closed.
    ///
    /// Idempotent and cheap on a quiet database, so no dirty-tracking here — unlike the leader's
    /// checkpoint this runs against many databases, and the bookkeeping would cost more than the
    /// flush it saves.
    pub async fn flush_stale(self: &Arc<Self>) {
        let now = Instant::now();
        let due: Vec<(String, Arc<Db>)> = {
            let map = self.entries.lock().unwrap();
            map.iter()
                .filter(|(_, e)| !e.releasing.load(Ordering::SeqCst))
                .filter(|(_, e)| now.duration_since(*e.last_flush.lock().unwrap()) >= self.flush_every())
                .filter_map(|(k, e)| e.db.get().map(|db| (k.clone(), db.clone())))
                .collect()
        };
        for (key, db) in due {
            let flush = db.flush_with_options(slatedb::config::FlushOptions {
                flush_type: slatedb::config::FlushType::MemTable,
            });
            match tokio::time::timeout(Self::FLUSH_PATIENCE, flush).await {
                Ok(Ok(())) => {
                    if let Some(e) = self.entries.lock().unwrap().get(&key) {
                        *e.last_flush.lock().unwrap() = Instant::now();
                    }
                }
                // Left un-stamped on purpose, both ways: a flush that failed or timed out did not
                // move the pointer, so the next sweep must try again rather than wait another
                // FLUSH_EVERY on the assumption it worked.
                Ok(Err(e)) => tracing::warn!(repo = %key, error = %e, "flushing failed; the next sweep retries"),
                Err(_) => tracing::warn!(repo = %key, patience = ?Self::FLUSH_PATIENCE, "flushing still running after the patience window; will retry"),
            }
        }
    }
}
