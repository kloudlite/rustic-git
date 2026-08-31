//! Commit/checkout primitive tests: real btrfs on a loopback pool. Every test opens with
//! `have_btrfs()` and returns cleanly when it's false (this Mac, any non-root CI runner) — they
//! run for real on the btrfs review VM. Fixture copied from `engine_ops.rs`'s `LoopbackPool`:
//! integration test files cannot share code across `tests/*.rs`.

use object_store::memory::InMemory;
use rustic_git_workspaces::engine::{Engine, Pool, have_btrfs};
use rustic_git_workspaces::registry_client::RegistryClient;
use std::sync::Arc;

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

fn engine(pool: Pool) -> Engine {
    // Neither store nor registry is touched by commit/checkout/local_commits/drop_commit — a
    // fake in-memory store and an unreachable registry base are enough.
    Engine::new(pool, Arc::new(InMemory::new()), RegistryClient::new("http://127.0.0.1:1", "unused"))
}

#[test]
fn commit_checkout_round_trip_preserves_content() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    // Bootstrap: empty worktree, write a file, commit it.
    e.checkout("v1", None, "ws1").unwrap();
    let f = e.pool.worktree("v1", "ws1").join("hello.txt");
    std::fs::write(&f, b"hi from ws1").unwrap();
    e.commit_worktree("v1", "ws1", "v1-commit1").unwrap();

    // Checkout that commit into a second worktree and read the content back.
    e.checkout("v1", Some("v1-commit1"), "ws2").unwrap();
    let got = std::fs::read(e.pool.worktree("v1", "ws2").join("hello.txt")).unwrap();
    assert_eq!(got, b"hi from ws1");

    // The commit itself must be read-only: `snapshot -r` is what makes the retention/GC story
    // safe (shared, never mutated), so writing into snap/{name} directly must fail.
    let write_into_commit = std::fs::write(e.pool.snap("v1", "v1-commit1").join("new.txt"), b"nope");
    assert!(write_into_commit.is_err(), "a commit subvolume must be read-only");
}

/// F1: commit_worktree must converge, not fail, when the snapshot already exists — the shape of
/// a retry after a crash between the snapshot landing and the CR's status update.
#[test]
fn commit_worktree_is_idempotent_on_an_existing_snapshot() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    std::fs::write(e.pool.worktree("v1", "ws1").join("f.txt"), b"payload").unwrap();
    e.commit_worktree("v1", "ws1", "v1-commit1").unwrap();

    // Same name again: must return Ok, not "File exists" — and the commit's content must be
    // exactly what the first call cut, not touched by the retry.
    e.commit_worktree("v1", "ws1", "v1-commit1").unwrap();
    e.checkout("v1", Some("v1-commit1"), "ws2").unwrap();
    let got = std::fs::read(e.pool.worktree("v1", "ws2").join("f.txt")).unwrap();
    assert_eq!(got, b"payload");
}

/// F3: drop_commit of a commit that never existed (or was already dropped) is a no-op — retry
/// convergence, same shape as `commit_worktree`'s.
#[test]
fn drop_commit_of_an_absent_commit_is_a_no_op() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.drop_commit("v1", "no-such-commit").unwrap();
}

#[test]
fn checkout_of_missing_commit_errors_without_creating_anything() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    let err = e.checkout("v1", Some("no-such-commit"), "ws1").unwrap_err();
    assert!(err.0.contains("commit record not found"), "unexpected error: {}", err.0);
    assert!(!e.pool.worktree("v1", "ws1").exists(), "a failed checkout must leave no worktree");
}

#[test]
fn bootstrap_checkout_makes_an_empty_worktree() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    let wt = e.pool.worktree("v1", "ws1");
    assert!(wt.is_dir());
    assert_eq!(std::fs::read_dir(&wt).unwrap().count(), 0, "a bootstrap worktree starts empty");
}

/// The CoW independence the commit model rests on: dropping a commit that a checkout was cut
/// FROM must leave that checkout fully readable, because a checkout is its own snapshot the
/// instant `btrfs subvolume snapshot` returns.
#[test]
fn drop_commit_leaves_a_checkout_from_it_fully_readable() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    std::fs::write(e.pool.worktree("v1", "ws1").join("f.txt"), b"payload").unwrap();
    e.commit_worktree("v1", "ws1", "v1-commit1").unwrap();
    e.checkout("v1", Some("v1-commit1"), "ws2").unwrap();

    e.drop_commit("v1", "v1-commit1").unwrap();

    let got = std::fs::read(e.pool.worktree("v1", "ws2").join("f.txt")).unwrap();
    assert_eq!(got, b"payload", "checkout must survive its source commit being dropped");
}

#[test]
fn local_commits_lists_committed_snapshots() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    assert_eq!(e.local_commits("v1").unwrap(), Vec::<String>::new(), "no snap dir yet");

    e.checkout("v1", None, "ws1").unwrap();
    e.commit_worktree("v1", "ws1", "v1-a").unwrap();
    e.commit_worktree("v1", "ws1", "v1-b").unwrap();

    assert_eq!(e.local_commits("v1").unwrap(), vec!["v1-a".to_string(), "v1-b".to_string()]);
}

#[test]
fn checkout_refuses_an_existing_worktree_path() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    let err = e.checkout("v1", None, "ws1").unwrap_err();
    assert!(err.0.contains("worktree already exists"), "unexpected error: {}", err.0);
}
