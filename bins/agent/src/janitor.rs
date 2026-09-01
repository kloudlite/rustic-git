//! Local storage janitor for `rustic-git-agent`: the ten-minute beat that reclaims attach
//! directories and stale nix profile index entries, plus the full local reclaim a deleted volume
//! triggers. Split out of `lib.rs`, which is process setup and nothing else.
//!
//! The object-store era's sweeps (`recv/`, `stage/`, `img/`, lineage-driven snapshot retention)
//! are gone with the subsystem they served (Task 8) — nothing writes those directories any more,
//! and commit-model retention (which commit subvolumes to keep) is `crd::Snapshot`'s own
//! reconcile, not this beat.

use crate::nix;
use rustic_git_workspaces::engine::{is_subvolume, Engine};
use std::sync::Arc;

/// Local storage janitor: every ten minutes, reclaims `{pool}/attach/*` orphans and stale nix
/// profile index entries. Split out of `spawn_janitor`'s loop so the reclaim counts are testable
/// without waiting on the interval.
///
/// The whole beat runs on ONE blocking thread: every step shells out or walks a directory, and on
/// the reactor each of those stalled every in-flight reconcile for as long as it takes — hundreds
/// of volumes on a two-vCPU node is minutes of that, aligned to the ten-minute interval.
///
/// Takes no `Engine` any more — every remaining sweep is a plain directory walk keyed by pool
/// path, not by anything the engine knows. `cleanup_local` (below), the one caller that does need
/// one, is not driven by this beat at all.
pub fn spawn_janitor(pool: String, nix: Arc<dyn nix::Nix>) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(600));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let pool = pool.clone();
            let beat = tokio::task::spawn_blocking(move || {
                let (attach, profiles) = janitor_beat(&pool);
                if attach > 0 || profiles > 0 {
                    tracing::info!(attach, profiles, "agent: janitor reclaimed attach dir(s), profile index entries");
                }
                // The store is a per-node cache; the profile out-links are its only roots, so a
                // GC is always safe and the only question is when. Size by `du` of the store dir,
                // best effort — a wrong number costs an early or late GC, never data.
                // ponytail: du of a 60 GB store every 10 min is real IO; `statvfs` of the /nix
                // filesystem is the cheaper signal once /nix is its own mount.
                (dir_bytes(std::path::Path::new("/nix/store")), profiles)
            })
            .await;
            // Never collect behind a beat that swept index entries: the sweep unlinks GC roots,
            // and a reconcile that read one of those entries moments earlier is about to publish
            // `{id}/current` pointing at it. Collecting in the same beat is what turns a benign
            // unlink into a collected LIVE path — the pod starts with an empty profile and only
            // heals on its next reconcile. Deferring the GC one ten-minute beat costs nothing;
            // this sweep bounds the index, it does not reclaim urgently.
            match beat {
                Ok((used, 0)) if used > NIX_GC_HIGH_BYTES => match nix.collect_garbage().await {
                    Ok(freed) => tracing::info!(used, freed, "agent: nix store over threshold, collected garbage"),
                    Err(e) => tracing::warn!(error = %e, "agent: nix-collect-garbage failed"),
                },
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "agent: the janitor beat panicked; skipping it"),
            }
        }
    });
}

/// One sweep of the pool: (attach dirs reclaimed, profile index entries reclaimed).
fn janitor_beat(pool: &str) -> (usize, usize) {
    warn_oversized_homes(std::path::Path::new(pool));
    let attach = janitor_sweep_attach(std::path::Path::new(pool), SWEEP_MIN_AGE);
    let profiles = janitor_sweep_profiles(std::path::Path::new(nix::PROFILES_DIR), SWEEP_MIN_AGE);
    (attach, profiles)
}

/// Reclaims `by-inputs/{hash}` index entries (Task 2) that no `{id}/current` link points at.
///
/// Each index entry is a GC root — `PROFILES_DIR` is covered by `gcroots/rustic-profiles`, so a
/// store path it names is kept alive forever, even after every workspace that built it is long
/// gone. This bounds that set; it does not try to reclaim quickly (`SWEEP_MIN_AGE`, same floor as
/// every other sweep here, since a reconcile can `record_index` an entry moments before
/// `link_profile` makes it live).
///
/// The keep-set is store PATHS, not hashes: `nix::indexed` keys by input hash, but two workspaces
/// with identical inputs share one entry, and what actually keeps a store path alive is whether
/// ANY `current` resolves to it — read once, same shape as `janitor_sweep_attach`'s `vol/` read,
/// so an unreadable profiles root sweeps nothing rather than reading as "nothing is live".
///
/// Unlike `janitor_sweep_attach`, the keep-set build below does not `.flatten()`/`.ok()` its way
/// past errors: `janitor_sweep_attach`'s mistake recreates a directory, but this sweep's mistake
/// unlinks a GC root, and the next `nix-collect-garbage` then collects a store path a running
/// workspace still has mounted. So any error other than the `current` link simply not existing yet
/// (a workspace between `record_index` and `link_profile`, which the age bound already covers) is
/// treated as "the keep-set might be incomplete" and the whole sweep bails.
fn janitor_sweep_profiles(profiles: &std::path::Path, min_age: std::time::Duration) -> usize {
    let Some(live) = live_profile_targets(profiles) else { return 0 };
    let Ok(entries) = std::fs::read_dir(profiles.join("by-inputs")) else { return 0 };
    let mut swept = 0;
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(target) = std::fs::read_link(&p) else { continue };
        if live.contains(&target) || younger_than(&entry, min_age) {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            swept += 1;
        }
    }
    swept
}

/// Every store path some `{id}/current` resolves to, or `None` if that can't be established with
/// confidence. `None` on the first `read_dir`/`read_link` error that isn't "no `current` yet" —
/// see `janitor_sweep_profiles`'s doc for why this is stricter than its sibling sweeps.
fn live_profile_targets(profiles: &std::path::Path) -> Option<std::collections::HashSet<std::path::PathBuf>> {
    let mut live = std::collections::HashSet::new();
    for entry in std::fs::read_dir(profiles).ok()? {
        let entry = entry.ok()?;
        // `by-inputs` is the index itself, not a workspace. Any other non-directory is a stray
        // nothing creates today — but `read_link({file}/current)` fails with NotADirectory, which
        // isn't NotFound, so tolerating it here (rather than widening the error kinds below, which
        // would undo the strictness) is what keeps one stray file from disabling the sweep forever.
        if entry.file_name() == "by-inputs" || !entry.file_type().ok()?.is_dir() {
            continue;
        }
        match std::fs::read_link(entry.path().join("current")) {
            Ok(target) => {
                live.insert(target);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // benign: mid-build, covered by the age bound
            Err(_) => return None,
        }
    }
    Some(live)
}

/// Reclaims `{pool}/attach/{id}` directories a deleted workspace leaves behind. There is no
/// Workspace finalizer (see `crates/workspaces/src/api.rs`'s `delete_ws` — the Volume carries the
/// ownerReference and its own finalizer, so deleting a Workspace is pure garbage collection), so
/// nothing ever observes the delete to clean this up directly; this sweep is the actual mechanism.
/// `{pool}/vol/{id}`
/// exists for every live workspace and disappears with it (ownerReference -> Volume -> finalizer ->
/// subvolume gone), so an attach directory with no matching `vol/{id}` is an orphan.
///
/// The keep-set is ONE read of `vol/` — never a `Path::exists` probe per entry, because an
/// unreadable or unmounted `vol/` would then make every probe answer false and read as "nothing is
/// live", sweeping every attach directory on the pool. Bailing keep-biased on that read failing is
/// the same shape every other sweep here uses. Same age floor as the rest: a workspace mid-create
/// can have its attach directory written before the Volume shows up in `vol/`.
fn janitor_sweep_attach(pool: &std::path::Path, min_age: std::time::Duration) -> usize {
    let Ok(vol_entries) = std::fs::read_dir(pool.join("vol")) else { return 0 };
    let live: std::collections::HashSet<String> =
        vol_entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
    let mut swept = 0;
    let Ok(entries) = std::fs::read_dir(pool.join("attach")) else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        if live.contains(&id) || younger_than(&entry, min_age) {
            continue;
        }
        if std::fs::remove_dir_all(&p).is_ok() {
            swept += 1;
        }
    }
    swept
}

/// A shared home holds configs — dotfiles, keys, shell config — and nothing else: it is on the
/// region's NFS export, and therefore on S3, and it has no quota (the per-home btrfs qgroup went
/// away with the per-node home volume). This warning is its ONLY replacement, so the number is a
/// tripwire, not a limit: configs never come near 100 MB, and a home that does means a tool cache
/// escaped `login_env`'s redirection onto the node-local `homecache` volume and is now paying
/// network I/O and object-store bytes for something disposable.
///
/// Warns only — the janitor never deletes anything inside a person's home.
/// ponytail: a full recursive walk of every home each beat; if homes ever get big enough for that
/// to cost real IO, keep per-owner sizes from `btrfs qgroup`/`du --max-depth=1` instead.
fn warn_oversized_homes(pool: &std::path::Path) {
    for (owner, bytes) in oversized_homes(pool) {
        tracing::warn!(%owner, bytes, "agent: shared home far exceeds what configs need; a cache has escaped HOME_CACHE_DIR");
    }
}

/// The `(owner, bytes)` pairs `warn_oversized_homes` reports — split out so the threshold is
/// testable on a plain tmpdir, no NFS and no btrfs.
fn oversized_homes(pool: &std::path::Path) -> Vec<(String, u64)> {
    let Ok(entries) = std::fs::read_dir(crate::homes_root(pool.to_string_lossy().as_ref())) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let bytes = dir_bytes(&e.path());
            (bytes > HOME_WARN_BYTES).then(|| (e.file_name().to_string_lossy().into_owned(), bytes))
        })
        .collect()
}

/// See `warn_oversized_homes`: a tripwire on a home holding something other than configs.
const HOME_WARN_BYTES: u64 = 100 * 1024 * 1024;

/// The store size past which the janitor triggers a `nix-collect-garbage` sweep.
const NIX_GC_HIGH_BYTES: u64 = 60 * 1024 * 1024 * 1024;

/// Recursive size of `root`, best effort: an unreadable entry is skipped rather than failing the
/// whole scan, since a wrong number only costs an early or late GC or a missed warning, never
/// data. Uses
/// `DirEntry::file_type` (an `lstat`, not a `stat`) so it never follows a symlink — `/nix/store`
/// is full of symlinks between store paths, and following them would double-count shared files
/// and could cycle forever on a symlink back up the tree.
fn dir_bytes(root: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            total += dir_bytes(&entry.path());
        } else if ft.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
        // symlinks: skip — not real bytes owned by this dir, and following one risks a cycle.
    }
    total
}

/// True when `entry` is younger than `min_age`. An unreadable mtime counts as young: keeping a
/// file costs disk, deleting one costs data — the sweep never guesses in the delete direction.
fn younger_than(entry: &std::fs::DirEntry, min_age: std::time::Duration) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e < min_age).unwrap_or(true))
        .unwrap_or(true)
}

/// Shared floor for every age-gated sweep in this file: young enough to still be mid-operation is
/// presumed live, not garbage.
pub(crate) const SWEEP_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Every btrfs subvolume `dir` contains, deepest first — deleting a subvolume before its own
/// nested subvolumes (a worktree under an old-layout `live/`, say) fails, so this recurses into a
/// found subvolume before adding it, same "children before parents" order `btrfs subvolume delete
/// -R` would give a whole tree at once. A plain directory is walked through, not deleted; a
/// subvolume's own contents are never descended into beyond finding nested subvolumes — btrfs
/// deletes the rest with the subvolume itself.
fn subvolumes_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if is_subvolume(&p) {
            subvolumes_under(&p, out);
            out.push(p);
        } else {
            subvolumes_under(&p, out);
        }
    }
}

/// Full local reclaim for a deleted workspace/environment: every btrfs subvolume under
/// `{pool}/vol/{id}` (old layout's single `live`, or the commit model's `snap/*` and `live/*`
/// worktrees alike — `subvolumes_under` doesn't care which), then the directory itself. Registry/
/// blob bytes are never a concern here any more — the object store this used to also clean up
/// (`stage/`, `img/`) is gone with the subsystem that wrote it. Best-effort throughout (a warning,
/// never a panic): a retried delete must still finish even if a prior attempt got partway through.
pub fn cleanup_local(engine: &Engine, id: &str) {
    let voldir = engine.pool.voldir(id);
    let mut subvols = Vec::new();
    subvolumes_under(&voldir, &mut subvols);
    for p in &subvols {
        btrfs_delete(p, id);
    }
    if let Err(e) = std::fs::remove_dir_all(&voldir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%id, path = %voldir.display(), error = %e, "agent: cleanup: remove");
        }
    }
    let vol_root = engine.pool.root.join("vol");
    for ext in ["owner", "lock", "pushed-gen"] {
        let _ = std::fs::remove_file(vol_root.join(format!("{id}.{ext}")));
    }
}

fn btrfs_delete(path: &std::path::Path, id: &str) {
    match std::process::Command::new("btrfs").args(["subvolume", "delete", path.to_str().unwrap()]).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            %id,
            path = %path.display(),
            stderr = %String::from_utf8_lossy(&out.stderr),
            "agent: cleanup: btrfs subvolume delete"
        ),
        Err(e) => tracing::warn!(%id, path = %path.display(), error = %e, "agent: cleanup: btrfs subvolume delete"),
    }
}

#[cfg(test)]
mod janitor_tests {
    use super::*;
    use rustic_git_workspaces::engine::have_btrfs;
    use rustic_git_workspaces::engine::Pool;

    /// Mirrors `crates/workspaces/tests/engine_pool.rs`'s `LoopbackPool`: a truncated sparse
    /// btrfs image, mounted for the test and unmounted on drop.
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
            run(&["truncate", "-s", "1G", img.to_str().unwrap()]);
            run(&["mkfs.btrfs", "-q", img.to_str().unwrap()]);
            run(&["mount", "-o", "loop", img.to_str().unwrap(), mount.to_str().unwrap()]);
            let pool = Pool::new(mount.clone());
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

    fn bare_engine(pool_root: std::path::PathBuf) -> Engine {
        Engine::new(Pool::new(pool_root))
    }

    /// The mechanism Task 6's ruling replaced a (dead) Workspace finalizer branch with: no
    /// `vol/{id}` for the id at all (deleted, or never existed) and past the age floor is an
    /// orphan.
    #[test]
    fn attach_sweep_reclaims_an_old_orphan_with_no_matching_volume() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
        let dir = tmp.path().join("attach").join("ws-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("resolv.conf"), b"search env-abc.svc.").unwrap();

        assert_eq!(janitor_sweep_attach(tmp.path(), std::time::Duration::ZERO), 1);
        assert!(!dir.exists());
    }

    /// The age floor: a workspace mid-create can have its attach directory written before its
    /// Volume shows up under `vol/`, so a young orphan is presumed live, same as every other
    /// sweep's crash window.
    #[test]
    fn attach_sweep_spares_a_young_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
        let dir = tmp.path().join("attach").join("ws-1");
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(janitor_sweep_attach(tmp.path(), SWEEP_MIN_AGE), 0, "a young attach dir is presumed live");
        assert!(dir.exists());
    }

    /// The keep half: a `vol/{id}` still on the pool means the workspace is still live, however
    /// old its attach directory is.
    #[test]
    fn attach_sweep_keeps_a_directory_whose_workspace_is_still_live() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("attach").join("ws-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(tmp.path().join("vol").join("ws-1")).unwrap();

        assert_eq!(janitor_sweep_attach(tmp.path(), std::time::Duration::ZERO), 0, "the workspace is still live");
        assert!(dir.exists());
    }

    /// The keep-biased bail Finding 1 asked for: an unreadable (here, absent) `vol/` must never
    /// read as "nothing is live" — that would sweep every attach directory on the pool at once,
    /// live workspaces included, the moment the pool the check depends on is unmounted.
    #[test]
    fn attach_sweep_sweeps_nothing_when_vol_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        // No `vol/` at all — same failure shape as an unmounted or unreadable one.
        let dir = tmp.path().join("attach").join("ws-1");
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(janitor_sweep_attach(tmp.path(), std::time::Duration::ZERO), 0, "an unreadable vol/ keeps everything");
        assert!(dir.exists());
    }

    /// The sole replacement for the deleted per-home quota: a home holding more than configs is
    /// a cache that escaped the env redirection onto the node-local volume, and now costs NFS and
    /// S3 bytes. Warn-only, so the check is the list it warns from.
    #[test]
    fn the_home_size_alarm_fires_only_past_the_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let homes = tmp.path().join("homes");
        std::fs::create_dir_all(homes.join("alice")).unwrap();
        std::fs::create_dir_all(homes.join("bob")).unwrap();
        std::fs::write(homes.join("alice").join("gitconfig"), b"[user]").unwrap();
        std::fs::write(homes.join("bob").join("blob"), vec![0u8; HOME_WARN_BYTES as usize + 1]).unwrap();

        let over = oversized_homes(tmp.path());
        assert_eq!(over.len(), 1, "{over:?}");
        assert_eq!(over[0].0, "bob");
    }

    /// An entry no workspace's `current` resolves to, older than the bound, is reclaimable.
    #[test]
    fn the_profile_sweep_removes_old_unreferenced_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = root.join("store-x");
        std::fs::create_dir_all(&store).unwrap();
        nix::record_index(root, "orphan", &store).unwrap();
        assert_eq!(janitor_sweep_profiles(root, std::time::Duration::ZERO), 1);
        assert!(nix::indexed(root, "orphan").is_none());
    }

    /// An entry a live workspace points at is never swept, however old.
    #[test]
    fn the_profile_sweep_keeps_entries_a_workspace_uses() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = root.join("store-y");
        std::fs::create_dir_all(&store).unwrap();
        nix::record_index(root, "used", &store).unwrap();
        nix::link_profile(root, "ws-1", &store).unwrap();
        assert_eq!(janitor_sweep_profiles(root, std::time::Duration::ZERO), 0);
        assert!(nix::indexed(root, "used").is_some());
    }

    /// Keep-biased, like every other sweep: an unreadable directory reclaims nothing.
    #[test]
    fn the_profile_sweep_sweeps_nothing_when_the_directory_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(janitor_sweep_profiles(&tmp.path().join("missing"), std::time::Duration::ZERO), 0);
    }

    /// The age bound is what covers the benign "mid-build" gap in the keep-set — pin it with a
    /// non-zero bound instead of only ever exercising `Duration::ZERO`.
    #[test]
    fn the_profile_sweep_spares_a_young_unreferenced_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = root.join("store-z");
        std::fs::create_dir_all(&store).unwrap();
        nix::record_index(root, "fresh-orphan", &store).unwrap();
        assert_eq!(janitor_sweep_profiles(root, SWEEP_MIN_AGE), 0, "a young orphan is presumed live");
        assert!(nix::indexed(root, "fresh-orphan").is_some());
    }

    /// A stray file under the profiles root must not disable the sweep: `read_link({file}/current)`
    /// answers NotADirectory, and treating that as "the keep-set might be incomplete" would bail
    /// on every beat from then on, silently and forever.
    #[test]
    fn the_profile_sweep_survives_a_stray_file_under_the_profiles_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = root.join("store-s");
        std::fs::create_dir_all(&store).unwrap();
        nix::record_index(root, "orphan", &store).unwrap();
        std::fs::write(root.join("notes.txt"), b"").unwrap();
        assert_eq!(janitor_sweep_profiles(root, std::time::Duration::ZERO), 1);
        assert!(nix::indexed(root, "orphan").is_none());
    }

    /// `cleanup_local` must reach a nested worktree subvolume, not just the voldir's own top-level
    /// `live` — the commit model's `live/{ws}` layout, reproduced here without a full agent: two
    /// nested subvolumes (`snap/c1`, `live/ws1`) under one voldir, both gone afterward.
    #[test]
    fn cleanup_local_deletes_nested_commit_model_subvolumes() {
        if !have_btrfs() {
            eprintln!("skipping: btrfs unavailable or not root");
            return;
        }
        let lp = LoopbackPool::new();
        let engine = bare_engine(lp.pool.root.clone());
        std::fs::create_dir_all(engine.pool.snap_dir("v1")).unwrap();
        std::fs::create_dir_all(engine.pool.voldir("v1").join("live")).unwrap();
        run(&["btrfs", "subvolume", "create", engine.pool.snap("v1", "c1").to_str().unwrap()]);
        run(&["btrfs", "subvolume", "create", engine.pool.worktree("v1", "ws1").to_str().unwrap()]);

        cleanup_local(&engine, "v1");
        assert!(!engine.pool.voldir("v1").exists());
    }
}

#[cfg(test)]
mod nix_gc_tests {
    use super::*;

    #[test]
    fn the_store_walk_does_not_follow_symlinks_and_terminates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nested = root.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("f"), vec![0u8; 100]).unwrap();
        // A symlink back up at the root — following it would double-count `f` and, on a deeper
        // tree, cycle forever.
        std::os::unix::fs::symlink(root, nested.join("up")).unwrap();
        assert_eq!(dir_bytes(root), 100);
    }
}
