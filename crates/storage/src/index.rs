//! Path-encoded listing index: `index/{public|private}/{repo|img}/{owner}/{name}` markers that
//! let a listing answer "what does this owner have, and is it public" without opening every
//! repo/image database. Two rules, load-bearing:
//!
//! - Markers are views, never authorization. A marker says what to show in a list; it is never
//!   consulted to decide whether a request may read or write the thing it describes. That check
//!   always goes through the real owning database.
//! - Remove-permissive-first. Wherever a change could leave a stale permissive marker next to (or
//!   instead of) the real state, the public/permissive marker is deleted before anything else
//!   happens, so a crash mid-operation is caught by `both_markers_read_as_private` failing closed
//!   rather than a dangling public marker leaking a name that should be private.

use crate::store::Store;
use slatedb::object_store::{path::Path, Error as OsError, ObjectStore, ObjectStoreExt, PutPayload};
use std::sync::Arc;

/// The two kinds of thing a marker can describe; `seg` is the path segment for each.
#[derive(Clone, Copy)]
pub enum Kind {
    Repo,
    Img,
}

impl Kind {
    pub fn seg(&self) -> &'static str {
        match self {
            Kind::Repo => "repo",
            Kind::Img => "img",
        }
    }
}

/// A listing entry. `manifests` and `updated_ms` are always 0 for code repos — only images use
/// them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Marker {
    pub name: String,
    pub public: bool,
    pub created_by: String,
    pub created_ms: i64,
    pub description: String,
    pub manifests: u64,
    pub updated_ms: i64,
}

/// Where a marker lives: `index/{public|private}/{repo|img}/{owner}/{name}`.
pub fn path(public: bool, kind: Kind, owner: &str, name: &str) -> Path {
    let vis = if public { "public" } else { "private" };
    Path::from(format!("index/{vis}/{}/{owner}/{name}", kind.seg()))
}

/// `k=v` lines, `description` last so it may itself contain `=`.
fn body(m: &Marker) -> Vec<u8> {
    format!(
        "v=1\npublic={}\ncreated_by={}\ncreated_ms={}\nmanifests={}\nupdated_ms={}\ndescription={}",
        m.public, m.created_by, m.created_ms, m.manifests, m.updated_ms, m.description
    )
    .into_bytes()
}

/// Decodes a marker body. Unknown keys are ignored (forward compat); missing keys default
/// (`manifests`/`updated_ms` to 0). `name` and `public` come from the path, not the body, since
/// `public` in the body only records what was true when it was written.
fn decode(name: &str, public: bool, bytes: &[u8]) -> crate::Result<Marker> {
    let s = std::str::from_utf8(bytes).map_err(|e| crate::err(format!("index marker: {e}")))?;
    let mut created_by = String::new();
    let mut created_ms = 0i64;
    let mut manifests = 0u64;
    let mut updated_ms = 0i64;
    let mut description = String::new();
    for line in s.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "created_by" => created_by = v.to_string(),
            "created_ms" => created_ms = v.parse().unwrap_or(0),
            "manifests" => manifests = v.parse().unwrap_or(0),
            "updated_ms" => updated_ms = v.parse().unwrap_or(0),
            "description" => description = v.to_string(),
            _ => {}
        }
    }
    Ok(Marker { name: name.to_string(), public, created_by, created_ms, description, manifests, updated_ms })
}

pub fn ignore_not_found(r: Result<(), OsError>) -> crate::Result<()> {
    match r {
        Ok(()) | Err(OsError::NotFound { .. }) => Ok(()),
        Err(e) => Err(crate::err(format!("index: {e}"))),
    }
}

/// Fail-closed flip: the permissive (public) marker is always deleted before the new one is
/// written, in both directions, so the public marker is never present alongside a fresher private
/// write — the moment between delete and put is the only window, and it is strictly more private,
/// never less.
pub async fn write(store: &Store, kind: Kind, owner: &str, m: &Marker) -> crate::Result<()> {
    let os = &store.os;
    let public_path = path(true, kind, owner, &m.name);
    let private_path = path(false, kind, owner, &m.name);
    if m.public {
        ignore_not_found(os.delete(&private_path).await)?;
        os.put(&public_path, PutPayload::from(body(m))).await.map_err(|e| crate::err(format!("index: {e}")))?;
    } else {
        ignore_not_found(os.delete(&public_path).await)?;
        os.put(&private_path, PutPayload::from(body(m))).await.map_err(|e| crate::err(format!("index: {e}")))?;
    }
    forget_listing(store, kind, owner).await
}

/// The cached listings of one owner, keyed like a repo's responses so `bump_generation` orphans
/// every page at once. Every marker write goes through this file, so every writer — the api tier
/// creating a repo, an owning node flipping an image, the GC worker's reconcile — drops it, and
/// a listing can never outlive the change it hides by more than `LIST_TTL_SECS`.
fn listing_key(kind: Kind, owner: &str) -> String {
    format!("index/{}/{owner}", kind.seg())
}

/// Short on purpose: a marker written by a tier whose cache is disabled (dev, tests) is not
/// dropped from anyone's cache, and the TTL is all that bounds how long it stays hidden.
const LIST_TTL_SECS: u64 = 5;

/// Not fire-and-forget: a private flip whose listing purge failed would keep answering the name
/// to anonymous callers for the TTL — `write` reports it, like `bump_generation` documents.
async fn forget_listing(store: &Store, kind: Kind, owner: &str) -> crate::Result<()> {
    store.cache.bump_generation(&listing_key(kind, owner)).await
}

/// Rewrites a marker at the path its current visibility already lives at, without touching the
/// other side. For repair paths that only have object-store reads to go on (the GC sweep,
/// cross-process, no lock shared with a live visibility flip) — deleting the *other* marker from
/// here could race a concurrent flip and undo it. Worst case this leaves both markers for an
/// instant; `list`/`read` already treat that as private (fail closed), so it's safe to leave for
/// the owning node's own write to clean up.
pub async fn put_in_place(store: &Store, kind: Kind, owner: &str, m: &Marker) -> crate::Result<()> {
    store
        .os
        .put(&path(m.public, kind, owner, &m.name), PutPayload::from(body(m)))
        .await
        .map_err(|e| crate::err(format!("index: {e}")))?;
    forget_listing(store, kind, owner).await
}

/// Deletes both paths, public first (permissive first — never leave the permissive one behind
/// while the private one is already gone). `NotFound` on either is tolerated.
pub async fn remove(store: &Store, kind: Kind, owner: &str, name: &str) -> crate::Result<()> {
    let public_path = path(true, kind, owner, name);
    let private_path = path(false, kind, owner, name);
    ignore_not_found(store.os.delete(&public_path).await)?;
    ignore_not_found(store.os.delete(&private_path).await)?;
    forget_listing(store, kind, owner).await
}

/// Reads a marker by name, trying the public path then the private one — callers that need to
/// preserve fields (`manifests`/`created_*`/`description`) across a visibility flip don't know
/// which prefix the marker currently lives under. `None` if neither exists (marker never written,
/// or a prior write failed — the DB write it followed is still the source of truth).
pub async fn read(os: &Arc<dyn ObjectStore>, kind: Kind, owner: &str, name: &str) -> Option<Marker> {
    // Both paths at once: a private repo used to pay the public-path miss as a full round trip
    // before even asking for the path it lives on. Public still wins a (never-legal) tie, and a
    // found-but-unparseable public marker still answers None without trying private, matching
    // the old sequential loop byte-for-byte.
    let (pu, pr) = tokio::join!(
        fetch_one(os, path(true, kind, owner, name), true),
        fetch_one(os, path(false, kind, owner, name), false),
    );
    match pu {
        Some(r) => r.ok(),
        None => pr?.ok(),
    }
}

async fn fetch_one(os: &Arc<dyn ObjectStore>, p: Path, public: bool) -> Option<crate::Result<Marker>> {
    let name = p.filename()?.to_string();
    match os.get(&p).await {
        Ok(res) => {
            let bytes = match res.bytes().await {
                Ok(b) => b,
                Err(e) => return Some(Err(crate::err(format!("index: {e}")))),
            };
            Some(decode(&name, public, &bytes))
        }
        Err(OsError::NotFound { .. }) => None,
        Err(e) => Some(Err(crate::err(format!("index: {e}")))),
    }
}

/// Lists an owner's markers of one kind. Always includes public entries; includes private ones
/// only when `include_private` is true — an anonymous listing must never pass `true`, since that
/// is the only thing keeping a private name out of the result. A name present under both
/// prefixes (a crashed flip) is returned once, as private, matching `write`'s fail-closed
/// contract. Sorted by name.
pub async fn list(store: &Store, kind: Kind, owner: &str, include_private: bool) -> crate::Result<Vec<Marker>> {
    list_page(store, kind, owner, include_private, None, usize::MAX).await
}

/// `list`, but only the markers named after `after` (exclusive), at most `n` of them. The two
/// prefix LISTs are names only and cheap; it is the body GETs that made an owner with thousands
/// of repos fire thousands of concurrent reads per page, so only the page's bodies are fetched,
/// eight at a time. The decoded page is cached per owner for `LIST_TTL_SECS` and dropped by
/// every marker write (see `forget_listing`).
pub async fn list_page(
    store: &Store,
    kind: Kind,
    owner: &str,
    include_private: bool,
    after: Option<&str>,
    n: usize,
) -> crate::Result<Vec<Marker>> {
    use futures::StreamExt;
    let os = &store.os;
    let cache_key = listing_key(kind, owner);
    let suffix = format!("list/{include_private}/{}/{n}", after.unwrap_or(""));
    // The generation is read BEFORE the object store is, so a write landing mid-listing cannot be
    // hidden by a `put` keyed on a generation it already moved past (`put_at`'s contract).
    let generation = store.cache.generation(&cache_key).await;
    if let Some(hit) = store.cache.get(&cache_key, &suffix).await {
        if let Ok(markers) = serde_json::from_slice::<Vec<Marker>>(&hit) {
            return Ok(markers);
        }
    }

    async fn names(os: &Arc<dyn ObjectStore>, prefix: Path) -> crate::Result<Vec<Path>> {
        os.list(Some(&prefix))
            .map(|r| r.map(|m| m.location))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| crate::err(format!("index: {e}")))
    }
    let public_names = names(os, Path::from(format!("index/public/{}/{owner}/", kind.seg()))).await?;
    // Listed even when private entries are not being returned: the private prefix is what makes
    // a crashed flip (both markers present) read as private, and that fail-closed rule has to
    // hold hardest for exactly the caller who may not see private names.
    let private_names = names(os, Path::from(format!("index/private/{}/{owner}/", kind.seg()))).await?;

    // A private marker wins over a same-named public one (fail-closed on a crashed flip), so
    // drop any public entry whose name also has a private one before fetching bodies.
    let private_stems: std::collections::HashSet<String> =
        private_names.iter().filter_map(|p| p.filename().map(|s| s.to_string())).collect();
    let mut wanted: Vec<(Path, bool)> = public_names
        .into_iter()
        .filter(|p| p.filename().is_none_or(|n| !private_stems.contains(n)))
        .map(|p| (p, true))
        .collect();
    if include_private {
        wanted.extend(private_names.into_iter().map(|p| (p, false)));
    }
    wanted.sort_by(|a, b| a.0.filename().cmp(&b.0.filename()));
    let page = wanted
        .into_iter()
        .filter(|(p, _)| after.is_none_or(|a| p.filename().is_some_and(|f| f > a)))
        .take(n);

    let mut markers: Vec<Marker> = futures::stream::iter(page)
        .map(|(p, public)| fetch_one(os, p, public))
        .buffer_unordered(8)
        .filter_map(|r| async move { r })
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<crate::Result<_>>()?;
    markers.sort_by(|a, b| a.name.cmp(&b.name));
    if let (Some(generation), Ok(encoded)) = (generation, serde_json::to_vec(&markers)) {
        store.cache.put_at(generation, &cache_key, &suffix, &encoded, LIST_TTL_SECS).await;
    }
    Ok(markers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::memory::InMemory;

    async fn mem_store() -> Store {
        let tmp = std::env::temp_dir().join(format!("index-test-{}", crate::ownership::now_ms()));
        Store::open(Arc::new(InMemory::new()), tmp, false).await.unwrap()
    }

    fn marker(name: &str, public: bool) -> Marker {
        Marker {
            name: name.to_string(),
            public,
            created_by: "alice@example.com".to_string(),
            created_ms: 1755772800000,
            description: "my thing".to_string(),
            manifests: 0,
            updated_ms: 0,
        }
    }

    #[tokio::test]
    async fn flip_never_leaves_a_public_marker_beside_private() {
        let s = mem_store().await;
        let m = marker("web", true);
        write(&s, Kind::Repo, "alice", &m).await.unwrap();
        write(&s, Kind::Repo, "alice", &marker("web", false)).await.unwrap();
        // public path must be gone, private present
        assert!(s.os.get(&path(true, Kind::Repo, "alice", "web")).await.is_err());
        assert!(s.os.get(&path(false, Kind::Repo, "alice", "web")).await.is_ok());
    }

    #[tokio::test]
    async fn both_markers_read_as_private() {
        let s = mem_store().await;
        // simulate a crashed flip: both present
        s.os.put(&path(true, Kind::Repo, "a", "x"), body(&marker("x", true)).into()).await.unwrap();
        s.os.put(&path(false, Kind::Repo, "a", "x"), body(&marker("x", false)).into()).await.unwrap();
        let l = list(&s, Kind::Repo, "a", true).await.unwrap();
        assert_eq!(l.len(), 1);
        assert!(!l[0].public);
    }

    #[tokio::test]
    async fn anonymous_listing_never_contains_private_names() {
        let s = mem_store().await;
        write(&s, Kind::Img, "a", &marker("secret", false)).await.unwrap();
        write(&s, Kind::Img, "a", &marker("open", true)).await.unwrap();
        let l = list(&s, Kind::Img, "a", false).await.unwrap();
        assert_eq!(l.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["open"]);
    }

    /// An object store that counts body GETs and their high-water concurrency: the two numbers a
    /// listing page is bounded by. Delegation only — every other method is the inner store's.
    #[derive(Debug)]
    struct Counting {
        inner: InMemory,
        gets: std::sync::atomic::AtomicUsize,
        in_flight: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    impl std::fmt::Display for Counting {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Counting")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Counting {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: slatedb::object_store::PutOptions,
        ) -> slatedb::object_store::Result<slatedb::object_store::PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }
        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: slatedb::object_store::PutMultipartOptions,
        ) -> slatedb::object_store::Result<Box<dyn slatedb::object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }
        async fn get_opts(
            &self,
            location: &Path,
            options: slatedb::object_store::GetOptions,
        ) -> slatedb::object_store::Result<slatedb::object_store::GetResult> {
            use std::sync::atomic::Ordering::SeqCst;
            self.gets.fetch_add(1, SeqCst);
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            // Long enough that an unbounded fan-out would overlap every GET.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let r = self.inner.get_opts(location, options).await;
            self.in_flight.fetch_sub(1, SeqCst);
            r
        }
        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<'static, slatedb::object_store::Result<Path>>,
        ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }
        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::ObjectMeta>> {
            self.inner.list(prefix)
        }
        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> slatedb::object_store::Result<slatedb::object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: slatedb::object_store::CopyOptions,
        ) -> slatedb::object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// A page fetches only its own bodies, at most eight at once, and a cached page fetches none.
    #[tokio::test]
    async fn a_page_fetches_its_own_bodies_eight_at_a_time_and_is_cached() {
        let counting = Arc::new(Counting {
            inner: InMemory::new(),
            gets: Default::default(),
            in_flight: Default::default(),
            peak: Default::default(),
        });
        let tmp = std::env::temp_dir().join(format!("index-page-{}", crate::ownership::now_ms()));
        let mut s = Store::open(counting.clone(), tmp, false).await.unwrap();
        s.cache = Arc::new(crate::cache::Cache::memory());
        for i in 0..40 {
            write(&s, Kind::Repo, "a", &marker(&format!("r{i:02}"), i % 2 == 0)).await.unwrap();
        }
        counting.gets.store(0, std::sync::atomic::Ordering::SeqCst);

        let page = list_page(&s, Kind::Repo, "a", true, Some("r09"), 10).await.unwrap();
        assert_eq!(page.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), (10..20).map(|i| format!("r{i}")).collect::<Vec<_>>().iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(counting.gets.load(std::sync::atomic::Ordering::SeqCst), 10, "only the page's bodies");
        assert!(counting.peak.load(std::sync::atomic::Ordering::SeqCst) <= 8, "concurrency must be bounded");

        let again = list_page(&s, Kind::Repo, "a", true, Some("r09"), 10).await.unwrap();
        assert_eq!(again, page);
        assert_eq!(counting.gets.load(std::sync::atomic::Ordering::SeqCst), 10, "a cached page reads nothing");

        // Any marker write drops the owner's listings: the next page is fresh.
        write(&s, Kind::Repo, "a", &marker("r15", false)).await.unwrap();
        let fresh = list_page(&s, Kind::Repo, "a", true, Some("r09"), 10).await.unwrap();
        assert!(!fresh.iter().find(|m| m.name == "r15").unwrap().public);
        assert_eq!(counting.gets.load(std::sync::atomic::Ordering::SeqCst), 20);

        // The full list is the whole thing, still eight at a time.
        assert_eq!(list(&s, Kind::Repo, "a", true).await.unwrap().len(), 40);
        assert!(counting.peak.load(std::sync::atomic::Ordering::SeqCst) <= 8);
    }

    #[tokio::test]
    async fn body_roundtrips_including_equals_in_description() {
        let m = Marker {
            name: "x".into(),
            public: true,
            created_by: "b".into(),
            created_ms: 5,
            description: "a=b=c".into(),
            manifests: 2,
            updated_ms: 9,
        };
        assert_eq!(decode("x", true, &body(&m)).unwrap().description, "a=b=c");
    }
}
