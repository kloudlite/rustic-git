//! Where an image's bytes and metadata live.
//!
//! Blobs are per-owner (`blobs/{owner}/sha256/{hex}`): a team that pushes twenty images off one
//! base layer stores it once, and the garbage collector only ever has to read one team's images to
//! know what is unreferenced. Manifest BYTES are objects; the tag map is not — tags live in the
//! image's database, where the single-writer guarantee makes two pushes to `:latest` order against
//! each other instead of racing in the object store.
use crate::store::Store;
use crate::Result;
use slatedb::object_store::path::Path as OsPath;
use slatedb::object_store::ObjectStoreExt;
use slatedb::Db;
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
    S256(russh::keys::ssh_key::sha2::Sha256),
    S512(russh::keys::ssh_key::sha2::Sha512),
}

impl Hasher {
    /// `algo` is untrusted client input, so an unknown one is `None` rather than a default hash.
    pub fn new(algo: &str) -> Option<Hasher> {
        use russh::keys::ssh_key::sha2::Digest as _;
        match algo {
            "sha256" => Some(Hasher::S256(russh::keys::ssh_key::sha2::Sha256::new())),
            "sha512" => Some(Hasher::S512(russh::keys::ssh_key::sha2::Sha512::new())),
            _ => None,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        use russh::keys::ssh_key::sha2::Digest as _;
        match self {
            Hasher::S256(h) => h.update(bytes),
            Hasher::S512(h) => h.update(bytes),
        }
    }

    pub fn finish(self) -> Digest {
        use russh::keys::ssh_key::sha2::Digest as _;
        let (algo, hex) = match self {
            Hasher::S256(h) => ("sha256", crate::hex(&h.finalize())),
            Hasher::S512(h) => ("sha512", crate::hex(&h.finalize())),
        };
        Digest { algo: algo.into(), hex }
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

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.algo, self.hex)
    }
}

pub fn blob_path(owner: &str, d: &Digest) -> OsPath {
    OsPath::from(format!("blobs/{owner}/{}/{}", d.algo, d.hex))
}

pub fn manifest_path(owner: &str, name: &str, d: &Digest) -> OsPath {
    OsPath::from(format!("manifests/{owner}/{name}/{}/{}", d.algo, d.hex))
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

const IMAGE_KEY: &[u8] = b"image";
const PUBLIC_KEY: &[u8] = b"image/public";
const TAG_PREFIX: &str = "image/tag/";
fn tag_key(tag: &str) -> Vec<u8> {
    format!("{TAG_PREFIX}{tag}").into_bytes()
}

impl Store {
    /// The image's database. Opening one CREATES it, so callers that merely probe must go through
    /// `image_exists` — the same rule `db_for`/`repo_exists` follow for repos.
    pub async fn image_db(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        let (o, n) = crate::registry::pool_coords(owner, name);
        self.pool.get(o, &n).await
    }

    pub async fn image_exists(&self, owner: &str, name: &str) -> Result<bool> {
        let (o, n) = crate::registry::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(false);
        }
        Ok(self.image_db(owner, name).await?.get(IMAGE_KEY).await?.is_some())
    }

    /// Marks the image as existing. Registries create on first write, so every write path calls
    /// this rather than there being a create endpoint. Marking existence and setting visibility
    /// are two different writes — later tasks must call this, never `set_image_visibility`, to
    /// record that an image exists.
    pub(crate) async fn touch_image(&self, owner: &str, name: &str) -> Result<()> {
        self.image_db(owner, name).await?.put(IMAGE_KEY, b"1".as_slice()).await?;
        Ok(())
    }

    pub async fn put_tag(&self, owner: &str, name: &str, tag: &str, d: &Digest) -> Result<()> {
        // One handle for both puts: `touch_image` would resolve the pool entry a second time on
        // the hottest write path for no gain.
        let db = self.image_db(owner, name).await?;
        db.put(IMAGE_KEY, b"1".as_slice()).await?;
        db.put(tag_key(tag), d.to_string().into_bytes()).await?;
        Ok(())
    }

    pub async fn tag(&self, owner: &str, name: &str, tag: &str) -> Result<Option<Digest>> {
        // `pool.exists`, not `image_exists`: the probe only has to keep `image_db` from CREATING
        // a database for an image nobody pushed. A missing tag row already answers `None`, so the
        // extra IMAGE_KEY read `image_exists` adds proves nothing here — and this runs on every
        // pull.
        let (o, n) = crate::registry::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(None);
        }
        let v = self.image_db(owner, name).await?.get(tag_key(tag)).await?;
        Ok(v.and_then(|v| Digest::parse(&String::from_utf8_lossy(&v))))
    }

    pub async fn delete_tag(&self, owner: &str, name: &str, tag: &str) -> Result<()> {
        self.image_db(owner, name).await?.delete(tag_key(tag)).await?;
        Ok(())
    }

    pub async fn tags(&self, owner: &str, name: &str) -> Result<Vec<String>> {
        let (o, n) = crate::registry::pool_coords(owner, name);
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
    pub async fn tags_pointing_at(&self, owner: &str, name: &str, d: &Digest) -> Result<Vec<String>> {
        let (o, n) = crate::registry::pool_coords(owner, name);
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
    pub async fn bump_pulls(&self, owner: &str, name: &str, tag: &str) -> Result<()> {
        // Two concurrent pulls of the same tag on this node both read the same count and each
        // write `n+1` back, so one increment is lost — a single owning node is not a single
        // concurrent request. Serialize the read-increment-write per {owner}/{name}/{tag}.
        let lock = self.keyed_lock(&format!("pulls/{owner}/{name}/{tag}"));
        let _guard = lock.lock().await;
        let db = self.image_db(owner, name).await?;
        let key = format!("image/pulls/{tag}").into_bytes();
        let n: u64 = db
            .get(key.clone())
            .await?
            .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
            .unwrap_or(0);
        db.put(key, (n + 1).to_string().into_bytes()).await?;
        Ok(())
    }

    pub async fn pulls(&self, owner: &str, name: &str, tag: &str) -> Result<u64> {
        let v = self.image_db(owner, name).await?.get(format!("image/pulls/{tag}").into_bytes()).await?;
        Ok(v.and_then(|v| String::from_utf8_lossy(&v).parse().ok()).unwrap_or(0))
    }

    pub async fn image_is_public(&self, owner: &str, name: &str) -> Result<bool> {
        let (o, n) = crate::registry::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(false);
        }
        Ok(self.image_db(owner, name).await?.get(PUBLIC_KEY).await?.as_deref() == Some(b"1"))
    }

    /// Refreshes the listing-index marker after a manifest push: fresh `manifests`/`updated_ms`,
    /// visibility read from the DB (fail closed — a first push with no existing marker is created
    /// PRIVATE unless `image_is_public` already says otherwise). Serialized under the same
    /// `index/img/{owner}/{name}` key `set_image_visibility` uses, so a push racing a flip cannot
    /// interleave the marker swap. Callers must log-and-continue on error: a marker is a view, not
    /// something a push should ever fail over.
    pub async fn refresh_image_marker(&self, owner: &str, name: &str) -> Result<()> {
        let lock = self.keyed_lock(&format!("index/img/{owner}/{name}"));
        let _guard = lock.lock().await;
        let public = self.image_is_public(owner, name).await?;
        let (count, newest) = manifest_stat(self, owner, name).await?;
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
        crate::index::write(&self.os, crate::index::Kind::Img, owner, &m).await
    }

    /// Flips the DB row (source of truth for auth) and the listing-index marker together. Serialized
    /// per {owner}/{name} so two racing flips cannot interleave `index::write`'s delete-then-put
    /// (spec §6.5) — without the lock, a-public-then-b-private and a-private-then-b-public racing
    /// could leave both markers, or neither, present.
    pub async fn set_image_visibility(&self, owner: &str, name: &str, public: bool) -> Result<()> {
        let lock = self.keyed_lock(&format!("index/img/{owner}/{name}"));
        let _guard = lock.lock().await;
        // Remove-permissive-first (spec §6.2) applies to the whole flip, not just the marker
        // write below: on a private flip, delete the PUBLIC marker before the DB row changes, so
        // a crash between here and `index::write` can never leave a stale public marker sitting
        // over what the DB already calls private.
        if !public {
            let public_path = crate::index::path(true, crate::index::Kind::Img, owner, name);
            if let Err(e) = crate::index::ignore_not_found(self.os.delete(&public_path).await) {
                eprintln!("index pre-delete img {owner}/{name}: {e}"); // ponytail: eprintln
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
        if let Err(e) = crate::index::write(&self.os, crate::index::Kind::Img, owner, &m).await {
            eprintln!("index write img {owner}/{name}: {e}"); // ponytail: eprintln
        }
        Ok(())
    }

    // ponytail: a push or page-load racing this delete can re-open the database between the
    // evict and the file removal, leaving a db whose manifest names SSTs that are gone — a
    // broken image rather than a deleted one. The window is one node and milliseconds wide;
    // a delete-in-progress marker in the image db closes it if it ever bites.
    /// Wipes every database row this image owns: the bare `image` marker, `image/public`, every
    /// `image/tag/*`, every `image/pulls/*`, every `image/manifest-type/*` and every
    /// `image/referrer/*`. All of them start with `image`, and nothing else in this database does
    /// (`upload/*` is the only other key space here — see `referrers::key`'s doc comment) — so one
    /// prefix scan is exhaustive and safe. Scoped to THIS image's own database
    /// (`image_db(owner, name)`), so a sibling image's rows, which live in a different database
    /// entirely, are never touched. Does not touch the object store: callers delete manifest
    /// objects separately, and blobs are never this route's to remove (see `blobs::delete_blob`).
    pub async fn delete_image_rows(&self, owner: &str, name: &str) -> Result<()> {
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
    pub async fn delete_image(&self, owner: &str, name: &str) -> Result<()> {
        use slatedb::object_store::ObjectStore;
        self.delete_image_rows(owner, name).await?;
        // Cache keys are `{owner}/{name}/{digest}`; without this, a manifest GET'd just before
        // delete keeps serving stale bytes for this image until the 256-entry clear-on-full sweep.
        let cache_prefix = format!("{owner}/{name}/");
        self.manifest_cache.lock().unwrap().retain(|k, _| !k.starts_with(&cache_prefix));
        let (o, n) = crate::registry::pool_coords(owner, name);
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
