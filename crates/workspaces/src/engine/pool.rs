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
    pub fn img(&self, blob: &str) -> PathBuf {
        self.root.join("img").join(format!("{blob}.img"))
    }
    /// Local staging area for `commit`'s output: the compressed layer bytes (`{blob}.zst`) and
    /// a sidecar (`{blob}.json`, `StageMeta`) sit here between commit and push, entirely off
    /// the network. `push` deletes both once the bytes are durable in the object store (or,
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
    pub fn set_lineage(&self, name: &str, l: &[LineageEntry]) {
        let s: Vec<String> = l.iter().map(LineageEntry::encode).collect();
        std::fs::write(self.root.join("vol").join(format!("{name}.lineage")), s.join("\n")).unwrap();
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
