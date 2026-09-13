//! The owner's persistent home on the region-shared NFS export: made to exist (with a private
//! `.ssh`) before any pod mounts into it. See the project guide's "one home per region".



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


/// `{pool}/homes/.benches/{team}/{owner}`: re-verifies the share, then mkdir; the team directory
/// root-owned 0755, the person's directory uid 1000 mode 0700 so another person's bench pod cannot
/// read it. Segments go through `k8s::bench_folder` (Task 3) so the agent and the pod builder can
/// never disagree on the path.
// ponytail: unwired until the bench reconciler (a later task) calls this on its own apply pass.
#[allow(dead_code)]
pub(crate) fn ensure_bench_folder(pool: &str, export: &str, team: &str, owner: &str, uid: u32) -> Result<(), String> {
    if crate::may_mount() {
        crate::mount_homes(pool, export)?;
    }
    let folder = kloudlite_workspaces::k8s::bench_folder(pool, team, owner)?;
    let dir = std::path::PathBuf::from(&folder);
    let team_dir = crate::homes_root(pool).join(".benches").join(team);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(&dir, Some(uid), Some(uid)).map_err(|e| e.to_string())?;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&team_dir, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    Ok(())
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

    #[test]
    fn a_bench_folder_is_made_on_the_share_private_and_refuses_an_escaping_segment() {
        use super::super::ensure_bench_folder;
        let tmp = tempfile::tempdir().unwrap();
        let pool = tmp.path().display().to_string();
        std::fs::create_dir_all(crate::homes_root(&pool)).unwrap();
        ensure_bench_folder(&pool, "unused", "acme", "alice", 1000).unwrap();
        let dir = crate::homes_root(&pool).join(".benches/acme/alice");
        assert!(dir.is_dir());
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        ensure_bench_folder(&pool, "unused", "acme", "alice", 1000).unwrap();
        assert!(ensure_bench_folder(&pool, "unused", "..", "alice", 1000).is_err());
        assert!(ensure_bench_folder(&pool, "unused", "acme", "../bob", 1000).is_err());
        assert!(!crate::homes_root(&pool).join("bob").exists());
    }
}
