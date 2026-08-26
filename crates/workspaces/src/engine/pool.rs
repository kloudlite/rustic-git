//! Local btrfs pool: paths, lineage file, and the cross-process lock guarding it.

use crate::model::LineageEntry;
use std::path::PathBuf;

pub struct Pool {
    pub root: PathBuf,
}

impl Pool {
    pub fn new(root: impl Into<PathBuf>) -> Pool {
        Pool { root: root.into() }
    }
    pub fn recv(&self) -> PathBuf {
        self.root.join("recv")
    }
    /// `{pool}/img` — block-layer images: squash's throwaway build image (deleted as soon as its
    /// bytes are uploaded) and a block-restore's live loop-mount backing file.
    pub fn img_dir(&self) -> PathBuf {
        self.root.join("img")
    }
    pub fn img(&self, blob: &str) -> PathBuf {
        self.img_dir().join(format!("{blob}.img"))
    }
    /// Local staging area for `push`'s internal snapshot phase: the compressed layer bytes
    /// (`{blob}.zst`) and a sidecar (`{blob}.json`, `StageMeta`) sit here between staging and
    /// upload, entirely off the network. `push` deletes both once the bytes are durable in the
    /// object store (or,
    /// for a blob already uploaded directly — a squash block layer, an inherited clone
    /// entry — deletes just the sidecar, since `stage_path` never existed for those).
    pub fn stage_dir(&self) -> PathBuf {
        self.root.join("stage")
    }
    pub fn stage_path(&self, blob: &str) -> PathBuf {
        self.stage_dir().join(format!("{blob}.zst"))
    }
    pub fn stage_meta_path(&self, blob: &str) -> PathBuf {
        self.stage_dir().join(format!("{blob}.json"))
    }
    /// `{pool}/vol/{name}` — "vol" not "ws": environments live here too, and it matches the
    /// registry namespace's `vol/{owner}/{id}` naming.
    pub fn voldir(&self, name: &str) -> PathBuf {
        self.root.join("vol").join(name)
    }
    pub fn live(&self, name: &str) -> PathBuf {
        self.voldir(name).join("live")
    }
    /// Where this workspace's snapshots live: inside the image mount for a block-restored
    /// workspace (its own fs — snapshots cannot cross filesystems), else the shared recv/.
    pub fn snap_root(&self, name: &str) -> PathBuf {
        if is_mountpoint(&self.voldir(name)) { self.voldir(name) } else { self.recv() }
    }
    pub fn lineage(&self, name: &str) -> Vec<LineageEntry> {
        std::fs::read_to_string(self.root.join("vol").join(format!("{name}.lineage")))
            .map(|s| s.lines().map(LineageEntry::parse).collect())
            .unwrap_or_default()
    }
    /// tmp+rename, never truncate-in-place: this file's `unpushed` marks are the ONLY record that
    /// staged data exists, so a half-written `.lineage` reads back as "no entries" and the janitor
    /// then sweeps the only copy of that data. Returns `Result` rather than unwrapping because the
    /// caller is usually mid-push on a box that just hit ENOSPC — a panic there takes down every
    /// other in-flight job on the agent too.
    pub fn set_lineage(&self, name: &str, l: &[LineageEntry]) -> Result<(), String> {
        let s: Vec<String> = l.iter().map(LineageEntry::encode).collect();
        let dst = self.root.join("vol").join(format!("{name}.lineage"));
        let tmp = self.root.join("vol").join(format!("{name}.lineage.tmp"));
        std::fs::write(&tmp, s.join("\n")).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))
    }
}

#[cfg(test)]
mod lineage_tests {
    use super::Pool;
    use crate::model::{LayerKind, LineageEntry};

    fn e(blob: &str, unpushed: bool) -> LineageEntry {
        LineageEntry { kind: LayerKind::Stream, blob: blob.into(), snap: None, sha256: "sha".into(), unpushed }
    }

    #[test]
    fn set_lineage_is_atomic_and_leaves_no_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = Pool::new(tmp.path());
        std::fs::create_dir_all(pool.root.join("vol")).unwrap();

        pool.set_lineage("v1", &[e("b1", false), e("b2", true)]).unwrap();
        assert_eq!(pool.lineage("v1").len(), 2);

        // Crash simulation: a stale tmp file from a previous write must neither be read back as
        // the lineage nor stop the next write from landing.
        let stale = pool.root.join("vol").join("v1.lineage.tmp");
        std::fs::write(&stale, b"s:garbage").unwrap();
        pool.set_lineage("v1", &[e("b1", false), e("b2", true), e("b3", true)]).unwrap();

        let back = pool.lineage("v1");
        assert_eq!(back.len(), 3, "a stale tmp file must not corrupt the real lineage");
        assert_eq!(back.iter().filter(|x| x.unpushed).count(), 2, "unpushed marks survive the write");
        assert!(!stale.exists(), "the tmp file is renamed away, never left behind");
    }

    #[test]
    fn set_lineage_returns_err_instead_of_panicking_on_an_unwritable_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = Pool::new(tmp.path());
        // `vol/` deliberately absent: the ENOSPC shape of the same failure, which used to panic
        // the whole agent mid-push.
        assert!(!pool.set_lineage("v1", &[]).unwrap_err().is_empty());
    }
}

/// One-time upgrade for a pool still laid out under the old `ws` name: btrfs subvolumes don't
/// care what their containing directory is called, so a plain rename is enough — no per-entry
/// work needed. No-op when `{pool}/vol` already exists (already migrated, or a fresh pool) or
/// `{pool}/ws` doesn't (fresh pool, nothing to move).
pub fn migrate_ws_to_vol(root: &std::path::Path) {
    let old = root.join("ws");
    let new = root.join("vol");
    if new.exists() || !old.exists() {
        return;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => eprintln!("pool: migrated {} -> {}", old.display(), new.display()),
        Err(e) => eprintln!("pool: migrating {} -> {}: {e}", old.display(), new.display()),
    }
}

#[cfg(test)]
mod migrate_tests {
    use super::migrate_ws_to_vol;

    #[test]
    fn renames_ws_to_vol_with_plain_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("some-id")).unwrap();
        std::fs::write(ws.join("some-id.lineage"), "s:b1:abc").unwrap();

        migrate_ws_to_vol(tmp.path());

        assert!(!ws.exists());
        let vol = tmp.path().join("vol");
        assert!(vol.join("some-id").is_dir());
        assert_eq!(std::fs::read_to_string(vol.join("some-id.lineage")).unwrap(), "s:b1:abc");
    }

    #[test]
    fn no_op_when_vol_already_exists_or_ws_absent() {
        let tmp = tempfile::tempdir().unwrap();
        // Neither exists: no-op, no panic.
        migrate_ws_to_vol(tmp.path());
        assert!(!tmp.path().join("vol").exists());

        // Both exist: vol wins, ws is left untouched (never silently merged/clobbered).
        std::fs::create_dir_all(tmp.path().join("ws")).unwrap();
        std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
        migrate_ws_to_vol(tmp.path());
        assert!(tmp.path().join("ws").exists());
        assert!(tmp.path().join("vol").exists());
    }
}

pub fn is_mountpoint(p: &std::path::Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    mounts.lines().any(|l| l.split_whitespace().nth(1) == p.to_str())
}

/// Serialize every lineage read-modify-write for one workspace across processes (push vs the
/// background squash) — the double-squash came from exactly this race.
pub fn ws_lock(pool: &Pool, ws: &str) -> Result<std::fs::File, String> {
    let path = pool.root.join("vol").join(format!("{ws}.lock"));
    let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("flock failed".into());
    }
    Ok(f)
}
