//! Repack: consolidate a repo's packs, which accumulate one per push and are never rewritten.
//!
//! Two shapes over one tail. `repack` (admin, server stopped) rebuilds from the refs and so also
//! drops whatever they no longer reach. `consolidate` (the owning node's lane, online) rewrites
//! every object of the packs it listed into one, WITHOUT reachability — that is what keeps it
//! from racing a push: pushes are not serialised against each other (refs go through a DB
//! transaction, packs are uploaded first), so "reachable from the refs" and "in the packs" can
//! disagree for the width of one push, and a pack rebuilt from the refs in that window would
//! drop the push's objects. Copying every object of every listed pack has no such window: a
//! push's pack is either listed (copied whole) or newer (never touched).
//!
//! Ordering is the crash-safety: the new pack is uploaded and recorded before any old one is
//! touched, and each old file leaves the index BEFORE it leaves the object store. A crash
//! anywhere leaves duplicates (both packs indexed, or an unindexed orphan in the store), never an
//! index row naming a file that is gone — which is a repo `open_repo` can no longer open.

use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use slatedb::object_store::{path::Path as OsPath, ObjectStoreExt};
use std::sync::atomic::AtomicBool;

/// How many packs a repo may hold before the owner's lane consolidates them.
pub fn max_packs() -> usize {
    rustic_git_storage::config::env("RUSTIC_GIT_REPACK_PACKS", "32").parse().unwrap_or(32)
}

/// Extension trait, not an inherent `impl Store`: `Store` lives in `storage` and the orphan rule
/// forbids it; the code stays here because it needs `gix-pack`, which `storage` must not depend
/// on. Import this wherever `.repack(...)`/`.consolidate(...)` is called.
#[allow(async_fn_in_trait)]
pub trait RepackExt {
    /// Rebuild from the refs and drop the rest: garbage goes too. Offline only (`admin repack`).
    /// Returns (packs_before, packs_after).
    async fn repack(&self, owner: &str, name: &str) -> Result<(usize, usize)>;
    /// Rewrite every object of the current packs into one. Safe while the repo serves pushes.
    /// Returns (packs_before, packs_after).
    async fn consolidate(&self, owner: &str, name: &str) -> Result<(usize, usize)>;
}

impl RepackExt for Store {
    // ponytail: cached `blob:`/`tree:` answers for objects this prunes keep serving for the rest of
    // their 7-day TTL — already-unreachable data, not a new exposure, so it is left alone. Call
    // `bump_generation` here if repack ever has to be visible to the browse API promptly.
    async fn repack(&self, owner: &str, name: &str) -> Result<(usize, usize)> {
        run(self, owner, name, true).await
    }
    async fn consolidate(&self, owner: &str, name: &str) -> Result<(usize, usize)> {
        run(self, owner, name, false).await
    }
}

async fn run(store: &Store, owner: &str, name: &str, gc: bool) -> Result<(usize, usize)> {
    let repo = store
        .open_repo(owner, name)
        .await?
        .ok_or_else(|| err("repository not found"))?;
    // In-process, because the owning node is the only process that may touch this repo's packs
    // (CLAUDE.md's ownership invariant) — a DB key here once outlived a crashed run and blocked
    // every repack after it.
    let lock = store.keyed_lock(&format!("repack/{owner}/{name}"));
    let _held = lock
        .try_lock()
        .map_err(|_| err("a repack is already running for this repository"))?;
    let before = pack_count(store, &repo).await?;
    if let Some(old) = rebuild(store, &repo, gc).await? {
        retire(store, &repo, &old).await?;
    }
    Ok((before, pack_count(store, &repo).await?))
}

async fn pack_count(store: &Store, repo: &Repo) -> Result<usize> {
    Ok(store
        .pack_index(&repo.owner, &repo.name)
        .await?
        .iter()
        .filter(|(f, _)| f.ends_with(".pack"))
        .count())
}

/// Build, upload and record the replacement pack. Returns the files it supersedes — every pack
/// file indexed when this started — or `None` when there was nothing to rewrite. `pub` so a
/// test can stop here and prove a crash before `retire` leaves the repo whole.
pub async fn rebuild(store: &Store, repo: &Repo, gc: bool) -> Result<Option<Vec<(String, u64)>>> {
    // The listing comes first. Any pack recorded after it survives untouched, so the only packs
    // this run may delete are ones whose every object it is about to copy.
    let old = store.pack_index(&repo.owner, &repo.name).await?;
    let before = old.iter().filter(|(f, _)| f.ends_with(".pack")).count();
    let tips: Vec<ObjectId> = if gc {
        store.list_refs(repo).await?.into_iter().map(|(_, oid)| oid).collect()
    } else {
        Vec::new()
    };
    if gc && tips.is_empty() {
        // No refs at all ⇒ every object is unreachable by definition: drop the packs outright.
        // A repo that never had a successful push must not accumulate garbage forever.
        retire(store, repo, &old).await?;
        return Ok(None);
    }
    if before <= 1 {
        // nothing to consolidate: a single pack cannot hide garbage that a rebuild would drop,
        // because it was itself built from the live set
        return Ok(None);
    }
    let ids = if gc {
        Ids::Tips(tips)
    } else {
        let mut set = std::collections::HashSet::new();
        for (f, _) in old.iter().filter(|(f, _)| f.ends_with(".idx")) {
            let idx = gix_pack::index::File::at(repo.pack_dir.join(f), gix_hash::Kind::Sha1)?;
            set.extend(idx.iter().map(|e| e.oid));
        }
        Ids::AsIs(set.into_iter().collect())
    };
    let (pack, idx) = tokio::task::block_in_place(|| build_pack(repo, ids))?;
    // A rebuild that lands on a name already indexed (identical content ⇒ identical pack
    // checksum, e.g. a retry after a crash in `retire`) must not then retire itself.
    let keep: std::collections::HashSet<String> = [&pack, &idx]
        .iter()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
        .collect();
    if let Err(e) = store.upload_pack_files(repo, &pack, &idx).await {
        store.delete_pack_files(repo, &pack, &idx).await?;
        return Err(e);
    }
    Ok(Some(old.into_iter().filter(|(f, _)| !keep.contains(f)).collect()))
}

/// Drop superseded pack files: `.idx` before `.pack` (readers discover a pack via its index, so
/// nobody lists an index whose data is gone), and per file the index row before the object.
pub async fn retire(store: &Store, repo: &Repo, old: &[(String, u64)]) -> Result<()> {
    let mut ordered: Vec<&str> = old.iter().map(|(f, _)| f.as_str()).collect();
    ordered.sort_by_key(|f| !f.ends_with(".idx"));
    for fname in ordered {
        store.forget_pack_public(&repo.owner, &repo.name, fname).await?;
        store
            .os
            .delete(&OsPath::from(format!("{}/{}", repo.s3_prefix(), fname)))
            .await?;
        let _ = std::fs::remove_file(repo.pack_dir.join(fname));
    }
    Ok(())
}

enum Ids {
    /// everything reachable from these commits
    Tips(Vec<ObjectId>),
    /// exactly these objects
    AsIs(Vec<ObjectId>),
}

/// Build a single self-contained pack and index it into `repo.pack_dir`, returning
/// (pack_path, idx_path). Synchronous (gix is sync); call under block_in_place.
fn build_pack(repo: &Repo, ids: Ids) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    use std::io::{Seek, Write};
    let odb = repo.odb()?;
    let mut tmp = tempfile_in(&repo.pack_dir)?;
    let interrupt = AtomicBool::new(false);
    match ids {
        Ids::Tips(tips) => {
            crate::protocol::upload::write_pack(&odb, tips, Vec::new(), &mut tmp, &interrupt)?
        }
        Ids::AsIs(ids) => crate::protocol::upload::pack_from_ids(
            &odb,
            ids,
            gix_pack::data::output::count::objects::ObjectExpansion::AsIs,
            &mut tmp,
            &interrupt,
        )?,
    }
    tmp.flush()?;
    tmp.seek(std::io::SeekFrom::Start(0))?;

    let mut progress = gix_features::progress::Discard;
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut std::io::BufReader::new(tmp),
        Some(&repo.pack_dir),
        &mut progress,
        &interrupt,
        None::<gix_odb::Handle>,
        gix_pack::bundle::write::Options {
            thread_limit: None,
            iteration_mode: gix_pack::data::input::Mode::Verify,
            index_version: gix_pack::index::Version::V2,
            object_hash: gix_hash::Kind::Sha1,
            alloc_limit_bytes: None,
            compression: Default::default(),
        },
    )?;
    if let Some(k) = outcome.keep_path {
        let _ = std::fs::remove_file(k);
    }
    match (outcome.data_path, outcome.index_path) {
        (Some(p), Some(i)) => Ok((p, i)),
        _ => Err(err("repack produced an empty pack")),
    }
}

/// A temp file in `dir` (same filesystem, so the bundle's rename is atomic). Unique per pid+seq.
fn tempfile_in(dir: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(".repack.{}.{seq}.tmp", std::process::id()));
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)
}
