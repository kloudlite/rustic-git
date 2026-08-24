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
    /// for a blob already uploaded directly — a squash block layer, an inherited fork/clone
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
    pub fn wsdir(&self, name: &str) -> PathBuf {
        self.root.join("ws").join(name)
    }
    pub fn live(&self, name: &str) -> PathBuf {
        self.wsdir(name).join("live")
    }
    /// Where this workspace's snapshots live: inside the image mount for a block-restored
    /// workspace (its own fs — snapshots cannot cross filesystems), else the shared recv/.
    pub fn snap_root(&self, name: &str) -> PathBuf {
        if is_mountpoint(&self.wsdir(name)) { self.wsdir(name) } else { self.recv() }
    }
    pub fn lineage(&self, name: &str) -> Vec<LineageEntry> {
        std::fs::read_to_string(self.root.join("ws").join(format!("{name}.lineage")))
            .map(|s| s.lines().map(LineageEntry::parse).collect())
            .unwrap_or_default()
    }
    pub fn set_lineage(&self, name: &str, l: &[LineageEntry]) {
        let s: Vec<String> = l.iter().map(LineageEntry::encode).collect();
        std::fs::write(self.root.join("ws").join(format!("{name}.lineage")), s.join("\n")).unwrap();
    }
}

pub fn is_mountpoint(p: &std::path::Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    mounts.lines().any(|l| l.split_whitespace().nth(1) == p.to_str())
}

/// Serialize every lineage read-modify-write for one workspace across processes (push vs the
/// background squash) — the double-squash came from exactly this race.
pub fn ws_lock(pool: &Pool, ws: &str) -> Result<std::fs::File, String> {
    let path = pool.root.join("ws").join(format!("{ws}.lock"));
    let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("flock failed".into());
    }
    Ok(f)
}
