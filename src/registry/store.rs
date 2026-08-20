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
use slatedb::Db;
use std::sync::Arc;

/// A content digest, as it appears on the wire.
///
/// Parsing is the ONLY way a path segment becomes part of an object key, so it is strict on
/// purpose: lowercase hex, exactly 64 of it, algorithm `sha256`. Anything else — an upper-case
/// digest, a `..`, a second colon — is not a digest and never reaches the object store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    pub algo: String,
    pub hex: String,
}

impl Digest {
    pub fn parse(s: &str) -> Option<Digest> {
        let (algo, hex) = s.split_once(':')?;
        if algo != "sha256" || hex.len() != 64 {
            return None;
        }
        if !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return None;
        }
        Some(Digest { algo: algo.to_string(), hex: hex.to_string() })
    }

    pub fn of(bytes: &[u8]) -> Digest {
        use russh::keys::ssh_key::sha2::{Digest as _, Sha256};
        let hex: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
        Digest { algo: "sha256".into(), hex }
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

/// How many manifests an image has pushed — an object-store count, not a database read. Used by
/// the `images` browse route, which (being owner-scoped, not repo-scoped) cannot route to any one
/// image's database: see `browse_api::images`.
pub async fn manifest_count(store: &Store, owner: &str, name: &str) -> Result<usize> {
    Ok(manifest_stat(store, owner, name).await?.0)
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
        self.touch_image(owner, name).await?;
        self.image_db(owner, name)
            .await?
            .put(tag_key(tag), d.to_string().into_bytes())
            .await?;
        Ok(())
    }

    pub async fn tag(&self, owner: &str, name: &str, tag: &str) -> Result<Option<Digest>> {
        if !self.image_exists(owner, name).await? {
            return Ok(None);
        }
        let v = self.image_db(owner, name).await?.get(tag_key(tag)).await?;
        Ok(v.and_then(|v| Digest::parse(&String::from_utf8_lossy(&v))))
    }

    pub async fn delete_tag(&self, owner: &str, name: &str, tag: &str) -> Result<()> {
        self.image_db(owner, name).await?.delete(tag_key(tag)).await?;
        Ok(())
    }

    /// Sorted lexically, which is the order the spec requires `tags/list` to return.
    pub async fn tags(&self, owner: &str, name: &str) -> Result<Vec<String>> {
        if !self.image_exists(owner, name).await? {
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
        out.sort();
        Ok(out)
    }

    /// One more pull of `tag`. A pull is a manifest GET by tag — the request docker makes exactly
    /// once per `docker pull` — counted on the node that owns the image, so there is one writer
    /// and the count cannot race. GETs by digest are deliberately uncounted: docker re-reads by
    /// digest after resolving the tag, and counting both would double every pull.
    pub async fn bump_pulls(&self, owner: &str, name: &str, tag: &str) -> Result<()> {
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
        if !self.image_exists(owner, name).await? {
            return Ok(false);
        }
        Ok(self.image_db(owner, name).await?.get(PUBLIC_KEY).await?.as_deref() == Some(b"1"))
    }

    pub async fn set_image_visibility(&self, owner: &str, name: &str, public: bool) -> Result<()> {
        self.touch_image(owner, name).await?;
        self.image_db(owner, name)
            .await?
            .put(PUBLIC_KEY, if public { b"1".as_slice() } else { b"0".as_slice() })
            .await?;
        Ok(())
    }
}
