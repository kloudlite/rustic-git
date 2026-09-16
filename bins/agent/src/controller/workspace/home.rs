//! The owner's persistent home on the region-shared NFS export: made to exist (with a private
//! `.ssh`) before any pod mounts into it. See the project guide's "one home per region".
//!
//! The home itself outlives every object that mounts it. A LEGACY BENCH FOLDER
//! (`.benches/{team}/{owner}`, where the transcripts lived before a bench became a Workspace) is
//! moved into the bench's own volume once and then renamed aside — `migrate_bench_folder` — so the
//! transcripts are snapshotted, replicated and pushed with the workspace like everything else.



/// `{pool}/homes/{owner}`: on the shared-home NFS mount (`mount_homes` in `lib.rs` puts the export
/// there at agent startup), so materializing an owner's home is plain `mkdir` + `chown` — no
/// subvolume, no snapshot, nothing btrfs-specific. Idempotent; safe on every reconcile.
///
/// Re-verifies the export first and REPAIRS it (`mount_homes`: mounted and answering is a no-op;
/// stale or missing is detach-and-remount) — mkdir under a vanished mount point would build the
/// person an empty home on the node's rootfs and report it Ready, and mkdir under a stale one
/// (the export moved nodes) fails EIO on every reconcile forever without this.
pub(crate) fn ensure_shared_home(pool: &str, export: &str, owner: &str, uid: u32) -> Result<(), String> {
    // Gated on the CAPABILITY to mount, not on uid 0: in production the agent is privileged and
    // `/proc/mounts` tells the truth, while a dev/test pool is an ordinary directory nobody ever
    // mounted — and the tests now also run as root inside an unprivileged dev pod, where a uid
    // gate let this reach for a real NFS mount of a fixture address and hang two minutes on it.
    if crate::may_mount() {
        crate::mount_homes(pool, export)?;
    }
    let dir = crate::homes_root(pool).join(owner);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // `.ssh` too, and BEFORE the pod: `authorized_keys` is a hostPath file mounted at
    // `~/.ssh/authorized_keys`, and a kubelet that finds no `.ssh` creates it itself — as root,
    // 0755, with an empty root-owned file under the mount point — after which the person cannot
    // write `known_hosts` ("Failed to add the host to the list of known hosts"). Every reconcile,
    // so a home the kubelet already did that to is healed on the next pass, not only new ones.
    let ssh = dir.join(".ssh");
    std::fs::create_dir_all(&ssh).map_err(|e| e.to_string())?;
    // Only root may chown to an arbitrary uid; the agent always runs privileged in production
    // (DaemonSet, see CLAUDE.md), so this only ever no-ops in a dev/test environment, letting the
    // reconcile loop under test exercise the surrounding logic without needing root itself.
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(&dir, Some(uid), Some(uid)).map_err(|e| e.to_string())?;
        std::os::unix::fs::chown(&ssh, Some(uid), Some(uid)).map_err(|e| e.to_string())?;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    Ok(())
}


/// The ONE-TIME move of a legacy bench folder into the bench workspace's own volume.
///
/// Before the unification a bench's transcripts lived at `{pool}/homes/.benches/{team}/{owner}` on
/// the region share, with no volume behind them; they now live at `{worktree}/.bench`, inside the
/// btrfs subvolume, so they are snapshotted, replicated and pushed with the workspace. This runs on
/// the first pass that finds the worktree with no `.bench` in it and a legacy folder still there.
///
/// `Ok(None)` = nothing to do (already migrated, or this bench never had a legacy folder).
/// `Ok(Some((files, bytes)))` = copied, and the legacy folder renamed aside — never deleted; a
/// later cleanup is the owner's call, and nothing has read the new copy yet.
///
/// The lock files are not copied: a lock belongs to a process, not to data, and the pod that holds
/// the new folder takes its own.
pub(crate) fn migrate_bench_folder(pool: &str, export: &str, team: &str, owner: &str, worktree: &std::path::Path) -> Result<Option<(u64, u64)>, String> {
    let dest = worktree.join(kloudlite_workspaces::k8s::BENCH_SUBDIR);
    // The whole "already done" answer, and true as well for a bench made after this shipped, which
    // never had a legacy folder at all — the pod creates `.bench` itself on its first start.
    if dest.exists() {
        return Ok(None);
    }
    if crate::may_mount() {
        crate::mount_homes(pool, export)?;
    }
    // Through `bench_folder` so the path is validated exactly as the old pod builder validated it:
    // `team`/`owner` come from the CRD, not from a body `/v1` has already checked.
    let legacy = std::path::PathBuf::from(kloudlite_workspaces::k8s::bench_folder(pool, team, owner)?);
    match std::fs::symlink_metadata(&legacy) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return Err(format!("{}: not a directory", legacy.display())),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", legacy.display())),
    }
    // Staged under a name the pod ignores and renamed into place only once the whole tree landed:
    // a crash mid-copy would otherwise leave a half `.bench` that reads as "already migrated"
    // forever, and the person would lose whatever had not been copied yet.
    let staging = worktree.join(".bench.migrating");
    if let Err(e) = std::fs::remove_dir_all(&staging) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("{}: {e}", staging.display()));
        }
    }
    let counted = copy_tree(&legacy, &staging, kloudlite_workspaces::k8s::SSH_UID as u32)?;
    std::fs::rename(&staging, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let unix = k8s_openapi::jiff::Timestamp::now().as_second();
    let aside = legacy.with_file_name(format!("{owner}.migrated-{unix}"));
    std::fs::rename(&legacy, &aside).map_err(|e| format!("{}: {e}", aside.display()))?;
    Ok(Some(counted))
}

/// `cp -a` minus the locks: files and directories only — a symlink, socket or device in a
/// transcript folder is not data we can meaningfully carry into a subvolume, and following one
/// would copy from wherever it points. Returns `(files, bytes)`.
fn copy_tree(src: &std::path::Path, dst: &std::path::Path, uid: u32) -> Result<(u64, u64), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(dst, Some(uid), Some(uid)).map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    let (mut files, mut bytes) = (0, 0);
    for entry in std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", src.display()))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".lock") {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(&name));
        let meta = std::fs::symlink_metadata(&from).map_err(|e| format!("{}: {e}", from.display()))?;
        if meta.file_type().is_symlink() {
            continue;
        } else if meta.is_dir() {
            let (f, b) = copy_tree(&from, &to, uid)?;
            files += f;
            bytes += b;
        } else if meta.is_file() {
            bytes += std::fs::copy(&from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
            files += 1;
            if unsafe { libc::geteuid() } == 0 {
                std::os::unix::fs::chown(&to, Some(uid), Some(uid)).map_err(|e| format!("{}: {e}", to.display()))?;
            }
        }
    }
    Ok((files, bytes))
}


#[cfg(test)]
pub(crate) mod home_tests {
    use super::super::ensure_shared_home;
    use std::os::unix::fs::PermissionsExt;

    /// The kubelet must never be the one to create `.ssh`: it exists, 0700, before any pod, and a
    /// root-made one (the pre-2026-09-09 shape) is put back to 0700 on the next pass.
    #[test]
    fn the_home_has_a_private_ssh_dir_before_any_pod() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = tmp.path().display().to_string();
        std::fs::create_dir_all(crate::homes_root(&pool)).unwrap();
        ensure_shared_home(&pool, "unused", "alice", 1000).unwrap();
        let ssh = crate::homes_root(&pool).join("alice/.ssh");
        assert_eq!(std::fs::metadata(&ssh).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_shared_home(&pool, "unused", "alice", 1000).unwrap();
        assert_eq!(std::fs::metadata(&ssh).unwrap().permissions().mode() & 0o777, 0o700);
    }

    /// The one-time move: the transcripts land in the volume, the lock is left behind, and the
    /// legacy folder is renamed aside rather than deleted.
    #[test]
    fn a_legacy_bench_folder_moves_into_the_worktree_once_and_is_renamed_aside() {
        use super::super::migrate_bench_folder;
        let tmp = tempfile::tempdir().unwrap();
        let pool = tmp.path().display().to_string();
        let legacy = crate::homes_root(&pool).join(".benches/acme/alice");
        std::fs::create_dir_all(legacy.join("sessions")).unwrap();
        std::fs::write(legacy.join("sessions/a.jsonl"), "hello").unwrap();
        std::fs::write(legacy.join(".lock"), "").unwrap();
        std::fs::write(legacy.join(".lock.holder"), "node-b").unwrap();
        let worktree = tmp.path().join("vol/v1/live/bench-1");
        std::fs::create_dir_all(&worktree).unwrap();

        let (files, bytes) = migrate_bench_folder(&pool, "unused", "acme", "alice", &worktree).unwrap().expect("migrated");
        assert_eq!((files, bytes), (1, 5));
        assert_eq!(std::fs::read_to_string(worktree.join(".bench/sessions/a.jsonl")).unwrap(), "hello");
        assert!(!worktree.join(".bench/.lock").exists(), "a lock belongs to a process, not to data");
        assert!(!legacy.exists(), "the legacy folder is renamed, not left in place");
        let aside: Vec<_> = std::fs::read_dir(crate::homes_root(&pool).join(".benches/acme")).unwrap().flatten().collect();
        assert_eq!(aside.len(), 1);
        assert!(aside[0].file_name().to_string_lossy().starts_with("alice.migrated-"), "{:?}", aside[0].file_name());
        assert!(!worktree.join(".bench.migrating").exists(), "the staging name is gone");

        // Idempotent: `.bench` present is the whole answer, whatever is left on the share.
        assert_eq!(migrate_bench_folder(&pool, "unused", "acme", "alice", &worktree), Ok(None));
    }

    /// Nothing to migrate is not a failure — every bench made after this shipped is this case, and
    /// a pod must not be held back waiting for a folder that never existed.
    #[test]
    fn a_bench_with_no_legacy_folder_migrates_nothing_and_refuses_an_escaping_segment() {
        use super::super::migrate_bench_folder;
        let tmp = tempfile::tempdir().unwrap();
        let pool = tmp.path().display().to_string();
        std::fs::create_dir_all(crate::homes_root(&pool)).unwrap();
        let worktree = tmp.path().join("vol/v1/live/bench-1");
        std::fs::create_dir_all(&worktree).unwrap();
        assert_eq!(migrate_bench_folder(&pool, "unused", "acme", "alice", &worktree), Ok(None));
        assert!(!worktree.join(".bench").exists());
        assert!(migrate_bench_folder(&pool, "unused", "..", "alice", &worktree).is_err());
        assert!(migrate_bench_folder(&pool, "unused", "acme", "../bob", &worktree).is_err());
    }

    /// A symlink where the folder should be is refused, not followed: this runs as root on a share
    /// every bench pod writes to.
    #[test]
    fn a_symlinked_legacy_folder_is_refused() {
        use super::super::migrate_bench_folder;
        let tmp = tempfile::tempdir().unwrap();
        let pool = tmp.path().display().to_string();
        let victim = tmp.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::create_dir_all(crate::homes_root(&pool).join(".benches/acme")).unwrap();
        std::os::unix::fs::symlink(&victim, crate::homes_root(&pool).join(".benches/acme/alice")).unwrap();
        let worktree = tmp.path().join("vol/v1/live/bench-1");
        std::fs::create_dir_all(&worktree).unwrap();
        assert!(migrate_bench_folder(&pool, "unused", "acme", "alice", &worktree).is_err());
        assert!(victim.is_dir());
    }
}
