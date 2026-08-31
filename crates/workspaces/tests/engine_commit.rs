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

/// Task 7a F5: `drop_worktree` is what reclaims a shared-volume clone's worktree on delete (no
/// ownerReference reaches `{pool}/vol/{volume}/live/{ws}`). Same retry-convergence shape as
/// `drop_commit`: gone once, and a second call against the same (now-absent) path is still Ok.
#[test]
fn drop_worktree_deletes_the_subvolume_and_is_ok_on_absent_retry() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    assert!(e.pool.worktree("v1", "ws1").exists());

    e.drop_worktree("v1", "ws1").unwrap();
    assert!(!e.pool.worktree("v1", "ws1").exists(), "the worktree subvolume must be gone");

    // Retried (a reconcile after this already landed, or a worktree never checked out at all).
    e.drop_worktree("v1", "ws1").unwrap();
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
        eprintln!("skipping: btrfs/root unavailable");
        return;
    }
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    let volume = "vol-1";
    let ws = "ws-1";

    // Bootstrap: an empty worktree, commit it as `c1`.
    e.checkout(volume, None, ws).unwrap();
    std::fs::write(e.pool.worktree(volume, ws).join("marker.txt"), b"c1 content").unwrap();
    e.commit_worktree(volume, ws, "c1").unwrap();

    // A crashed earlier restore attempt left a staging subvolume behind — `swap_worktree` must
    // discard it, not trip over it or graft its stale content in.
    let stale = format!("{ws}-restoring");
    e.checkout(volume, None, &stale).unwrap();
    std::fs::write(e.pool.worktree(volume, &stale).join("stale.txt"), b"leftover from a crash").unwrap();

    // Mutate `live` past `c1` — this is what the swap must undo.
    std::fs::write(e.pool.worktree(volume, ws).join("marker.txt"), b"mutated after c1").unwrap();
    std::fs::write(e.pool.worktree(volume, ws).join("extra.txt"), b"written after c1, must vanish").unwrap();

    e.swap_worktree(volume, ws, "c1").unwrap();

    let live = e.pool.worktree(volume, ws);
    assert_eq!(std::fs::read(live.join("marker.txt")).unwrap(), b"c1 content", "the swap must restore c1's own content");
    assert!(!live.join("extra.txt").exists(), "content written after c1 must not survive the swap back to c1");

    // No leftovers: the stale staging dir is gone (discarded before the real checkout), the
    // fresh staging dir is gone (renamed into place), and no `-before-restore` backup remains
    // (deleted after the swap completed).
    assert!(!e.pool.worktree(volume, &stale).exists(), "the stale staging subvolume must be discarded, not left in place");
    assert!(!e.pool.worktree(volume, &format!("{ws}-before-restore")).exists(), "the displaced worktree must be deleted, not left behind");
    let entries: Vec<_> = std::fs::read_dir(e.pool.voldir(volume).join("live")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(entries, vec![std::ffi::OsString::from(ws)], "only the swapped-in worktree remains: {entries:?}");
}
