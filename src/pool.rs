//! Repo databases, opened on demand and kept warm.
//!
//! One SlateDB database per repo. Which node serves a repo is the load balancer's decision, so
//! this node never elects anything; it opens whatever repo it is sent, for reads and writes both.
//! There are no read-only follower nodes: a follower can only serve refs as stale as its last
//! manifest poll, and routing a repo's whole traffic to one node costs nothing when repos are the
//! unit of balancing anyway.
//!
//! Opening is not free (a manifest read, a writer-epoch claim, a WAL replay: ~50-150ms against S3),
//! so a database stays open after the request that needed it. Three properties keep that safe:
//!
//! * **Single flight.** Two concurrent requests for one repo must share a handle. SlateDB fences
//!   the second writer of a database — including a second writer inside this same process — so
//!   racing opens would fence us against ourselves. The `OnceCell` per entry is what prevents it.
//! * **In-flight guard.** Idle eviction must not close a database someone is streaming from. The
//!   `Arc` itself is the refcount: an entry is only closable when the pool holds the sole reference.
//! * **A bound.** A burst across many repos would otherwise pin one memtable, cache and set of
//!   background tasks per repo. Eviction is by idle time, and by count once `max_warm` is passed.

use crate::Result;
use slatedb::object_store::ObjectStore;
use slatedb::Db;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One repo's slot. The `OnceCell` is the single-flight point: the first caller opens, every other
/// caller awaits that same open rather than starting a competing one.
struct Entry {
    db: tokio::sync::OnceCell<Arc<Db>>,
    last_used: Mutex<Instant>,
}

pub struct Pool {
    os: Arc<dyn ObjectStore>,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    /// How long a database stays open with nobody using it.
    pub idle_ttl: Duration,
    /// Ceiling on warm databases, so a wide burst cannot pin unbounded memory.
    pub max_warm: usize,
    settings: slatedb::config::Settings,
}

/// Where a repo's database lives.
pub fn path(owner: &str, name: &str) -> String {
    format!("repo/{owner}/{name}")
}

fn env_u64(k: &str, default: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Pool {
    /// `background`: run SlateDB's compactor and garbage collector inside each repo database.
    /// Off by default here — at one database per repo, N compactors would poll and compete with
    /// live requests for the same CPU and bandwidth. Run compaction from a maintenance process
    /// that sweeps repos serially instead.
    pub fn new(os: Arc<dyn ObjectStore>, background: bool) -> Pool {
        let defaults = slatedb::config::Settings::default();
        // Every ref update waits for the next WAL flush, so this interval — not object-store
        // latency — sets how long a push takes when pushes arrive one at a time. An idle database
        // does not flush, so a short interval costs nothing until there is something to write.
        let flush_ms = env_u64("RUSTIC_GIT_FLUSH_INTERVAL_MS", 100);
        let settings = slatedb::config::Settings {
            flush_interval: Some(Duration::from_millis(flush_ms)),
            object_store_max_retries: Some(10), // fail loudly instead of retrying forever
            compactor_options: background.then(|| defaults.compactor_options.clone()).flatten(),
            garbage_collector_options: background
                .then(|| defaults.garbage_collector_options.clone())
                .flatten(),
            ..defaults
        };
        Pool {
            os,
            entries: Mutex::new(HashMap::new()),
            idle_ttl: Duration::from_secs(env_u64("RUSTIC_GIT_WARM_TTL_SECS", 300)),
            max_warm: env_u64("RUSTIC_GIT_MAX_WARM", 64) as usize,
            settings,
        }
    }

    /// The database for a repo, opening it if this node does not already hold it warm.
    ///
    /// A warm handle can go stale: when the balancer moves a repo's writer, the new holder takes
    /// the writer epoch and fences this one, and every call against it fails from then on. Drop
    /// the dead handle and open once more rather than serving errors from it forever.
    ///
    /// ponytail: fencing is observed asynchronously, so the request that races the handoff still
    /// sees one `Fenced` error before the status reflects it; recovery starts from the next call.
    /// Closing that window needs a watcher per warm database (`Db::subscribe`) evicting on close.
    pub async fn get(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        let h = self.get_once(owner, name).await?;
        if h.status().close_reason.is_none() {
            return Ok(h);
        }
        drop(h);
        self.evict(owner, name).await;
        self.get_once(owner, name).await
    }

    async fn get_once(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        let key = format!("{owner}/{name}");
        let entry = {
            let mut map = self.entries.lock().unwrap();
            let e = map
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(Entry {
                        db: tokio::sync::OnceCell::new(),
                        last_used: Mutex::new(Instant::now()),
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
            // inheriting the error. Drop the slot so a poisoned key cannot accumulate.
            .inspect_err(|_| {
                self.entries.lock().unwrap().remove(&key);
            })?
            .clone();
        self.enforce_bound().await;
        Ok(handle)
    }

    async fn open(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        Ok(Arc::new(
            Db::builder(path(owner, name), self.os.clone())
                .with_settings(self.settings.clone())
                .build()
                .await?,
        ))
    }

    /// Drop a repo from the pool and close it. Called when a write comes back fenced: the balancer
    /// has moved this repo's writer elsewhere, and the new holder has taken the epoch. Holding the
    /// stale handle would fail every subsequent write against it.
    pub async fn evict(&self, owner: &str, name: &str) {
        let entry = self
            .entries
            .lock()
            .unwrap()
            .remove(&format!("{owner}/{name}"));
        // Closing flushes, which a fenced database cannot do; the error is expected and ignored.
        if let Some(h) = entry.and_then(Arc::into_inner).and_then(|e| e.db.into_inner()) {
            let _ = h.close().await;
        }
    }

    /// Close databases nobody is using: idle past the TTL, or the least recently used once the
    /// pool is over `max_warm`.
    pub async fn sweep(&self) {
        self.close_all(self.evictable(Instant::now())).await
    }

    async fn enforce_bound(&self) {
        if self.entries.lock().unwrap().len() > self.max_warm {
            self.close_all(self.evictable(Instant::now())).await
        }
    }

    /// Pick what may be closed, and unlink it, under one lock. An entry still referenced outside
    /// the pool is skipped — that is a request in flight, and closing under it would fail the
    /// request. It becomes evictable on a later sweep.
    fn evictable(&self, now: Instant) -> Vec<Arc<Db>> {
        let mut map = self.entries.lock().unwrap();
        let mut idle: Vec<(Instant, String)> = map
            .iter()
            .filter(|(_, e)| e.db.get().is_none_or(|db| Arc::strong_count(db) == 1))
            .map(|(k, e)| (*e.last_used.lock().unwrap(), k.clone()))
            .collect();
        idle.sort_by_key(|(t, _)| *t); // oldest first
        let over = map.len().saturating_sub(self.max_warm);
        let mut out = Vec::new();
        for (i, (last, key)) in idle.into_iter().enumerate() {
            if i >= over && now.duration_since(last) < self.idle_ttl {
                continue; // young enough, and we are not over the bound
            }
            if let Some(h) = map
                .remove(&key)
                .and_then(Arc::into_inner)
                .and_then(|e| e.db.into_inner())
            {
                out.push(h);
            }
        }
        out
    }

    async fn close_all(&self, handles: Vec<Arc<Db>>) {
        for h in handles {
            if let Err(e) = h.close().await {
                eprintln!("closing warm database failed: {e}"); // ponytail: eprintln; swap for a logger when one exists
            }
        }
    }

    /// Close every database. Used on shutdown, so the next node to open them replays no WAL.
    pub async fn close(&self) {
        let all: Vec<Arc<Db>> = {
            let mut map = self.entries.lock().unwrap();
            std::mem::take(&mut *map)
                .into_values()
                .filter_map(Arc::into_inner)
                .filter_map(|e| e.db.into_inner())
                .collect()
        };
        self.close_all(all).await
    }

    /// Whether a repo's database exists, without opening it.
    ///
    /// Opening creates: `Db::builder(...).build()` has no create-if-missing switch, so probing an
    /// unknown path through `get` would bring a database into being for every bad request. A warm
    /// entry is proof enough; otherwise ask the object store, which costs one LIST.
    pub async fn exists(&self, owner: &str, name: &str) -> Result<bool> {
        if self
            .entries
            .lock()
            .unwrap()
            .contains_key(&format!("{owner}/{name}"))
        {
            return Ok(true);
        }
        let prefix = slatedb::object_store::path::Path::from(path(owner, name));
        Ok(futures::StreamExt::next(&mut self.os.list(Some(&prefix)))
            .await
            .transpose()?
            .is_some())
    }

    pub fn warm_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Evict idle databases in the background for as long as the pool lives.
    pub fn spawn_sweeper(self: &Arc<Self>) {
        let pool = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(pool.idle_ttl / 4).await;
                pool.sweep().await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::memory::InMemory;

    fn pool() -> Arc<Pool> {
        Arc::new(Pool::new(Arc::new(InMemory::new()), false))
    }

    /// Tuned per test rather than through the environment: env vars are process-global, and these
    /// tests run in parallel in one process.
    fn pool_with(idle_ttl: Duration, max_warm: usize) -> Arc<Pool> {
        let mut p = Pool::new(Arc::new(InMemory::new()), false);
        p.idle_ttl = idle_ttl;
        p.max_warm = max_warm;
        Arc::new(p)
    }

    /// The property the whole design rests on: concurrent callers must share one open, because a
    /// second open of the same database fences the first.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_get_opens_once() {
        let p = pool();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let p = p.clone();
                tokio::spawn(async move { p.get("alice", "web").await.unwrap() })
            })
            .collect();
        let dbs: Vec<Arc<Db>> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|h| h.unwrap())
            .collect();
        assert_eq!(p.warm_count(), 1);
        assert!(
            dbs.windows(2).all(|w| Arc::ptr_eq(&w[0], &w[1])),
            "every caller must receive the same database"
        );
        // Not fenced: the shared handle is still writable.
        dbs[0].put(b"k", b"v").await.unwrap();
    }

    #[tokio::test]
    async fn reopen_after_evict_sees_earlier_writes() {
        let p = pool();
        p.get("alice", "web").await.unwrap().put(b"k", b"v").await.unwrap();
        p.evict("alice", "web").await;
        assert_eq!(p.warm_count(), 0);

        let db = p.get("alice", "web").await.unwrap();
        assert_eq!(db.get(b"k").await.unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[tokio::test]
    async fn sweep_keeps_databases_that_are_in_use() {
        let p = pool();
        let held = p.get("alice", "web").await.unwrap(); // caller still holds it
        p.sweep().await;
        assert_eq!(p.warm_count(), 1, "must not close a database under a live request");

        drop(held);
        // Nothing holds it now, but it is not idle yet either.
        p.sweep().await;
        assert_eq!(p.warm_count(), 1);
    }

    #[tokio::test]
    async fn idle_databases_are_closed() {
        let p = pool_with(Duration::ZERO, 64);
        p.get("alice", "web").await.unwrap();
        p.sweep().await;
        assert_eq!(p.warm_count(), 0);
    }

    #[tokio::test]
    async fn pool_stays_within_max_warm() {
        let p = pool_with(Duration::from_secs(300), 4);
        for i in 0..12 {
            p.get("alice", &format!("r{i}")).await.unwrap();
        }
        assert!(p.warm_count() <= 4, "warm set grew to {}", p.warm_count());
    }

    /// When the balancer moves a repo's writer, the node that lost it holds a fenced handle. The
    /// pool must notice and reopen rather than serve errors from a dead database forever.
    #[tokio::test]
    async fn fenced_handle_is_replaced_on_next_get() {
        let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let p = Arc::new(Pool::new(os.clone(), false));
        p.get("alice", "web").await.unwrap().put(b"k", b"v").await.unwrap();

        // another node takes the repo: opening the same database claims the writer epoch
        let usurper = Db::builder(path("alice", "web"), os).build().await.unwrap();
        usurper.put(b"k2", b"v2").await.unwrap();

        // the request racing the handoff still fails: fencing surfaces on use, not before it
        let stale = p.get("alice", "web").await.unwrap();
        assert!(stale.put(b"k3", b"v3").await.is_err());
        drop(stale);

        // but the pool must recover by itself rather than serve that dead handle forever
        let db = p.get("alice", "web").await.unwrap();
        assert!(db.status().close_reason.is_none(), "pool kept a dead database");
        db.put(b"k4", b"v4").await.unwrap();
        assert_eq!(db.get(b"k2").await.unwrap().as_deref(), Some(&b"v2"[..]));
    }

}
