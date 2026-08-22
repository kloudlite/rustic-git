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

use futures::future::join_all;
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
#[derive(Debug, Clone, PartialEq)]
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

pub(crate) fn ignore_not_found(r: Result<(), OsError>) -> crate::Result<()> {
    match r {
        Ok(()) | Err(OsError::NotFound { .. }) => Ok(()),
        Err(e) => Err(crate::err(format!("index: {e}"))),
    }
}

/// Fail-closed flip: the permissive (public) marker is always deleted before the new one is
/// written, in both directions, so the public marker is never present alongside a fresher private
/// write — the moment between delete and put is the only window, and it is strictly more private,
/// never less.
pub async fn write(os: &Arc<dyn ObjectStore>, kind: Kind, owner: &str, m: &Marker) -> crate::Result<()> {
    let public_path = path(true, kind, owner, &m.name);
    let private_path = path(false, kind, owner, &m.name);
    if m.public {
        ignore_not_found(os.delete(&private_path).await)?;
        os.put(&public_path, PutPayload::from(body(m))).await.map_err(|e| crate::err(format!("index: {e}")))?;
    } else {
        ignore_not_found(os.delete(&public_path).await)?;
        os.put(&private_path, PutPayload::from(body(m))).await.map_err(|e| crate::err(format!("index: {e}")))?;
    }
    Ok(())
}

/// Rewrites a marker at the path its current visibility already lives at, without touching the
/// other side. For repair paths that only have object-store reads to go on (the GC sweep,
/// cross-process, no lock shared with a live visibility flip) — deleting the *other* marker from
/// here could race a concurrent flip and undo it. Worst case this leaves both markers for an
/// instant; `list`/`read` already treat that as private (fail closed), so it's safe to leave for
/// the owning node's own write to clean up.
pub async fn put_in_place(os: &Arc<dyn ObjectStore>, kind: Kind, owner: &str, m: &Marker) -> crate::Result<()> {
    os.put(&path(m.public, kind, owner, &m.name), PutPayload::from(body(m)))
        .await
        .map_err(|e| crate::err(format!("index: {e}")))?;
    Ok(())
}

/// Deletes both paths, public first (permissive first — never leave the permissive one behind
/// while the private one is already gone). `NotFound` on either is tolerated.
pub async fn remove(os: &Arc<dyn ObjectStore>, kind: Kind, owner: &str, name: &str) -> crate::Result<()> {
    let public_path = path(true, kind, owner, name);
    let private_path = path(false, kind, owner, name);
    ignore_not_found(os.delete(&public_path).await)?;
    ignore_not_found(os.delete(&private_path).await)?;
    Ok(())
}

/// Reads a marker by name, trying the public path then the private one — callers that need to
/// preserve fields (`manifests`/`created_*`/`description`) across a visibility flip don't know
/// which prefix the marker currently lives under. `None` if neither exists (marker never written,
/// or a prior write failed — the DB write it followed is still the source of truth).
pub async fn read(os: &Arc<dyn ObjectStore>, kind: Kind, owner: &str, name: &str) -> Option<Marker> {
    for public in [true, false] {
        if let Some(r) = fetch_one(os, path(public, kind, owner, name), public).await {
            return r.ok();
        }
    }
    None
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
pub async fn list(os: &Arc<dyn ObjectStore>, kind: Kind, owner: &str, include_private: bool) -> crate::Result<Vec<Marker>> {
    use futures::StreamExt;

    let public_prefix = Path::from(format!("index/public/{}/{owner}/", kind.seg()));
    let public_names: Vec<Path> = os
        .list(Some(&public_prefix))
        .map(|r| r.map(|m| m.location))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| crate::err(format!("index: {e}")))?;

    // Listed even when private entries are not being returned: the private prefix is what makes
    // a crashed flip (both markers present) read as private, and that fail-closed rule has to
    // hold hardest for exactly the caller who may not see private names.
    let private_prefix = Path::from(format!("index/private/{}/{owner}/", kind.seg()));
    let mut private_names: Vec<Path> = os
        .list(Some(&private_prefix))
        .map(|r| r.map(|m| m.location))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| crate::err(format!("index: {e}")))?;

    // A private marker wins over a same-named public one (fail-closed on a crashed flip), so
    // drop any public entry whose name also has a private one before fetching bodies.
    let private_stems: std::collections::HashSet<String> =
        private_names.iter().filter_map(|p| p.filename().map(|s| s.to_string())).collect();
    let public_names: Vec<Path> =
        public_names.into_iter().filter(|p| p.filename().is_none_or(|n| !private_stems.contains(n))).collect();
    if !include_private {
        private_names.clear();
    }

    let mut futs = Vec::new();
    for p in public_names {
        futs.push(fetch_one(os, p, true));
    }
    for p in private_names {
        futs.push(fetch_one(os, p, false));
    }
    let results = join_all(futs).await;
    let mut markers = Vec::with_capacity(results.len());
    for r in results.into_iter().flatten() {
        markers.push(r?);
    }
    markers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(markers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::memory::InMemory;

    fn mem_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
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
        let os = mem_store();
        let m = marker("web", true);
        write(&os, Kind::Repo, "alice", &m).await.unwrap();
        write(&os, Kind::Repo, "alice", &marker("web", false)).await.unwrap();
        // public path must be gone, private present
        assert!(os.get(&path(true, Kind::Repo, "alice", "web")).await.is_err());
        assert!(os.get(&path(false, Kind::Repo, "alice", "web")).await.is_ok());
    }

    #[tokio::test]
    async fn both_markers_read_as_private() {
        let os = mem_store();
        // simulate a crashed flip: both present
        os.put(&path(true, Kind::Repo, "a", "x"), body(&marker("x", true)).into()).await.unwrap();
        os.put(&path(false, Kind::Repo, "a", "x"), body(&marker("x", false)).into()).await.unwrap();
        let l = list(&os, Kind::Repo, "a", true).await.unwrap();
        assert_eq!(l.len(), 1);
        assert!(!l[0].public);
    }

    #[tokio::test]
    async fn anonymous_listing_never_contains_private_names() {
        let os = mem_store();
        write(&os, Kind::Img, "a", &marker("secret", false)).await.unwrap();
        write(&os, Kind::Img, "a", &marker("open", true)).await.unwrap();
        let l = list(&os, Kind::Img, "a", false).await.unwrap();
        assert_eq!(l.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["open"]);
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
