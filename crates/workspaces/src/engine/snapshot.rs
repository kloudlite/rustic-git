//! Snapshot/checkout primitives for the snapshot model: a volume's history lives as RO subvolumes
//! under `snap/`, and a workspace's live tree is an RW subvolume checked out from one of them
//! under `live/{ws}` (see `engine/pool.rs`'s `snap_dir`/`worktree`). The CR for a snapshot is
//! created by the caller BEFORE `snapshot_worktree` is called (CR first, subvolume second, per
//! `docs/superpowers/specs/2026-09-03-durable-snapshots-design.md`) so a retry after a crash finds
//! the CR and can redo the snapshot; this module only ever touches btrfs.

use crate::engine::ops::{path_str, EngErr, is_subvolume, run};
use crate::engine::{Engine, ws_lock};

/// A checkout that would land on an existing worktree path — the caller treats "already there"
/// as its own case (idempotent reconcile vs. genuine conflict), so this is a distinct, stable
/// marker rather than a generic IO error.
pub const WORKTREE_EXISTS: &str = "worktree already exists";

/// `swap_worktree`'s two intermediate names — dot-prefixed so a worktree scanner (`set_quota_worktrees`,
/// any `read_dir` of `live/`) skips them: they are worktree-SHAPED subvolumes but not a worktree, and
/// a crash between the two renames used to leave one behind indefinitely, counted and quota-limited
/// as if it were live.
pub fn restoring_name(ws: &str) -> String {
    format!(".restoring-{ws}")
}

pub fn before_restore_name(ws: &str) -> String {
    format!(".before-restore-{ws}")
}

impl Engine {
    /// Cut snapshot `name` from worktree `ws` of `volume`: sync the pool first (btrfs only commits
    /// its transaction periodically, so an unsynced snapshot can miss writes made moments
    /// earlier — same reasoning as `sync_pool`'s doc comment), then RO-snapshot the worktree into
    /// `snap/{name}`. Under `ws_lock` because a concurrent checkout/drop on the same volume must
    /// not race the snapshot.
    pub fn snapshot_worktree(&self, volume: &str, ws: &str, name: &str) -> Result<(), EngErr> {
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let dst = self.pool.snap(volume, name);
        // Converge, don't retry-and-fail: a crash between this snapshot landing and the CR's
        // status update leaves the CR Working forever, and the reconciler calls this again on
        // every pass. A visible btrfs snapshot is transaction-atomic (sync_pool below only
        // matters for the write path, not for observing one that already landed), so "the
        // path exists" IS "this snapshot is done" — never re-snapshot over it.
        if dst.exists() {
            return Ok(());
        }
        self.sync_pool()?;
        std::fs::create_dir_all(self.pool.snap_dir(volume)).map_err(EngErr::io)?;
        let src = self.pool.worktree(volume, ws);
        run(&["btrfs", "subvolume", "snapshot", "-r", path_str(&src)?, path_str(&dst)?])
    }

    /// Create worktree `ws` from snapshot `name` (an RW snapshot of `snap/{name}`), or an empty
    /// subvolume when `name` is `None` (bootstrap — the volume's very first worktree, with no
    /// snapshot yet to check out from). Refuses an existing worktree path rather than silently
    /// reusing or overwriting it: `WORKTREE_EXISTS` lets the caller decide whether that's fine.
    pub fn checkout(&self, volume: &str, name: Option<&str>, ws: &str) -> Result<(), EngErr> {
        // vol/ may not exist yet on a pool's first-ever checkout — ws_lock writes its lock file
        // under vol/, so it must exist before we lock, not just before we snapshot.
        std::fs::create_dir_all(self.pool.voldir(volume)).map_err(EngErr::io)?;
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let dst = self.pool.worktree(volume, ws);
        // Checked and validated BEFORE any directory is created, so a refused checkout — existing
        // worktree, or (below) a missing snapshot — truly creates nothing.
        match std::fs::symlink_metadata(&dst) {
            Ok(meta) if meta.file_type().is_dir() => return Err(EngErr::other(WORKTREE_EXISTS)),
            Ok(_) => return Err(EngErr::other("worktree path is not a subvolume")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(EngErr::io(e)),
        }
        if let Some(name) = name {
            let src = self.pool.snap(volume, name);
            if !src.exists() {
                return Err(EngErr::other(crate::engine::ops::NO_SUCH_RECORD));
            }
        }
        std::fs::create_dir_all(dst.parent().unwrap()).map_err(EngErr::io)?;
        match name {
            Some(name) => {
                let src = self.pool.snap(volume, name);
                run(&["btrfs", "subvolume", "snapshot", src.to_str().unwrap(), dst.to_str().unwrap()])?;
                // A snapshot inherits the source's ownership, so a restored worktree is already
                // the person's. Nothing to do.
                Ok(())
            }
            None => Self::create_tenant_worktree(&dst),
        }
    }

    fn create_tenant_worktree(dst: &std::path::Path) -> Result<(), EngErr> {
        let name = dst.file_name().ok_or_else(|| EngErr::other("worktree has no name"))?;
        let staging = dst.with_file_name(format!(".creating-{}", name.to_string_lossy()));
        match std::fs::symlink_metadata(&staging) {
            Ok(meta) if meta.file_type().is_dir() && is_subvolume(&staging) => {}
            Ok(_) => return Err(EngErr::other("worktree staging path is not a subvolume")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                run(&["btrfs", "subvolume", "create", path_str(&staging)?])?;
            }
            Err(e) => return Err(EngErr::io(e)),
        }
        if !is_subvolume(&staging) {
            return Err(EngErr::other("worktree staging path is not a subvolume"));
        }
        Self::chown_tenant(&staging)?;
        std::fs::rename(staging, dst).map_err(EngErr::io)
    }

    /// Hand a freshly created subvolume to the pod's uid.
    ///
    /// Only the subvolume ROOT: what is inside a fresh one is nothing, and a `-R` walk here would
    /// be the prelude's own cost moved rather than removed. A non-root process cannot give a file
    /// away, so a dev or test run that is not root leaves it alone — the same rule the keys
    /// writer already follows.
    fn chown_tenant(dst: &std::path::Path) -> Result<(), EngErr> {
        if unsafe { libc::geteuid() } != 0 {
            return Ok(());
        }
        use std::os::unix::ffi::OsStrExt;

        let path = std::ffi::CString::new(dst.as_os_str().as_bytes()).map_err(|_| EngErr::other("worktree path contains NUL"))?;
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
        if fd < 0 {
            return Err(EngErr::io(std::io::Error::last_os_error()));
        }
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        let valid = unsafe { libc::fstat(fd, &mut stat) == 0 && stat.st_ino == 256 };
        let result = if valid {
            let uid = crate::k8s::SSH_UID as libc::uid_t;
            unsafe { libc::fchown(fd, uid, uid) }
        } else {
            -1
        };
        let error = if valid && result == 0 {
            None
        } else if !valid {
            Some(EngErr::other("worktree staging path is not a subvolume"))
        } else {
            Some(EngErr::io(std::io::Error::last_os_error()))
        };
        unsafe { libc::close(fd) };
        error.map_or(Ok(()), Err)
    }

    /// Restore-in-place, reinterpreted as a checkout: replace worktree `ws` of `volume` with a
    /// fresh checkout of `name`. Checked out into a THROWAWAY sibling first — reusing
    /// `checkout`'s own missing-snapshot validation (`NO_SUCH_RECORD`) — so a bad snapshot id fails
    /// before the live worktree is touched at all; only then two plain renames (a btrfs subvolume
    /// renames like any other directory) swap it in, and the displaced old worktree is deleted.
    /// Any leftover staging subvolume from an earlier, crashed attempt is discarded first, same
    /// as `checkout`'s own idempotent-retry rule.
    pub fn swap_worktree(&self, volume: &str, ws: &str, name: &str) -> Result<(), EngErr> {
        let staging = restoring_name(ws);
        let staging_path = self.pool.worktree(volume, &staging);
        match std::fs::symlink_metadata(&staging_path) {
            Ok(meta) if meta.file_type().is_dir() && is_subvolume(&staging_path) => {
                run(&["btrfs", "subvolume", "delete", staging_path.to_str().unwrap()])?;
            }
            Ok(_) => return Err(EngErr::other("restore staging path is not a subvolume")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(EngErr::io(e)),
        }
        self.checkout(volume, Some(name), &staging)?;

        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let live = self.pool.worktree(volume, ws);
        let backup = self.pool.worktree(volume, &before_restore_name(ws));
        if backup.exists() {
            run(&["btrfs", "subvolume", "delete", backup.to_str().unwrap()])?;
        }
        if live.exists() {
            std::fs::rename(&live, &backup).map_err(EngErr::io)?;
        }
        std::fs::rename(&staging_path, &live).map_err(EngErr::io)?;
        if backup.exists() {
            run(&["btrfs", "subvolume", "delete", backup.to_str().unwrap()])?;
        }
        Ok(())
    }

    /// Snapshots present on this pool for `volume`: a plain dir listing of `snap_dir`, not a
    /// `Snapshot` CR list — this answers "what can this node check out locally right now", which
    /// the CRs (durable, cluster-wide, but not necessarily pulled to this node yet) can't.
    pub fn local_snapshots(&self, volume: &str) -> Result<Vec<String>, EngErr> {
        let dir = self.pool.snap_dir(volume);
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            // Only "no snap dir yet" (no snapshot ever cut) reads as empty; any other error
            // (permissions, ENOSPC on the readdir buffer, ...) must not silently look like "no
            // snapshots" — retention would then delete a snapshot it simply failed to see.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(EngErr::io(e)),
        };
        // Propagate any per-entry error too, not just the read_dir() call itself — a name that
        // isn't valid UTF-8 must fail loudly rather than silently vanish from the list.
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(EngErr::io)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|n| EngErr::other(format!("{}: non-UTF-8 snapshot name", n.to_string_lossy())))?;
            names.push(name);
        }
        names.sort();
        Ok(names)
    }

    /// Delete a snapshot subvolume (retention / reconcile). Any worktree checked out FROM it stays
    /// fully readable afterward — a checkout is a snapshot, CoW-independent of its source the
    /// instant `btrfs subvolume snapshot` returns, so dropping the source never touches the
    /// worktree's own blocks.
    pub fn drop_snapshot(&self, volume: &str, name: &str) -> Result<(), EngErr> {
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let path = self.pool.snap(volume, name);
        // Reconcile convergence, same shape as snapshot_worktree: a retry after this already
        // succeeded (or after a snapshot that never existed) must not error.
        if !path.exists() {
            return Ok(());
        }
        // A plain directory under `snap/` is not a snapshot and `btrfs subvolume delete` only ever
        // errors on it — skipping it once beats warning about it on every beat forever.
        if !is_subvolume(&path) {
            return Ok(());
        }
        run(&["btrfs", "subvolume", "delete", path.to_str().unwrap()])
    }

    /// Delete worktree `ws` of `volume` (a deleted shared-volume clone's
    /// worktree, `{pool}/vol/{volume}/live/{ws}`, has no child `Volume` and no ownerReference to
    /// garbage-collect it — this is the only thing that removes it). Ok-on-absent, same
    /// reconcile-convergence shape as `drop_snapshot`: a retry after this already ran (or a
    /// worktree that was never checked out) must not error.
    pub fn drop_worktree(&self, volume: &str, ws: &str) -> Result<(), EngErr> {
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let path = self.pool.worktree(volume, ws);
        if !path.exists() {
            return Ok(());
        }
        // Subagent trees are NESTED subvolumes inside this one, and btrfs refuses to delete a
        // subvolume that still has children. Here rather than in the delete path's caller because
        // every caller of this — the finalizer, a takeover, a rescue — hits the same wall, and a
        // guard in one of them leaves the others wedged on a workspace somebody once ran an agent
        // in.
        for name in self.tree_names(volume, ws)? {
            self.drop_subvolume(&self.tree_dir(volume, ws).join(name))?;
        }
        run(&["btrfs", "subvolume", "delete", path.to_str().unwrap()])
    }

    /// `{pool}/vol/{volume}/live/{ws}/.agents` — where this worktree's subagent trees live.
    fn tree_dir(&self, volume: &str, ws: &str) -> std::path::PathBuf {
        self.pool.worktree(volume, ws).join(crate::crd::TREES_DIR)
    }

    /// The tree names actually on disk: the SUBVOLUMES under `.agents/`, never every entry. A
    /// plain directory there is somebody's mistake to look at, not this code's to delete — and a
    /// replica's `.agents/{name}` is exactly such a directory, since `btrfs send` left the nested
    /// subvolume behind and only its mount point travelled.
    ///
    /// A missing `.agents` is an empty list, not an error: most workspaces never hold a tree.
    pub fn tree_names(&self, volume: &str, ws: &str) -> Result<Vec<String>, EngErr> {
        let dir = self.tree_dir(volume, ws);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(EngErr::io(e)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(EngErr::io)?;
            if is_subvolume(&entry.path()) {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// The reconcile step's read of the same listing, under the volume lock so it cannot observe
    /// a cut half-made by this node's own other pass.
    pub fn list_trees(&self, volume: &str, ws: &str) -> Result<Vec<String>, EngErr> {
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        self.tree_names(volume, ws)
    }

    /// Cut one tree: a WRITABLE snapshot of the worktree, nested inside it.
    ///
    /// Nested is the design, not an accident of layout — `btrfs send` skips a nested subvolume, so
    /// this is what keeps a tree out of every push, sync point and replica without a single
    /// exclusion rule anywhere else.
    ///
    /// The `.agents` directory is created first, owned by the pod's uid so the tool server (which
    /// runs as that uid and cannot make subvolumes) can read and write inside the trees it is
    /// handed. The snapshot itself keeps the source's ownership, so nothing is chowned after it.
    pub fn cut_tree(&self, volume: &str, ws: &str, name: &str) -> Result<(), EngErr> {
        if !crate::crd::tree_name_ok(name) {
            return Err(EngErr::other(format!("{name}: not a tree name")));
        }
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let dir = self.tree_dir(volume, ws);
        std::fs::create_dir_all(&dir).map_err(EngErr::io)?;
        if unsafe { libc::geteuid() } == 0 {
            std::os::unix::fs::chown(&dir, Some(crate::k8s::SSH_UID as u32), Some(crate::k8s::SSH_UID as u32))
                .map_err(EngErr::io)?;
        }
        let dst = dir.join(name);
        // Level-triggered like every other create here: an existing tree is the state being asked
        // for, and re-snapshotting over it would replace a subagent's work with the main tree's.
        if dst.exists() {
            return Ok(());
        }
        run(&["btrfs", "subvolume", "snapshot", path_str(&self.pool.worktree(volume, ws))?, path_str(&dst)?])
    }

    /// Delete one tree. Ok-on-absent, same convergence shape as `drop_worktree`.
    pub fn drop_tree(&self, volume: &str, ws: &str, name: &str) -> Result<(), EngErr> {
        if !crate::crd::tree_name_ok(name) {
            return Err(EngErr::other(format!("{name}: not a tree name")));
        }
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        self.drop_subvolume(&self.tree_dir(volume, ws).join(name))
    }

    /// `btrfs subvolume delete` on a path that may not be one. Shared by the tree deletes and by
    /// `drop_worktree`'s nested pass, which must not take the volume lock a second time.
    fn drop_subvolume(&self, path: &std::path::Path) -> Result<(), EngErr> {
        if !is_subvolume(path) {
            return Ok(());
        }
        run(&["btrfs", "subvolume", "delete", path_str(path)?])
    }

    /// Move a pre-snapshot-model volume from the old layout — `{pool}/vol/{volume}/live` IS the
    /// single RW subvolume — to the snapshot model's `{pool}/vol/{volume}/live/{volume}` (`live/`
    /// a directory of worktrees). Old-layout workspaces have no separate worktree id, so the
    /// worktree this leaves behind is named after the volume itself — the pre-model workspace and
    /// its volume already shared one id (see `Pool::live`/`Pool::worktree`).
    ///
    /// Idempotent and crash-safe: returns `Ok(true)` only when THIS call performed the rename
    /// (the caller uses that to decide whether a migration snapshot still needs cutting), `Ok(false)`
    /// when the volume is already on the new layout (or never had a `live` at all — nothing here
    /// migrates). A crash between the two renames leaves `live-migrating` behind with no `live`
    /// directory yet; the next call finds that and finishes the second rename rather than
    /// re-touching the first (`std::fs::rename` of a subvolume into a fresh directory on the same
    /// filesystem is a plain metadata operation, not a copy — legal and atomic).
    pub fn migrate_volume(&self, volume: &str) -> Result<bool, EngErr> {
        // Same rule as `checkout`'s own opening line: `vol/` (and this volume's own voldir) may
        // not exist yet — a volume with no `live` at all has nothing to migrate, but `ws_lock`'s
        // lock file still needs somewhere to be created before it can even answer that.
        std::fs::create_dir_all(self.pool.voldir(volume)).map_err(EngErr::io)?;
        let _lock = ws_lock(&self.pool, volume).map_err(EngErr::other)?;
        let live = self.pool.live(volume);
        let staging = self.pool.voldir(volume).join("live-migrating");
        let dst = self.pool.worktree(volume, volume);

        if dst.exists() {
            // Either a prior call already finished this migration, or the volume was created
            // snapshot-model-native to begin with (checkout() makes exactly this path). Either way
            // there is nothing left to move.
            return Ok(false);
        }
        if staging.exists() {
            // Recovered mid-migration: the first rename landed, the second didn't.
            std::fs::create_dir_all(&live).map_err(EngErr::io)?;
            std::fs::rename(&staging, &dst).map_err(EngErr::io)?;
            return Ok(true);
        }
        if !is_subvolume(&live) {
            // No old-layout subvolume to move — a volume that never had a `live` at all (nothing
            // to migrate), or a `live` directory with no `{volume}`-named worktree under it (not
            // this function's problem to invent one).
            return Ok(false);
        }
        std::fs::rename(&live, &staging).map_err(EngErr::io)?;
        std::fs::create_dir_all(&live).map_err(EngErr::io)?;
        std::fs::rename(&staging, &dst).map_err(EngErr::io)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn checkout_refuses_a_symlink_destination_without_touching_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::engine::Pool::new(tmp.path());
        let dst = pool.worktree("v", "ws");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &dst).unwrap();

        let engine = Engine::new(pool);
        let err = engine.checkout("v", None, "ws").unwrap_err();
        assert!(err.0.contains("not a subvolume"));
        assert!(target.is_dir());
        assert!(std::fs::symlink_metadata(&dst).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn checkout_refuses_a_dangling_symlink_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::engine::Pool::new(tmp.path());
        let dst = pool.worktree("v", "ws");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), &dst).unwrap();

        let err = Engine::new(pool).checkout("v", None, "ws").unwrap_err();
        assert!(err.0.contains("not a subvolume"));
        assert!(std::fs::symlink_metadata(&dst).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn tenant_checkout_rejects_a_symlink_staging_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("live/ws");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let staging = dst.with_file_name(".creating-ws");
        std::os::unix::fs::symlink(&target, &staging).unwrap();

        let err = Engine::create_tenant_worktree(&dst).unwrap_err();
        assert!(err.0.contains("staging path is not a subvolume"));
        assert!(target.is_dir());
        assert!(!dst.exists());
    }
}
