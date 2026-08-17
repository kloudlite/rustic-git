//! Network-wide repack (garbage collection).
//!
//! A repo's packs accumulate one per push and are never rewritten, so clones slow
//! down over time and unreachable objects (from force-pushes, deleted branches, deleted repos)
//! linger. `repack` consolidates everything reachable from every repo's refs into one new pack
//! and drops the old ones. It runs from `admin`, i.e. with the server stopped, so there are no
//! concurrent writers to race.

use crate::store::Store;
use crate::{err, Result};
use gix_hash::ObjectId;
use slatedb::object_store::{path::Path as OsPath, ObjectStore, ObjectStoreExt};
use std::sync::atomic::AtomicBool;

impl Store {
    /// Consolidate a repo's packs into one and drop the rest, collecting anything no longer
    /// reachable from its refs. Purely local to the repo: its objects belong to it alone, so no
    /// other repo's refs can keep them alive and no other node needs consulting.
    /// Returns (packs_before, packs_after).
    pub async fn repack(&self, owner: &str, name: &str) -> Result<(usize, usize)> {
        let repo = self
            .open_repo(owner, name)
            .await?
            .ok_or_else(|| err("repository not found"))?;

        // Exclusive lease: repack deletes packs, so two of them must never overlap.
        let lock = format!("lock/repack/{owner}/{name}");
        let db = self.db_for(owner, name).await?;
        {
            let txn = db
                .begin(slatedb::IsolationLevel::SerializableSnapshot)
                .await?;
            if txn.get(&lock).await?.is_some() {
                return Err(err(
                    "a repack is already running for this repository (stale lock: delete the lock/repack key)",
                ));
            }
            txn.put(&lock, b"1")?;
            txn.commit().await?;
        }
        let result = self.repack_locked(&repo).await;
        db.delete(&lock).await?;
        result
    }

    async fn repack_locked(&self, repo: &crate::store::Repo) -> Result<(usize, usize)> {
        let tips: Vec<ObjectId> = self
            .list_refs(repo)
            .await?
            .into_iter()
            .map(|(_, oid)| oid)
            .collect();

        // packs present now — everything this run may replace
        let s3_prefix = OsPath::from(repo.s3_prefix());
        let old: Vec<OsPath> = {
            use futures::TryStreamExt;
            self.os
                .list(Some(&s3_prefix))
                .map_ok(|m| m.location)
                .try_collect()
                .await?
        };
        let before = old.iter().filter(|p| p.extension() == Some("pack")).count();
        if tips.is_empty() || before <= 1 {
            // nothing referenced, or nothing to consolidate: a single pack cannot hide garbage
            // that a rebuild would drop, because it was itself built from the live set
            return Ok((before, before));
        }

        let (pack, idx) = tokio::task::block_in_place(|| build_pack(repo, tips))?;
        let keep: std::collections::HashSet<String> = [&pack, &idx]
            .iter()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
            .collect();

        if let Err(e) = self.upload_pack_files(repo, &pack, &idx).await {
            for p in [&pack, &idx] {
                if let Some(f) = p.file_name().and_then(|s| s.to_str()) {
                    let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), f));
                    let _ = self.os.delete(&key).await;
                }
                let _ = std::fs::remove_file(p);
            }
            return Err(e);
        }

        // .idx before .pack: readers discover a pack via its index, so removing the index first
        // means no one ever lists an index whose data is gone.
        let mut ordered: Vec<&OsPath> = old.iter().collect();
        ordered.sort_by_key(|p| p.extension() != Some("idx"));
        for loc in ordered {
            let fname = loc.filename().unwrap_or_default().to_string();
            if keep.contains(&fname) {
                continue;
            }
            self.os.delete(loc).await?;
            self.forget_pack_public(&repo.owner, &repo.name, &fname)
                .await?;
            let _ = std::fs::remove_file(repo.pack_dir.join(&fname));
        }
        let after = {
            use futures::TryStreamExt;
            let remaining: Vec<OsPath> = self
                .os
                .list(Some(&s3_prefix))
                .map_ok(|m| m.location)
                .try_collect()
                .await?;
            remaining
                .iter()
                .filter(|p| p.extension() == Some("pack"))
                .count()
        };
        Ok((before, after))
    }
}

/// Build a single self-contained pack from `tips` and index it into `repo.pack_dir`,
/// returning (pack_path, idx_path). Synchronous (gix is sync); call under block_in_place.
fn build_pack(
    repo: &crate::store::Repo,
    tips: Vec<ObjectId>,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    use std::io::{Seek, Write};
    let odb = repo.odb()?;
    let mut tmp = tempfile_in(&repo.pack_dir)?;
    crate::protocol::upload::write_pack(&odb, tips, Vec::new(), &mut tmp, &AtomicBool::new(false))?;
    tmp.flush()?;
    tmp.seek(std::io::SeekFrom::Start(0))?;

    let should_interrupt = AtomicBool::new(false);
    let mut progress = gix_features::progress::Discard;
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut std::io::BufReader::new(tmp),
        Some(&repo.pack_dir),
        &mut progress,
        &should_interrupt,
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
