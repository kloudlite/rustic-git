//! Engine operations that predate the commit model and still stand: subvolume creation, quota
//! enforcement, generation reads, and the LOCAL-FIRST clone (`clone_local_ids`) that `clone_env`
//! still uses (environments were deliberately left out of the commit model's shared-worktree
//! change — see `crates/workspaces/src/crd.rs`'s `VolumeSource::CloneOf` doc). Push, pull, squash
//! and the object-store lineage they built on are gone; commit/checkout live in `commit.rs` now.

use crate::engine::{Pool, ws_lock};

#[derive(Debug)]
pub struct EngErr(pub String);

/// A restore whose snapshot id has no record behind it. Named because the agent classifies it as a
/// PERMANENT failure — a missing commit is an answer, not an outage, and retrying it once a minute
/// forever only fills the log. Still produced by `commit.rs::checkout` for a named commit that
/// doesn't exist locally.
pub const NO_SUCH_RECORD: &str = "commit record not found";

/// A reconcile that finds a pre-cutover `VolumeSource::RestoreOf` on a stored spec: the mechanism
/// it named (a registry fetch into a fresh volume) is gone as of Task 8 — restore-to-new is now
/// `CloneOf{volume, commit: Some(id)}`, written only by `/v1` from here on. PERMANENT, not
/// retried: no amount of waiting fetches from a registry that no longer exists. The fix is to
/// re-issue the restore from the API, which writes the new shape.
pub const RESTORE_OF_GONE: &str = "restore-to-new via the object-store registry is gone; re-issue the restore";

/// A home whose newest Ready commit is not yet on this node's disk — the replica pull beat (7c)
/// is behind, not absent; TRANSIENT, requeued at RETRY like any other in-flight condition. Never
/// bootstrap empty here: an empty worktree checked out next to real history is the never-started-
/// dataless bug, and it would go on to be committed and replicated as if it were real.
pub const HOME_AWAITING_SYNC: &str = "home commit not yet replicated to this node";

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
}

impl Engine {
    pub fn new(pool: Pool) -> Engine {
        Engine { pool }
    }

    /// Bare `{pool}/vol/{id}/live` subvolume creation — shared by `init` (a workspace) and
    /// `EnvUp`'s first-ever-mount path (an environment).
    pub fn create_subvol(&self, id: &str) -> Result<(), EngErr> {
        std::fs::create_dir_all(self.pool.voldir(id)).map_err(EngErr::io)?;
        // Reconcile is level-triggered and a restarted controller replays it from scratch, so an
        // existing `live` is the expected steady state, not a conflict. Keep-biased: never delete
        // and recreate — that would be data loss dressed up as convergence.
        if !self.pool.live(id).exists() {
            run(&["btrfs", "subvolume", "create", self.pool.live(id).to_str().unwrap()])?;
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
    /// to limit any more. RULING (Task 7a, the spec's open quota question): apply the volume's
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

    /// The nested subvolumes that keep a home's caches out of every push and out of its quota
    /// (`k8s::HOME_LOCAL_DIRS`). Run after every path that leaves a new `live` behind — because a
    /// received/cloned tree carries no trace of them: without this `.cache` comes back as nothing
    /// at all and the next `npm install` writes it INTO the home.
    ///
    /// Keep-biased: an entry that already exists — as a subvolume, or as a plain directory the
    /// person made themselves — is left exactly as it is. Every directory made here is chowned to
    /// the owner, parents included: root-made `~/.cargo` is a `mkdir ~/.cargo/x: Permission denied`
    /// for the person the home belongs to.
    pub fn ensure_home_dirs(&self, id: &str, uid: u32) -> Result<(), EngErr> {
        // Same mixed-layout decision as `set_quota`: a migrated home's real $HOME is the
        // worktree (`Pool::worktree`), and `live` names the directory OF worktrees, not the
        // subvolume any more — creating the caches under `live` itself would put plain
        // directories INSIDE the snapshotted worktree tree, so every commit/send would carry
        // them and `homeQuotaGb` would count them. A not-yet-migrated home is still the old
        // single subvolume at `live`, so that stays the target there.
        let old_live = self.pool.live(id);
        let live = if is_subvolume(&old_live) { old_live } else { self.pool.worktree(id, id) };
        for rel in crate::k8s::HOME_LOCAL_DIRS {
            let p = live.join(rel);
            if p.exists() {
                continue;
            }
            let mut made = Vec::new();
            let mut d = p.parent().map(std::path::Path::to_path_buf).unwrap_or_else(|| live.clone());
            while d != live && !d.exists() {
                made.push(d.clone());
                d = d.parent().map(std::path::Path::to_path_buf).unwrap_or_else(|| live.clone());
            }
            for d in made.iter().rev() {
                std::fs::create_dir(d).map_err(EngErr::io)?;
                std::os::unix::fs::chown(d, Some(uid), Some(uid)).map_err(EngErr::io)?;
            }
            run(&["btrfs", "subvolume", "create", p.to_str().unwrap()])?;
            std::os::unix::fs::chown(&p, Some(uid), Some(uid)).map_err(EngErr::io)?;
        }
        Ok(())
    }

    /// The btrfs generation of `id`'s live subvolume: a counter the filesystem bumps on every
    /// committed transaction that touched it, so "has anything changed since the last push" is one
    /// `subvolume show` rather than a walk of the tree.
    pub fn generation(&self, id: &str) -> Result<u64, EngErr> {
        self.generation_of(&self.pool.live(id))
    }

    /// The generation of a commit's own snapshot — `layer` is the commit name under `snap/`.
    /// This, not the live subvolume read after the cut, is what the home beat records: the
    /// snapshot holds everything committed up to its own transaction, so "live is past it" is
    /// exactly "something changed since the commit", whether the snapshot's transaction moved the
    /// live generation (it does when the root item is rewritten) or not. Reading live afterwards
    /// folds any write that landed between the snapshot and the read into the recorded number.
    pub fn pushed_generation(&self, id: &str, layer: &str) -> Result<u64, EngErr> {
        self.generation_of(&self.pool.snap(id, layer))
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

    /// Commit the pool's open transaction. `generation`/`pushed_generation` read the COMMITTED
    /// number, and btrfs commits on its own only every ~30s — so a beat that reads without this
    /// can miss a write made just before it. `commit::commit_worktree` calls this before every
    /// snapshot for the same reason. One call per beat, not per home.
    pub fn sync_pool(&self) -> Result<(), EngErr> {
        run(&["btrfs", "filesystem", "sync", self.pool.root.to_str().unwrap()])
    }

    /// LOCAL-FIRST clone: snapshot `src_id`'s current live subvolume straight into `dst_id`'s —
    /// no registry, no lineage, works even on a source that was never pushed/committed at all.
    /// The one caller left is `clone_env` (`api.rs`), which deliberately still copies bytes into a
    /// fresh child `Volume` rather than sharing a worktree the way a commit-model workspace clone
    /// does. `src_id` not materialized on this pool (or not on this node at all, cross-pool) is a
    /// plain error now — the registry fallback that used to fetch it from elsewhere is gone.
    pub async fn clone_local_ids(&self, src_id: &str, dst_id: &str) -> Result<(), EngErr> {
        if !self.pool.live(src_id).exists() {
            return Err(EngErr::other(format!("clone source {src_id} is not materialized on this node")));
        }
        let _lock = ws_lock(&self.pool, src_id).map_err(EngErr::other)?;
        let src = self.pool.live(src_id);
        std::fs::create_dir_all(self.pool.voldir(dst_id)).map_err(EngErr::io)?;
        // A replayed reconcile must converge, not fail: `dst` already existing means a previous
        // attempt got this far. Keep it — see `create_subvol`.
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

    #[test]
    fn the_generation_is_read_off_subvolume_show() {
        let out = "vol/home-alice/live\n\tName: \t\t\tlive\n\tUUID: \t\t\t1234\n\tCreation time: \t\t2026-08-29 10:00:00 +0000\n\tSubvolume ID: \t\t257\n\tGeneration: \t\t4711\n\tGen at creation: \t7\n\tFlags: \t\t\t-\n";
        assert_eq!(super::parse_generation(out), Some(4711));
        assert_eq!(super::parse_generation("nothing here"), None);
    }

    #[test]
    fn clone_local_ids_refuses_a_source_not_materialized_here() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        let err = futures::executor::block_on(e.clone_local_ids("nope", "dst")).unwrap_err();
        assert!(err.to_string().contains("not materialized"));
    }
}
