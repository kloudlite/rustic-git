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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

mod evict;
mod lease;

/// How the pool gives a repo's lease back before it closes the database. Implemented by `App`,
/// which owns the ownership client; the pool holds it as a `Weak` because `App -> Store -> Pool`
/// already points the other way and an `Arc` here would be a cycle that never drops.
pub trait ReleaseHook: Send + Sync + 'static {
    fn release(&self, repo: String) -> futures::future::BoxFuture<'_, ()>;
}

/// One repo's slot. The `OnceCell` is the single-flight point: the first caller opens, every other
/// caller awaits that same open rather than starting a competing one.
struct Entry {
    db: tokio::sync::OnceCell<Arc<Db>>,
    last_used: Mutex<Instant>,
    /// When this database last had its memtable flushed. Only ever moved by `flush_stale`, which
    /// exists for the database that is never idle and so never gets the flush that `close()` would
    /// otherwise have given it.
    last_flush: Mutex<Instant>,
    /// Set once eviction has picked this entry. It stays in the map through the drain — the node
    /// is still the owner and still serving — so this is what stops a second sweep picking it
    /// again, and what keeps the renewal task from extending a lease that was just released.
    releasing: AtomicBool,
}

pub struct Pool {
    os: Arc<dyn ObjectStore>,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    /// How long a database stays open with nobody using it, in milliseconds. Atomic rather than a
    /// plain field so a test can tune one pool without touching the process-global environment.
    idle_ttl_ms: std::sync::atomic::AtomicU64,
    /// Ceiling on warm databases, so a wide burst cannot pin unbounded memory.
    max_warm: std::sync::atomic::AtomicUsize,
    flush_every_ms: std::sync::atomic::AtomicU64,
    settings: slatedb::config::Settings,
    /// Shared across every database this pool opens — see `shared_db_cache`.
    db_cache: Arc<dyn slatedb::db_cache::DbCache>,
    hook: Mutex<Option<Weak<dyn ReleaseHook>>>,
    retires: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Set by `close()` and never cleared. A pool closed on the way out must stay closed: the
    /// listeners are still draining, and a request landing there would otherwise reopen a database
    /// and retake the writer epoch this node has just released.
    closed: AtomicBool,
}

/// This node's handle on a repo was closed under it — fenced by another opener.
#[derive(Debug)]
pub struct FencedError {
    pub repo: String,
}
impl std::fmt::Display for FencedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} fenced", self.repo)
    }
}
impl std::error::Error for FencedError {}

/// Whether an error, anywhere in a request, is a fence: ours or SlateDB's own.
pub fn is_fenced(e: &crate::Error) -> bool {
    e.downcast_ref::<FencedError>().is_some()
        // slatedb 0.15: a fence surfaces as ErrorKind::Closed(CloseReason::Fenced). There is no
        // bare ErrorKind::Fenced.
        || e.downcast_ref::<slatedb::Error>()
            .is_some_and(|e| matches!(e.kind(), slatedb::ErrorKind::Closed(slatedb::CloseReason::Fenced)))
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

/// SlateDB's on-disk cache for object-store parts, rooted under `RUSTIC_GIT_CACHE_DIR`.
///
/// It is OFF by default (`root_folder: None`), which is how this has been running: every SST block
/// miss under a tag read, a visibility check or a ref read is an S3 GET. Sized by env because the
/// budget is the pod's ephemeral disk, which nothing here can see; `..._DISK_CACHE_MB=0` turns it
/// back off for a node with no scratch space. `cache_on_flush`/`cache_on_compaction` stay off: the
/// repo pool runs neither by default, and a leader that does would be caching SSTs it is not about
/// to re-read.
///
/// `subdir` separates the repo pool from the ownership map, which is read on every route decision
/// and must not share an eviction budget with 64 repo databases.
///
/// Keyed on `RUSTIC_GIT_CACHE_DIR` being SET, not on a default path, and that is load-bearing:
/// the cache is keyed by database path, so two object stores holding different data under
/// `repo/alice/web` would read each other's parts. In the fleet there is exactly one bucket per
/// node and the deployment sets the variable; in a test process there are dozens of `InMemory`
/// stores using the same paths, and an implicit `./.local/cache` would silently cross them.
pub(crate) fn disk_cache_options(subdir: &str) -> slatedb::config::ObjectStoreCacheOptions {
    let mb = env_u64("RUSTIC_GIT_SLATEDB_DISK_CACHE_MB", 4096);
    let root = std::env::var("RUSTIC_GIT_CACHE_DIR")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join(subdir));
    slatedb::config::ObjectStoreCacheOptions {
        root_folder: root.filter(|_| mb > 0),
        max_cache_size_bytes: Some((mb * 1024 * 1024) as usize),
        ..Default::default()
    }
}

/// One block cache for every repo database on this node, not one per database.
///
/// `Db::builder` installs its own 512 MiB block + 128 MiB meta cache when none is given
/// (`DEFAULT_BLOCK_CACHE_CAPACITY`/`DEFAULT_META_CACHE_CAPACITY` in slatedb 0.15) — 640 MiB
/// nominal times `RUSTIC_GIT_MAX_WARM` (64) is 40 GiB against a pod limit in single-digit GiB, so
/// sharing is a memory-safety fix as much as a hit-rate one. SlateDB scopes each database's keys
/// inside its own wrapper, so one instance across every repo cannot mix them up, and a cache
/// handed in this way is never closed by SlateDB — which is what we want when the pool outlives
/// every database in it. The defaults below are node-wide totals and so deliberately far under
/// SlateDB's per-database ones.
fn shared_db_cache() -> Arc<dyn slatedb::db_cache::DbCache> {
    use slatedb::db_cache::foyer::{FoyerCache, FoyerCacheOptions};
    let mk = |mb: u64| {
        Some(Arc::new(FoyerCache::new_with_opts(FoyerCacheOptions {
            max_capacity: mb * 1024 * 1024,
            ..Default::default()
        })) as Arc<dyn slatedb::db_cache::DbCache>)
    };
    Arc::new(
        slatedb::db_cache::SplitCache::new()
            .with_block_cache(mk(env_u64("RUSTIC_GIT_SLATEDB_BLOCK_CACHE_MB", 256)))
            .with_meta_cache(mk(env_u64("RUSTIC_GIT_SLATEDB_META_CACHE_MB", 64)))
            .build(),
    )
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
        // WAL collection runs whether or not `background` is set, and has to. A database takes a
        // write on every ref update or tag move, and nothing else ever reclaims those WAL objects:
        // the ownership map, configured the same way, reached 18,521 WAL files in four days and
        // could no longer be opened inside the liveness probe's window, because opening replays
        // every one of them. It is the object COUNT that breaks an open, not the bytes — so a repo
        // that is written to steadily is on the same path.
        //
        // Only the WAL. Compaction stays behind `background` for the reason above (N repos means N
        // compactors competing with live requests), and manifest/compacted objects are left alone
        // so a reader on an older manifest never loses the objects it references. A flushed WAL
        // entry is referenced by nobody; `min_age` keeps it well past durability regardless.
        //
        // What makes a repo's WAL collectable at all is `close()`, which flushes the memtable to
        // L0 and so moves `replay_after_wal_id` — collection only ever considers entries behind
        // that point. Repo databases get that for free because they are evicted when idle and
        // closed, over and over; the ownership map is the one that grew without bound precisely
        // because it is opened once and never closed, so nothing ever moved its pointer.
        // A repo busy enough never to go idle is never closed either, so it had the same exposure;
        // `flush_stale` closes that by forcing the flush on a timer, the same way the leader does
        // for the ownership map.
        let wal_gc = slatedb::config::GarbageCollectorOptions {
            wal_options: Some(slatedb::config::GarbageCollectorDirectoryOptions {
                interval: Some(Duration::from_secs(300)),
                min_age: Duration::from_secs(300),
                dry_run: false,
            }),
            ..Default::default()
        };
        let settings = slatedb::config::Settings {
            flush_interval: Some(Duration::from_millis(flush_ms)),
            object_store_max_retries: Some(10), // fail loudly instead of retrying forever
            compactor_options: background.then(|| defaults.compactor_options.clone()).flatten(),
            garbage_collector_options: Some(wal_gc),
            object_store_cache_options: disk_cache_options("slatedb"),
            ..defaults
        };
        Pool {
            os,
            db_cache: shared_db_cache(),
            entries: Mutex::new(HashMap::new()),
            idle_ttl_ms: (env_u64("RUSTIC_GIT_WARM_TTL_SECS", 300) * 1000).into(),
            max_warm: (env_u64("RUSTIC_GIT_MAX_WARM", 64) as usize).into(),
            flush_every_ms: (300_000u64).into(),
            settings,
            hook: Mutex::new(None),
            retires: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }

    pub fn idle_ttl(&self) -> Duration {
        Duration::from_millis(self.idle_ttl_ms.load(Ordering::Relaxed))
    }
    pub fn set_idle_ttl(&self, d: Duration) {
        self.idle_ttl_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }
    pub fn max_warm(&self) -> usize {
        self.max_warm.load(Ordering::Relaxed)
    }
    pub fn set_max_warm(&self, n: usize) {
        self.max_warm.store(n, Ordering::Relaxed);
    }

    /// Bind eviction to the lease. Set after construction because the hook is `App`, and `App` is
    /// built around this pool. Unset (single node, admin commands) means eviction closes straight
    /// away, exactly as it did before leases existed.
    pub fn set_release_hook(&self, h: Weak<dyn ReleaseHook>) {
        *self.hook.lock().unwrap() = Some(h);
    }

    fn hook(&self) -> Option<Arc<dyn ReleaseHook>> {
        self.hook.lock().unwrap().as_ref().and_then(Weak::upgrade)
    }

    /// Whether this pool has been closed on the way out. A closed pool never reopens, so a node in
    /// this state must not take a lease either — see `App::route`.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Whether a repo's database exists, without opening it.
    ///
    /// Opening creates: `Db::builder(...).build()` has no create-if-missing switch, so probing an
    /// unknown path through `get` would bring a database into being for every bad request. A warm
    /// entry is proof enough; otherwise ask the object store, which costs one LIST.
    /// How many repo databases this node is holding open. Reported by `/healthz`.
    pub fn warm_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

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

    /// The repos this node holds open and still owns, as `owner/name` — what the renewal task
    /// renews. Entries already picked for retirement are excluded: their lease is about to be
    /// deleted outright, and extending it first would only widen the window in which the map
    /// names a node that is closing.
    pub fn warm_repos(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| !e.releasing.load(Ordering::SeqCst))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Evict idle databases in the background for as long as the pool lives.
    /// How long a warm database may go without a flush before the sweeper forces one. Settable so
    /// a test can drive the real path rather than assert the constant back to itself.
    pub fn flush_every(&self) -> Duration {
        Duration::from_millis(self.flush_every_ms.load(Ordering::Relaxed))
    }
    pub fn set_flush_every(&self, d: Duration) {
        self.flush_every_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn spawn_sweeper(self: &Arc<Self>) {
        let pool = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(pool.idle_ttl() / 4).await;
                pool.sweep().await;
                pool.flush_stale().await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::BoxStream;
    use slatedb::object_store::memory::InMemory;
    use slatedb::object_store::path::Path as OsPath;
    use slatedb::object_store::{
        GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, PutMultipartOptions, PutOptions,
        PutPayload, PutResult, Result as OsResult,
    };

    fn pool() -> Arc<Pool> {
        Arc::new(Pool::new(Arc::new(InMemory::new()), false))
    }

    /// Tuned per test rather than through the environment: env vars are process-global, and these
    /// tests run in parallel in one process.
    fn pool_with(idle_ttl: Duration, max_warm: usize) -> Arc<Pool> {
        let p = Pool::new(Arc::new(InMemory::new()), false);
        p.set_idle_ttl(idle_ttl);
        p.set_max_warm(max_warm);
        Arc::new(p)
    }

    /// The disk cache is keyed by database path, so it must never turn itself on at a guessed
    /// location: two stores holding different data under `repo/alice/web` would read each other's
    /// parts, which is exactly the shape of a test process full of `InMemory` stores.
    #[test]
    fn the_disk_cache_is_off_unless_a_cache_dir_is_configured() {
        let set = std::env::var("RUSTIC_GIT_CACHE_DIR").is_ok();
        assert_eq!(disk_cache_options("slatedb").root_folder.is_some(), set);
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

    /// An entry inserted but not yet opened must be left entirely alone: flagging it releasing
    /// would strand it (no handle to close, and `warm_repos` would stop renewing its lease while
    /// the open finishes and the database ends up held without one).
    #[tokio::test]
    async fn an_open_in_flight_is_not_marked_releasing() {
        let p = pool_with(Duration::ZERO, 64); // everything is idle enough to evict
        p.entries.lock().unwrap().insert(
            "alice/web".to_string(),
            Arc::new(Entry {
                db: tokio::sync::OnceCell::new(),
                last_used: Mutex::new(Instant::now() - Duration::from_secs(3600)),
                last_flush: Mutex::new(Instant::now()),
                releasing: AtomicBool::new(false),
            }),
        );
        assert!(p.evictable(Instant::now()).is_empty(), "an unopened entry is not evictable");
        assert_eq!(p.warm_repos(), vec!["alice/web".to_string()], "and must still be renewed");
        p.sweep().await;
        assert_eq!(p.warm_repos(), vec!["alice/web".to_string()], "still renewed after a sweep");
    }

    /// An evict that lands while the open is in flight removes a slot with no handle in it. The
    /// open then finishes and, before this fix, the handle was returned with no map entry naming
    /// it: never swept, never closed, holding the writer epoch for the life of the process.
    #[tokio::test]
    async fn a_handle_whose_slot_was_evicted_mid_open_is_closed() {
        let p = pool();
        let entry = Arc::new(Entry {
            db: tokio::sync::OnceCell::new(),
            last_used: Mutex::new(Instant::now()),
            last_flush: Mutex::new(Instant::now()),
            releasing: AtomicBool::new(false),
        });
        // Deliberately NOT in the map: the shape an evict leaves behind. A DIFFERENT entry sits
        // under the same key, as a reopen after the evict would leave it — so what is being pinned
        // here is slot identity, not merely the key being absent.
        p.entries.lock().unwrap().insert(
            "alice/web".to_string(),
            Arc::new(Entry {
                db: tokio::sync::OnceCell::new(),
                last_used: Mutex::new(Instant::now()),
                last_flush: Mutex::new(Instant::now()),
                releasing: AtomicBool::new(false),
            }),
        );
        let db = entry.db.get_or_try_init(|| p.open("alice", "web")).await.unwrap().clone();
        let err = match p.adopt("alice/web", &entry, db.clone()).await {
            Ok(_) => panic!("an orphaned handle must not be adopted"),
            Err(e) => e,
        };
        assert!(is_fenced(&err), "the caller must re-route: {err}");
        assert!(db.status().close_reason.is_some(), "the orphaned handle must be closed");
        // And the slot that IS in the map is adopted as before.
        let live = p.get("alice", "web").await.unwrap();
        assert!(live.status().close_reason.is_none());
    }

    /// A closed pool stays closed: the listeners are still draining when `close()` returns, and a
    /// request landing there must not reopen a database and retake the epoch we just released.
    #[tokio::test]
    async fn a_closed_pool_does_not_reopen() {
        let p = pool();
        p.get("alice", "web").await.unwrap();
        p.close().await;
        assert!(p.get("alice", "web").await.is_err(), "a closed pool must not reopen");
        assert_eq!(p.warm_count(), 0);
    }

    /// Wraps an in-memory store and, once `hang` is set, never completes a put: what an object
    /// store outage looks like to the flush inside `close()`.
    #[derive(Debug)]
    struct HangingStore {
        inner: InMemory,
        hang: Arc<AtomicBool>,
    }

    impl std::fmt::Display for HangingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "HangingStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for HangingStore {
        async fn put_opts(
            &self,
            location: &OsPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> OsResult<PutResult> {
            if self.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            self.inner.put_opts(location, payload, opts).await
        }
        async fn put_multipart_opts(
            &self,
            location: &OsPath,
            opts: PutMultipartOptions,
        ) -> OsResult<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }
        async fn get_opts(&self, location: &OsPath, options: GetOptions) -> OsResult<GetResult> {
            self.inner.get_opts(location, options).await
        }
        fn delete_stream(
            &self,
            locations: BoxStream<'static, OsResult<OsPath>>,
        ) -> BoxStream<'static, OsResult<OsPath>> {
            self.inner.delete_stream(locations)
        }
        fn list(&self, prefix: Option<&OsPath>) -> BoxStream<'static, OsResult<ObjectMeta>> {
            self.inner.list(prefix)
        }
        async fn list_with_delimiter(&self, prefix: Option<&OsPath>) -> OsResult<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &OsPath,
            to: &OsPath,
            options: slatedb::object_store::CopyOptions,
        ) -> OsResult<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// `evict` runs on the renewal task: a close whose flush never returns must not hold every
    /// other lease's renewal hostage. Paused time makes the 30s patience instant; the elapsed
    /// check proves the close really did hang rather than finish with nothing to flush.
    #[tokio::test(start_paused = true)]
    async fn evict_does_not_wait_forever_on_a_hung_close() {
        let hang = Arc::new(AtomicBool::new(false));
        let os: Arc<dyn ObjectStore> =
            Arc::new(HangingStore { inner: InMemory::new(), hang: hang.clone() });
        let p = Arc::new(Pool::new(os, false));
        p.get("alice", "web").await.unwrap().put(b"k", b"v").await.unwrap();
        hang.store(true, Ordering::SeqCst);

        let started = tokio::time::Instant::now();
        tokio::time::timeout(Pool::FLUSH_PATIENCE * 2, p.evict("alice", "web"))
            .await
            .expect("evict must return within the patience window");
        assert!(started.elapsed() >= Pool::FLUSH_PATIENCE, "the close did not hang; test proves nothing");
        assert_eq!(p.warm_count(), 0, "the handle is out of the map even while its close hangs");
    }

    /// A stale evict (keyed on a handle that was fenced) must not close a fresh handle that a
    /// concurrent caller already reopened into the same slot — that would flap both requests.
    #[tokio::test]
    async fn evict_spares_a_freshly_reopened_handle() {
        let p = pool();
        let h1 = p.get("alice", "web").await.unwrap(); // handle A, observed fenced by some caller
        p.evict("alice", "web").await; // simulate: someone else already evicted A
        let h2 = p.get("alice", "web").await.unwrap(); // handle B, a fresh reopen into the slot
        assert!(!Arc::ptr_eq(&h1, &h2));

        p.evict_if_same("alice", "web", &h1).await; // stale evict, still keyed on A

        let h3 = p.get("alice", "web").await.unwrap();
        assert!(Arc::ptr_eq(&h2, &h3), "evict_if_same must not have closed the fresh handle");
    }

    /// A handle that was closed for any reason but a fence is dead, not stolen: report a plain
    /// error (the next call reopens) rather than a fence (the caller would re-route and
    /// force-claim a repo nobody took).
    #[tokio::test]
    async fn a_cleanly_closed_handle_is_not_reported_as_fenced() {
        let p = pool();
        let h = p.get("alice", "web").await.unwrap();
        h.close().await.unwrap();
        drop(h);
        let e = match p.get("alice", "web").await {
            Ok(_) => panic!("a closed handle must be reported, not handed out"),
            Err(e) => e,
        };
        assert!(!is_fenced(&e), "clean close reported as a fence: {e}");
        assert_eq!(p.warm_count(), 0, "the dead handle is dropped");
        p.get("alice", "web").await.unwrap().put(b"k", b"v").await.unwrap();
    }

    /// When another node takes a repo's writer epoch, the handle here is fenced. The pool must
    /// evict it and REPORT the fence rather than silently reopening — reopening would take the
    /// repo straight back and flap. Only a caller that has re-run routing may reopen.
    #[tokio::test]
    async fn fenced_handle_is_evicted_and_reported() {
        let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let p = Arc::new(Pool::new(os.clone(), false));
        let held = p.get("alice", "web").await.unwrap();
        held.put(b"k", b"v").await.unwrap();

        // another node takes the repo: opening the same database claims the writer epoch
        let usurper = Db::builder(path("alice", "web"), os).build().await.unwrap();
        usurper.put(b"k2", b"v2").await.unwrap();

        // fencing is observed asynchronously (manifest poll, ~1s), so wait for it to surface
        {
            let mut st = held.subscribe();
            tokio::time::timeout(Duration::from_secs(5), async {
                while st.borrow().close_reason.is_none() {
                    st.changed().await.unwrap();
                }
            })
            .await
            .expect("the handle must observe the fence within 5s");
        }
        drop(held);

        let e = match p.get("alice", "web").await {
            Ok(_) => panic!("a fenced handle must be reported, not reopened"),
            Err(e) => e,
        };
        assert!(crate::pool::is_fenced(&e), "got: {e}");
        assert_eq!(p.warm_count(), 0, "the fenced handle must be evicted");

        // and only now, at the caller's decision, does a fresh open succeed
        let db = p.get("alice", "web").await.unwrap();
        assert!(db.status().close_reason.is_none());
        db.put(b"k4", b"v4").await.unwrap();
        assert_eq!(db.get(b"k2").await.unwrap().as_deref(), Some(&b"v2"[..]));
    }

    /// The gap `flush_stale` exists for: a database in constant use is never idle, so it is never
    /// evicted, so it is never closed — and `close()` is what would otherwise flush its memtable
    /// and let its WAL be collected. Held across the call here exactly as a busy repo would be.
    #[tokio::test]
    async fn a_database_that_is_never_idle_still_gets_flushed() {
        let p = pool_with(Duration::from_secs(3600), 64); // never idle-evictable
        p.set_flush_every(Duration::ZERO); // due immediately
        let held = p.get("alice", "web").await.unwrap();
        held.put(b"k", b"v").await.unwrap();

        assert_eq!(p.warm_count(), 1);
        p.flush_stale().await;
        // Still open and still usable: this is a flush, not a close.
        assert_eq!(p.warm_count(), 1, "flush_stale must not evict");
        held.put(b"k2", b"v2").await.unwrap();

        // A sweep cannot have been what did it — the entry is still referenced, so it is not
        // evictable at all.
        p.sweep().await;
        assert_eq!(p.warm_count(), 1);
    }

    /// Not due yet, so nothing is touched. Guards the timer actually being consulted: a
    /// `flush_stale` that flushed unconditionally would flush every warm database every sweep.
    #[tokio::test]
    async fn flush_stale_leaves_recently_flushed_databases_alone() {
        let p = pool_with(Duration::from_secs(3600), 64);
        p.set_flush_every(Duration::from_secs(3600));
        let held = p.get("alice", "web").await.unwrap();
        held.put(b"k", b"v").await.unwrap();
        p.flush_stale().await;
        assert_eq!(p.warm_count(), 1);
    }
}
