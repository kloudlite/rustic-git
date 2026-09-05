//! Engine operations that predate the snapshot model and still stand: subvolume creation, quota
//! enforcement, generation reads, and the LOCAL-FIRST clone (`clone_local_ids`) that `clone_env`
//! still uses (environments were deliberately left out of the snapshot model's shared-worktree
//! change — see `crates/workspaces/src/crd.rs`'s `VolumeSource::CloneOf` doc). Push, pull, squash
//! and the object-store lineage they built on are gone; snapshot/checkout live in `snapshot.rs` now.

use crate::engine::{Pool, ws_lock};

#[derive(Debug)]
pub struct EngErr(pub String);

/// A restore whose snapshot id has no record behind it. Named because the agent classifies it as a
/// PERMANENT failure — a missing snapshot is an answer, not an outage, and retrying it once a
/// minute forever only fills the log. Still produced by `snapshot.rs::checkout` for a named
/// snapshot that doesn't exist locally.
pub const NO_SUCH_RECORD: &str = "snapshot record not found";

impl std::fmt::Display for EngErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for EngErr {}
impl From<String> for EngErr {
    fn from(s: String) -> Self {
        EngErr(s)
    }
}
impl EngErr {
    pub(crate) fn io(e: std::io::Error) -> Self {
        EngErr(e.to_string())
    }
    pub(crate) fn other(s: impl Into<String>) -> Self {
        EngErr(s.into())
    }
}

pub(crate) fn run(argv: &[&str]) -> Result<(), EngErr> {
    let out = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| EngErr::other(format!("spawn {}: {e}", argv[0])))?;
    if !out.status.success() {
        return Err(EngErr::other(format!("{argv:?}: {}", String::from_utf8_lossy(&out.stderr))));
    }
    Ok(())
}

/// A btrfs subvolume's root directory always has inode number 256 — the cheapest correct way to
/// tell "this path is a subvolume" from "this path is a plain directory a subvolume happens to
/// contain" without shelling out to `btrfs subvolume show` (which errors on a non-subvolume path
/// anyway, making it no cheaper). A path that doesn't exist reads as "not a subvolume", not an
/// error: the caller's existence check runs separately.
pub fn is_subvolume(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).is_ok_and(|m| m.ino() == 256)
}

/// The `Generation:` line of `btrfs subvolume show`. Split from the command so the parse has a test
/// that runs where btrfs does not.
pub fn parse_generation(subvolume_show: &str) -> Option<u64> {
    subvolume_show
        .lines()
        .find_map(|l| l.trim().strip_prefix("Generation:"))
        .and_then(|g| g.trim().parse().ok())
}

pub struct Engine {
    pub pool: Pool,
    /// Sampled once at construction, not re-probed per call: a degradation (no btrfs on PATH, not
    /// root) is a fact about this process's whole lifetime, not a per-reconcile coin flip, and a
    /// field makes it a state `ensure_homecache` reads rather than a probe it could answer
    /// differently between its own existence check and its create call.
    has_btrfs: bool,
}

impl Engine {
    pub fn new(pool: Pool) -> Engine {
        let has_btrfs = super::have_btrfs();
        Engine { pool, has_btrfs }
    }

    /// Bare `{pool}/vol/{id}/live` subvolume creation — shared by `init` (a workspace) and
    /// `EnvUp`'s first-ever-mount path (an environment).
    pub fn create_subvol(&self, id: &str) -> Result<(), EngErr> {
        std::fs::create_dir_all(self.pool.voldir(id)).map_err(EngErr::io)?;
        // A voldir that already holds snapshots is a REPLICA this node pulled, not a volume being
        // born: its worktree must come from `checkout` of the newest sync point, never from an
        // empty `live` that `migrate_volume` would then promote into the worktree slot. That
        // exact sequence re-hosted a taken-over workspace as an empty directory beside a stop
        // snapshot holding its files.
        let has_history = std::fs::read_dir(self.pool.snap_dir(id)).map(|mut d| d.next().is_some()).unwrap_or(false);
        if has_history {
            return Ok(());
        }
        // Reconcile is level-triggered and a restarted controller replays it from scratch, so an
        // existing `live` is the expected steady state, not a conflict. Keep-biased: never delete
        // and recreate — that would be data loss dressed up as convergence.
        if !self.pool.live(id).exists() {
            run(&["btrfs", "subvolume", "create", self.pool.live(id).to_str().unwrap()])?;
        }
        Ok(())
    }

    /// `{pool}/homecache/{owner}`: the node-local half of the shared-home split (spec
    /// 2026-09-01) — caches (editor servers, package manager state) that must never leave this
    /// node and are disposable by contract, so no qgroup limit here (unlike `set_quota`, which
    /// caps volumes that ARE the tenant's durable data). A btrfs subvolume, not a plain dir, so
    /// deleting it later is one `btrfs subvolume delete` instead of a `rm -rf` racing writers.
    pub fn ensure_homecache(&self, owner: &str, uid: u32) -> Result<(), EngErr> {
        let root = self.pool.root.join("homecache").join(owner);
        std::fs::create_dir_all(root.parent().unwrap()).map_err(EngErr::io)?;
        // ponytail: the agent is root-only and btrfs is always present in production (it's a
        // privileged DaemonSet, see CLAUDE.md); a dev/test pool gets a plain directory instead of
        // a subvolume so the reconcile loop converges without either — this function's real
        // subvolume path is exercised by `engine_ops.rs`'s btrfs-gated test. Loud, not silent: a
        // production node that ever took this branch (btrfs missing from PATH, a bad image) needs
        // to show up in logs, because the `is_subvolume` guard below is itself skipped in that
        // state and would otherwise let a plain dir sit there unnoticed.
        if !root.exists() {
            if self.has_btrfs {
                run(&["btrfs", "subvolume", "create", root.to_str().unwrap()])?;
            } else {
                tracing::warn!(path = %root.display(), reason = "no-btrfs-root", "homecache.not_subvolume");
                std::fs::create_dir(&root).map_err(EngErr::io)?;
            }
        } else if self.has_btrfs && !is_subvolume(&root) {
            // Only reachable once btrfs/root come back after a node ran this reconcile without
            // them (the branch above just warned and left a plain dir) — self-heal is a `rm -rf`,
            // never an automatic convert, because the cache is disposable by contract but this
            // function does not know if it is mid-write.
            return Err(EngErr::other(format!(
                "{}: exists but is not a btrfs subvolume (left by a reconcile without btrfs/root); \
                 disposable by contract — rm -rf it and this reconcile will recreate it",
                root.display()
            )));
        }
        let chown_ok = unsafe { libc::geteuid() } == 0;
        for d in ["cache", "vscode-server", "cursor-server", "state"] {
            let p = root.join(d);
            std::fs::create_dir_all(&p).map_err(EngErr::io)?;
            if chown_ok {
                std::os::unix::fs::chown(&p, Some(uid), Some(uid)).map_err(EngErr::io)?;
            }
        }
        Ok(())
    }

    /// Cap `id`'s live subvolume at `quota_gb` with a btrfs qgroup limit — the only thing that
    /// stops one tenant writing the whole pool to ENOSPC and taking every sibling down with it.
    /// Per SUBVOLUME, so it has to be re-applied whenever `live` is a new subvolume, not only at
    /// create.
    ///
    /// `Ok(Some(why))` is "the pool cannot enforce this": qgroups are enabled per filesystem
    /// (`btrfs quota enable`, see `deploy/k3s/format-pool.sh`) and a pool formatted before that
    /// line existed has none. That is not the volume's fault, so it is not an `Err` — the caller
    /// surfaces it as a condition and the volume stays usable, unenforced, until an operator
    /// enables quotas on the pool. Level-triggered: the next reconcile re-applies.
    /// btrfs qgroups are per-SUBVOLUME, and `live` is a directory of worktree subvolumes
    /// (`Pool::worktree`) once a volume is migrated, not one subvolume — there is no single tree
    /// to limit any more. RULING (see `docs/superpowers/specs/2026-09-03-durable-snapshots-design.md`):
    /// apply the volume's
    /// `quota_gb` to EACH worktree subvolume individually, same number per tree. Not billing-exact
    /// for shared extents across worktrees of the same volume (CoW shares don't double-count in a
    /// qgroup's exclusive counter anyway, so this undercounts if anything), but it caps runaway
    /// growth from any one worktree, which is what the limit is for.
    pub fn set_quota(&self, id: &str, quota_gb: u64) -> Result<Option<String>, EngErr> {
        let live = self.pool.live(id);
        // Mixed-state pool: a volume can be not yet MIGRATED — `live` is still the old single
        // subvolume, not a directory of worktrees. Descending into it as a worktree directory
        // would `read_dir` straight into the user's own files and qgroup-limit each one, which
        // fails (a plain file/dir is not a subvolume) into "unenforced" — silently uncapping the
        // volume. The layout is decided by what's actually on disk: `is_subvolume` is the same
        // "root inode 256" check btrfs itself uses.
        if !is_subvolume(&live) {
            return self.set_quota_worktrees(id, quota_gb);
        }
        if !live.exists() {
            return Err(EngErr::other(format!("{}: no live subvolume to limit", live.display())));
        }
        let limit = if quota_gb == 0 { "none".to_string() } else { format!("{quota_gb}G") };
        Ok(run(&["btrfs", "qgroup", "limit", &limit, live.to_str().unwrap()]).err().map(|e| e.0))
    }

    /// Limit every worktree subvolume this pool currently has checked out for `id`
    /// (`{pool}/vol/{id}/live/*`) to `quota_gb`. No worktrees yet (volume Ready before any
    /// workspace has checked one out) is not an error — nothing to limit, and the checkout path
    /// applies the same limit to a worktree the moment it exists.
    fn set_quota_worktrees(&self, id: &str, quota_gb: u64) -> Result<Option<String>, EngErr> {
        let live_dir = self.pool.voldir(id).join("live");
        let entries = match std::fs::read_dir(&live_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(EngErr::io(e)),
        };
        let limit = if quota_gb == 0 { "none".to_string() } else { format!("{quota_gb}G") };
        let mut first_failure = None;
        for entry in entries {
            let entry = entry.map_err(EngErr::io)?;
            // `swap_worktree`'s dot-prefixed staging/backup subvolumes are worktree-SHAPED but not
            // worktrees — quota-limiting them is harmless but pointless, and counting them here is
            // how a crash-recovery leftover would otherwise get treated as a real worktree.
            if entry.file_name().to_str().is_some_and(|n| n.starts_with('.')) {
                continue;
            }
            if let Err(e) = run(&["btrfs", "qgroup", "limit", &limit, entry.path().to_str().unwrap()]) {
                first_failure.get_or_insert(e.0);
            }
        }
        Ok(first_failure)
    }

    /// Same limit as `set_quota_worktrees`, applied to exactly ONE worktree — called right after
    /// `checkout` so a freshly created worktree is never briefly unquota'd while it waits for the
    /// volume's next reconcile pass.
    pub fn set_quota_worktree(&self, volume: &str, ws: &str, quota_gb: u64) -> Result<Option<String>, EngErr> {
        let path = self.pool.worktree(volume, ws);
        let limit = if quota_gb == 0 { "none".to_string() } else { format!("{quota_gb}G") };
        Ok(run(&["btrfs", "qgroup", "limit", &limit, path.to_str().unwrap()]).err().map(|e| e.0))
    }

    /// Commit the pool's open transaction. `generation` reads the COMMITTED number, and btrfs
    /// commits on its own only every ~30s — so a read without this can miss a write made just
    /// before it. `snapshot::snapshot_worktree` calls this before every snapshot for the same reason.
    pub fn sync_pool(&self) -> Result<(), EngErr> {
        run(&["btrfs", "filesystem", "sync", self.pool.root.to_str().unwrap()])
    }

    /// The btrfs generation of a worktree subvolume: a counter the filesystem bumps on every
    /// committed transaction that touched it, so "has anything changed since the last sync point"
    /// is one `subvolume show` rather than a walk of the tree. Reads the WORKTREE path
    /// (`pool.worktree`), not `pool.live(id)` — under the snapshot model `live` is a directory of
    /// per-worktree subvolumes, not a subvolume itself.
    pub fn generation(&self, volume: &str, ws: &str) -> Result<u64, EngErr> {
        self.generation_of(&self.pool.worktree(volume, ws))
    }

    fn generation_of(&self, subvol: &std::path::Path) -> Result<u64, EngErr> {
        let out = std::process::Command::new("btrfs")
            .args(["subvolume", "show", subvol.to_str().unwrap()])
            .output()
            .map_err(EngErr::io)?;
        if !out.status.success() {
            return Err(EngErr::other(format!(
                "btrfs subvolume show {}: {}",
                subvol.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        parse_generation(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| EngErr::other(format!("btrfs subvolume show {}: no Generation line", subvol.display())))
    }

    /// LOCAL-FIRST clone: snapshot `src_id`'s current live subvolume straight into `dst_id`'s —
    /// no registry, no lineage, works even on a source that was never pushed/snapshotted at all.
    /// The one caller left is `clone_env` (`api.rs`), which deliberately still copies bytes into a
    /// fresh child `Volume` rather than sharing a worktree the way a snapshot-model workspace clone
    /// does. `src_id` not materialized on this pool (or not on this node at all, cross-pool) is a
    /// plain error now — the registry fallback that used to fetch it from elsewhere is gone.
    pub async fn clone_local_ids(&self, src_id: &str, dst_id: &str) -> Result<(), EngErr> {
        if !self.pool.live(src_id).exists() {
            return Err(EngErr::other(format!("clone source {src_id} is not materialized on this node")));
        }
        let _lock = ws_lock(&self.pool, src_id).map_err(EngErr::other)?;
        let src = self.clone_source(src_id);
        std::fs::create_dir_all(self.pool.voldir(dst_id)).map_err(EngErr::io)?;
        // A replayed reconcile must converge, not fail: `dst` already existing means a previous
        // attempt got this far. Keep it — see `create_subvol`.
        if !self.pool.live(dst_id).exists() {
            run(&["btrfs", "subvolume", "snapshot", src.to_str().unwrap(), self.pool.live(dst_id).to_str().unwrap()])?;
        }
        Ok(())
    }

    /// The subvolume a local clone copies: the source's own worktree, `live/{id}`, once the
    /// volume is in the snapshot model — where `live/` is a plain DIRECTORY of worktrees and
    /// `btrfs subvolume snapshot` of it answers "Not a Btrfs subvolume" — and `live` itself only
    /// for a volume still in the single-subvolume shape. Every environment clone failed on the
    /// first form until the probe caught it.
    fn clone_source(&self, src_id: &str) -> std::path::PathBuf {
        let worktree = self.pool.worktree(src_id, src_id);
        if worktree.exists() {
            worktree
        } else {
            self.pool.live(src_id)
        }
    }

    /// Seed a fresh volume from a LOCAL read-only snapshot of another one: the materialize step of
    /// `VolumeSource::SeededFrom`. Same btrfs snapshot `clone_local_ids` takes, from `snap/{name}`
    /// instead of `live` — the source's node is down, so its `live` is somewhere else entirely and
    /// the only bytes here are the copy this node pulled.
    ///
    /// `NO_SUCH_RECORD` when the snapshot is not held here: the claim admits only a node whose
    /// replica names it, so a missing one means the claim and the disk disagree — permanent (see
    /// `permanent_reason`), never a retry against a path that will not appear.
    pub async fn seed_from_snapshot(&self, src_volume: &str, snapshot: &str, dst_id: &str) -> Result<(), EngErr> {
        // Lock BEFORE the existence check, as `clone_local_ids` does: retention deleting the cut
        // between a passing check and the snapshot turns a clean `NO_SUCH_RECORD` into an opaque
        // btrfs failure the reconciler reads as transient and retries forever. `ws_lock`'s file
        // lives under the source's voldir, so that has to exist before we can lock — same opening
        // line `checkout` has, and it creates nothing for the destination.
        std::fs::create_dir_all(self.pool.voldir(src_volume)).map_err(EngErr::io)?;
        let _lock = ws_lock(&self.pool, src_volume).map_err(EngErr::other)?;
        let src = self.pool.snap(src_volume, snapshot);
        if !src.exists() {
            return Err(EngErr::other(NO_SUCH_RECORD));
        }
        std::fs::create_dir_all(self.pool.voldir(dst_id)).map_err(EngErr::io)?;
        // Converge on a replayed reconcile, exactly as `clone_local_ids` does.
        if !self.pool.live(dst_id).exists() {
            run(&["btrfs", "subvolume", "snapshot", src.to_str().unwrap(), self.pool.live(dst_id).to_str().unwrap()])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(root: &std::path::Path) -> Engine {
        Engine::new(Pool::new(root))
    }

    /// The degradation path: forced via the private field rather than gated on the real
    /// `have_btrfs()`, so this runs (and proves the fallback works) on every machine, not just a
    /// btrfs box — the real subvolume path is `engine_ops.rs`'s `have_btrfs()`-gated test.
    #[test]
    fn ensure_homecache_falls_back_to_a_plain_dir_without_btrfs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut e = engine(tmp.path());
        e.has_btrfs = false;
        e.ensure_homecache("alice", 12345).unwrap();
        let root = e.pool.root.join("homecache/alice");
        assert!(root.is_dir());
        for d in ["cache", "vscode-server", "cursor-server", "state"] {
            assert!(root.join(d).is_dir(), "{d}");
        }
    }

    /// The self-heal case the review flagged: a plain dir left by a degraded reconcile must not
    /// wedge forever once btrfs/root come back — the error has to name the fix.
    #[test]
    fn ensure_homecache_names_the_remedy_once_a_plain_dir_meets_real_btrfs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut e = engine(tmp.path());
        e.has_btrfs = false;
        e.ensure_homecache("alice", 12345).unwrap(); // leaves a plain dir, as above
        e.has_btrfs = true; // the node's btrfs/root came back
        let err = e.ensure_homecache("alice", 12345).unwrap_err();
        assert!(err.0.contains("rm -rf"), "{}", err.0);
    }

    #[test]
    fn parse_generation_reads_the_generation_line() {
        let show = "vol/ws-1/live/ws-1\n\tName: ws-1\n\tGeneration: 10197\n\tGen at creation: 4\n";
        assert_eq!(parse_generation(show), Some(10197));
        assert_eq!(parse_generation("no such line"), None);
    }

    #[test]
    fn a_clone_copies_the_worktree_when_live_is_a_directory_of_them() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        std::fs::create_dir_all(e.pool.worktree("env-a", "env-a")).unwrap();
        assert_eq!(e.clone_source("env-a"), e.pool.worktree("env-a", "env-a"));
        std::fs::create_dir_all(e.pool.live("ws-old")).unwrap();
        assert_eq!(e.clone_source("ws-old"), e.pool.live("ws-old"));
    }

    #[test]
    fn clone_local_ids_refuses_a_source_not_materialized_here() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        let err = futures::executor::block_on(e.clone_local_ids("nope", "dst")).unwrap_err();
        assert!(err.to_string().contains("not materialized"));
    }

    /// The claim only admits a node whose replica names the cut, so "not held here" means the
    /// claim and the disk disagree — permanent, not a retry. Checked BEFORE any btrfs call, which
    /// is what lets this run off a btrfs box.
    #[test]
    fn seed_from_snapshot_refuses_a_snapshot_this_node_does_not_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        let err = futures::executor::block_on(e.seed_from_snapshot("vol-src", "sync-ws-1-aaaa", "ws-new")).unwrap_err();
        assert!(err.to_string().contains(NO_SUCH_RECORD), "{err}");
        assert!(!e.pool.voldir("ws-new").exists(), "a refused seed creates nothing");
    }
}
