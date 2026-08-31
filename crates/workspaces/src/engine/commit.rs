//! Commit/checkout primitives for the commit model: a volume's history lives as RO subvolumes
//! under `snap/`, and a workspace's live tree is an RW subvolume checked out from one of them
//! under `live/{ws}` (see `engine/pool.rs`'s `snap_dir`/`worktree`). The CR for a commit is
//! created by the caller BEFORE `commit_worktree` is called (CR first, subvolume second, per the
//! task brief) so a retry after a crash finds the CR and can redo the snapshot; this module only
//! ever touches btrfs.

use crate::engine::ops::{EngErr, run};
use crate::engine::{Engine, ws_lock};

/// A checkout that would land on an existing worktree path — the caller treats "already there"
/// as its own case (idempotent reconcile vs. genuine conflict), so this is a distinct, stable
/// marker rather than a generic IO error.
pub const WORKTREE_EXISTS: &str = "worktree already exists";

impl Engine {
    /// Cut commit `name` from worktree `ws` of `volume`: sync the pool first (btrfs only commits
    /// its transaction periodically, so an unsynced snapshot can miss writes made moments
    /// earlier — same reasoning as `sync_pool`'s doc comment), then RO-snapshot the worktree into
    /// `snap/{name}`. Under `ws_lock` because a concurrent checkout/drop on the same volume must
    /// not race the snapshot.
    pub fn commit_worktree(&self, volume: &str, ws: &str, name: &str) -> Result<(), EngErr> {
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        self.sync_pool()?;
        std::fs::create_dir_all(self.pool.snap_dir(volume)).map_err(EngErr::io)?;
        let src = self.pool.worktree(volume, ws);
        let dst = self.pool.snap(volume, name);
        run(&["btrfs", "subvolume", "snapshot", "-r", src.to_str().unwrap(), dst.to_str().unwrap()])
    }

    /// Create worktree `ws` from commit `name` (an RW snapshot of `snap/{name}`), or an empty
    /// subvolume when `name` is `None` (bootstrap — the volume's very first worktree, with no
    /// commit yet to check out from). Refuses an existing worktree path rather than silently
    /// reusing or overwriting it: `WORKTREE_EXISTS` lets the caller decide whether that's fine.
    pub fn checkout(&self, volume: &str, name: Option<&str>, ws: &str) -> Result<(), EngErr> {
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let dst = self.pool.worktree(volume, ws);
        if dst.exists() {
            return Err(EngErr::other(WORKTREE_EXISTS));
        }
        std::fs::create_dir_all(self.pool.voldir(volume).join("live")).map_err(EngErr::io)?;
        match name {
            Some(name) => {
                let src = self.pool.snap(volume, name);
                if !src.exists() {
                    return Err(EngErr::other(crate::engine::ops::NO_SUCH_RECORD));
                }
                run(&["btrfs", "subvolume", "snapshot", src.to_str().unwrap(), dst.to_str().unwrap()])
            }
            None => run(&["btrfs", "subvolume", "create", dst.to_str().unwrap()]),
        }
    }

    /// Commits present on this pool for `volume`: a plain dir listing of `snap_dir`, not a
    /// registry read — this answers "what can this node check out locally right now", which the
    /// registry (durable, shared, but not necessarily pulled here yet) can't.
    pub fn local_commits(&self, volume: &str) -> Result<Vec<String>, EngErr> {
        let dir = self.pool.snap_dir(volume);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .map_err(EngErr::io)?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Delete a commit subvolume (retention / reconcile). Any worktree checked out FROM it stays
    /// fully readable afterward — a checkout is a snapshot, CoW-independent of its source the
    /// instant `btrfs subvolume snapshot` returns, so dropping the source never touches the
    /// worktree's own blocks.
    pub fn drop_commit(&self, volume: &str, name: &str) -> Result<(), EngErr> {
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let path = self.pool.snap(volume, name);
        run(&["btrfs", "subvolume", "delete", path.to_str().unwrap()])
    }
}
