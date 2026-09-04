use crate::{err, Result};
use futures::{StreamExt, TryStreamExt};
use slatedb::object_store::{path::Path as OsPath, ObjectStore, ObjectStoreExt};
use slatedb::Db;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `{owner}/{name}/{digest}` → (bytes, media type), bounded by BYTES rather than entries: a
/// manifest is up to 4 MiB, so a 256-entry cap was a 1 GiB ceiling that cleared itself whole on
/// the 257th hot manifest. 64 MiB, oldest insert evicted first.
///
/// ponytail: insert-order eviction, not LRU — a re-inserted key leaves its older order entry
/// behind, so it can be evicted by that entry's age rather than its own. A cache miss is the
/// only cost; swap for an LRU crate if the hit rate ever matters.
#[derive(Default)]
pub struct ManifestCache {
    map: std::collections::HashMap<String, (slatedb::bytes::Bytes, String)>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
}

impl ManifestCache {
    const MAX_BYTES: usize = 64 * 1024 * 1024;

    pub fn get(&self, key: &str) -> Option<&(slatedb::bytes::Bytes, String)> {
        self.map.get(key)
    }

    pub fn insert(&mut self, key: String, value: (slatedb::bytes::Bytes, String)) {
        self.bytes += value.0.len();
        if let Some(old) = self.map.insert(key.clone(), value) {
            self.bytes -= old.0.len();
        }
        self.order.push_back(key);
        while self.bytes > Self::MAX_BYTES {
            let Some(k) = self.order.pop_front() else { break };
            self.remove(&k);
        }
    }

    pub fn remove(&mut self, key: &str) {
        if let Some(old) = self.map.remove(key) {
            self.bytes -= old.0.len();
        }
    }

    pub fn retain(&mut self, f: impl FnMut(&String, &mut (slatedb::bytes::Bytes, String)) -> bool) {
        self.map.retain(f);
        self.bytes = self.map.values().map(|v| v.0.len()).sum();
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod manifest_cache_tests {
    use super::ManifestCache;

    /// Filling past the byte cap evicts the OLDEST entries, and only as many as it takes.
    #[test]
    fn evicts_oldest_first_by_bytes() {
        let mut c = ManifestCache::default();
        let big = slatedb::bytes::Bytes::from(vec![0u8; ManifestCache::MAX_BYTES / 2 + 1]);
        for k in ["a", "b", "c"] {
            c.insert(k.into(), (big.clone(), String::new()));
        }
        assert!(c.get("a").is_none() && c.get("b").is_none(), "the two oldest go");
        assert!(c.get("c").is_some());
        assert_eq!(c.bytes(), big.len());
        c.remove("c");
        assert_eq!(c.bytes(), 0);
    }
}

pub struct Store {
    pub os: Arc<dyn ObjectStore>,
    /// The SAME store as `os`, seen through the resumable multipart API, when the backend has one.
    /// `MultipartUpload` is a live handle that cannot outlive a request; `MultipartStore` hands out
    /// an upload id and part ids that can, which is what lets a chunked blob upload send each
    /// chunk once instead of re-streaming everything received so far (`registry::uploads`).
    /// `None` for `LocalFileSystem` (`file://` dev mode) — object_store has no impl for it — so
    /// every consumer must keep a fallback that works without this.
    pub mp: Option<Arc<dyn slatedb::object_store::multipart::MultipartStore>>,
    /// Repo databases, opened on demand and kept warm. Which repos reach this node is the load
    /// balancer's decision, so there is nothing to elect here.
    pub pool: Arc<crate::pool::Pool>,
    pub cache_dir: PathBuf,
    /// Credential lookups, cached briefly (see auth.rs).
    pub(crate) auth_cache:
        std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Option<String>)>>,
    /// Whether the object store answered recently. Sampled by a background task; read by /healthz.
    pub healthy: std::sync::atomic::AtomicBool,
    /// The shared response cache, so the write paths can invalidate what they invalidate.
    /// Disabled unless a caller replaces it (main.rs does, from `KLOUDLITE_GIT_REDIS_URL`), which
    /// keeps every other caller — tests included — free of a handle they do not need.
    pub cache: Arc<crate::cache::Cache>,
    /// Per-key async locks for read-modify-write sequences that a single node can still run
    /// concurrently for the same key (e.g. two PATCHes to one upload session, two pulls of one
    /// tag). See `keyed_lock`.
    /// ponytail: in-process lock; correct because one node owns the image DB.
    pub(crate) keyed_locks: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Manifest pull cache, digest-addressed. The bytes are immutable by construction (the digest
    /// is over them), and the two mutable companions — media type and existence — are invalidated
    /// by `put_manifest`/`delete_manifest`, which only ever run on the node serving these GETs
    /// (single-opener routing). Per-node and unbounded-in-time on purpose; bounded in bytes.
    pub manifest_cache: std::sync::Mutex<ManifestCache>,
    /// Pull counts not yet written: `{owner}/{name}/{tag}` → pulls since the last flush. A tag
    /// GET is the hottest registry read, and a durable put under a per-tag lock on that path
    /// serialised every concurrent pull of one tag behind a WAL flush each. The count is display
    /// only, so it is eventually consistent by design: `registry::store::ImageExt::flush_pulls`
    /// folds this into the image's database on the owning node's 30 s lane, and a crash loses at
    /// most that window.
    pub pending_pulls: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    /// `owner/name` → when `open_repo` last reconciled its listing marker. The reconcile is two
    /// `index/` GETs and a DB read, and it ran on every request; a marker only ever drifts on a
    /// crash mid-flip, and the 30 s lane repairs every warm repo anyway, so once per
    /// `RECONCILE_EVERY` per repo keeps the first-touch repair and drops the rest.
    // ponytail: unbounded map, one Instant per repo touched; swept past 4096 entries.
    pub(crate) reconciled: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// `owner/name` → the pack list `open_repo` last synced to disk, tied to the database handle
    /// it was read from. A new handle (the repo moved away and back) misses by construction, so
    /// nothing has to hook eviction; `record_pack`/`forget_pack` drop the entry on this node.
    pub(crate) packs: std::sync::Mutex<std::collections::HashMap<String, SyncedPacks>>,
}

/// `open_repo`'s synced pack list and the database handle it was read under.
type SyncedPacks = (std::sync::Weak<Db>, Vec<(String, u64)>);

/// How long `open_repo` trusts its last marker reconcile of a repo.
const RECONCILE_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

impl Store {
    /// The manifest cache, locked poison-tolerantly.
    ///
    /// A panic anywhere else while this is held must not turn every later manifest GET, PUT and
    /// DELETE into a 500 — the map holds only digest-addressed bytes, which nothing half-finished
    /// can leave inconsistent, so a poisoned cache is still valid data. Same rule, and same
    /// reasoning, as `auth_cache`.
    pub fn manifests(&self) -> std::sync::MutexGuard<'_, ManifestCache> {
        self.manifest_cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The async mutex guarding read-modify-write sequences for `key`.
    ///
    /// Unused entries are dropped as we go: upload-session keys carry a client-chosen uuid, so
    /// "bounded by the live key space" only holds while sessions are short-lived — an
    /// authenticated client opening sessions it never finishes would otherwise grow this map for
    /// the life of the process. An `Arc` with one reference is held by nobody but this map, which
    /// makes it safe to remove: any caller still using that lock holds a clone, and a caller that
    /// arrives later simply creates a fresh one and serialises against itself as before.
    pub fn keyed_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.keyed_locks.lock().unwrap();
        // Swept only past a size no honest in-flight set reaches — an every-acquisition retain
        // was O(live keys) on every ref write. Entries with one strong count are held by nobody
        // but this map, so dropping them can never break a caller (it holds a clone).
        const SWEEP_AT: usize = 512;
        if m.len() >= SWEEP_AT {
            m.retain(|_, v| Arc::strong_count(v) > 1);
        }
        m.entry(key.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }

    /// Heals a crashed flip: a crash between the DB visibility write and its marker swap leaves
    /// the two disagreeing, and only the owning node can see DB truth to fix it (the structural
    /// sweep in `registry::gc` only ever sees the object store, never a repo/image DB). Reads
    /// this node's own DB visibility, compares it against the marker (either path), and rewrites
    /// the marker via `index::write` when they disagree or the marker is missing entirely,
    /// preserving every other body field. `Ok(true)` means a repair was written.
    ///
    /// Safe to call from anywhere: it is only ever reachable from code paths that already run
    /// exclusively on the node that owns `owner/name` (repo/image DB opens, and the renewal
    /// loop's `warm_repos()`, which only lists repos this node currently holds open) — the same
    /// single-writer invariant that lets `is_public`/`image_is_public` be trusted at all.
    ///
    /// Locked under the same `index/{repo,img}/{owner}/{name}` key the real flips use, so a
    /// reconcile racing a genuine flip can't interleave `index::write`'s delete-then-put.
    ///
    /// `db_public` is computed by the caller rather than read here: `Kind::Img`'s answer
    /// (`image_is_public`) lives in the registry module (root crate — it needs
    /// `registry::pool_coords`, a reserved-owner-name concept that has no business in `storage`),
    /// and `storage` cannot call back into a crate that depends on it. `Kind::Repo`'s answer
    /// (`is_public`) stays an inherent method here since it needs nothing outside this crate.
    pub async fn reconcile_marker(
        &self,
        owner: &str,
        name: &str,
        kind: crate::index::Kind,
        db_public: bool,
    ) -> Result<bool> {
        let lock = self.keyed_lock(&format!("index/{}/{owner}/{name}", kind.seg()));
        let _guard = lock.lock().await;
        let existing = crate::index::read(&self.os, kind, owner, name).await;
        if existing.as_ref().is_some_and(|m| m.public == db_public) {
            return Ok(false);
        }
        let now = crate::ownership::now_ms() as i64;
        let m = crate::index::Marker {
            name: name.to_string(),
            public: db_public,
            created_by: existing.as_ref().map(|m| m.created_by.clone()).unwrap_or_default(),
            created_ms: existing.as_ref().map(|m| m.created_ms).unwrap_or(now),
            description: existing.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
            manifests: existing.as_ref().map(|m| m.manifests).unwrap_or(0),
            updated_ms: existing.as_ref().map(|m| m.updated_ms).unwrap_or(0),
        };
        crate::index::write(self, kind, owner, &m).await?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn auth_cache_len(&self) -> usize {
        self.auth_cache().len()
    }
}

#[derive(Clone)]
pub struct Repo {
    pub owner: String,
    pub name: String,
    pub objects_dir: PathBuf,
    pub pack_dir: PathBuf,
}

impl Repo {
    /// Every repo owns its objects outright. Forks copy rather than share: forks are rare, storage
    /// is cheap, and sharing a pile is what forced garbage collection to see every repo using it —
    /// which in turn constrained where repos could live and made cross-repo object exposure
    /// possible at all.
    pub fn s3_prefix(&self) -> String {
        format!("objects/{}/{}/pack", self.owner, self.name)
    }
    pub fn odb(&self) -> Result<gix_odb::Handle> {
        Ok(gix_odb::at(&self.objects_dir)?)
    }
}

/// Remove local pack files the index no longer names.
///
/// The cache was only ever added to. After a repo moves away, is repacked there and moves back,
/// the superseded packs are still here: gix-odb discovers packs by `.idx`, so objects the repack
/// dropped stay servable, and the disk is never reclaimed. Only files past `STALE_AFTER` go — a
/// push in flight has written its pack locally and not yet uploaded or recorded it, and must not
/// lose it underneath. `.idx` first, as everywhere: no reader sees an index without its data.
///
/// The two temp shapes go the same way: `fetch_pack_file`'s `.{name}.{pid}.{seq}.tmp` and
/// `objects.rs`'s `incoming-{pid}-{seq}.pack` were removed by the code that wrote them (that
/// path now indexes from memory and never creates one), but a killed process could still leave
/// one behind, and nothing else would ever reclaim it.
// ponytail: an mtime guard, not a lock; a single push whose upload takes over an hour would lose
// its pack here. Track in-flight packs explicitly if uploads ever get that slow.
fn prune_stale_packs(pack_dir: &Path, indexed: &[(String, u64)]) -> std::io::Result<()> {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);
    let now = std::time::SystemTime::now();
    // At most one scan per STALE_AFTER per repo: open_repo is on every request's path, and a
    // fresher scan can never reclaim more — nothing it deletes is younger than STALE_AFTER.
    // The marker never matches the pack/temp shapes below, so it is never pruned itself.
    let marker = pack_dir.join(".pruned");
    let fresh = marker
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .is_some_and(|age| age < STALE_AFTER);
    if fresh {
        return Ok(());
    }
    std::fs::write(&marker, b"")?;
    let indexed: std::collections::HashSet<&str> =
        indexed.iter().map(|(f, _)| f.as_str()).collect();
    let mut stale: Vec<PathBuf> = Vec::new();
    for ent in std::fs::read_dir(pack_dir)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let is_pack = name.starts_with("pack-") && (name.ends_with(".pack") || name.ends_with(".idx"));
        let is_temp = (name.starts_with('.') && name.ends_with(".tmp"))
            || (name.starts_with("incoming-") && name.ends_with(".pack"));
        if !(is_pack || is_temp) || indexed.contains(name.as_str()) {
            continue;
        }
        let old = ent
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if old {
            stale.push(ent.path());
        }
    }
    stale.sort_by_key(|p| p.extension().and_then(|x| x.to_str()) != Some("idx"));
    for p in stale {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

fn pack_index_prefix(owner: &str, name: &str) -> String {
    format!("pack/{owner}/{name}/")
}

pub fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Repo names the web app's URL space has already spent.
///
/// `/{owner}/{name}` and `/{owner}/activity` occupy the same position, and a
/// static segment wins over a dynamic one — so a repo called `activity` would be
/// created happily and then be permanently unreachable, its page showing the
/// namespace's feed instead. Refusing the name at creation is the only point
/// where that is still fixable.
///
/// Checked where repos are CREATED, exactly like the `api` owner rule: a repo
/// that predates this list keeps working over git, where none of these names
/// mean anything.
pub const RESERVED_REPO_NAMES: [&str; 8] =
    ["activity", "repos", "settings", "registries", "workspaces", "environments", "snapshots", "ci"];

pub fn reserved_repo_name(name: &str) -> bool {
    RESERVED_REPO_NAMES.iter().any(|r| name.eq_ignore_ascii_case(r))
}

/// Owner names the URL space has already spent.
///
/// `api` is the browse prefix. `v2` is the registry prefix, for the same reason: a repo owned by
/// `v2` would make `/v2/alice/info/refs` both that repo's git route and an image path. `img` is
/// not a URL prefix at all — it is the routing key registry paths derive, and a repo owned by
/// `img` would put its database at `repo/img/{name}`, nesting it inside the prefix every image
/// database lives under. `vol` is the same story one keyspace over: the volume registry's
/// routing key, and a repo owned by `vol` would nest its database under `repo/vol/{name}`.
/// `superadmin` is the web's operations area at `/superadmin/*`: an owner of that name would put
/// a namespace page under every path the console owns.
pub const RESERVED_OWNERS: [&str; 5] = ["api", "v2", "img", "vol", "superadmin"];

/// Owner names are segments, minus the ones the URL space has already spent.
///
/// Checked where repos are CREATED, not where paths are parsed: a repo owned by a reserved name
/// that predates the reservation keeps working over SSH and can be moved with `admin fork`; only
/// its HTTP routes are gone.
pub fn valid_owner(s: &str) -> bool {
    valid_segment(s) && !RESERVED_OWNERS.contains(&s)
}

#[cfg(test)]
mod open_repo_tests {
    use super::Store;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::Arc;

    /// A page load is several `open_repo`s of one warm repo. The first touch reads the marker
    /// (both `index/` paths) and syncs packs; every one after it, until a pack is recorded or
    /// forgotten, must read no marker at all.
    #[tokio::test]
    async fn a_warm_reopen_reads_no_markers() {
        let counting = Arc::new(crate::index::tests::Counting::default());
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(counting.clone(), tmp.path().join("cache"), false).await.unwrap();
        s.create_repo("a", "r").await.unwrap();
        counting.index_gets.store(0, SeqCst);
        s.open_repo("a", "r").await.unwrap().unwrap();
        assert_eq!(counting.index_gets.load(SeqCst), 2, "first touch: one reconcile");
        for _ in 0..5 {
            s.open_repo("a", "r").await.unwrap().unwrap();
        }
        assert_eq!(counting.index_gets.load(SeqCst), 2, "warm reopens read no marker");
        // The synced pack list is dropped the moment the index changes.
        s.record_pack("a", "r", "pack-x.pack", 0).await.unwrap();
        assert!(s.packs.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod reserved_owner_tests {
    use super::valid_owner;

    /// A user cannot claim owner `vol`: it is the volume registry's routing key
    /// (`repo/vol/{owner}/{name}`), exactly as `img` is the image registry's. `create_repo`
    /// (`crates/api/src/repos.rs`) and every other repo-creation path check `valid_owner`, so this
    /// one assertion is the root-cause coverage for all of them.
    #[test]
    fn vol_is_reserved() {
        assert!(!valid_owner("vol"));
        assert!(!valid_owner("superadmin"));
        assert!(valid_owner("volley")); // a prefix match must not over-reserve
    }
}

impl Store {
    /// `background`: run SlateDB's compactor and garbage collector inside each repo database.
    pub async fn open(
        os: Arc<dyn ObjectStore>,
        cache_dir: PathBuf,
        background: bool,
    ) -> Result<Store> {
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Store {
            pool: Arc::new(crate::pool::Pool::new(os.clone(), background)),
            os,
            // Off unless a caller fills it in (`config::open_store` does, from the same concrete
            // store it just built) — the same shape as `cache` above, and for the same reason:
            // every other caller, tests included, stays free of a handle it does not need.
            mp: None,
            cache_dir,
            auth_cache: Default::default(),
            healthy: std::sync::atomic::AtomicBool::new(true),
            cache: Arc::new(crate::cache::Cache::connect(None).await),
            keyed_locks: Default::default(),
            manifest_cache: Default::default(),
            pending_pulls: Default::default(),
            reconciled: Default::default(),
            packs: Default::default(),
        })
    }

    /// Probe the object store every few seconds and record the result. Reachability and liveness
    /// both key off /healthz, so a node whose blob-store client is dead must fail it — otherwise
    /// it keeps its repos and returns 500 to every client with no failover and no restart.
    ///
    /// Hysteresis: three consecutive failures to flip unhealthy, one success to flip back. Without
    /// it, one slow round trip during an object-store blip makes every node unhealthy at once and
    /// every node stops routing Local for one probe interval.
    pub fn spawn_health_probe(self: &Arc<Self>) {
        let s = self.clone();
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                // The store is healthy if it *answered the question*: Ok, or NotFound (the probe
                // key need not exist). Everything else — Generic (transport, 5xx), Unauthenticated
                // (401: rotated key), PermissionDenied (403) — is unhealthy. Treating auth failures
                // as healthy would keep a node with a revoked key holding its repos and returning
                // 500 forever, which is exactly what this exists to catch.
                let ok = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    s.os.head(&OsPath::from("auth/.health")),
                )
                .await
                .map(|r| matches!(r, Ok(_) | Err(slatedb::object_store::Error::NotFound { .. })))
                .unwrap_or(false);
                failures = if ok { 0 } else { failures + 1 };
                s.healthy.store(failures < 3, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether a repo exists, without creating its database as a side effect of asking.
    pub async fn repo_db_exists(&self, owner: &str, name: &str) -> Result<bool> {
        self.pool.exists(owner, name).await
    }

    /// A repo's database. Every node that serves a repo serves it for reads and writes both.
    pub async fn db_for(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        self.pool.get(owner, name).await
    }

    /// Whether this repo's marker is due a reconcile, recording the attempt if so.
    fn reconcile_due(&self, key: &str) -> bool {
        let mut m = self.reconciled.lock().unwrap_or_else(|p| p.into_inner());
        let now = std::time::Instant::now();
        if m.get(key).is_some_and(|t| now.duration_since(*t) < RECONCILE_EVERY) {
            return false;
        }
        if m.len() >= 4096 {
            m.retain(|_, t| now.duration_since(*t) < RECONCILE_EVERY);
        }
        m.insert(key.to_string(), now);
        true
    }

    /// Ensure local cache mirrors S3 pack list. `Ok(None)` if the repo (or path) does not exist.
    pub async fn open_repo(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        if !valid_segment(owner) || !valid_segment(name) {
            return Ok(None);
        }
        if !self.repo_exists(owner, name).await? {
            return Ok(None);
        }
        let key = format!("{owner}/{name}");
        // Lazily heal a crashed flip the first time this repo is touched. `open_repo` only runs
        // on the node the routing middleware sent the request to (the owning node — see
        // `CLAUDE.md`'s ownership invariant); images instead rely on the renewal loop's
        // `warm_repos()` lane. Marker repair is a view, not authorization — log-and-continue.
        if self.reconcile_due(&key) {
            match self.is_public(owner, name).await {
                Ok(db_public) => {
                    if let Err(e) =
                        self.reconcile_marker(owner, name, crate::index::Kind::Repo, db_public).await
                    {
                        tracing::warn!(owner = %owner, repo = %name, reason = "write", error = %e, "index.marker.reconcile.failed");
                    }
                }
                Err(e) => tracing::warn!(owner = %owner, repo = %name, reason = "read", error = %e, "index.marker.reconcile.failed"),
            }
        }
        let objects_dir = self.cache_dir.join(owner).join(name).join("objects");
        let pack_dir = objects_dir.join("pack");
        let repo = Repo {
            owner: owner.into(),
            name: name.into(),
            objects_dir,
            pack_dir,
        };
        // A pack list already synced under this very database handle needs no scan, no stat per
        // pack and no sweep: nothing on this node removes an indexed pack, and no other node can
        // touch our cache. The handle identity is what makes the cache safe across a move.
        // ponytail: one stat on the pack DIRECTORY covers a wiped cache (a restart, a test); a
        // single pack deleted from under a live pod is not covered — stat per pack if that shows.
        let db = self.db_for(owner, name).await?;
        let cached = self
            .packs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .filter(|(w, _)| std::sync::Weak::ptr_eq(w, &Arc::downgrade(&db)))
            .map(|(_, files)| files.clone());
        let files = match cached.filter(|_| repo.pack_dir.is_dir()) {
            Some(files) => files,
            None => {
                let (a, b) = tokio::join!(
                    tokio::fs::create_dir_all(&repo.pack_dir),
                    tokio::fs::create_dir_all(repo.objects_dir.join("info")) // gix-odb wants a normal objects dir
                );
                a?;
                b?;
                // Which files the repo has comes from the ref store, not from listing the object
                // store: the writer records each pack as it uploads it, so this is a local read
                // instead of a network round trip. It also keeps the pack list consistent with
                // the refs alongside it, since both come from the same database.
                let files = self.pack_index(owner, name).await?;
                // .pack before .idx: gix-odb discovers packs via .idx, so the idx must land last.
                let (packs, idxs): (Vec<_>, Vec<_>) = files
                    .clone()
                    .into_iter()
                    .partition(|(fname, _)| !fname.ends_with(".idx"));
                for batch in [packs, idxs] {
                    futures::stream::iter(batch)
                        .map(|(fname, size)| self.fetch_pack_file(&repo, fname, size))
                        .buffer_unordered(8)
                        .try_collect::<Vec<_>>()
                        .await?;
                }
                self.packs
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key, (Arc::downgrade(&db), files.clone()));
                files
            }
        };
        // Gated by its own hourly marker, so on a warm repo this is one stat. A cache that could
        // not be swept is not a reason to refuse the repo — the packs it needs are already here.
        // Off the runtime: it is a directory walk with unlinks.
        let (pd, fl) = (repo.pack_dir.clone(), files);
        let pruned = tokio::task::spawn_blocking(move || prune_stale_packs(&pd, &fl)).await;
        if let Err(e) = pruned.map_err(std::io::Error::other).and_then(|r| r) {
            tracing::warn!(owner = %owner, repo = %name, error = %e, "packs.cache.prune.failed");
        }
        Ok(Some(repo))
    }

    /// The repo's pack files as `(filename, size)`, from the ref store.
    ///
    /// Falls back to listing the object store when the index is empty or unreadable, which covers
    /// repos written before the index existed; the listing is then recorded so the fallback
    /// happens once.
    pub async fn pack_index(&self, owner: &str, name: &str) -> Result<Vec<(String, u64)>> {
        let prefix = pack_index_prefix(owner, name);
        let mut it = self
            .db_for(owner, name).await?
            .scan_prefix(prefix.as_bytes(), ..)
            .await?;
        let mut out = Vec::new();
        while let Some(kv) = it.next().await? {
            let fname = String::from_utf8_lossy(&kv.key[prefix.len()..]).to_string();
            // One bad row makes the whole index suspect: fall through to the listing, which
            // re-records every file. Defaulting the size to 0 instead meant the size-equality
            // skip in `fetch_pack_file` never matched, and the pack was downloaded on every open.
            let Ok(size) = String::from_utf8_lossy(&kv.value).parse::<u64>() else {
                out.clear();
                break;
            };
            out.push((fname, size));
        }
        if !out.is_empty() {
            return Ok(out);
        }
        // no index yet: list once, and record it
        let listing: Vec<_> = self
            .os
            .list(Some(&OsPath::from(format!("objects/{owner}/{name}/pack"))))
            .try_collect()
            .await?;
        for meta in &listing {
            if let Some(f) = meta.location.filename() {
                out.push((f.to_string(), meta.size));
                let _ = self.record_pack(owner, name, f, meta.size).await;
            }
        }
        Ok(out)
    }

    /// Note that a pack file exists, so serving a repo needs no object-store listing.
    pub async fn record_pack(&self, owner: &str, name: &str, fname: &str, size: u64) -> Result<()> {
        self.forget_cached_packs(owner, name);
        self.db_for(owner, name).await?
            .put(
                format!("{}{}", pack_index_prefix(owner, name), fname),
                size.to_string().as_bytes(),
            )
            .await?;
        Ok(())
    }

    /// Drop `open_repo`'s synced pack list: the index is about to change under it.
    fn forget_cached_packs(&self, owner: &str, name: &str) {
        self.packs.lock().unwrap_or_else(|p| p.into_inner()).remove(&format!("{owner}/{name}"));
    }

    pub async fn forget_pack(&self, owner: &str, name: &str, fname: &str) -> Result<()> {
        self.forget_cached_packs(owner, name);
        self.db_for(owner, name).await?
            .delete(format!("{}{}", pack_index_prefix(owner, name), fname))
            .await?;
        Ok(())
    }

    /// Download one pack file unless an identically sized copy is already cached.
    async fn fetch_pack_file(&self, repo: &Repo, fname: String, size: u64) -> Result<()> {
        let pack_dir = &repo.pack_dir;
        let local = pack_dir.join(&fname);
        if local.metadata().map(|m| m.len() == size).unwrap_or(false) {
            return Ok(());
        }
        let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), fname));
        // Streamed, never buffered: `open_repo` runs eight of these at once, and a whole pack in
        // memory per download is gigabytes for a repo with a few large packs.
        let stream = self.os.get(&key).await?.into_stream().map_err(std::io::Error::other);
        let mut reader = tokio_util::io::StreamReader::new(stream);
        // unique per process+call: concurrent opens must not share a temp path
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = pack_dir.join(format!(".{fname}.{}.{seq}.tmp", std::process::id()));
        // fsync the data before the rename: otherwise a host crash can leave a renamed file with
        // the right length but unwritten contents, and the size-only skip above would then serve
        // that corrupt pack forever without re-fetching.
        let written = async {
            let mut w = tokio::fs::File::create(&tmp).await?;
            tokio::io::copy(&mut reader, &mut w).await?;
            w.sync_all().await
        }
        .await;
        // A half-written temp is nobody's to finish: a truncated download that stayed on disk
        // would only be reclaimed an hour later by the prune, and until then it is dead space
        // on every failed open.
        if let Err(e) = written {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        tokio::fs::rename(&tmp, &local).await?;
        Ok(())
    }

    /// Copy every pack from one repo's prefix to another's. Uses the object store's own copy, so
    /// the bytes never travel through this process.
    pub async fn copy_packs(&self, from: &Repo, to: &Repo) -> Result<usize> {
        let src = OsPath::from(from.s3_prefix());
        let metas: Vec<_> = self.os.list(Some(&src)).try_collect().await?;
        // .pack before .idx, as everywhere: a reader must never see an index without its data
        let (packs, idxs): (Vec<_>, Vec<_>) = metas
            .into_iter()
            .partition(|m| m.location.extension() != Some("idx"));
        let mut copied = 0;
        for batch in [packs, idxs] {
            for meta in batch {
                let Some(fname) = meta.location.filename() else {
                    continue;
                };
                let dst = OsPath::from(format!("{}/{}", to.s3_prefix(), fname));
                self.os.copy(&meta.location, &dst).await?;
                self.record_pack(&to.owner, &to.name, fname, meta.size)
                    .await?;
                copied += 1;
            }
        }
        Ok(copied)
    }

    /// Delete every object belonging to a repo. Safe unconditionally now that objects are never
    /// shared between repos.
    pub async fn delete_objects(&self, owner: &str, name: &str) -> Result<()> {
        let prefix = OsPath::from(format!("objects/{owner}/{name}/pack"));
        let locs: Vec<OsPath> = self
            .os
            .list(Some(&prefix))
            .map_ok(|m| m.location)
            .try_collect()
            .await?;
        for loc in locs {
            if let Some(f) = loc.filename() {
                let _ = self.forget_pack(owner, name, f).await;
            }
            self.os.delete(&loc).await?;
        }
        let _ = std::fs::remove_dir_all(self.cache_dir.join(owner).join(name));
        Ok(())
    }

    /// Remove the repo's DATABASE files — everything under `repo/{owner}/{name}/`.
    ///
    /// Separate from `delete_objects`, which only clears git packs. Without this a deleted repo
    /// leaves its whole SlateDB behind: storage that is never reclaimed, and — worse — a directory
    /// the GC sweep reads as "this repo exists, it just lost its marker", so it helpfully recreates
    /// one and the repo reappears in every listing. Presence of the directory is only a truthful
    /// signal of existence if delete actually removes it.
    ///
    /// The pool does the deleting, because the pool is what must close the handle first and
    /// refuse a concurrent open — see `Pool::delete`.
    pub async fn delete_repo_db(&self, owner: &str, name: &str) -> Result<()> {
        self.pool.delete(owner, name).await
    }

    /// Undo `upload_pack_files` for a pack that was accepted onto S3 but must not survive
    /// (e.g. the push it came from was rejected): remove it from the object store, the pack
    /// index, and the local cache. Idempotent — used from both the upload-failure path and a
    /// rejected-push cleanup.
    pub async fn delete_pack_files(&self, repo: &Repo, pack: &Path, idx: &Path) -> Result<()> {
        // .idx before .pack, same ordering as everywhere else: no reader should ever see an
        // index without its data.
        for p in [idx, pack] {
            let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), fname));
            let _ = self.os.delete(&key).await;
            let _ = self.forget_pack(&repo.owner, &repo.name, fname).await;
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }

    pub async fn upload_pack_files(&self, repo: &Repo, pack: &Path, idx: &Path) -> Result<()> {
        use slatedb::object_store::WriteMultipart;
        use tokio::io::AsyncReadExt;
        // pack first, idx last: a concurrent reader must never see an idx without its pack.
        for p in [pack, idx] {
            let fname = p
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| err("bad pack path"))?;
            let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), fname));
            // Streamed, never buffered — the download path (`fetch_pack_file`) streams for the
            // same reason: a whole pack in memory per concurrent push is RSS equal to the push.
            let size = tokio::fs::metadata(p).await?.len();
            let mut f = tokio::fs::File::open(p).await?;
            let mut w = WriteMultipart::new(self.os.put_multipart(&key).await?);
            // 5 MiB parts, at most 4 in flight: the same memory bound the registry's `pour` uses.
            let mut buf = vec![0u8; 5 * 1024 * 1024];
            let streamed = async {
                loop {
                    let n = f.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    w.wait_for_capacity(4).await.map_err(std::io::Error::other)?;
                    w.put(slatedb::bytes::Bytes::copy_from_slice(&buf[..n]));
                }
                Ok::<_, std::io::Error>(())
            }
            .await;
            // A failed part must not leave the multipart dangling with the handle gone — same
            // rule as the registry's pour; leaked halves are the bucket's lifecycle rule's job.
            if let Err(e) = streamed {
                let _ = w.abort().await;
                return Err(e.into());
            }
            w.finish().await?;
            // record after the upload, so the index never names a file that is not there yet
            self.record_pack(&repo.owner, &repo.name, fname, size)
                .await?;
        }
        Ok(())
    }
}
