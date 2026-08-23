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
            ..defaults
        };
        Pool {
            os,
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

    /// The database for a repo, opening it if this node does not already hold it warm.
    ///
    /// A closed handle is evicted and reported, NOT reopened. Under routing, "closed" almost always
    /// means "fenced": another node opened this repo because it believes it owns it. Reopening here
    /// would take it straight back and turn any disagreement into a flap. The caller decides — via
    /// the routing rule — whether this node should hold the repo, and only then reopens.
    /// Whether this pool has been closed on the way out. A closed pool never reopens, so a node in
    /// this state must not take a lease either — see `App::route`.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

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
                        last_used: Mutex::new(Instant::now()),
                        last_flush: Mutex::new(Instant::now()),
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
            // inheriting the error. Drop the slot so a poisoned key cannot accumulate.
            .inspect_err(|_| {
                self.entries.lock().unwrap().remove(&key);
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
    async fn adopt(&self, key: &str, entry: &Arc<Entry>, handle: Arc<Db>) -> Result<Arc<Db>> {
        let current = self.entries.lock().unwrap().get(key).is_some_and(|e| Arc::ptr_eq(e, entry));
        if current {
            return Ok(handle);
        }
        let _ = handle.close().await;
        Err(FencedError { repo: key.to_string() }.into())
    }

    async fn open(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
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

    /// Close databases nobody is using: idle past the TTL, or the least recently used once the
    /// pool is over `max_warm`.
    pub async fn sweep(self: &Arc<Self>) {
        let picked = self.evictable(Instant::now());
        self.retire(picked).await
    }

    async fn enforce_bound(self: &Arc<Self>) {
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
    fn evictable(&self, now: Instant) -> Vec<(String, Arc<Db>)> {
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
            eprintln!("release hook unavailable: closing {} database(s) WITHOUT releasing; the lease may outlive the handle", picked.len()); // ponytail: eprintln
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
                eprintln!("closing warm database failed: {e}"); // ponytail: eprintln; swap for a logger when one exists
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
                eprintln!("closing an in-use database at shutdown: {e}"); // ponytail: eprintln
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
                Ok(Err(e)) => eprintln!("flushing {key}: {e}"), // ponytail: eprintln
                Err(_) => eprintln!("flushing {key}: still running after {:?}; will retry", Self::FLUSH_PATIENCE), // ponytail: eprintln
            }
        }
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
    use slatedb::object_store::memory::InMemory;

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
        // Deliberately NOT in the map: the shape an evict leaves behind.
        let db = entry.db.get_or_try_init(|| p.open("alice", "web")).await.unwrap().clone();
        let err = match p.adopt("alice/web", &entry, db.clone()).await {
            Ok(_) => panic!("an orphaned handle must not be adopted"),
            Err(e) => e,
        };
        assert!(is_fenced(&err), "the caller must re-route: {err}");
        assert!(db.status().close_reason.is_some(), "the orphaned handle must be closed");
        assert_eq!(p.warm_count(), 0);
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
