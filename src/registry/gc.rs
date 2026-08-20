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
/// with `continue` — `put_manifest` writes whatever bytes a client PUTs without validating them
/// as JSON, so an unparseable manifest is reachable, and skipping it would silently judge every
/// blob it names an orphan.
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
        // The manifest itself.
        if let Some(hex) = p.parts().next_back() {
            out.insert(format!("sha256:{}", hex.as_ref()));
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

fn collect(v: &serde_json::Value, out: &mut HashSet<String>) {
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
        let Some(hex) = m.location.parts().next_back() else { continue };
        let digest = format!("sha256:{}", hex.as_ref());
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
    let keep_again = referenced(store, owner).await?;
    doomed.retain(|p| {
        p.parts()
            .next_back()
            .map(|hex| !keep_again.contains(&format!("sha256:{}", hex.as_ref())))
            .unwrap_or(false)
    });
    let n = doomed.len();
    for p in doomed {
        match store.os.delete(&p).await {
            Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}
