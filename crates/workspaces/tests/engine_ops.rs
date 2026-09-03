//! Engine op tests that touch btrfs directly. Every test opens with `have_btrfs()` and returns
//! cleanly when it's false (this Mac, any non-root CI runner) — they run for real on the btrfs
//! review VM. Fixture copied from `engine_snapshot.rs`'s `LoopbackPool`: integration test files
//! cannot share code across `tests/*.rs`.

use rustic_git_workspaces::engine::{Engine, Pool, have_btrfs};

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
        run(&["truncate", "-s", "4G", img.to_str().unwrap()]);
        run(&["mkfs.btrfs", "-q", img.to_str().unwrap()]);
        run(&["mount", "-o", "loop", img.to_str().unwrap(), mount.to_str().unwrap()]);
        let pool = Pool::new(mount.clone());
        std::fs::create_dir_all(pool.root.join("vol")).unwrap();
        LoopbackPool { pool, mount, _tmp: tmp }
    }

    fn pool(&self) -> Pool {
        Pool::new(self.pool.root.clone())
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

fn engine() -> (Engine, LoopbackPool) {
    let lb = LoopbackPool::new();
    (Engine::new(lb.pool()), lb)
}

#[test]
fn ensure_homecache_creates_a_subvolume_with_the_four_dirs_owned_by_the_uid() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let (engine, _tmp) = engine(); // the file's existing btrfs-pool fixture
    engine.ensure_homecache("alice", 1000).unwrap();
    let root = engine.pool.root.join("homecache/alice");
    assert!(rustic_git_workspaces::engine::ops::is_subvolume(&root));
    for d in ["cache", "vscode-server", "cursor-server", "state"] {
        let m = std::fs::metadata(root.join(d)).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(m.uid(), 1000, "{d}");
    }
    engine.ensure_homecache("alice", 1000).unwrap(); // idempotent
}
