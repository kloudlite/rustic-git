//! Sweeping blobs no manifest references.
//!
//! Scoped to ONE owner, which is the whole reason blobs are per-owner: a global content-addressed
//! store would make this sweep read every image in the fleet before it could delete anything, and
//! a sweep that must be right about everything is a sweep nobody dares run.
//!
//! The order is load-bearing. Read every manifest FIRST, then list the blobs, then delete only
//! blobs that are both unreferenced and older than the grace window. Listing first would let a
//! manifest written mid-sweep reference a blob the sweep had already decided was an orphan.
//!
//! `src/registry/blobs.rs`'s delete handler removes exactly the blob a client named; this is the
//! only other code path in the registry allowed to delete a blob.
use super::{blob_state, store::{manifest_stat, Digest}};
use crate::dbstore::Store;
use crate::Result;
use slatedb::object_store::{ObjectStore, ObjectStoreExt};
use std::collections::HashSet;
use std::time::Duration;

async fn get_bytes(store: &Store, p: &slatedb::object_store::path::Path) -> Result<slatedb::bytes::Bytes> {
    Ok(store.os.get(p).await?.bytes().await?)
}

/// Every digest referenced by any manifest of any of this owner's images — the manifests
/// themselves included, since a manifest referenced from an index is named by digest too.
///
/// The rule for this whole file: any uncertainty about what is referenced means delete nothing.
/// A manifest that cannot be read or parsed must ABORT the sweep with an error, never be skipped
/// with `continue`. `put_manifest` now refuses a body that is not a JSON object, but that only
/// narrows the door going forward: bytes written before that check existed, or written straight
/// to the object store bypassing the handler, are still reachable and still unparseable — and
/// skipping one would silently judge every blob it names an orphan. So the keep-biased abort stays.
/// `pub` (not `pub(crate)`) so `tests/registry_gc.rs` can call the two scan phases directly to
/// prove the mount-race fix in `sweep_owner`: there is no clean seam inside `sweep_owner` itself
/// to inject a write between its two internal reads, so the test drives `referenced()` the same
/// way `sweep_owner` does rather than contorting production code to expose one.
pub async fn referenced(store: &Store, owner: &str) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let prefix = slatedb::object_store::path::Path::from(format!("manifests/{owner}"));
    let mut listing = store.os.list(Some(&prefix));
    let mut paths = vec![];
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        paths.push(m?.location);
    }
    // Concurrent GETs, bounded: 500 manifests were 500 serial round trips per sweep tick.
    // `buffered` (ordered) rather than `buffer_unordered` so at most 16 manifest bodies are in
    // memory at once and the abort-on-first-error below fires deterministically.
    let mut fetched = futures::StreamExt::buffered(
        futures::StreamExt::map(futures::stream::iter(paths), |p| async move {
            let b = get_bytes(store, &p).await;
            (p, b)
        }),
        16,
    );
    while let Some((p, bytes)) = futures::StreamExt::next(&mut fetched).await {
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                // Aborting the sweep here is correct — see the module doc — but a silent abort
                // means GC for this owner stops forever with nothing said. Name the owner and the
                // manifest so whoever is paged (or grepping logs later) knows exactly what to fix.
                tracing::error!(owner = %owner, manifest = %p, reason = "unreadable_manifest", error = %e, "gc.sweep.aborted");
                return Err(e);
            }
        };
        // The manifest itself. Path is `manifests/{owner}/{name}/{algo}/{hex}`: the algo segment
        // is second-to-last, not always `sha256` — a sha512-pushed manifest must self-protect too.
        if let Some(digest) = digest_from_path(&p) {
            out.insert(digest);
        }
        let v: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(owner = %owner, manifest = %p, reason = "unparseable_manifest", error = %e, "gc.sweep.aborted");
                return Err(e.into());
            }
        };
        // config, layers, an index's "manifests", and "subject" all name digests. Walking the
        // JSON for every "digest" string catches all of them without a schema per media type —
        // and a digest this over-collects is a blob kept, never one deleted.
        collect(&v, &mut out);
    }
    Ok(out)
}

/// Reassembles `algo:hex` from the LAST TWO path segments (`.../{algo}/{hex}`), rather than
/// hardcoding `sha256:` — both `blobs/{owner}/{algo}/{hex}` and `manifests/{owner}/{name}/{algo}/{hex}`
/// carry the algorithm in the path, and a sha512 blob whose digest was mis-assembled as
/// `sha256:{hex}` would never match `referenced()`'s set and would be swept as an orphan.
pub(crate) fn digest_from_path(p: &slatedb::object_store::path::Path) -> Option<String> {
    let parts: Vec<_> = p.parts().collect();
    let hex = parts.last()?;
    let algo = parts.get(parts.len().checked_sub(2)?)?;
    Some(format!("{}:{}", algo.as_ref(), hex.as_ref()))
}

/// Every `"digest"` string anywhere in a manifest. Shared with `put_manifest`'s existence check so
/// the sweep and the push agree on what "referenced" means — a digest one walks and the other
/// does not is a blob one of them gets wrong.
pub(crate) fn collect(v: &serde_json::Value, out: &mut HashSet<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, v) in m {
                if k == "digest" {
                    if let Some(s) = v.as_str() {
                        out.insert(s.to_string());
                    }
                }
                collect(v, out);
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|v| collect(v, out)),
        _ => {}
    }
}

/// How long a blob is protected from the sweep after it is written — a fixed hour, not an
/// operator knob. `worker.rs` is the only caller; it lives here so the window's definition sits
/// next to the sweep it governs.
pub const BLOB_GRACE: Duration = Duration::from_secs(3600);

/// Reconciles this owner's image listing markers against object-store-visible truth.
///
/// This runs in the WORKER, which must never open an image/repo database (opening one on the
/// wrong node fences the legitimate owner — see the fencing invariant in the crate root docs).
/// That confines this function to two of the three ways a marker can drift, split with Task 7b:
///
/// - STRUCTURAL (this function, object-store reads only):
///   (a) an image directory with no marker at all → create one, PRIVATE (fail closed), stats
///   from `manifest_stat`;
///   (b) a marker whose image directory is gone → remove it;
///   (c) a marker whose `manifests`/`updated_ms` no longer match `manifest_stat` → rewrite,
///   preserving every other field (visibility included).
/// - VISIBILITY (owning node's duty, not this sweep's): a marker whose public/private side
///   disagrees with the image DB's own visibility row is left alone here — only the node that
///   owns the DB can read that row without fencing itself, so that repair belongs to Task 7b's
///   `reconcile_marker`, not this one.
///
/// Keep-biased like `sweep_owner`: any read/list error on one entry SKIPs that entry rather than
/// treating the uncertainty as grounds to remove or fabricate a marker.
pub async fn reconcile_owner(store: &Store, owner: &str) -> Result<usize> {
    use crate::index::{self, Kind, Marker};

    let image_names = crate::list_dir_names(&store.os, &format!("repo/img/{owner}/")).await?;
    let image_set: HashSet<String> = image_names.into_iter().collect();

    let markers = index::list(store, Kind::Img, owner, true).await?;
    let marker_names: HashSet<String> = markers.iter().map(|m| m.name.clone()).collect();

    let mut repaired = 0usize;

    // (b) marker with no backing image directory → remove.
    for m in &markers {
        if !image_set.contains(&m.name) && index::remove(store, Kind::Img, owner, &m.name).await.is_ok() {
            repaired += 1;
        }
    }

    // (a) image directory with no marker → create PRIVATE, fail closed.
    // `put_in_place`, not `index::write`: write deletes the other visibility's path first, and
    // this worker shares no lock with a visibility flip landing on the owning node at the same
    // moment — same reasoning as case (c) below.
    // `buffered`, not `join_all`: each stat is a LIST, and an owner with thousands of images
    // fanned out one per image at once — the same bound `referenced()` puts on its GETs.
    let missing: Vec<&String> = image_set.iter().filter(|n| !marker_names.contains(*n)).collect();
    let missing_names: Vec<&str> = missing.iter().map(|n| n.as_str()).collect();
    let missing_stats = stats_of(store, owner, &missing_names).await;
    for (name, stat) in missing.into_iter().zip(missing_stats) {
        let Ok((count, newest)) = stat else { continue };
        // `markers` came from a listing that is cached for `LIST_TTL_SECS`, so a marker written
        // seconds ago can be absent from it. Re-read the objects themselves before deciding one
        // is missing: without this the repair overwrites a live marker with a blank one, which
        // is how descriptions were being emptied.
        if index::read(&store.os, Kind::Img, owner, name).await.is_some() {
            continue;
        }
        let now = crate::ownership::now_ms() as i64;
        let m = Marker {
            name: name.clone(),
            public: false,
            created_by: String::new(),
            created_ms: now,
            description: String::new(),
            manifests: count as u64,
            updated_ms: newest.unwrap_or(now),
        };
        if index::put_in_place(store, Kind::Img, owner, &m).await.is_ok() {
            repaired += 1;
        }
    }

    // (c) marker present with a backing image directory, but stale stats → rewrite in place,
    // preserving visibility and every other field.
    let retained: Vec<Marker> = markers.into_iter().filter(|m| image_set.contains(&m.name)).collect();
    let retained_names: Vec<&str> = retained.iter().map(|m| m.name.as_str()).collect();
    let retained_stats = stats_of(store, owner, &retained_names).await;
    for (m, stat) in retained.into_iter().zip(retained_stats) {
        let Ok((count, newest)) = stat else { continue };
        let updated_ms = newest.unwrap_or(m.updated_ms);
        // Not equality: the owning node stamps `updated_ms` from its own clock AFTER the manifest
        // object lands, while this recomputes it from the object's `last_modified`, which S3
        // rounds to whole seconds. The two never agree exactly, so an exact compare rewrote every
        // marker once after every push for nothing. Only a gap a real missed push could open —
        // longer than any push takes to record itself — counts as stale.
        const UPDATED_SLOP_MS: i64 = 60_000;
        if m.manifests == count as u64 && (m.updated_ms - updated_ms).abs() <= UPDATED_SLOP_MS {
            continue;
        }
        let fixed = Marker { manifests: count as u64, updated_ms, ..m };
        // In-place, not `index::write`: this worker has no lock shared with a concurrent
        // visibility flip (cross-process, owning node only), so deleting "the other side" here
        // could race and undo a flip that just landed. Worst case both markers exist for a
        // moment, which `index::list` already reads as private — fail-closed by construction.
        if index::put_in_place(store, Kind::Img, owner, &fixed).await.is_ok() {
            repaired += 1;
        }
    }

    Ok(repaired)
}

/// `manifest_stat` per image, at most `STAT_CONCURRENCY` LISTs in flight, results in input order.
pub(crate) const STAT_CONCURRENCY: usize = 16;
/// The futures are collected before the stream is built: a closure mapping names to futures
/// held across the await is what made the worker's spawned lane "not general enough" over
/// lifetimes.
pub(crate) async fn stats_of(store: &Store, owner: &str, names: &[&str]) -> Vec<Result<(usize, Option<i64>)>> {
    let futs: Vec<_> = names.iter().map(|n| manifest_stat(store, owner, n)).collect();
    futures::StreamExt::collect(futures::StreamExt::buffered(futures::stream::iter(futs), STAT_CONCURRENCY)).await
}

/// The same structural repair for CODE REPO markers, with the same object-store-only discipline
/// and the same keep-biased rule — see `reconcile_owner` above for the split with the owning
/// node's visibility repair, which applies here verbatim: `meta/public` may only be read by the
/// node that owns the database.
///
/// Only two of the three cases exist here. Repo directories with no marker gain a PRIVATE one,
/// markers with no directory are removed — but there is no case (c): `manifests`/`updated_ms`
/// are image-only fields on `Marker`, and a code repo has no equivalent stat this sweep could
/// recompute from the object store, so a repo marker's body is never rewritten.
pub async fn reconcile_repo_owner(store: &Store, owner: &str) -> Result<usize> {
    use crate::index::{self, Kind, Marker};

    // Images live at `repo/img/{owner}/{name}` — the SAME prefix repos use. `img` is a reserved
    // owner name precisely so the two keyspaces stay distinguishable; sweeping it as a repo owner
    // would read every image OWNER as a repo name, find no matching repo markers, and go on to
    // delete markers it never should have looked at.
    if owner == "img" {
        return Ok(0);
    }

    let repo_set: HashSet<String> =
        crate::list_dir_names(&store.os, &format!("repo/{owner}/")).await?.into_iter().collect();
    let markers = index::list(store, Kind::Repo, owner, true).await?;

    let mut repaired = 0usize;

    // (b) marker with no backing repo directory → remove.
    for m in &markers {
        if !repo_set.contains(&m.name) && index::remove(store, Kind::Repo, owner, &m.name).await.is_ok() {
            repaired += 1;
        }
    }

    // (a) repo directory with no marker → create PRIVATE, fail closed. `created_by`/`created_ms`
    // are what the owning node knows and this one does not; an empty author beats a guess.
    // `put_in_place`, not `index::write`: write deletes the other visibility's path first, and
    // this worker shares no lock with a visibility flip landing on the owning node at the same
    // moment — same reasoning as case (c) below.
    let marker_names: HashSet<String> = markers.iter().map(|m| m.name.clone()).collect();
    for name in repo_set.iter().filter(|n| !marker_names.contains(*n)) {
        // Same re-read as case (a) above, same reason: the listing is cached, and recreating a
        // marker that already exists blanks its description.
        if index::read(&store.os, Kind::Repo, owner, name).await.is_some() {
            continue;
        }
        let m = Marker {
            name: name.clone(),
            public: false,
            created_by: String::new(),
            created_ms: crate::ownership::now_ms() as i64,
            description: String::new(),
            manifests: 0,
            updated_ms: 0,
        };
        if index::put_in_place(store, Kind::Repo, owner, &m).await.is_ok() {
            repaired += 1;
        }
    }

    Ok(repaired)
}

/// Is this generation older than `cutoff`? Reads the record's own `installed_at` when it has one
/// (no object-store call at all — this is CHANGE 1's whole point: a HEAD per candidate is what
/// made an idle sweep expensive), and HEADs the physical key ONLY as a fallback for a record
/// written before `installed_at` existed (`<= 0`, the serde default — `from_timestamp_millis(0)`
/// is 1970, which is why the old code judged every such record ancient regardless of its real
/// age). `None` means "skip this blob" (the object is gone — a race with GC or a client delete,
/// not this sweep's business), `Err` propagates untouched (keep-bias: an unreadable HEAD must
/// abort, never be read as either old or fresh).
async fn installed_before(store: &Store, active: &blob_state::BlobGeneration, cutoff: chrono::DateTime<chrono::Utc>) -> Result<Option<bool>> {
    if active.installed_at > 0 {
        let installed_at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(active.installed_at).unwrap_or(cutoff);
        return Ok(Some(installed_at <= cutoff));
    }
    let meta = match store.os.head(&slatedb::object_store::path::Path::from(active.physical_key.as_str())).await {
        Ok(meta) => meta,
        Err(slatedb::object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some(meta.last_modified <= cutoff))
}

/// One retired generation's fate this tick — CHANGE 3. `retired_at > 0` is judged against the
/// cutoff exactly like an active generation's `installed_at`; `retired_at == 0` (an older build's
/// record) is stamped with `now`, never deleted THIS round — the sweep does not know its age yet,
/// and keep-bias means an unknown age keeps. `now_millis` is threaded in rather than read here so
/// every retired entry in one sweep tick is stamped with the SAME instant.
async fn sweep_retired(store: &Store, owner: &str, digest: &Digest, retired: &blob_state::RetiredGeneration, cutoff_millis: i64, now_millis: i64) -> Result<()> {
    if retired.retired_at > 0 {
        if retired.retired_at <= cutoff_millis {
            blob_state::delete_retired(&store.os, owner, digest, &retired.physical_key).await?;
        }
        return Ok(());
    }
    blob_state::stamp_retired(&store.os, owner, digest, &retired.physical_key, now_millis).await
}

/// Delete this owner's unreferenced blobs. `grace` protects an in-flight push: a blob uploaded
/// before its manifest exists is unreferenced for as long as the push takes.
///
/// ponytail: a `BlobRecord` whose `active` is `None` and `retired` is empty is never deleted — the
/// object store has no conditional delete, so removing it could race an `install` writing a fresh
/// generation into that same key and orphan a live blob. `candidates()` therefore costs one GET
/// per digest this owner has EVER pushed, forever. Upgrade path: a conditional delete in the
/// store, or compacting empty records under a generation prefix.
pub async fn sweep_owner(store: &Store, owner: &str, grace: Duration) -> Result<usize> {
    let cutoff = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now() - grace);
    // The SAME cutoff, as millis, for `blob_state::has_live_pin` and `sweep_retired`: a pin's or a
    // retirement's own time is judged against the identical instant the generation's
    // `installed_at` is, so every reading of "is this old enough" in one sweep tick agrees.
    let cutoff_millis = cutoff.timestamp_millis();
    let now_millis = chrono::Utc::now().timestamp_millis();
    // CHANGE 2: candidates FIRST. In steady state every blob this owner has ever pushed already
    // has a state record, so the legacy-adoption listing below finds nothing to adopt and costs
    // one LIST with no GETs at all — the old order ran `resolve` (a GET, sometimes a write) on
    // every legacy path before even checking whether it needed to.
    let mut candidates = blob_state::candidates(&store.os, owner).await?;
    let mut known: HashSet<String> = candidates.iter().map(|(d, _, _)| d.to_string()).collect();
    let prefix = slatedb::object_store::path::Path::from(format!("blobs/{owner}"));
    let mut legacy = store.os.list(Some(&prefix));
    let mut adopted = false;
    while let Some(meta) = futures::StreamExt::next(&mut legacy).await {
        let meta = meta?;
        let Some(digest) = digest_from_path(&meta.location).and_then(|s| Digest::parse(&s)) else { continue };
        if known.contains(&digest.to_string()) {
            continue;
        }
        let _ = blob_state::resolve(&store.os, owner, &digest).await?;
        known.insert(digest.to_string());
        adopted = true;
    }
    if adopted {
        candidates = blob_state::candidates(&store.os, owner).await?;
    }
    let mut old_candidates = HashSet::new();
    for (digest, record, _) in &candidates {
        // A LEAKED pin (R-2: a push that died between pin and unpin) must age out, not block the
        // blob forever — `has_live_pin` reads each pin's own `@time`, not just "is the list empty".
        let Some(active) = record.active.as_ref().filter(|active| !blob_state::has_live_pin(active, cutoff_millis)) else { continue };
        if installed_before(store, active, cutoff).await? == Some(true) {
            old_candidates.insert(digest.to_string());
        }
    }
    if old_candidates.is_empty() {
        for (digest, record, _) in &candidates {
            for retired in &record.retired {
                sweep_retired(store, owner, digest, retired, cutoff_millis, now_millis).await?;
            }
        }
        return Ok(0);
    }
    let keep = referenced(store, owner).await?;
    let mut n = 0;
    for (digest, record, version) in candidates {
        for retired in &record.retired {
            sweep_retired(store, owner, &digest, retired, cutoff_millis, now_millis).await?;
        }
        if keep.contains(&digest.to_string()) || record.active.as_ref().is_none_or(|a| blob_state::has_live_pin(a, cutoff_millis)) {
            continue;
        }
        if !old_candidates.contains(&digest.to_string()) {
            continue;
        }
        let Some(active) = record.active.as_ref() else { continue };
        if installed_before(store, active, cutoff).await? != Some(true) {
            continue;
        }
        let Some(retired) = blob_state::retire_if_unpinned(&store.os, owner, &digest, &version, cutoff_millis).await? else { continue };
        // EXCEPTION to CHANGE 3's grace period, kept exactly as before: a blob the sweep ITSELF
        // just retired here is deleted immediately — it already served its grace as an
        // unreferenced ACTIVE generation (the `old_candidates` age check above), so making it
        // wait a second grace period as a retired one would be double-counting the same wait.
        blob_state::delete_retired(&store.os, owner, &digest, &retired).await?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::{memory::InMemory, path::Path as OsPath, PutPayload};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// Counts `head` and `get` calls — the two ways `sweep_owner` used to reach the object store
    /// per candidate. T1 asserts on `heads` specifically (CHANGE 1's promise: an idle sweep of
    /// already-adopted blobs makes none); `gets` exists for completeness / future tests.
    #[derive(Debug, Default)]
    struct Counting {
        inner: InMemory,
        heads: AtomicUsize,
        gets: AtomicUsize,
    }
    impl std::fmt::Display for Counting {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Counting")
        }
    }
    #[async_trait::async_trait]
    impl ObjectStore for Counting {
        async fn put_opts(&self, l: &OsPath, p: PutPayload, o: slatedb::object_store::PutOptions) -> slatedb::object_store::Result<slatedb::object_store::PutResult> {
            self.inner.put_opts(l, p, o).await
        }
        async fn put_multipart_opts(&self, l: &OsPath, o: slatedb::object_store::PutMultipartOptions) -> slatedb::object_store::Result<Box<dyn slatedb::object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(l, o).await
        }
        // `ObjectStore` has no separate `head` method to override — `ObjectStoreExt::head` (what
        // `sweep_owner` calls) is `get_opts` with `GetOptions.head = true`, so that flag is what
        // distinguishes the two here.
        async fn get_opts(&self, l: &OsPath, o: slatedb::object_store::GetOptions) -> slatedb::object_store::Result<slatedb::object_store::GetResult> {
            if o.head { self.heads.fetch_add(1, SeqCst); } else { self.gets.fetch_add(1, SeqCst); }
            self.inner.get_opts(l, o).await
        }
        fn delete_stream(&self, l: futures::stream::BoxStream<'static, slatedb::object_store::Result<OsPath>>) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<OsPath>> {
            self.inner.delete_stream(l)
        }
        fn list(&self, prefix: Option<&OsPath>) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::ObjectMeta>> {
            self.inner.list(prefix)
        }
        async fn list_with_delimiter(&self, prefix: Option<&OsPath>) -> slatedb::object_store::Result<slatedb::object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }
        async fn copy_opts(&self, from: &OsPath, to: &OsPath, o: slatedb::object_store::CopyOptions) -> slatedb::object_store::Result<()> {
            self.inner.copy_opts(from, to, o).await
        }
    }

    async fn env() -> (tempfile::TempDir, std::sync::Arc<Counting>, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let os = std::sync::Arc::new(Counting::default());
        let store = Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap();
        (tmp, os, store)
    }

    fn digest(b: &[u8]) -> Digest {
        Digest::of(b)
    }

    /// T1: three already-adopted (installed, `installed_at` set), fresh blobs — nothing to sweep,
    /// so this must not HEAD any of them. Today (before CHANGE 1/2): 3 HEADs, one per candidate,
    /// just to read `last_modified` for an age the record could already answer from `installed_at`.
    #[tokio::test]
    async fn an_idle_sweep_of_adopted_blobs_makes_no_head() {
        let (_tmp, os, store) = env().await;
        for i in 0..3 {
            let bytes = format!("blob-{i}").into_bytes();
            let d = digest(&bytes);
            let (_, path) = blob_state::new_generation("acme", &d);
            store.os.put(&path, PutPayload::from(bytes)).await.unwrap();
            blob_state::install(&store.os, "acme", &d, path.as_ref()).await.unwrap();
        }
        os.heads.store(0, SeqCst);
        let n = sweep_owner(&store, "acme", BLOB_GRACE).await.unwrap();
        assert_eq!(n, 0, "nothing is old enough to sweep");
        assert_eq!(os.heads.load(SeqCst), 0, "an idle sweep of adopted blobs must not HEAD any of them");
    }

    /// T2: the 1970 bug. A record with `installed_at: 0` (the shape a pre-this-field build wrote)
    /// must fall back to the blob's OWN age (its `last_modified`), not be judged ancient outright.
    /// Constructed by hand — `install` always sets a real `installed_at` now — writing the record
    /// bytes directly the way an older build's row would already sit in the store.
    #[tokio::test]
    async fn a_record_with_no_install_time_falls_back_to_the_blobs_own_age() {
        let (_tmp, _os, store) = env().await;
        let d = digest(b"no install time");
        let (_, path) = blob_state::new_generation("acme", &d);
        store.os.put(&path, PutPayload::from(b"no install time".to_vec())).await.unwrap();
        let record = blob_state::BlobRecord {
            nonce: "n".into(),
            active: Some(blob_state::BlobGeneration { physical_key: path.to_string(), pins: Vec::new(), installed_at: 0 }),
            retired: Vec::new(),
        };
        store.os.put(&blob_state::state_path("acme", &d), PutPayload::from(serde_json::to_vec(&record).unwrap())).await.unwrap();

        // Just written: not old enough under the default grace.
        let n = sweep_owner(&store, "acme", BLOB_GRACE).await.unwrap();
        assert_eq!(n, 0, "a just-written blob with installed_at: 0 must not read as 1970-ancient");
        assert!(store.os.head(&path).await.is_ok(), "the blob must still be here");

        // A grace long enough to put the blob's real (recent) age before the cutoff.
        let n = sweep_owner(&store, "acme", Duration::from_secs(0)).await.unwrap();
        assert_eq!(n, 1, "the same record, judged by its own recent age, is old enough with grace ZERO");
    }

    /// T3: a generation retired by a re-upload survives one sweep (the grace period), then goes.
    #[tokio::test]
    async fn a_generation_retired_by_a_reupload_survives_one_sweep() {
        let (_tmp, _os, store) = env().await;
        let d = digest(b"reuploaded");
        let (_, old) = blob_state::new_generation("acme", &d);
        store.os.put(&old, PutPayload::from("old")).await.unwrap();
        let old_key = old.to_string();
        blob_state::install(&store.os, "acme", &d, &old_key).await.unwrap();
        let (_, new) = blob_state::new_generation("acme", &d);
        store.os.put(&new, PutPayload::from("new")).await.unwrap();
        blob_state::install(&store.os, "acme", &d, new.as_ref()).await.unwrap();

        // Default grace: the retirement just happened, well inside the window.
        sweep_owner(&store, "acme", BLOB_GRACE).await.unwrap();
        assert!(store.os.head(&old).await.is_ok(), "the retired generation must survive its grace period");

        // A cutoff after `retired_at`: grace ZERO puts "now" at the cutoff, and the retirement
        // landed a moment before this call — so it is old enough.
        sweep_owner(&store, "acme", Duration::from_secs(0)).await.unwrap();
        assert!(store.os.head(&old).await.is_err(), "past its grace period, the retired generation is gone");
    }

    /// T4: a retired generation with no time at all (an older build's shape) is stamped, not
    /// deleted, on the sweep that first finds it — and NOT deleted even under grace ZERO, because
    /// the sweep does not yet know how old it is.
    #[tokio::test]
    async fn a_retired_generation_with_no_time_is_stamped_not_deleted() {
        let (_tmp, _os, store) = env().await;
        let d = digest(b"old format retirement");
        let (_, old) = blob_state::new_generation("acme", &d);
        store.os.put(&old, PutPayload::from("old")).await.unwrap();
        let (_, new) = blob_state::new_generation("acme", &d);
        store.os.put(&new, PutPayload::from("new")).await.unwrap();
        let record = blob_state::BlobRecord {
            nonce: "n".into(),
            active: Some(blob_state::BlobGeneration { physical_key: new.to_string(), pins: Vec::new(), installed_at: chrono::Utc::now().timestamp_millis() }),
            retired: vec![blob_state::RetiredGeneration { physical_key: old.to_string(), retired_at: 0 }],
        };
        store.os.put(&blob_state::state_path("acme", &d), PutPayload::from(serde_json::to_vec(&record).unwrap())).await.unwrap();

        sweep_owner(&store, "acme", Duration::from_secs(0)).await.unwrap();
        assert!(store.os.head(&old).await.is_ok(), "an unknown-age retirement must not be deleted on the sweep that first sees it");
        let (_, stored_record, _) = blob_state::candidates(&store.os, "acme").await.unwrap().into_iter().find(|(dig, _, _)| *dig == d).unwrap();
        let retired_at = stored_record.retired.iter().find(|r| r.physical_key == old.to_string()).unwrap().retired_at;
        assert!(retired_at > 0, "the retirement must be stamped with a real time this round");
    }

    /// T5: the exception to CHANGE 3 — a blob the sweep retires ITSELF this tick (an unreferenced,
    /// unpinned, aged-out ACTIVE generation `retire_if_unpinned` moves to `retired`) is deleted in
    /// the SAME sweep, not held for a second grace period.
    #[tokio::test]
    async fn a_blob_the_sweep_retires_is_deleted_in_the_same_sweep() {
        let (_tmp, _os, store) = env().await;
        let d = digest(b"swept and retired in one tick");
        let (_, generation) = blob_state::new_generation("acme", &d);
        store.os.put(&generation, PutPayload::from("swept and retired in one tick")).await.unwrap();
        blob_state::install(&store.os, "acme", &d, generation.as_ref()).await.unwrap();

        let n = sweep_owner(&store, "acme", Duration::from_secs(0)).await.unwrap();
        assert_eq!(n, 1, "the unreferenced, aged-out blob should have been swept");
        assert!(store.os.head(&generation).await.is_err(), "a blob the sweep retires itself must not wait a second grace period");
    }
}
