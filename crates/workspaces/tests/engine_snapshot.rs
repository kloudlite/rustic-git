//! Snapshot/checkout primitive tests: real btrfs on a loopback pool. Every btrfs test is
//! `#[ignore]`d and asserts `have_btrfs()` rather than returning quietly (2026-09-12): a silent
//! skip counted fourteen tests as passing on every machine that could not run one of them, so an
//! unprivileged run now reports them as ignored and the btrfs review VM runs them with
//! `--ignored`, where an unmet prerequisite is a failure and not a green. Fixture copied from `engine_ops.rs`'s `LoopbackPool`:
//! integration test files cannot share code across `tests/*.rs`.

use kloudlite_workspaces::engine::{Engine, Pool, have_btrfs, is_subvolume};

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
    Engine::new(pool)
}

#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn snapshot_checkout_round_trip_preserves_content() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    // Bootstrap: empty worktree, write a file, snapshot it.
    e.checkout("v1", None, "ws1").unwrap();
    let f = e.pool.worktree("v1", "ws1").join("hello.txt");
    std::fs::write(&f, b"hi from ws1").unwrap();
    e.snapshot_worktree("v1", "ws1", "v1-snap1").unwrap();

    // Checkout that snapshot into a second worktree and read the content back.
    e.checkout("v1", Some("v1-snap1"), "ws2").unwrap();
    let got = std::fs::read(e.pool.worktree("v1", "ws2").join("hello.txt")).unwrap();
    assert_eq!(got, b"hi from ws1");

    // The snapshot itself must be read-only: `snapshot -r` is what makes the retention/GC story
    // safe (shared, never mutated), so writing into snap/{name} directly must fail.
    let write_into_snapshot = std::fs::write(e.pool.snap("v1", "v1-snap1").join("new.txt"), b"nope");
    assert!(write_into_snapshot.is_err(), "a snapshot subvolume must be read-only");
}

/// F1: snapshot_worktree must converge, not fail, when the snapshot already exists — the shape of
/// a retry after a crash between the snapshot landing and the CR's status update.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn snapshot_worktree_is_idempotent_on_an_existing_snapshot() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    std::fs::write(e.pool.worktree("v1", "ws1").join("f.txt"), b"payload").unwrap();
    e.snapshot_worktree("v1", "ws1", "v1-snap1").unwrap();

    // Same name again: must return Ok, not "File exists" — and the snapshot's content must be
    // exactly what the first call cut, not touched by the retry.
    e.snapshot_worktree("v1", "ws1", "v1-snap1").unwrap();
    e.checkout("v1", Some("v1-snap1"), "ws2").unwrap();
    let got = std::fs::read(e.pool.worktree("v1", "ws2").join("f.txt")).unwrap();
    assert_eq!(got, b"payload");
}

/// F3: drop_snapshot of a snapshot that never existed (or was already dropped) is a no-op — retry
/// convergence, same shape as `snapshot_worktree`'s.
///
/// "Returned Ok" alone was not a test of that (2026-09-12): a `drop_snapshot` that deleted the
/// whole `snap/` directory, or the worktree, would have passed it. The volume is populated first
/// and everything else asserted intact afterwards.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn drop_snapshot_of_an_absent_snapshot_is_a_no_op() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    std::fs::write(e.pool.worktree("v1", "ws1").join("f.txt"), b"payload").unwrap();
    e.snapshot_worktree("v1", "ws1", "keep-1").unwrap();
    e.snapshot_worktree("v1", "ws1", "keep-2").unwrap();

    e.drop_snapshot("v1", "no-such-snapshot").unwrap();

    for n in ["keep-1", "keep-2"] {
        assert!(e.pool.snap("v1", n).join("f.txt").exists(), "{n} was taken by a no-op drop");
    }
    assert!(e.pool.worktree("v1", "ws1").join("f.txt").exists(), "the worktree was taken by a no-op drop");
}

/// `drop_worktree` is what reclaims a shared-volume clone's worktree on delete (no
/// ownerReference reaches `{pool}/vol/{volume}/live/{ws}`). Same retry-convergence shape as
/// `drop_snapshot`: gone once, and a second call against the same (now-absent) path is still Ok.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn drop_worktree_deletes_the_subvolume_and_is_ok_on_absent_retry() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    assert!(e.pool.worktree("v1", "ws1").exists());

    // Durable snapshots rule 1: the snapshots under `snap/` must outlive the parent whose delete
    // path calls this — and an owned workspace's worktree is named after the volume itself, so
    // "drop the worktree" and "drop the snapshots" are one character apart in the pool.
    e.snapshot_worktree("v1", "ws1", "c1").unwrap();

    e.drop_worktree("v1", "ws1").unwrap();
    assert!(!e.pool.worktree("v1", "ws1").exists(), "the worktree subvolume must be gone");
    assert!(e.pool.snap("v1", "c1").exists(), "the snapshot must survive its worktree");

    // Retried (a reconcile after this already landed, or a worktree never checked out at all).
    e.drop_worktree("v1", "ws1").unwrap();
}

#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn checkout_of_missing_snapshot_errors_without_creating_anything() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    let err = e.checkout("v1", Some("no-such-snapshot"), "ws1").unwrap_err();
    assert!(err.0.contains("snapshot record not found"), "unexpected error: {}", err.0);
    assert!(!e.pool.worktree("v1", "ws1").exists(), "a failed checkout must leave no worktree");
}

#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn bootstrap_checkout_makes_an_empty_worktree() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    let wt = e.pool.worktree("v1", "ws1");
    assert!(wt.is_dir());
    assert_eq!(std::fs::read_dir(&wt).unwrap().count(), 0, "a bootstrap worktree starts empty");
}

/// The CoW independence the snapshot model rests on: dropping a snapshot that a checkout was cut
/// FROM must leave that checkout fully readable, because a checkout is its own snapshot the
/// instant `btrfs subvolume snapshot` returns.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn drop_snapshot_leaves_a_checkout_from_it_fully_readable() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    std::fs::write(e.pool.worktree("v1", "ws1").join("f.txt"), b"payload").unwrap();
    e.snapshot_worktree("v1", "ws1", "v1-snap1").unwrap();
    e.checkout("v1", Some("v1-snap1"), "ws2").unwrap();

    e.drop_snapshot("v1", "v1-snap1").unwrap();

    let got = std::fs::read(e.pool.worktree("v1", "ws2").join("f.txt")).unwrap();
    assert_eq!(got, b"payload", "checkout must survive its source snapshot being dropped");
}

#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn local_snapshots_lists_cut_snapshots() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    assert_eq!(e.local_snapshots("v1").unwrap(), Vec::<String>::new(), "no snap dir yet");

    e.checkout("v1", None, "ws1").unwrap();
    e.snapshot_worktree("v1", "ws1", "v1-a").unwrap();
    e.snapshot_worktree("v1", "ws1", "v1-b").unwrap();

    assert_eq!(e.local_snapshots("v1").unwrap(), vec!["v1-a".to_string(), "v1-b".to_string()]);
}

#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn checkout_refuses_an_existing_worktree_path() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws1").unwrap();
    let err = e.checkout("v1", None, "ws1").unwrap_err();
    assert!(err.0.contains("worktree already exists"), "unexpected error: {}", err.0);
}

/// The full restore-in-place lifecycle, end to end: checkout a worktree from a snapshot, cut a
/// second snapshot off it, mutate the live worktree past that point, swap it back to the FIRST
/// snapshot, and prove (a) the mutation is gone — the swap actually restored old content, not a
/// no-op — and (b) nothing is left behind: no `-restoring` staging subvolume, no
/// `-before-restore` backup, only the swapped-in worktree. Also seeds a STALE staging subvolume
/// from an earlier, crashed attempt first, so the same run exercises `swap_worktree`'s
/// discard-and-redo branch, not just the clean path.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn swap_worktree_restores_old_content_and_leaves_no_staging_or_backup() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    let volume = "vol-1";
    let ws = "ws-1";

    // Bootstrap: an empty worktree, snapshot it as `c1`.
    e.checkout(volume, None, ws).unwrap();
    std::fs::write(e.pool.worktree(volume, ws).join("marker.txt"), b"c1 content").unwrap();
    e.snapshot_worktree(volume, ws, "c1").unwrap();

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

/// An old-layout volume (`live` itself is the RW subvolume) moves to the snapshot model's
/// `live/{volume}` worktree layout, and the content survives the move untouched.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn migrate_volume_moves_old_layout_live_into_a_worktree() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    let volume = "v1";

    // Old layout: `live` created directly as a subvolume (create_subvol's shape), not via
    // checkout() — this is exactly what every pre-cutover volume looks like on disk.
    e.create_subvol(volume).unwrap();
    std::fs::write(e.pool.live(volume).join("marker.txt"), b"pre-model content").unwrap();

    assert!(e.migrate_volume(volume).unwrap(), "the first call must perform the move");

    let live = e.pool.live(volume);
    assert!(live.is_dir(), "live must now be a plain directory, not the subvolume itself");
    let wt = e.pool.worktree(volume, volume);
    assert_eq!(std::fs::read(wt.join("marker.txt")).unwrap(), b"pre-model content", "content must survive the layout move");
}

/// Idempotent: a volume already on the new layout (or already migrated) is left alone, and a
/// second call reports nothing-to-do.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn migrate_volume_is_idempotent() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    let volume = "v1";

    e.create_subvol(volume).unwrap();
    assert!(e.migrate_volume(volume).unwrap());
    assert!(!e.migrate_volume(volume).unwrap(), "a second call must be a no-op");

    // A volume that was always snapshot-model-native (checkout(), never create_subvol()) migrates
    // to nothing too — there is no old-layout subvolume to move.
    e.checkout("v2", None, "v2").unwrap();
    assert!(!e.migrate_volume("v2").unwrap(), "a native snapshot-model volume has nothing to migrate");
}

/// Crash recovery: a partial migration (the first rename landed, the second didn't) is completed
/// by the next call rather than re-touching the already-renamed subvolume.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn migrate_volume_recovers_from_a_partial_rename() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    let volume = "v1";

    e.create_subvol(volume).unwrap();
    std::fs::write(e.pool.live(volume).join("marker.txt"), b"pre-model content").unwrap();

    // Simulate the crash point: `live` renamed to `live-migrating`, no `live` dir made yet.
    let staging = e.pool.voldir(volume).join("live-migrating");
    std::fs::rename(e.pool.live(volume), &staging).unwrap();

    assert!(e.migrate_volume(volume).unwrap(), "recovery from a partial rename still counts as performing the move");
    assert!(!staging.exists(), "the staging subvolume must not be left behind");
    let wt = e.pool.worktree(volume, volume);
    assert_eq!(std::fs::read(wt.join("marker.txt")).unwrap(), b"pre-model content");
}

/// `set_quota` picks its arm by what's actually on disk (`is_subvolume`), not
/// by a migration flag. An old-layout volume (`live` itself is the RW subvolume) gets the
/// single-target arm; a migrated volume (`live` is a directory of worktrees) gets the limit
/// applied to each worktree individually.
#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn set_quota_picks_the_arm_by_the_layout_actually_on_disk() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    // The fixture's fresh filesystem has qgroups OFF (like a pool where `btrfs quota enable` was
    // never run) — `qgroup limit` then reports unenforced by design, which is set_quota's
    // Ok(Some(..)) arm, not its clean arm. Enable quotas so the assertions below prove the limit
    // actually APPLIES per layout, which is the arm-selection property this test pins.
    let st = std::process::Command::new("btrfs").args(["quota", "enable"]).arg(&lb.mount).status().unwrap();
    assert!(st.success(), "btrfs quota enable failed on the loopback pool");

    // Old layout: `live` is the subvolume itself.
    e.create_subvol("v1").unwrap();
    assert!(e.set_quota("v1", 1).unwrap().is_none(), "old-layout quota must apply cleanly");

    // Migrated layout: `live` is a directory of per-worktree subvolumes.
    e.checkout("v2", None, "ws1").unwrap();
    e.checkout("v2", None, "ws2").unwrap();
    assert!(e.set_quota("v2", 1).unwrap().is_none(), "migrated-layout quota must apply to every worktree");
}

/// M6: `swap_worktree`'s intermediate names are worktree-shaped, so `set_quota_worktrees`
/// qgroup-limits them and every `read_dir` of `live/` counts them as worktrees. A crash between the
/// two renames left one behind indefinitely.
#[test]
fn a_swap_leaves_no_worktree_shaped_leftovers() {
    use kloudlite_workspaces::engine::snapshot::{before_restore_name, restoring_name};
    // Build the two names the swap uses and assert the scanner skips them.
    for n in [restoring_name("ws-1"), before_restore_name("ws-1")] {
        assert!(n.starts_with('.'), "{n} must be skipped by the worktree scanners");
    }
}


/// The listing filters by SUBVOLUME, not by directory entry, and a missing `.agents` is an empty
/// list rather than an error. Both matter off btrfs too: a replica's `.agents/{name}` is a plain
/// directory `btrfs send` left behind, and reading it as a tree would have the owning node's
/// reconcile delete a tree that node never held.
#[test]
fn tree_names_reads_subvolumes_only_and_tolerates_no_agents_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let e = engine(Pool::new(tmp.path()));
    assert_eq!(e.tree_names("v1", "ws-1").unwrap(), Vec::<String>::new(), "no .agents at all");
    let agents = tmp.path().join("vol/v1/live/ws-1/.agents");
    std::fs::create_dir_all(agents.join("left-behind")).unwrap();
    std::fs::write(agents.join("a-file"), "x").unwrap();
    assert_eq!(
        e.tree_names("v1", "ws-1").unwrap(),
        Vec::<String>::new(),
        "a plain directory is not a tree — that is what a replica holds"
    );
}

/// The round trip on real btrfs: a tree is a nested subvolume of the worktree, it is listed by
/// name, and dropping the WORKTREE takes it with it (btrfs refuses to delete a parent that still
/// has children, which is the whole reason `drop_worktree` sweeps them first).
#[test]
#[ignore = "needs root and a btrfs-capable kernel"]
fn a_tree_is_a_nested_subvolume_and_the_worktree_drop_takes_it() {
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    e.checkout("v1", None, "ws-1").unwrap();
    e.cut_tree("v1", "ws-1", "fix-auth").unwrap();
    assert_eq!(e.tree_names("v1", "ws-1").unwrap(), vec!["fix-auth".to_string()]);
    // Level-triggered: cutting the same name again is the state being asked for, never a second
    // snapshot over a subagent's work.
    std::fs::write(lb.pool.worktree("v1", "ws-1").join(".agents/fix-auth/mine"), "x").unwrap();
    e.cut_tree("v1", "ws-1", "fix-auth").unwrap();
    assert!(lb.pool.worktree("v1", "ws-1").join(".agents/fix-auth/mine").exists(), "the tree was not re-cut over");

    e.drop_tree("v1", "ws-1", "fix-auth").unwrap();
    assert_eq!(e.tree_names("v1", "ws-1").unwrap(), Vec::<String>::new());

    // And the finalizer's path: a live tree must not wedge the worktree delete.
    e.cut_tree("v1", "ws-1", "other").unwrap();
    e.drop_worktree("v1", "ws-1").unwrap();
    assert!(!lb.pool.worktree("v1", "ws-1").exists());
}

/// A freshly created worktree belongs to the POD's uid, not to the root agent that made it.
///
/// `btrfs subvolume create` run by the agent leaves a root-owned tree. An ordinary workspace hid
/// that because its pod's prelude chowns the workspace dir on every start; a BENCH pod has no
/// workspace container and so no prelude, so every team bench crash-looped on
/// `EACCES: mkdir '/home/kl/workspaces/bench/.bench'` (2026-09-18).
///
/// A RESTORED worktree is a snapshot and inherits its source's ownership, so only the create path
/// needs this — which is what the second half asserts.
#[test]
#[ignore = "needs root and a btrfs-capable kernel"]
fn a_created_worktree_belongs_to_the_pods_uid() {
    use std::os::unix::fs::MetadataExt;
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let uid = kloudlite_workspaces::k8s::SSH_UID as u32;
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());

    e.checkout("v1", None, "ws-1").unwrap();
    let made = std::fs::metadata(lb.pool.worktree("v1", "ws-1")).unwrap();
    assert_eq!((made.uid(), made.gid()), (uid, uid), "a fresh worktree must be the tenant's");

    // A restore carries the source's ownership through the snapshot, with no chown of its own.
    e.snapshot_worktree("v1", "ws-1", "cut-1").unwrap();
    e.checkout("v1", Some("cut-1"), "ws-2").unwrap();
    let restored = std::fs::metadata(lb.pool.worktree("v1", "ws-2")).unwrap();
    assert_eq!((restored.uid(), restored.gid()), (uid, uid), "a restored worktree keeps the tenant");
}

/// The chown is skipped when this process cannot give a file away, so `checkout` still succeeds
/// unprivileged — a dev run and `cargo test` are not root, and a hard failure there would make
/// the engine untestable off a node. Runs everywhere, unlike the btrfs test above.
#[test]
fn an_unprivileged_checkout_is_not_refused_for_want_of_a_chown() {
    let tmp = tempfile::tempdir().unwrap();
    let e = engine(Pool::new(tmp.path()));
    // No btrfs here, so the `btrfs` call itself fails — what is asserted is that the failure is
    // that one, never a permission error from the chown path.
    let err = e.checkout("v1", None, "ws-1").unwrap_err().0;
    assert!(!err.to_lowercase().contains("operation not permitted"), "{err}");
}

#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn r07_interrupted_fresh_checkout_repairs_ownership_on_retry() {
    use std::os::unix::fs::MetadataExt;
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let uid = kloudlite_workspaces::k8s::SSH_UID as u32;
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    let volume = "v1";
    let ws = "ws-1";

    let live_dir = e.pool.live(volume);
    std::fs::create_dir_all(&live_dir).unwrap();
    let staging = live_dir.join(format!(".creating-{ws}"));
    let staging_str = staging.to_str().unwrap();
    run(&["btrfs", "subvolume", "create", staging_str]);
    assert_eq!(std::fs::metadata(&staging).unwrap().uid(), 0, "the seeded staging subvolume starts root-owned");
    run(&["btrfs", "property", "set", "-ts", staging_str, "ro", "true"]);

    let err = e.checkout(volume, None, ws).unwrap_err();
    assert!(err.0.to_lowercase().contains("read-only"), "unexpected error: {}", err.0);
    assert!(!e.pool.worktree(volume, ws).exists(), "a failed chown must not publish a live worktree");
    assert!(is_subvolume(&staging), "the staging subvolume must survive for the retry");
    assert_eq!(std::fs::metadata(&staging).unwrap().uid(), 0, "the refused chown must not have re-owned the staging subvolume");

    run(&["btrfs", "property", "set", "-ts", staging_str, "ro", "false"]);
    let e2 = engine(lb.pool());
    e2.checkout(volume, None, ws).unwrap();
    assert!(!staging.exists(), "the staging subvolume must be published, not left behind");
    let live = e2.pool.worktree(volume, ws);
    assert!(is_subvolume(&live));
    let made = std::fs::metadata(&live).unwrap();
    assert_eq!((made.uid(), made.gid()), (uid, uid), "the published worktree must be the tenant's");

    let st = std::process::Command::new("setpriv")
        .args([
            "--reuid=1000",
            "--regid=1000",
            "--clear-groups",
            "sh",
            "-c",
            "mkdir -p .bench && printf tenant > .bench/state.txt",
        ])
        .current_dir(&live)
        .status()
        .unwrap();
    assert!(st.success(), "uid 1000 must be able to create bench state in the published worktree");
    let state = live.join(".bench/state.txt");
    assert_eq!(std::fs::read(&state).unwrap(), b"tenant");
    let written = std::fs::metadata(&state).unwrap();
    assert_eq!((written.uid(), written.gid()), (uid, uid), "the tenant's write must land owned by the tenant");
}

#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn r07_restored_checkout_keeps_the_sources_ownership() {
    use std::os::unix::fs::MetadataExt;
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    let volume = "v1";
    let other = 4242u32;

    e.checkout(volume, None, "src").unwrap();
    let src = e.pool.worktree(volume, "src");
    std::fs::write(src.join("root-owned.txt"), b"root owned").unwrap();
    std::fs::create_dir(src.join("other")).unwrap();
    std::fs::write(src.join("other/child.txt"), b"other owned").unwrap();
    for p in [src.clone(), src.join("root-owned.txt")] {
        std::os::unix::fs::chown(&p, Some(0), Some(0)).unwrap();
    }
    for p in [src.join("other"), src.join("other/child.txt")] {
        std::os::unix::fs::chown(&p, Some(other), Some(other)).unwrap();
    }

    e.snapshot_worktree(volume, "src", "cut-1").unwrap();
    e.checkout(volume, Some("cut-1"), "restored").unwrap();

    let dst = e.pool.worktree(volume, "restored");
    assert!(is_subvolume(&dst));
    let owners = |p: &std::path::Path| {
        let m = std::fs::metadata(p).unwrap();
        (m.uid(), m.gid())
    };
    assert_eq!(owners(&dst), (0, 0), "a restored worktree keeps its source's owner, not the tenant's");
    assert_eq!(owners(&dst.join("root-owned.txt")), (0, 0));
    assert_eq!(owners(&dst.join("other")), (other, other));
    assert_eq!(owners(&dst.join("other/child.txt")), (other, other));
    assert_eq!(std::fs::read(dst.join("root-owned.txt")).unwrap(), b"root owned");
    assert_eq!(std::fs::read(dst.join("other/child.txt")).unwrap(), b"other owned");
}

#[test]
#[ignore = "needs root and btrfs: run with --ignored on a btrfs node"]
fn r07_invalid_staging_paths_are_refused_without_publication() {
    use std::os::unix::fs::MetadataExt;
    assert!(have_btrfs(), "needs root and a btrfs-capable kernel");
    let lb = LoopbackPool::new();
    let e = engine(lb.pool());
    let other = 4242u32;
    let ws = "ws-1";

    let volume = "v-dir";
    let live = e.pool.live(volume);
    std::fs::create_dir_all(&live).unwrap();
    let staging = live.join(format!(".creating-{ws}"));
    std::fs::create_dir(&staging).unwrap();
    let sentinel = staging.join("sentinel.txt");
    std::fs::write(&sentinel, b"directory sentinel").unwrap();
    run(&["chown", "-h", "4242:4242", staging.to_str().unwrap()]);

    let err = e.checkout(volume, None, ws).unwrap_err();
    assert!(err.0.contains("staging path is not a subvolume"), "unexpected error: {}", err.0);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"directory sentinel");
    let m = std::fs::symlink_metadata(&staging).unwrap();
    assert!(m.file_type().is_dir() && !is_subvolume(&staging));
    assert_eq!((m.uid(), m.gid()), (other, other), "the refused staging dir must keep its owner");
    assert!(!e.pool.worktree(volume, ws).exists(), "nothing may be published from an invalid staging path");

    let volume = "v-file";
    let live = e.pool.live(volume);
    std::fs::create_dir_all(&live).unwrap();
    let staging = live.join(format!(".creating-{ws}"));
    std::fs::write(&staging, b"file sentinel").unwrap();
    run(&["chown", "-h", "4242:4242", staging.to_str().unwrap()]);

    let err = e.checkout(volume, None, ws).unwrap_err();
    assert!(err.0.contains("staging path is not a subvolume"), "unexpected error: {}", err.0);
    assert_eq!(std::fs::read(&staging).unwrap(), b"file sentinel");
    let m = std::fs::symlink_metadata(&staging).unwrap();
    assert!(m.file_type().is_file());
    assert_eq!((m.uid(), m.gid()), (other, other), "the refused staging file must keep its owner");
    assert!(!e.pool.worktree(volume, ws).exists());

    let volume = "v-link";
    let live = e.pool.live(volume);
    std::fs::create_dir_all(&live).unwrap();
    let target = lb.mount.join("link-target");
    std::fs::create_dir_all(&target).unwrap();
    let target_sentinel = target.join("sentinel.txt");
    std::fs::write(&target_sentinel, b"symlink target sentinel").unwrap();
    run(&["chown", "-hR", "4242:4242", target.to_str().unwrap()]);
    let staging = live.join(format!(".creating-{ws}"));
    std::os::unix::fs::symlink(&target, &staging).unwrap();
    run(&["chown", "-h", "4242:4242", staging.to_str().unwrap()]);

    let err = e.checkout(volume, None, ws).unwrap_err();
    assert!(err.0.contains("staging path is not a subvolume"), "unexpected error: {}", err.0);
    let m = std::fs::symlink_metadata(&staging).unwrap();
    assert!(m.file_type().is_symlink(), "the planted symlink must not be replaced");
    assert_eq!((m.uid(), m.gid()), (other, other), "the planted symlink must keep its owner");
    assert_eq!(std::fs::read_link(&staging).unwrap(), target);
    assert_eq!(std::fs::read(&target_sentinel).unwrap(), b"symlink target sentinel");
    let tm = std::fs::metadata(&target).unwrap();
    assert_eq!((tm.uid(), tm.gid()), (other, other), "the symlink target must keep its owner");
    assert!(!e.pool.worktree(volume, ws).exists(), "a planted symlink must never be published");
}
