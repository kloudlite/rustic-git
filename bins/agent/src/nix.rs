//! The agent's one Nix client: builds a workspace's profile through the host daemon, publishes it
//! by rename, and collects garbage. Behind a trait so the reconciler is tested with a fake — a
//! real `nix` needs a daemon and a store, which a unit test must not.
//!
//! The binary comes from the HOST store (`/nix/var/nix/profiles/default/bin`, seeded by the
//! DaemonSet's init container from the `nixos/nix` image), not from the agent image: a `nix` that
//! lives outside the store it talks to cannot exist, and shipping a second store just to hold the
//! client is what the seed step avoids.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const PROFILES_DIR: &str = "/nix/var/kloudlite/profiles";

/// The only substituter a profile build may use. A locked package is a store path somebody else
/// already built; if the public cache does not have it we say so and stop, because the fallback —
/// building it from source on a workspace node — is hours of CPU nobody asked for.
pub const CACHE: &str = "https://cache.nixos.org";

/// The marker `copy_from_cache` puts in front of its error when the substituter simply has no such
/// path, so the reconciler can report `NotCached` rather than a build failure.
pub const NOT_CACHED: &str = "NotCached";

// The root is always passed in (`Ctx::profiles_dir`) rather than read from a global: a process-wide
// override is a test that can reach the node's real /nix, and one that races every other test.
//
// A workspace's profile is a DIRECTORY holding `current` (and, mid-build, `current.building`), not
// a bare link: the pod mounts the directory by subPath, and the kubelet resolves a subPath ONCE at
// container start — a subPath that IS the link would freeze the pod on the profile it started
// with, so the live swap has to happen one level below what is mounted.
pub fn profile_dir(root: &Path, id: &str) -> PathBuf { root.join(id) }
pub fn profile_path(root: &Path, id: &str) -> PathBuf { profile_dir(root, id).join("current") }
pub fn building_path(root: &Path, id: &str) -> PathBuf { profile_dir(root, id).join("current.building") }

/// The one GC root for every profile on this node: an indirect root under `gcroots`, pointing at
/// the profiles dir. `nix build --no-link` registers nothing, and the auto-root a `-o` out-link
/// gets is orphaned the moment we rename over it — without this the live profile is collectable.
pub fn ensure_gcroot() {
    let gcroots = Path::new("/nix/var/nix/gcroots");
    if !gcroots.is_dir() {
        tracing::warn!(reason = "no-gcroots-dir", "nix.gcroot.missing");
        return;
    }
    let link = gcroots.join("kloudlite-profiles");
    if std::fs::read_link(&link).is_ok() {
        return;
    }
    if let Err(e) = std::os::unix::fs::symlink(PROFILES_DIR, &link) {
        tracing::warn!(error = %e, "nix.gcroot.failed");
    }
}

/// `WS_NIXPKGS` must be a nixpkgs flake ref pinned to a full revision: a branch ref would make two
/// nodes (or two days) build different profiles for the same hash, which is the one thing the hash
/// promises cannot happen.
pub fn valid_pin(pin: &str) -> bool {
    let Some(rev) = pin.strip_prefix("github:NixOS/nixpkgs/") else { return false };
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What every workspace on this node gets before its own list: the tools a shell session assumes
/// exist (`git` above all — a workspace is a checkout). `WS_BASE_PACKAGES`, whitespace-separated.
/// SET IN PRODUCTION: `deploy/k3s/agent-daemonset.yaml` passes it, so the constant below is the
/// fallback, not the value the cluster runs. Prepended, never written into `spec.packages`, so it
/// stays the platform's to change and a person cannot remove it from one workspace.
pub const DEFAULT_BASE_PACKAGES: &str =
    "bashInteractive zsh fish starship coreutils git openssh curl less which gnugrep gnused findutils";

pub fn base_packages(settings: &crate::controller::Settings) -> Vec<String> {
    settings.load().base_packages.split_whitespace().map(str::to_string).collect()
}

pub fn nixpkgs_pin(settings: &crate::controller::Settings) -> String {
    settings.load().nixpkgs.clone()
}

/// Env-only: read at boot, before `Ctx` (and so before `LiveSettings`) exists, to validate
/// `WS_NIXPKGS` ahead of anything that would build with it. The live handle's own value comes
/// from `AgentSettings::from_env()` seeded with the same read, so this and `nixpkgs_pin` never
/// disagree at the moment the process starts.
pub fn nixpkgs_pin_env() -> String {
    std::env::var("WS_NIXPKGS").unwrap_or_default()
}

pub fn build_timeout(settings: &crate::controller::Settings) -> Duration {
    Duration::from_secs(settings.load().nix_timeout_secs)
}

#[async_trait::async_trait]
pub trait Nix: Send + Sync {
    /// `nix build --expr <expr> --no-link --print-out-paths`; the store path it realised. No
    /// out-link, because the caller makes the symlink and renames it into place itself.
    async fn build(&self, expr: &str, timeout: Duration) -> Result<PathBuf, String>;
    /// `nix copy --from <cache> <store path>`: pull one already-built path into this node's store.
    /// `Err` starting with `NOT_CACHED` means the substituter does not have it — a different thing
    /// from a broken daemon or a timeout, and the only one a person can act on.
    async fn copy_from_cache(&self, store_path: &str, timeout: Duration) -> Result<(), String>;
    /// `nix eval` of `<rev>#<attr_path>`'s `outPath`: what a MIRROR lock (one with no store path
    /// of its own) resolves to. Its own step so the path can be copied from the cache like any
    /// other lock — an expression that evaluated the foreign nixpkgs at build time would instead
    /// build whatever it found there from source.
    async fn eval_out_path(&self, rev: &str, attr_path: &str, timeout: Duration) -> Result<String, String>;
    /// `nix store ping`.
    async fn ping(&self) -> Result<(), String>;
    /// `nix-collect-garbage`; returns bytes freed as nix reports them (0 if unparseable).
    async fn collect_garbage(&self) -> Result<u64, String>;
}

pub struct RealNix {
    pub bin: PathBuf,
}

impl RealNix {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(self.bin.join("nix"));
        c.args(args)
            .env("NIX_REMOTE", "daemon")
            .env("NIX_CONFIG", "experimental-features = nix-command flakes")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group: `nix` forks substituters/builders, and a plain
            // `child.kill()` only signals the direct child, leaving the grandchildren running
            // (and the pipes open, so a drain thread never sees EOF). group(0) makes the child
            // its own group leader so the deadline path can signal the whole tree.
            .process_group(0)
            // A dropped future (the deadline path) must not leave `nix` running: the group kill
            // below reaps the tree, this reaps the direct child on every other early return.
            .kill_on_drop(true);
        c
    }

    /// Run with a deadline. `wait_with_output` drains stdout/stderr *while* the child runs —
    /// `nix build` writes far more than the ~64 KiB pipe buffer to stderr, and with nothing
    /// reading it the child blocks on `write()` and never exits, so every real build would "time
    /// out" while it is just waiting on us. (std's `wait_with_output` was unusable here because
    /// the drain had to be hand-rolled onto threads that took the pipes first; tokio's polls both
    /// pipes and the exit concurrently, so that reason is gone.)
    async fn run(&self, mut c: Command, timeout: Duration) -> Result<String, String> {
        let child = c.spawn().map_err(|e| format!("spawn nix: {e}"))?;
        let pid = child.id().ok_or("nix exited before it could be waited on")? as i32;
        let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(r) => r.map_err(|e| e.to_string())?,
            Err(_) => {
                // Signal the whole process group: `nix` forks substituters/builders that hold the
                // pipes open too, so killing only the direct child (all `kill_on_drop` does)
                // would leave the grandchildren running.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                return Err(format!("nix timed out after {}s", timeout.as_secs()));
            }
        };
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        // The last lines are the ones that name the attribute or the disk; the hundreds above
        // them are download progress.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
        Err(tail.join("\n"))
    }
}

#[async_trait::async_trait]
impl Nix for RealNix {
    async fn build(&self, expr: &str, timeout: Duration) -> Result<PathBuf, String> {
        // `--impure` for `builtins.getFlake` on a pinned ref; the expression is ONE argv element.
        // One substituter, the public cache. NOT `--max-jobs 0`: the `buildEnv` symlink tree is a
        // derivation nobody has ever built, so it is always realised locally — what keeps a PINNED
        // package from being compiled here is the copy that ran before this, not a jobs limit.
        let c = self.cmd(&["build", "--impure", "--expr", expr, "--no-link", "--print-out-paths", "--option", "substituters", CACHE]);
        let out = self.run(c, timeout).await?;
        match out.split_whitespace().next() {
            Some(p) => Ok(PathBuf::from(p)),
            None => Err("nix build printed no store path".into()),
        }
    }
    async fn eval_out_path(&self, rev: &str, attr_path: &str, timeout: Duration) -> Result<String, String> {
        // Re-checked here, not trusted from the caller: this is the one place a lock's fields
        // become part of an expression, and an expression is code.
        if !kloudlite_workspaces::packages::valid_rev(rev) {
            return Err(format!("{rev:?} is not a nixpkgs revision"));
        }
        kloudlite_workspaces::packages::validate_attr(attr_path).map_err(|e| e.to_string())?;
        // A pinned release is often an old one, and nixpkgs marks a release past its EOL
        // insecure, which refuses the evaluation outright. Pinning it is the person's decision;
        // whether a binary exists for it is the copy's to answer, not the evaluator's.
        let expr = format!("(import (builtins.getFlake \"github:NixOS/nixpkgs/{rev}\") {{ config.allowInsecurePredicate = _: true; }}).{attr_path}.outPath");
        let c = self.cmd(&["eval", "--impure", "--raw", "--expr", &expr]);
        Ok(self.run(c, timeout).await?.trim().to_string())
    }
    async fn copy_from_cache(&self, store_path: &str, timeout: Duration) -> Result<(), String> {
        let c = self.cmd(&["copy", "--from", CACHE, store_path]);
        self.run(c, timeout).await.map(|_| ()).map_err(|e| {
            if is_not_cached(&e) { format!("{NOT_CACHED}: {e}") } else { e }
        })
    }
    async fn ping(&self) -> Result<(), String> {
        self.run(self.cmd(&["store", "ping"]), Duration::from_secs(10)).await.map(|_| ())
    }
    async fn collect_garbage(&self) -> Result<u64, String> {
        let mut c = Command::new(self.bin.join("nix-collect-garbage"));
        c.env("NIX_REMOTE", "daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let out = self.run(c, Duration::from_secs(3600)).await?;
        Ok(freed_bytes(&out))
    }
}

/// Whether a failed `nix copy` failed because the substituter has no such path, as opposed to a
/// network or daemon problem. nix has no exit code for it, so this reads the message; these are
/// the four wordings nix 2.24 uses for a path the cache cannot give us:
///   `path '...' does not exist and cannot be created`
///   `cannot substitute path '...'` / `unable to substitute path '...'`
///   `path '...' is not valid`
/// A wording we do not recognise stays a plain error, which retries — the safe direction, since
/// calling a network blip `NotCached` would tell a person to change a version that is fine.
fn is_not_cached(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    // The last two are what `nix copy --from` says on 2026-09-08 for a path the cache never held.
    [
        "does not exist and cannot be created",
        "cannot substitute",
        "unable to substitute",
        "is not valid",
        "no substituter that can build it",
        "don't know how to build these paths",
    ]
        .iter()
        .any(|m| e.contains(m))
}

/// Parses `nix-collect-garbage`'s summary line, e.g. `1935 store paths deleted, 3423.35 MiB
/// freed`: the number and unit immediately before "freed" — best effort, the number is only for
/// the log line, so any surprise in the format just yields 0 rather than an error.
fn freed_bytes(out: &str) -> u64 {
    let words: Vec<&str> = out.split_whitespace().collect();
    let Some(freed_idx) = words.iter().position(|w| *w == "freed") else { return 0 };
    if freed_idx < 2 {
        return 0;
    }
    let unit = words[freed_idx - 1];
    let Ok(n) = words[freed_idx - 2].parse::<f64>() else { return 0 };
    let mult: f64 = match unit {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "bytes" => 1.0,
        _ => return 0,
    };
    (n * mult) as u64
}

/// `rename` INSIDE the mounted directory: atomic, and the pod's `/nix/profile` mount is the
/// directory, so its next `current/bin` lookup sees the new target — which is how a running
/// workspace gains a tool without a restart.
pub fn publish(root: &Path, id: &str) -> std::io::Result<()> {
    std::fs::rename(building_path(root, id), profile_path(root, id))
}

pub fn remove_profile(root: &Path, id: &str) -> std::io::Result<()> {
    match std::fs::remove_dir_all(profile_dir(root, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// A link whose target is gone (a GC that ran with the root missing, a wiped store) is a missing
/// profile: mounting it would give the pod an empty `bin`.
pub fn profile_exists(root: &Path, id: &str) -> bool {
    std::fs::metadata(profile_path(root, id)).is_ok()
}

/// The node's index of built profiles, keyed by `packages::hash` — the same hash the workspace
/// records in its status. Under `PROFILES_DIR`, so it inherits the one GC root that already keeps
/// live profiles from being collected, and it survives an agent restart because that directory is
/// on the host.
pub fn index_path(root: &Path, hash: &str) -> PathBuf {
    root.join("by-inputs").join(hash)
}

/// The store path a previous build produced for these inputs, or `None`.
///
/// The TARGET's existence is what is checked, not the link's: a dangling entry is a miss, never a
/// profile with an empty `bin`.
pub fn indexed(root: &Path, hash: &str) -> Option<PathBuf> {
    let link = index_path(root, hash);
    let target = std::fs::read_link(&link).ok()?;
    std::fs::metadata(&target).ok()?;
    Some(target)
}

/// Record a built profile under its inputs. Idempotent: two reconciles that build the same set
/// write the same link to the same path.
pub fn record_index(root: &Path, hash: &str, store_path: &Path) -> std::io::Result<()> {
    let link = index_path(root, hash);
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = link.with_extension("writing");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(store_path, &tmp)?;
    // Rename over the old entry: an index read must never see a half-written link.
    std::fs::rename(&tmp, &link)
}

/// Point `{id}/current` straight at a store path — the cache-hit path, which has no `.building`
/// link to rename. Writes through the same temp-then-rename as `publish` so a pod reading the
/// directory never sees a partial state.
pub fn link_profile(root: &Path, id: &str, store_path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(profile_dir(root, id))?;
    let tmp = building_path(root, id);
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(store_path, &tmp)?;
    publish(root, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fs helpers take their root as an argument, so a test passes a tempdir where production
    /// passes `PROFILES_DIR`.
    #[test]
    fn publish_renames_the_building_link_inside_the_mounted_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target_a = dir.path().join("a"); std::fs::create_dir(&target_a).unwrap();
        let target_b = dir.path().join("b"); std::fs::create_dir(&target_b).unwrap();
        std::fs::create_dir(profile_dir(dir.path(), "ws-1")).unwrap();
        std::os::unix::fs::symlink(&target_a, profile_path(dir.path(), "ws-1")).unwrap();
        std::os::unix::fs::symlink(&target_b, building_path(dir.path(), "ws-1")).unwrap();
        publish(dir.path(), "ws-1").unwrap();
        // The DIRECTORY is what the pod mounts and it never moves — only the link inside it does,
        // which is the whole reason the swap reaches a running container.
        assert_eq!(std::fs::read_link(profile_path(dir.path(), "ws-1")).unwrap(), target_b);
        assert!(!building_path(dir.path(), "ws-1").exists());
        assert!(profile_dir(dir.path(), "ws-1").is_dir());
    }

    #[test]
    fn the_pin_must_name_a_full_nixpkgs_revision() {
        assert!(valid_pin(&format!("github:NixOS/nixpkgs/{}", "a1".repeat(20))));
        for bad in [
            "",
            "github:NixOS/nixpkgs/nixos-24.05",
            "github:NixOS/nixpkgs/",
            &format!("github:NixOS/nixpkgs/{}", "a".repeat(39)),
            &format!("github:NixOS/nixpkgs/{}", "A".repeat(40)),
            &format!("github:NixOS/nixpkgs/{}", "z".repeat(40)),
            &format!("git+ssh://x/nixpkgs/{}", "a".repeat(40)),
        ] {
            assert!(!valid_pin(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_dangling_profile_link_does_not_count_as_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(profile_dir(dir.path(), "ws-1")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), profile_path(dir.path(), "ws-1")).unwrap();
        assert!(!profile_exists(dir.path(), "ws-1"));
        std::fs::create_dir(dir.path().join("gone")).unwrap();
        assert!(profile_exists(dir.path(), "ws-1"));
        remove_profile(dir.path(), "ws-1").unwrap();
        assert!(!profile_dir(dir.path(), "ws-1").exists(), "the whole directory goes");
        remove_profile(dir.path(), "ws-1").unwrap(); // idempotent
    }

    /// A hit is only a hit when the TARGET is still there. A GC that ran while the root was
    /// missing leaves a dangling link, and mounting it would give the pod an empty `bin`.
    #[test]
    fn an_index_entry_whose_target_is_gone_is_a_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = root.join("fake-store-path");
        std::fs::create_dir_all(&store).unwrap();
        record_index(root, "abc123", &store).unwrap();
        assert_eq!(indexed(root, "abc123").as_deref(), Some(store.as_path()));

        std::fs::remove_dir_all(&store).unwrap();
        assert!(indexed(root, "abc123").is_none(), "a dangling entry must not be reused");
    }

    /// Writing the same entry twice is what two reconciles racing on one package set do.
    #[test]
    fn recording_an_index_entry_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store-a");
        std::fs::create_dir_all(&store).unwrap();
        record_index(tmp.path(), "k", &store).unwrap();
        record_index(tmp.path(), "k", &store).unwrap();
        assert_eq!(indexed(tmp.path(), "k").as_deref(), Some(store.as_path()));
    }

    /// The cache-hit path publishes without a build, so it must produce exactly what a build
    /// would have: `{id}/current` pointing at the store path.
    #[test]
    fn linking_a_profile_points_current_at_the_store_path() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store-b");
        std::fs::create_dir_all(&store).unwrap();
        link_profile(tmp.path(), "ws-1", &store).unwrap();
        assert!(profile_exists(tmp.path(), "ws-1"));
        assert_eq!(std::fs::read_link(profile_path(tmp.path(), "ws-1")).unwrap(), store);
    }

    #[tokio::test]
    async fn the_real_runner_execs_an_argv_with_no_shell() {
        // A fake `nix` that records its argv proves the expression travels as ONE argument.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin"); std::fs::create_dir(&bin).unwrap();
        let log = dir.path().join("argv.log");
        std::fs::write(bin.join("nix"), format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> {}; done\necho /nix/store/deadbeef-ws-1-env\n", log.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin: bin.clone() };
        let store = nix.build("let x = \"$(id); rm -rf /\"; in x", Duration::from_secs(5)).await.unwrap();
        assert_eq!(store, PathBuf::from("/nix/store/deadbeef-ws-1-env"), "the store path is read off stdout");
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(argv.contains("let x = \"$(id); rm -rf /\"; in x\n"), "the expression is one argv element: {argv}");
        // `--no-link`: the out-link's auto GC root is orphaned by the publish rename, so we make
        // and root the link ourselves instead.
        assert!(argv.contains("--expr\n") && argv.contains("--no-link\n"), "{argv}");
    }

    #[tokio::test]
    async fn a_build_that_outlives_its_deadline_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin"); std::fs::create_dir(&bin).unwrap();
        // The direct child forks a grandchild and waits on it — a plain `kill()` of just the
        // direct child would leave the grandchild `sleep` running, still holding the piped
        // stdout/stderr write ends open, so the drain threads (and this test) would hang past
        // the deadline. Only a process-group kill reaps both, which is what this proves.
        // A 20 s grandchild against a 10 s bound: a leaked grandchild still fails the test, and a
        // loaded CI runner (two slots, a release build beside it) no longer trips a 3 s bound.
        std::fs::write(bin.join("nix"), "#!/bin/sh\nsleep 40 &\nwait\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin };
        // ETXTBSY: another thread of this test binary can fork while the script's write fd is
        // still open, and the child holds it until it execs — so the exec of a file we have only
        // just written intermittently fails with "Text file busy". It is a property of writing
        // and exec'ing in one multi-threaded process, not of the code under test, so retry it.
        let (started, err) = {
            let mut out = None;
            for _ in 0..10 {
                let at = std::time::Instant::now();
                let e = nix.build("1", Duration::from_millis(300)).await.unwrap_err();
                if !e.contains("Text file busy") {
                    out = Some((at, e));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            out.expect("the script never became executable")
        };
        // The bound is the grandchild's own lifetime, halved — not a guess at how fast a runner
        // reaps: if the process GROUP is not killed this waits out the full 40 s, and anything
        // under 20 s can only mean the kill worked. A tighter bound measures CI's load instead.
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn a_child_that_writes_more_than_a_pipe_buffer_of_stderr_still_completes() {
        // `nix build` writes far more than the ~64 KiB pipe buffer to stderr; if nothing drains
        // it while the child runs, the child blocks on `write()` and every real build "times
        // out". A script writing 1 MiB then exiting must return Ok well within the deadline.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin"); std::fs::create_dir(&bin).unwrap();
        std::fs::write(
            bin.join("nix"),
            "#!/bin/sh\nyes x | head -c 1048576 1>&2\necho /nix/store/x\n",
        ).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin };
        let started = std::time::Instant::now();
        // ETXTBSY is a Linux race in a multi-threaded test binary: a sibling test forking
        // between our write and close inherits the script's write fd, and execve refuses a file
        // someone still holds open for writing. Nothing this test is about — retry it away.
        let mut last = Err("never ran".to_string());
        for _ in 0..10 {
            last = nix.build("1", Duration::from_secs(10)).await;
            match &last {
                Err(e) if e.contains("Text file busy") => tokio::time::sleep(Duration::from_millis(50)).await,
                _ => break,
            }
        }
        last.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "child blocked writing stderr");
    }

    #[tokio::test]
    async fn copying_a_path_the_cache_does_not_have_is_not_cached_not_a_build_failure() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        // What nix prints for a path no substituter can give: the message is the only signal.
        std::fs::write(
            bin.join("nix"),
            "#!/bin/sh\necho \"error: path '/nix/store/x-y' does not exist and cannot be created\" 1>&2\nexit 1\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin };
        let mut err = String::new();
        for _ in 0..10 {
            err = nix.copy_from_cache("/nix/store/x-y", Duration::from_secs(10)).await.unwrap_err();
            if !err.contains("Text file busy") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(err.starts_with(NOT_CACHED), "{err}");
    }

    #[test]
    fn only_the_wordings_that_mean_the_cache_lacks_the_path_are_not_cached() {
        assert!(is_not_cached("error: path '/nix/store/x' does not exist and cannot be created"));
        // A file we simply cannot read is not "the cache lacks this path".
        assert!(!is_not_cached("error: file '/etc/nix/nix.conf' does not exist"));
        assert!(is_not_cached("error: cannot substitute path '/nix/store/x'"));
        assert!(is_not_cached("error: unable to substitute path '/nix/store/x'"));
        assert!(is_not_cached("error: path '/nix/store/x' is not valid"));
        // A network problem must retry, not tell a person their version is unavailable.
        assert!(!is_not_cached("error: unable to download 'https://cache.nixos.org': timeout"));
        assert!(!is_not_cached("nix timed out after 600s"));
    }

    #[test]
    fn freed_bytes_parses_the_real_nix_collect_garbage_summary() {
        assert_eq!(freed_bytes("1935 store paths deleted, 3423.35 MiB freed"), (3423.35 * 1024.0 * 1024.0) as u64);
        assert_eq!(freed_bytes("0 store paths deleted, 0.00 MiB freed"), 0);
        assert_eq!(freed_bytes("nothing to see here"), 0);
    }
}
