//! Where an image's bytes and metadata live.
//!
//! Blobs are per-owner (`blobs/{owner}/sha256/{hex}`): a team that pushes twenty images off one
//! base layer stores it once, and the garbage collector only ever has to read one team's images to
//! know what is unreferenced. Manifest BYTES are objects; the tag map is not — tags live in the
//! image's database, where the single-writer guarantee makes two pushes to `:latest` order against
//! each other instead of racing in the object store.
use crate::dbstore::Store;
use crate::Result;
use slatedb::object_store::path::Path as OsPath;
use slatedb::object_store::ObjectStoreExt;
use slatedb::{Db, WriteBatch};
use std::sync::Arc;

/// A content digest, as it appears on the wire.
///
/// Parsing is the ONLY way a path segment becomes part of an object key, so it is strict on
/// purpose: lowercase hex, algorithm `sha256` (64 hex) or `sha512` (128 hex) — the two the OCI
/// spec requires a conformant registry to accept. Anything else — an upper-case digest, a `..`, a
/// second colon, an unsupported algorithm — is not a digest and never reaches the object store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    pub algo: String,
    pub hex: String,
}

impl Digest {
    pub fn parse(s: &str) -> Option<Digest> {
        let (algo, hex) = s.split_once(':')?;
        let want_len = match algo {
            "sha256" => 64,
            "sha512" => 128,
            _ => return None,
        };
        if hex.len() != want_len {
            return None;
        }
        if !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return None;
        }
        Some(Digest { algo: algo.to_string(), hex: hex.to_string() })
    }

    /// sha256 of `bytes`, for content this code digests itself (manifests keyed by digest, etc.) —
    /// there the algorithm is our choice, not a claim from the client.
    pub fn of(bytes: &[u8]) -> Digest {
        Self::of_algo("sha256", bytes).expect("sha256 is always supported")
    }

    /// Hash `bytes` with whatever algorithm the CLIENT claimed, so a push can be verified against
    /// the digest it was pushed under instead of always assuming sha256. `algo` is untrusted input
    /// here too — anything but the two `parse` accepts returns `None` rather than silently picking
    /// a hash.
    pub fn of_algo(algo: &str, bytes: &[u8]) -> Option<Digest> {
        let mut h = Hasher::new(algo)?;
        h.update(bytes);
        Some(h.finish())
    }
}

/// The same two algorithms as `Digest::of_algo`, fed incrementally — so a layer can be verified
/// while it streams past instead of being buffered whole to be hashed at the end.
pub enum Hasher {
    S256(sha2::Sha256),
    S512(sha2::Sha512),
}

impl Hasher {
    /// `algo` is untrusted client input, so an unknown one is `None` rather than a default hash.
    pub fn new(algo: &str) -> Option<Hasher> {
        use sha2::Digest as _;
        match algo {
            "sha256" => Some(Hasher::S256(sha2::Sha256::new())),
            "sha512" => Some(Hasher::S512(sha2::Sha512::new())),
            _ => None,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        use sha2::Digest as _;
        match self {
            Hasher::S256(h) => h.update(bytes),
            Hasher::S512(h) => h.update(bytes),
        }
    }

    pub fn finish(self) -> Digest {
        use sha2::Digest as _;
        let (algo, hex) = match self {
            Hasher::S256(h) => ("sha256", crate::hex(&h.finalize())),
            Hasher::S512(h) => ("sha512", crate::hex(&h.finalize())),
        };
        Digest { algo: algo.into(), hex }
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.algo, self.hex)
    }
}

pub fn blob_path(owner: &str, d: &Digest) -> OsPath {
    OsPath::from(format!("blobs/{owner}/{}/{}", d.algo, d.hex))
}

pub fn manifest_path(owner: &str, name: &str, d: &Digest) -> OsPath {
    OsPath::from(format!("{}/{}/{}", manifest_prefix(owner, name), d.algo, d.hex))
}

/// Every manifest object of one image lives under this prefix; `manifest_path` is one of them.
pub fn manifest_prefix(owner: &str, name: &str) -> OsPath {
    OsPath::from(format!("manifests/{owner}/{name}"))
}

/// How many manifests an image has, and when the newest was written.
///
/// Both come from one listing, because both are wanted together and a second pass would be a
/// second round trip per image on a page that lists them all. The timestamp is the object store's,
/// not a field anything maintains: nothing writes a "pushed at", and an object's own mtime cannot
/// disagree with what was actually pushed.
pub async fn manifest_stat(store: &Store, owner: &str, name: &str) -> Result<(usize, Option<i64>)> {
    use slatedb::object_store::ObjectStore;
    let prefix = OsPath::from(format!("manifests/{owner}/{name}"));
    let mut listing = store.os.list(Some(&prefix));
    let (mut n, mut newest) = (0usize, None::<i64>);
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        let m = m?;
        n += 1;
        let ms = m.last_modified.timestamp_millis();
        if newest.is_none_or(|cur| ms > cur) {
            newest = Some(ms);
        }
    }
    Ok((n, newest))
}

/// `image/blob/{digest}/{via}`: this image legitimately holds `digest`. `via` is the digest of
/// the manifest that names it, or `upload` when the blob was pushed or mounted into this image
/// directly. Blob BYTES are per owner and shared between siblings, so this row is the only thing
/// that scopes a pull to an image — without it, one public image served every private sibling's
/// layers to anyone who knew a digest. Owners are never gated on it; strangers always are.
const BLOB_PREFIX: &str = "image/blob/";
const BLOB_ROWS_BACKFILLED: &[u8] = b"image/blob-rows";
const BLOB_VIA_UPLOAD: &str = "upload";

fn blob_key(d: &Digest, via: &str) -> Vec<u8> {
    format!("{BLOB_PREFIX}{d}/{via}").into_bytes()
}

/// Record every digest `via` (a manifest digest) names. Idempotent, so a re-push rewrites the
/// same rows. Into a batch: these ride with the manifest's other rows in one flush.
pub fn note_blobs<'a>(batch: &mut WriteBatch, digests: impl IntoIterator<Item = &'a Digest>, via: &str) {
    for d in digests {
        batch.put(blob_key(d, via), b"1".as_slice());
    }
}

/// The bare `image` row, into a batch — `touch_image` for a write that is already batching.
pub fn batch_image(batch: &mut WriteBatch) {
    batch.put(IMAGE_KEY, b"1".as_slice());
}

/// `put_tag`'s row, into a batch. Callers add `batch_image` themselves.
pub fn batch_tag(batch: &mut WriteBatch, tag: &str, d: &Digest) {
    batch.put(tag_key(tag), d.to_string().into_bytes());
}

/// A blob pushed or mounted straight into this image: `touch_image` plus the row, one write.
pub async fn hold_blob(store: &Store, owner: &str, name: &str, d: &Digest) -> Result<()> {
    let db = store.image_db(owner, name).await?;
    let mut b = WriteBatch::new();
    batch_image(&mut b);
    b.put(blob_key(d, BLOB_VIA_UPLOAD), b"1".as_slice());
    db.write(b).await?;
    Ok(())
}

/// Drop the rows manifest `m` contributed. A scan of the whole prefix rather than a re-parse of
/// the manifest being deleted: this is the rare path, and it must work even when the manifest
/// bytes are already gone. Rows written `via` another manifest, or by an upload, stay.
pub async fn forget_manifest_blobs(db: &Db, m: &Digest) -> Result<()> {
    let suffix = format!("/{m}");
    let mut it = db.scan_prefix(BLOB_PREFIX, ..).await?;
    let mut doomed = vec![];
    while let Some(kv) = it.next().await? {
        if String::from_utf8_lossy(&kv.key).ends_with(&suffix) {
            doomed.push(kv.key.to_vec());
        }
    }
    for k in doomed {
        db.delete(k).await?;
    }
    Ok(())
}

async fn has_blob_row(db: &Db, d: &Digest) -> Result<bool> {
    let mut it = db.scan_prefix(format!("{BLOB_PREFIX}{d}/"), ..).await?;
    Ok(it.next().await?.is_some())
}

/// Does this image hold `d`? Runs on the owning node (the only place the image's database may be
/// opened), which is also why the backfill lives here and not in the worker's reconcile.
///
/// ponytail: images pushed before these rows existed have none, so the first stranger's pull of
/// such an image walks its manifests once and writes the rows they imply (`BLOB_ROWS_BACKFILLED`
/// marks it done). Upload-only blobs of those images cannot be recovered and read as not held —
/// the safe direction. Delete the backfill branch once every image DB carries the mark, i.e.
/// after every pre-existing image has been pulled by a stranger or re-pushed.
pub async fn image_holds_blob(store: &Store, owner: &str, name: &str, d: &Digest) -> Result<bool> {
    let db = store.image_db(owner, name).await?;
    if has_blob_row(&db, d).await? {
        return Ok(true);
    }
    if db.get(BLOB_ROWS_BACKFILLED).await?.is_some() {
        return Ok(false);
    }
    // One walk per image, not one per concurrent stranger: the first pull of a pre-rows image
    // used to LIST and GET every manifest inside the blob request, and N simultaneous first
    // pulls each did the whole walk before any of them wrote the mark.
    let lock = store.keyed_lock(&format!("blobrows/{owner}/{name}"));
    let _guard = lock.lock().await;
    // Re-read under the lock: whoever held it before us may have just finished the walk.
    if db.get(BLOB_ROWS_BACKFILLED).await?.is_some() {
        return has_blob_row(&db, d).await;
    }
    backfill_blob_rows(store, owner, name, &db).await?;
    db.put(BLOB_ROWS_BACKFILLED, b"1".as_slice()).await?;
    has_blob_row(&db, d).await
}

/// The walk itself: every manifest of the image, the blob rows it implies.
///
/// Bounded exactly as `gc::referenced` is, and for the same reason — an image with hundreds of
/// manifests was hundreds of serial round trips. A manifest this cannot READ or PARSE names
/// nothing and is skipped: under-granting is the safe failure for authorization, unlike the
/// sweep, where the same manifest must abort. Propagating the store's error here answered a
/// pull with a 500 where the honest answer is a 404.
async fn backfill_blob_rows(store: &Store, owner: &str, name: &str, db: &Db) -> Result<()> {
    use slatedb::object_store::ObjectStore;
    let prefix = OsPath::from(format!("manifests/{owner}/{name}"));
    let mut listing = store.os.list(Some(&prefix));
    let mut paths = vec![];
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        paths.push(m?.location);
    }
    let mut fetched = futures::StreamExt::buffered(
        futures::StreamExt::map(futures::stream::iter(paths), |p| async move {
            let bytes = match store.os.get(&p).await {
                Ok(r) => r.bytes().await,
                Err(e) => Err(e),
            };
            (p, bytes)
        }),
        16,
    );
    while let Some((loc, bytes)) = futures::StreamExt::next(&mut fetched).await {
        let Some(via) = crate::gc::digest_from_path(&loc) else { continue };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(owner = %owner, name = %name, manifest = %loc, error = %e, "blob rows: skipping unreadable manifest");
                continue;
            }
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            tracing::warn!(owner = %owner, name = %name, manifest = %loc, "blob rows: skipping unparseable manifest");
            continue;
        };
        let mut named = std::collections::HashSet::new();
        crate::gc::collect(&v, &mut named);
        let digests: Vec<Digest> = named.iter().filter_map(|s| Digest::parse(s)).collect();
        let mut b = WriteBatch::new();
        note_blobs(&mut b, &digests, &via);
        if !b.is_empty() {
            db.write(b).await?;
        }
    }
    Ok(())
}

/// `(count, newest_ms)` for the image's manifests, kept in the image's own single-writer database
/// so a push does not have to LIST a prefix that only ever grows. Absent until the first push
/// after this shipped, which is what `manifest_stat_fast` falls back over — no migration.
const MANIFEST_COUNT_KEY: &[u8] = b"image/manifests/count";
const MANIFEST_NEWEST_KEY: &[u8] = b"image/manifests/newest_ms";

const IMAGE_KEY: &[u8] = b"image";
const PUBLIC_KEY: &[u8] = b"image/public";
const TAG_PREFIX: &str = "image/tag/";
fn tag_key(tag: &str) -> Vec<u8> {
    format!("{TAG_PREFIX}{tag}").into_bytes()
}

#[allow(async_fn_in_trait)]
/// `Store`'s image-registry methods, as an extension trait rather than an inherent `impl
/// Store`: `Store` now lives in the `storage` crate, and Rust's orphan rule forbids an
/// inherent impl on a foreign type. These stay in this crate (not `storage`) because
/// they need `registry::pool_coords`, a reserved-owner-name/routing concept that belongs to
/// this crate's registry namespace, not to generic storage. Import this trait wherever an
/// `image_*`/`tag*`/`pulls`/`delete_image*` method is called on a `Store`.
pub trait ImageExt {
    async fn image_db(&self, owner: &str, name: &str) -> Result<Arc<Db>>;
    async fn image_exists(&self, owner: &str, name: &str) -> Result<bool>;
    async fn touch_image(&self, owner: &str, name: &str) -> Result<()>;
    async fn put_tag(&self, owner: &str, name: &str, tag: &str, d: &Digest) -> Result<()>;
    async fn tag(&self, owner: &str, name: &str, tag: &str) -> Result<Option<Digest>>;
    async fn delete_tag(&self, owner: &str, name: &str, tag: &str) -> Result<()>;
    async fn tags(&self, owner: &str, name: &str) -> Result<Vec<String>>;
    async fn tags_pointing_at(&self, owner: &str, name: &str, d: &Digest) -> Result<Vec<String>>;
    fn bump_pulls(&self, owner: &str, name: &str, tag: &str);
    async fn flush_pulls(&self) -> Result<()>;
    async fn pulls(&self, owner: &str, name: &str, tag: &str) -> Result<u64>;
    async fn image_is_public(&self, owner: &str, name: &str) -> Result<bool>;
    async fn manifest_stat_fast(&self, owner: &str, name: &str) -> Result<(usize, Option<i64>)>;
    async fn note_manifest_put(&self, batch: &mut WriteBatch, owner: &str, name: &str, existed: bool) -> Result<()>;
    async fn note_manifest_deleted(&self, owner: &str, name: &str) -> Result<()>;
    async fn refresh_image_marker(&self, owner: &str, name: &str) -> Result<()>;
    async fn set_image_visibility(&self, owner: &str, name: &str, public: bool) -> Result<()>;
    async fn delete_image_rows(&self, owner: &str, name: &str) -> Result<()>;
    async fn delete_image(&self, owner: &str, name: &str) -> Result<()>;
}

impl ImageExt for Store {

    /// The image's database. Opening one CREATES it, so callers that merely probe must go through
    /// `image_exists` — the same rule `db_for`/`repo_exists` follow for repos.
    async fn image_db(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        let (o, n) = crate::pool_coords(owner, name);
        self.pool.get(o, &n).await
    }

    async fn image_exists(&self, owner: &str, name: &str) -> Result<bool> {
        let (o, n) = crate::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(false);
        }
        Ok(self.image_db(owner, name).await?.get(IMAGE_KEY).await?.is_some())
    }

    /// Marks the image as existing. Registries create on first write, so every write path calls
    /// this rather than there being a create endpoint. Marking existence and setting visibility
    /// are two different writes — later tasks must call this, never `set_image_visibility`, to
    /// record that an image exists.
    async fn touch_image(&self, owner: &str, name: &str) -> Result<()> {
        self.image_db(owner, name).await?.put(IMAGE_KEY, b"1".as_slice()).await?;
        Ok(())
    }

    async fn put_tag(&self, owner: &str, name: &str, tag: &str, d: &Digest) -> Result<()> {
        // One handle for both puts: `touch_image` would resolve the pool entry a second time on
        // the hottest write path for no gain.
        let db = self.image_db(owner, name).await?;
        let mut b = WriteBatch::new();
        batch_image(&mut b);
        batch_tag(&mut b, tag, d);
        db.write(b).await?;
        Ok(())
    }

    async fn tag(&self, owner: &str, name: &str, tag: &str) -> Result<Option<Digest>> {
        // `pool.exists`, not `image_exists`: the probe only has to keep `image_db` from CREATING
        // a database for an image nobody pushed. A missing tag row already answers `None`, so the
        // extra IMAGE_KEY read `image_exists` adds proves nothing here — and this runs on every
        // pull.
        let (o, n) = crate::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(None);
        }
        let v = self.image_db(owner, name).await?.get(tag_key(tag)).await?;
        Ok(v.and_then(|v| Digest::parse(&String::from_utf8_lossy(&v))))
    }

    async fn delete_tag(&self, owner: &str, name: &str, tag: &str) -> Result<()> {
        self.image_db(owner, name).await?.delete(tag_key(tag)).await?;
        Ok(())
    }

    async fn tags(&self, owner: &str, name: &str) -> Result<Vec<String>> {
        let (o, n) = crate::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(vec![]);
        }
        let db = self.image_db(owner, name).await?;
        let mut it = db.scan_prefix(TAG_PREFIX, ..).await?;
        let mut out = vec![];
        while let Some(kv) = it.next().await? {
            if let Ok(k) = std::str::from_utf8(&kv.key) {
                if let Some(t) = k.strip_prefix(TAG_PREFIX) {
                    out.push(t.to_string());
                }
            }
        }
        // Sorted lexically, which is the order the spec requires `tags/list` to return — free
        // here: `scan_prefix` yields ascending byte order and the tag grammar is ASCII, where
        // byte order and lexical order agree.
        Ok(out)
    }

    /// The tags resolving to `d`, from ONE scan — the delete-by-digest path was re-reading every
    /// tag row individually (list, then a get per tag) to learn what this reads in a single pass.
    async fn tags_pointing_at(&self, owner: &str, name: &str, d: &Digest) -> Result<Vec<String>> {
        let (o, n) = crate::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(vec![]);
        }
        let db = self.image_db(owner, name).await?;
        let want = d.to_string();
        let mut it = db.scan_prefix(TAG_PREFIX, ..).await?;
        let mut out = vec![];
        while let Some(kv) = it.next().await? {
            if String::from_utf8_lossy(&kv.value) == want {
                if let Some(t) = std::str::from_utf8(&kv.key).ok().and_then(|k| k.strip_prefix(TAG_PREFIX)) {
                    out.push(t.to_string());
                }
            }
        }
        Ok(out)
    }

    /// One more pull of `tag`. A pull is a manifest GET by tag — the request docker makes exactly
    /// once per `docker pull` — counted on the node that owns the image, so there is one writer
    /// and the count cannot race. GETs by digest are deliberately uncounted: docker re-reads by
    /// digest after resolving the tag, and counting both would double every pull.
    ///
    /// Nothing but a map increment happens here: the write is `flush_pulls`'s, off the request
    /// path (see `Store::pending_pulls` for why).
    fn bump_pulls(&self, owner: &str, name: &str, tag: &str) {
        let mut m = self.pending_pulls.lock().unwrap_or_else(|p| p.into_inner());
        *m.entry(format!("{owner}/{name}/{tag}")).or_insert(0) += 1;
    }

    /// Fold `pending_pulls` into each image's database. Runs on the owning node's lane, like every
    /// other write. An image this node no longer holds warm has moved (or gone idle — a 5 min TTL
    /// against a 30 s flush, so a moved one in practice) and its pending count is dropped rather
    /// than reopening a database another node now owns. The put is not awaited durable: the count
    /// is display only, and the pool's own periodic flush carries it.
    async fn flush_pulls(&self) -> Result<()> {
        let pending =
            std::mem::take(&mut *self.pending_pulls.lock().unwrap_or_else(|p| p.into_inner()));
        if pending.is_empty() {
            return Ok(());
        }
        let warm = self.pool.warm_repos();
        for (k, add) in pending {
            let mut parts = k.splitn(3, '/');
            let (Some(owner), Some(name), Some(tag)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if !warm.iter().any(|w| w == &format!("img/{owner}/{name}")) {
                continue;
            }
            // Two flushes may overlap (the lane and an explicit call); the read-add-write must
            // still not lose an increment.
            let lock = self.keyed_lock(&format!("pulls/{owner}/{name}/{tag}"));
            let _guard = lock.lock().await;
            let db = self.image_db(owner, name).await?;
            let key = format!("image/pulls/{tag}").into_bytes();
            let n: u64 = db
                .get(key.clone())
                .await?
                .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
                .unwrap_or(0);
            db.put_with_options(
                key,
                (n + add).to_string().into_bytes(),
                &slatedb::config::PutOptions::default(),
                &slatedb::config::WriteOptions { await_durable: false, ..Default::default() },
            )
            .await?;
        }
        Ok(())
    }

    /// Stored plus not-yet-flushed, so a pull shows in the listing at once even though the
    /// database learns of it later.
    async fn pulls(&self, owner: &str, name: &str, tag: &str) -> Result<u64> {
        let v = self.image_db(owner, name).await?.get(format!("image/pulls/{tag}").into_bytes()).await?;
        let stored: u64 = v.and_then(|v| String::from_utf8_lossy(&v).parse().ok()).unwrap_or(0);
        let pending = self
            .pending_pulls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&format!("{owner}/{name}/{tag}"))
            .copied()
            .unwrap_or(0);
        Ok(stored + pending)
    }

    async fn image_is_public(&self, owner: &str, name: &str) -> Result<bool> {
        let (o, n) = crate::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(false);
        }
        Ok(self.image_db(owner, name).await?.get(PUBLIC_KEY).await?.as_deref() == Some(b"1"))
    }

    /// `manifest_stat` for a caller that has the image's database open.
    ///
    /// The listing LIST is O(manifests) against a prefix that only ever grows, and it ran on every
    /// manifest push via `refresh_image_marker` — so a multi-arch push was N+1 full listings. Both
    /// numbers live in the image's own single-writer database instead, which is what makes keeping
    /// them safe: nothing else can be writing manifests for this image. The LIST stays as the
    /// fallback for an image pushed before the counters existed (the next push seeds them) and as
    /// the ONLY answer for the GC reconcile, which is the one reader with no writer's database.
    async fn manifest_stat_fast(&self, owner: &str, name: &str) -> Result<(usize, Option<i64>)> {
        let (o, n) = crate::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok((0, None));
        }
        let db = self.image_db(owner, name).await?;
        let count = db
            .get(MANIFEST_COUNT_KEY)
            .await?
            .and_then(|v| String::from_utf8_lossy(&v).parse::<usize>().ok());
        let Some(count) = count else {
            return manifest_stat(self, owner, name).await;
        };
        let newest = db
            .get(MANIFEST_NEWEST_KEY)
            .await?
            .and_then(|v| String::from_utf8_lossy(&v).parse::<i64>().ok());
        Ok((count, newest))
    }

    /// Record a manifest object landing. `existed` is whether that exact digest was already stored
    /// — a re-push of the same manifest overwrites one object and must not raise the count.
    ///
    /// Seeds from the LIST when the counters are absent, which is the one place the migration
    /// happens: the object is already written by the time this runs, so the listing counts it.
    /// The rows go into the push's `batch`, not their own puts: one flush for the whole push.
    async fn note_manifest_put(&self, batch: &mut WriteBatch, owner: &str, name: &str, existed: bool) -> Result<()> {
        let db = self.image_db(owner, name).await?;
        let now = crate::ownership::now_ms() as i64;
        let count = match db.get(MANIFEST_COUNT_KEY).await? {
            Some(v) => {
                let n: usize = String::from_utf8_lossy(&v).parse().unwrap_or(0);
                n + usize::from(!existed)
            }
            None => manifest_stat(self, owner, name).await?.0,
        };
        batch.put(MANIFEST_COUNT_KEY, count.to_string().into_bytes());
        batch.put(MANIFEST_NEWEST_KEY, now.to_string().into_bytes());
        Ok(())
    }

    /// Record a manifest object being deleted. Left alone when the counters were never seeded: a
    /// delete has no listing to seed from that would not already be wrong, and the next push seeds
    /// it correctly.
    async fn note_manifest_deleted(&self, owner: &str, name: &str) -> Result<()> {
        let db = self.image_db(owner, name).await?;
        let Some(v) = db.get(MANIFEST_COUNT_KEY).await? else {
            return Ok(());
        };
        let n: usize = String::from_utf8_lossy(&v).parse().unwrap_or(0);
        db.put(MANIFEST_COUNT_KEY, n.saturating_sub(1).to_string().into_bytes()).await?;
        Ok(())
    }

    /// Refreshes the listing-index marker after a manifest push: fresh `manifests`/`updated_ms`,
    /// visibility read from the DB (fail closed — a first push with no existing marker is created
    /// PRIVATE unless `image_is_public` already says otherwise). Serialized under the same
    /// `index/img/{owner}/{name}` key `set_image_visibility` uses, so a push racing a flip cannot
    /// interleave the marker swap. Callers must log-and-continue on error: a marker is a view, not
    /// something a push should ever fail over.
    async fn refresh_image_marker(&self, owner: &str, name: &str) -> Result<()> {
        let lock = self.keyed_lock(&rustic_git_storage::index::lock_key(rustic_git_storage::index::Kind::Img, owner, name));
        let _guard = lock.lock().await;
        let public = self.image_is_public(owner, name).await?;
        let (count, newest) = self.manifest_stat_fast(owner, name).await?;
        let existing = crate::index::read(&self.os, crate::index::Kind::Img, owner, name).await;
        let now = crate::ownership::now_ms() as i64;
        let m = crate::index::Marker {
            name: name.to_string(),
            // Always the DB value, never the stale marker's: a marker is a view of the DB, so
            // every push heals any drift left by a marker write that failed or raced.
            public,
            created_by: existing.as_ref().map(|m| m.created_by.clone()).unwrap_or_default(),
            created_ms: existing.as_ref().map(|m| m.created_ms).unwrap_or(now),
            description: existing.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
            manifests: count as u64,
            updated_ms: newest.unwrap_or(now),
        };
        crate::index::write(self, crate::index::Kind::Img, owner, &m).await
    }

    /// Flips the DB row (source of truth for auth) and the listing-index marker together. Serialized
    /// per {owner}/{name} so two racing flips cannot interleave `index::write`'s delete-then-put
    /// (spec §6.5) — without the lock, a-public-then-b-private and a-private-then-b-public racing
    /// could leave both markers, or neither, present.
    async fn set_image_visibility(&self, owner: &str, name: &str, public: bool) -> Result<()> {
        let lock = self.keyed_lock(&rustic_git_storage::index::lock_key(rustic_git_storage::index::Kind::Img, owner, name));
        let _guard = lock.lock().await;
        // Remove-permissive-first (spec §6.2) applies to the whole flip, not just the marker
        // write below: on a private flip, delete the PUBLIC marker before the DB row changes, so
        // a crash between here and `index::write` can never leave a stale public marker sitting
        // over what the DB already calls private.
        if !public {
            let public_path = crate::index::path(true, crate::index::Kind::Img, owner, name);
            if let Err(e) = crate::index::ignore_not_found(self.os.delete(&public_path).await) {
                tracing::warn!(owner = %owner, name = %name, error = %e, "index pre-delete img");
            }
        }
        self.touch_image(owner, name).await?;
        self.image_db(owner, name)
            .await?
            .put(PUBLIC_KEY, if public { b"1".as_slice() } else { b"0".as_slice() })
            .await?;
        // Read the existing marker (either visibility path) so `manifests`/`created_*`/
        // `description` survive the flip — this call only owns `public`.
        let existing = crate::index::read(&self.os, crate::index::Kind::Img, owner, name).await;
        let m = crate::index::Marker {
            name: name.to_string(),
            public,
            created_by: existing.as_ref().map(|m| m.created_by.clone()).unwrap_or_default(),
            created_ms: existing.as_ref().map(|m| m.created_ms).unwrap_or(0),
            description: existing.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
            manifests: existing.as_ref().map(|m| m.manifests).unwrap_or(0),
            updated_ms: existing.as_ref().map(|m| m.updated_ms).unwrap_or(0),
        };
        // Marker is a view, never the source of truth: log-and-continue on failure rather than
        // failing a visibility flip that already landed in the DB.
        if let Err(e) = crate::index::write(self, crate::index::Kind::Img, owner, &m).await {
            tracing::warn!(owner = %owner, name = %name, error = %e, "index write img");
        }
        Ok(())
    }

    // ponytail: a push or page-load racing this delete can re-open the database between the
    // evict and the file removal, leaving a db whose manifest names SSTs that are gone — a
    // broken image rather than a deleted one. The window is one node and milliseconds wide;
    // a delete-in-progress marker in the image db closes it if it ever bites.
    /// Wipes every database row this image owns: the bare `image` marker, `image/public`, every
    /// `image/tag/*`, every `image/pulls/*`, every `image/manifest-type/*`, every `image/blob/*` and every
    /// `image/referrer/*`. All of them start with `image`, and nothing else in this database does
    /// (`upload/*` is the only other key space here — see `referrers::key`'s doc comment) — so one
    /// prefix scan is exhaustive and safe. Scoped to THIS image's own database
    /// (`image_db(owner, name)`), so a sibling image's rows, which live in a different database
    /// entirely, are never touched. Does not touch the object store: callers delete manifest
    /// objects separately, and blobs are never this route's to remove (see `blobs::delete_blob`).
    async fn delete_image_rows(&self, owner: &str, name: &str) -> Result<()> {
        let db = self.image_db(owner, name).await?;
        let mut it = db.scan_prefix("image", ..).await?;
        let mut keys = vec![];
        while let Some(kv) = it.next().await? {
            keys.push(kv.key.to_vec());
        }
        for k in keys {
            db.delete(k).await?;
        }
        Ok(())
    }

    /// The whole image, gone: every database row (`delete_image_rows`), then the database's own
    /// storage evicted and removed from the object store.
    ///
    /// The caller (`imagedelete`) removes the listing-index marker (`index::remove`) before any of
    /// this runs, so by the time storage cleanup happens the image is already invisible to
    /// listings — this no longer answers "does it still list?" for `images`, only "is the bytes
    /// gone?". A crash partway through this function now just leaves orphaned rows/files for GC to
    /// sweep at leisure, not a visible phantom. The database is EVICTED first — closed and dropped
    /// from the pool's warm map — before its files are removed, so nothing local still holds it
    /// open underneath the delete. Scoped by `pool_coords`, which is `img/{owner}/{name}` alone, so
    /// a sibling image's storage (a different `{name}`, hence a different prefix entirely) is never
    /// touched.
    ///
    /// ponytail: single-node precedent (`Pool::evict` has no lease release either) — a warm handle
    /// on ANOTHER node is not evicted here. Fine for this deployment's one-node-owns-a-repo
    /// routing (the delete is forwarded to that owning node, see `http::repo_of`), add a release
    /// through `ReleaseHook` if a second node can ever hold the same image warm at once.
    async fn delete_image(&self, owner: &str, name: &str) -> Result<()> {
        use slatedb::object_store::ObjectStore;
        self.delete_image_rows(owner, name).await?;
        // Cache keys are `{owner}/{name}/{digest}`; without this, a manifest GET'd just before
        // delete keeps serving stale bytes for this image until byte-cap eviction reaches it.
        let cache_prefix = format!("{owner}/{name}/");
        self.manifests().retain(|k, _| !k.starts_with(&cache_prefix));
        let (o, n) = crate::pool_coords(owner, name);
        self.pool.evict(o, &n).await;
        let prefix = OsPath::from(crate::pool::path(o, &n));
        // Streamed, not collected-then-serial: the store batches (or at least overlaps) the
        // deletes, and an image's DB prefix can hold hundreds of SST objects.
        let locations = futures::StreamExt::boxed(futures::StreamExt::map(self.os.list(Some(&prefix)), |m| {
            m.map(|m| m.location)
        }));
        futures::TryStreamExt::try_collect::<Vec<_>>(self.os.delete_stream(locations)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod hasher_tests {
    use super::{Digest, Hasher};

    /// Incremental must agree with one-shot, on both algorithms and across chunk boundaries.
    #[test]
    fn incremental_matches_one_shot() {
        for algo in ["sha256", "sha512"] {
            let mut h = Hasher::new(algo).unwrap();
            h.update(b"abc");
            h.update(b"def");
            assert_eq!(h.finish(), Digest::of_algo(algo, b"abcdef").unwrap());
        }
        assert!(Hasher::new("md5").is_none());
    }
}
