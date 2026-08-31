//! Commit-model checkout/swap, against a REAL btrfs loopback pool. Gated on `have_btrfs()`
//! (root + the binary on PATH) exactly like `engine_pool.rs`'s own btrfs test — prints a skip
//! line and returns cleanly everywhere else, this Mac included.

use object_store::memory::InMemory;
use rustic_git_workspaces::engine::{Engine, Pool, have_btrfs};
use rustic_git_workspaces::registry_client::RegistryClient;

struct LoopbackPool {
    pool: Pool,
    mount: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

impl LoopbackPool {
    fn new() -> LoopbackPool {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("pool.img");
        let mount = tmp.path().join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        run(&["truncate", "-s", "2G", img.to_str().unwrap()]);
        run(&["mkfs.btrfs", "-q", img.to_str().unwrap()]);
        run(&["mount", "-o", "loop", img.to_str().unwrap(), mount.to_str().unwrap()]);
        let pool = Pool::new(mount.clone());
        std::fs::create_dir_all(pool.recv()).unwrap();
        std::fs::create_dir_all(pool.root.join("vol")).unwrap();
        LoopbackPool { pool, mount, _tmp: tmp }
    }
}

impl Drop for LoopbackPool {
    fn drop(&mut self) {
        let _ = std::process::Command::new("umount").arg(&self.mount).status();
    }
}

fn run(argv: &[&str]) {
    let st = std::process::Command::new(argv[0]).args(&argv[1..]).status().unwrap();
    assert!(st.success(), "{argv:?} failed");
}

fn engine(pool: Pool) -> Engine {
    Engine::new(pool, std::sync::Arc::new(InMemory::new()), RegistryClient::new("http://127.0.0.1:1", "test"))
}

/// The full restore-in-place lifecycle, end to end: checkout a worktree from a commit, cut a
/// second commit off it, mutate the live worktree past that point, swap it back to the FIRST
/// commit, and prove (a) the mutation is gone — the swap actually restored old content, not a
/// no-op — and (b) nothing is left behind: no `-restoring` staging subvolume, no
/// `-before-restore` backup, only the swapped-in worktree. Also seeds a STALE staging subvolume
/// from an earlier, crashed attempt first, so the same run exercises `swap_worktree`'s
/// discard-and-redo branch, not just the clean path.
#[test]
fn swap_worktree_restores_old_content_and_leaves_no_staging_or_backup() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let e = engine(Pool::new(lp.pool.root.clone()));
    let volume = "vol-1";
    let ws = "ws-1";

    // Bootstrap: an empty worktree, commit it as `c1`.
    e.checkout(volume, None, ws).unwrap();
    std::fs::write(lp.pool.worktree(volume, ws).join("marker.txt"), b"c1 content").unwrap();
    e.commit_worktree(volume, ws, "c1").unwrap();

    // A crashed earlier restore attempt left a staging subvolume behind — `swap_worktree` must
    // discard it, not trip over it or graft its stale content in.
    let stale = format!("{ws}-restoring");
    e.checkout(volume, None, &stale).unwrap();
    std::fs::write(lp.pool.worktree(volume, &stale).join("stale.txt"), b"leftover from a crash").unwrap();

    // Mutate `live` past `c1` — this is what the swap must undo.
    std::fs::write(lp.pool.worktree(volume, ws).join("marker.txt"), b"mutated after c1").unwrap();
    std::fs::write(lp.pool.worktree(volume, ws).join("extra.txt"), b"written after c1, must vanish").unwrap();

    e.swap_worktree(volume, ws, "c1").unwrap();

    let live = lp.pool.worktree(volume, ws);
    assert_eq!(std::fs::read(live.join("marker.txt")).unwrap(), b"c1 content", "the swap must restore c1's own content");
    assert!(!live.join("extra.txt").exists(), "content written after c1 must not survive the swap back to c1");

    // No leftovers: the stale staging dir is gone (discarded before the real checkout), the
    // fresh staging dir is gone (renamed into place), and no `-before-restore` backup remains
    // (deleted after the swap completed).
    assert!(!lp.pool.worktree(volume, &stale).exists(), "the stale staging subvolume must be discarded, not left in place");
    assert!(!lp.pool.worktree(volume, &format!("{ws}-before-restore")).exists(), "the displaced worktree must be deleted, not left behind");
    let entries: Vec<_> = std::fs::read_dir(lp.pool.voldir(volume).join("live")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(entries, vec![std::ffi::OsString::from(ws)], "only the swapped-in worktree remains: {entries:?}");
}
