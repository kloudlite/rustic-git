//! Local btrfs pool: paths, and the cross-process lock guarding per-volume state.

use std::path::PathBuf;

pub struct Pool {
    pub root: PathBuf,
}

impl Pool {
    pub fn new(root: impl Into<PathBuf>) -> Pool {
        Pool { root: root.into() }
    }
    /// Replica snapshots, sender side and receiver side both — the commit model's own transfer
    /// staging area (`bins/agent/src/peer.rs`), unrelated to the deleted object-store path.
    pub fn repl(&self, name: &str) -> PathBuf {
        self.root.join("repl").join(name)
    }
    /// `{pool}/vol/{name}` — "vol" not "ws": environments live here too, and it matches the
    /// registry namespace's `vol/{owner}/{id}` naming.
    pub fn voldir(&self, name: &str) -> PathBuf {
        self.root.join("vol").join(name)
    }
    pub fn live(&self, name: &str) -> PathBuf {
        self.voldir(name).join("live")
    }
    /// `{pool}/vol/{volume}/snap` — commit subvolumes (RO), one per `Snapshot` CR, named by
    /// commit id. Coexists with `live()`'s single-subvolume layout on a not-yet-migrated volume
    /// (`commit::migrate_volume`), so this never touches `live()` or its callers.
    pub fn snap_dir(&self, volume: &str) -> PathBuf {
        self.voldir(volume).join("snap")
    }
    pub fn snap(&self, volume: &str, name: &str) -> PathBuf {
        self.snap_dir(volume).join(name)
    }
    /// `{pool}/vol/{volume}/live/{ws}` — commit model's `live` is a DIRECTORY of worktrees, one
    /// RW subvolume per workspace checked out against this volume, not the single subvolume
    /// `live()` still names for the old layout.
    pub fn worktree(&self, volume: &str, ws: &str) -> PathBuf {
        self.voldir(volume).join("live").join(ws)
    }
    /// `{pool}/vol/{name}/.pushed-gen` — the btrfs generation recorded after the timer's last
    /// push of a home. Inside the voldir, next to `live`, and outside the subvolume: it must not
    /// be in the stream, and it must die with the volume (`cleanup_local` removes the voldir).
    pub fn pushed_gen_path(&self, name: &str) -> PathBuf {
        self.voldir(name).join(".pushed-gen")
    }
    /// `None` is "never pushed, or unreadable" — both push, because an extra push is cheap and a
    /// skipped one is a home whose last hour is on one disk.
    pub fn pushed_gen(&self, name: &str) -> Option<u64> {
        std::fs::read_to_string(self.pushed_gen_path(name)).ok()?.trim().parse().ok()
    }
    /// tmp+rename, never truncate-in-place: a torn number would parse as garbage and read as
    /// `None`, which is the safe direction, but a tmp file left behind is one to clean.
    pub fn record_pushed_gen(&self, name: &str, generation: u64) -> Result<(), String> {
        let dst = self.pushed_gen_path(name);
        let tmp = self.voldir(name).join(".pushed-gen.tmp");
        std::fs::write(&tmp, generation.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))
    }
}


pub fn is_mountpoint(p: &std::path::Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    mountpoint_in(&mounts, p)
}

/// `/proc/self/mounts` escapes space, tab, newline and backslash in octal — a pool path with a
/// space in it (a volume id never has one, but a pool root can) otherwise never matches and
/// `is_mountpoint` silently answers false for a real mountpoint.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Some(c) = std::str::from_utf8(&b[i + 1..i + 4]).ok().and_then(|o| u8::from_str_radix(o, 8).ok()) {
                out.push(c as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// Split out so the escape handling is testable without a real mount.
fn mountpoint_in(mounts: &str, p: &std::path::Path) -> bool {
    let Some(want) = p.to_str() else { return false };
    mounts.lines().any(|l| l.split_whitespace().nth(1).map(unescape_mount).as_deref() == Some(want))
}

#[cfg(test)]
mod commit_path_tests {
    use super::Pool;

    #[test]
    fn snap_dir_and_snap_are_under_voldir_snap() {
        let pool = Pool::new("/pool");
        assert_eq!(pool.snap_dir("v1"), std::path::Path::new("/pool/vol/v1/snap"));
        assert_eq!(pool.snap("v1", "v1-abcd1234"), std::path::Path::new("/pool/vol/v1/snap/v1-abcd1234"));
    }

    #[test]
    fn worktree_is_under_voldir_live() {
        let pool = Pool::new("/pool");
        assert_eq!(pool.worktree("v1", "ws1"), std::path::Path::new("/pool/vol/v1/live/ws1"));
    }
}

#[cfg(test)]
mod mount_tests {
    use super::mountpoint_in;

    #[test]
    fn mountpoint_matches_a_path_with_a_space() {
        let mounts = "/dev/loop0 /mnt/pool\\040one/vol/ws btrfs rw 0 0\n/dev/sda1 / ext4 rw 0 0\n";
        assert!(mountpoint_in(mounts, std::path::Path::new("/mnt/pool one/vol/ws")));
        assert!(mountpoint_in(mounts, std::path::Path::new("/")));
        assert!(!mountpoint_in(mounts, std::path::Path::new("/mnt/pool")));
    }
}

/// Serialize btrfs operations against one volume across processes — a checkout racing a commit
/// cut, or two reconcile passes landing at once, is exactly the shape of race this closes.
pub fn ws_lock(pool: &Pool, ws: &str) -> Result<std::fs::File, String> {
    let path = pool.root.join("vol").join(format!("{ws}.lock"));
    let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("flock failed".into());
    }
    Ok(f)
}
