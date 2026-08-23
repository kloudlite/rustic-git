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
use super::store::manifest_stat;
use crate::store::Store;
use crate::Result;
use slatedb::object_store::{ObjectStore, ObjectStoreExt};
use std::collections::HashSet;
use std::time::Duration;

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
async fn get_bytes(store: &Store, p: &slatedb::object_store::path::Path) -> Result<Vec<u8>> {
    Ok(store.os.get(p).await?.bytes().await?.to_vec())
}

pub async fn referenced(store: &Store, owner: &str) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let prefix = slatedb::object_store::path::Path::from(format!("manifests/{owner}"));
    let mut listing = store.os.list(Some(&prefix));
    let mut paths = vec![];
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        paths.push(m?.location);
    }
    for p in paths {
        let bytes = match get_bytes(store, &p).await {
            Ok(b) => b,
            Err(e) => {
                // Aborting the sweep here is correct — see the module doc — but a silent abort
                // means GC for this owner stops forever with nothing said. Name the owner and the
                // manifest so whoever is paged (or grepping logs later) knows exactly what to fix.
                eprintln!("gc: aborting sweep of {owner}: unreadable manifest {p}: {e}"); // ponytail: eprintln
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
                eprintln!("gc: aborting sweep of {owner}: unparseable manifest {p}: {e}"); // ponytail: eprintln
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
fn digest_from_path(p: &slatedb::object_store::path::Path) -> Option<String> {
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

/// Same default/env-override as `worker.rs` wires into `sweep_owner`'s `grace`. `worker.rs` is the
/// only caller; it lives here so the window's definition sits next to the sweep it governs.
pub fn blob_grace() -> Duration {
    std::env::var("RUSTIC_GIT_BLOB_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(3600))
}

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

    let image_names = crate::registry::list_dir_names(&store.os, &format!("repo/img/{owner}/")).await?;
    let image_set: HashSet<String> = image_names.into_iter().collect();

    let markers = index::list(&store.os, Kind::Img, owner, true).await?;
    let marker_names: HashSet<String> = markers.iter().map(|m| m.name.clone()).collect();

    let mut repaired = 0usize;

    // (b) marker with no backing image directory → remove.
    for m in &markers {
        if !image_set.contains(&m.name) && index::remove(&store.os, Kind::Img, owner, &m.name).await.is_ok() {
            repaired += 1;
        }
    }

    // (a) image directory with no marker → create PRIVATE, fail closed.
    for name in image_set.iter().filter(|n| !marker_names.contains(*n)) {
        let Ok((count, newest)) = manifest_stat(store, owner, name).await else { continue };
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
        if index::write(&store.os, Kind::Img, owner, &m).await.is_ok() {
            repaired += 1;
        }
    }

    // (c) marker present with a backing image directory, but stale stats → rewrite in place,
    // preserving visibility and every other field.
    for m in markers.into_iter().filter(|m| image_set.contains(&m.name)) {
        let Ok((count, newest)) = manifest_stat(store, owner, &m.name).await else { continue };
        let updated_ms = newest.unwrap_or(m.updated_ms);
        if m.manifests == count as u64 && m.updated_ms == updated_ms {
            continue;
        }
        let fixed = Marker { manifests: count as u64, updated_ms, ..m };
        // In-place, not `index::write`: this worker has no lock shared with a concurrent
        // visibility flip (cross-process, owning node only), so deleting "the other side" here
        // could race and undo a flip that just landed. Worst case both markers exist for a
        // moment, which `index::list` already reads as private — fail-closed by construction.
        if index::put_in_place(&store.os, Kind::Img, owner, &fixed).await.is_ok() {
            repaired += 1;
        }
    }

    Ok(repaired)
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
        crate::registry::list_dir_names(&store.os, &format!("repo/{owner}/")).await?.into_iter().collect();
    let markers = index::list(&store.os, Kind::Repo, owner, true).await?;

    let mut repaired = 0usize;

    // (b) marker with no backing repo directory → remove.
    for m in &markers {
        if !repo_set.contains(&m.name) && index::remove(&store.os, Kind::Repo, owner, &m.name).await.is_ok() {
            repaired += 1;
        }
    }

    // (a) repo directory with no marker → create PRIVATE, fail closed. `created_by`/`created_ms`
    // are what the owning node knows and this one does not; an empty author beats a guess.
    let marker_names: HashSet<String> = markers.iter().map(|m| m.name.clone()).collect();
    for name in repo_set.iter().filter(|n| !marker_names.contains(*n)) {
        let m = Marker {
            name: name.clone(),
            public: false,
            created_by: String::new(),
            created_ms: crate::ownership::now_ms() as i64,
            description: String::new(),
            manifests: 0,
            updated_ms: 0,
        };
        if index::write(&store.os, Kind::Repo, owner, &m).await.is_ok() {
            repaired += 1;
        }
    }

    Ok(repaired)
}

/// Delete this owner's unreferenced blobs. `grace` protects an in-flight push: a blob uploaded
/// before its manifest exists is unreferenced for as long as the push takes.
pub async fn sweep_owner(store: &Store, owner: &str, grace: Duration) -> Result<usize> {
    let keep = referenced(store, owner).await?;
    let prefix = slatedb::object_store::path::Path::from(format!("blobs/{owner}"));
    let mut listing = store.os.list(Some(&prefix));
    let mut doomed = vec![];
    let cutoff = std::time::SystemTime::now() - grace;
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        let m = m?;
        let Some(digest) = digest_from_path(&m.location) else { continue };
        if keep.contains(&digest) {
            continue;
        }
        if m.last_modified > chrono::DateTime::<chrono::Utc>::from(cutoff) {
            continue;
        }
        doomed.push(m.location);
    }
    // Grace protects a blob uploaded and not yet referenced. It does NOT protect an old blob a
    // client skipped uploading (a HEAD hit, or a cross-repo mount) and then referenced from a
    // manifest written after the scan above: that blob's own timestamp never changes, so the
    // grace check above cannot catch it. Re-reading `referenced()` now and deleting only what is
    // still unreferenced in both reads closes that window without any lock.
    // The other half of that protection is `put_manifest`, which refuses a manifest naming a blob
    // that is already gone, so a delete that wins this race produces a 404 the client can retry,
    // never a 201 over a missing layer.
    let keep_again = referenced(store, owner).await?;
    doomed.retain(|p| digest_from_path(p).is_some_and(|d| !keep_again.contains(&d)));
    let n = doomed.len();
    for p in doomed {
        match store.os.delete(&p).await {
            Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}
