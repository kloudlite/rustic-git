//! The agent's one Nix client: builds a workspace's profile through the host daemon, publishes it
//! by rename, and collects garbage. Behind a trait so the reconciler is tested with a fake — a
//! real `nix` needs a daemon and a store, which a unit test must not.
//!
//! The binary comes from the HOST store (`/nix/var/nix/profiles/default/bin`, seeded by the
//! DaemonSet's init container from the `nixos/nix` image), not from the agent image: a `nix` that
//! lives outside the store it talks to cannot exist, and shipping a second store just to hold the
//! client is what the seed step avoids.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const PROFILES_DIR: &str = "/nix/var/rustic/profiles";
const DEFAULT_TIMEOUT_SECS: u64 = 1200;

// ponytail: env override so tests can redirect the profile root without threading a root through
// every call site; promote to a field on RealNix if a second caller ever needs a different root.
pub fn profiles_dir() -> PathBuf {
    std::env::var("WS_PROFILES_DIR").unwrap_or_else(|_| PROFILES_DIR.into()).into()
}

pub fn profile_path(id: &str) -> PathBuf { profiles_dir().join(id) }
pub fn building_path(id: &str) -> PathBuf { profiles_dir().join(format!("{id}.building")) }

pub fn nixpkgs_pin() -> String {
    std::env::var("WS_NIXPKGS").unwrap_or_default()
}

pub fn build_timeout() -> Duration {
    Duration::from_secs(std::env::var("WS_NIX_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TIMEOUT_SECS))
}

pub trait Nix: Send + Sync {
    /// `nix build --expr <expr> -o <out_link>`; Ok(()) once the out-link exists.
    fn build(&self, expr: &str, out_link: &Path, timeout: Duration) -> Result<(), String>;
    /// `nix store ping`.
    fn ping(&self) -> Result<(), String>;
    /// `nix-collect-garbage`; returns bytes freed as nix reports them (0 if unparseable).
    fn collect_garbage(&self) -> Result<u64, String>;
}

pub struct RealNix {
    pub bin: PathBuf,
}

impl RealNix {
    fn cmd(&self, args: &[&str]) -> Command {
        use std::os::unix::process::CommandExt;
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
            .process_group(0);
        c
    }

    /// Run with a deadline: `wait_timeout` is not in std, so poll `try_wait` at 200 ms.
    ///
    /// stdout/stderr are drained on their own threads as the child runs, not after exit:
    /// `nix build` writes far more than the ~64 KiB pipe buffer to stderr, and with nothing
    /// reading it the child blocks on `write()` and never exits — every real build would "time
    /// out" even though it is just waiting on us. `wait_with_output` only works when nothing
    /// else already took the pipes, so it can't be used here.
    fn run(&self, mut c: Command, timeout: Duration) -> Result<String, String> {
        let mut child = c.spawn().map_err(|e| format!("spawn nix: {e}"))?;
        let pid = child.id() as i32;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let out_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut r = stdout;
            let _ = r.read_to_end(&mut buf);
            buf
        });
        let err_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut r = stderr;
            let _ = r.read_to_end(&mut buf);
            buf
        });

        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(status) => break Ok(status),
                None if started.elapsed() > timeout => break Err(()),
                None => std::thread::sleep(Duration::from_millis(200)),
            }
        };

        if status.is_err() {
            // Signal the whole process group — `nix`'s children hold the pipes open too, so a
            // kill of only the direct child would leave the drain threads (and `wait`) hanging.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = child.wait();
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();

        let status = match status {
            Ok(s) => s,
            Err(()) => return Err(format!("nix timed out after {}s", timeout.as_secs())),
        };
        if status.success() {
            return Ok(String::from_utf8_lossy(&stdout).into_owned());
        }
        // The last lines are the ones that name the attribute or the disk; the hundreds above
        // them are download progress.
        let stderr = String::from_utf8_lossy(&stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
        Err(tail.join("\n"))
    }
}

impl Nix for RealNix {
    fn build(&self, expr: &str, out_link: &Path, timeout: Duration) -> Result<(), String> {
        // `--impure` for `builtins.getFlake` on a pinned ref; the expression is ONE argv element.
        let link = out_link.to_string_lossy().into_owned();
        let c = self.cmd(&["build", "--impure", "--expr", expr, "-o", &link]);
        self.run(c, timeout).map(|_| ())
    }
    fn ping(&self) -> Result<(), String> {
        self.run(self.cmd(&["store", "ping"]), Duration::from_secs(10)).map(|_| ())
    }
    fn collect_garbage(&self) -> Result<u64, String> {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new(self.bin.join("nix-collect-garbage"));
        c.env("NIX_REMOTE", "daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let out = self.run(c, Duration::from_secs(3600))?;
        Ok(freed_bytes(&out))
    }
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

pub fn publish(id: &str) -> std::io::Result<()> { publish_in(&profiles_dir(), id) }
pub fn remove_profile(id: &str) -> std::io::Result<()> { remove_profile_in(&profiles_dir(), id) }
pub fn profile_exists(id: &str) -> bool { profile_exists_in(&profiles_dir(), id) }

/// `rename` over the live link: atomic, and the pod's `/nix/profile` bind of the old target keeps
/// working until its next path lookup — which is how a running workspace gains a tool without a
/// restart.
pub fn publish_in(root: &Path, id: &str) -> std::io::Result<()> {
    std::fs::rename(root.join(format!("{id}.building")), root.join(id))
}

pub fn remove_profile_in(root: &Path, id: &str) -> std::io::Result<()> {
    for p in [root.join(id), root.join(format!("{id}.building"))] {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A link whose target is gone (a GC that ran with the root missing, a wiped store) is a missing
/// profile: mounting it would give the pod an empty `bin`.
pub fn profile_exists_in(root: &Path, id: &str) -> bool {
    std::fs::metadata(root.join(id)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runner with the profile dir redirected: `PROFILES_DIR` is a const, so the fs helpers
    /// take a root. Tests pass a tempdir; production passes `PROFILES_DIR`.
    #[test]
    fn publish_renames_the_building_link_over_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let target_a = dir.path().join("a"); std::fs::create_dir(&target_a).unwrap();
        let target_b = dir.path().join("b"); std::fs::create_dir(&target_b).unwrap();
        std::os::unix::fs::symlink(&target_a, dir.path().join("ws-1")).unwrap();
        std::os::unix::fs::symlink(&target_b, dir.path().join("ws-1.building")).unwrap();
        publish_in(dir.path(), "ws-1").unwrap();
        assert_eq!(std::fs::read_link(dir.path().join("ws-1")).unwrap(), target_b);
        assert!(!dir.path().join("ws-1.building").exists());
    }

    #[test]
    fn a_dangling_profile_link_does_not_count_as_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("ws-1")).unwrap();
        assert!(!profile_exists_in(dir.path(), "ws-1"));
        std::fs::create_dir(dir.path().join("gone")).unwrap();
        assert!(profile_exists_in(dir.path(), "ws-1"));
        remove_profile_in(dir.path(), "ws-1").unwrap();
        assert!(!dir.path().join("ws-1").exists());
        remove_profile_in(dir.path(), "ws-1").unwrap(); // idempotent
    }

    #[test]
    fn the_real_runner_execs_an_argv_with_no_shell() {
        // A fake `nix` that records its argv proves the expression travels as ONE argument.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin"); std::fs::create_dir(&bin).unwrap();
        let log = dir.path().join("argv.log");
        std::fs::write(bin.join("nix"), format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> {}; done\nln -s /tmp \"$6\" 2>/dev/null; exit 0\n", log.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin: bin.clone() };
        let out = dir.path().join("out");
        nix.build("let x = \"$(id); rm -rf /\"; in x", &out, Duration::from_secs(5)).unwrap();
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(argv.contains("let x = \"$(id); rm -rf /\"; in x\n"), "the expression is one argv element: {argv}");
        assert!(argv.contains("--expr\n") && argv.contains("--no-link\n") == false);
    }

    #[test]
    fn a_build_that_outlives_its_deadline_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin"); std::fs::create_dir(&bin).unwrap();
        // The direct child forks a grandchild and waits on it — a plain `kill()` of just the
        // direct child would leave the grandchild `sleep` running, still holding the piped
        // stdout/stderr write ends open, so the drain threads (and this test) would hang past
        // the deadline. Only a process-group kill reaps both, which is what this proves.
        std::fs::write(bin.join("nix"), "#!/bin/sh\nsleep 5 &\nwait\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin };
        let started = std::time::Instant::now();
        let err = nix.build("1", &dir.path().join("out"), Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn a_child_that_writes_more_than_a_pipe_buffer_of_stderr_still_completes() {
        // `nix build` writes far more than the ~64 KiB pipe buffer to stderr; if nothing drains
        // it while the child runs, the child blocks on `write()` and every real build "times
        // out". A script writing 1 MiB then exiting must return Ok well within the deadline.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin"); std::fs::create_dir(&bin).unwrap();
        std::fs::write(
            bin.join("nix"),
            "#!/bin/sh\nyes x | head -c 1048576 1>&2\nexit 0\n",
        ).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin };
        let started = std::time::Instant::now();
        nix.build("1", &dir.path().join("out"), Duration::from_secs(10)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "child blocked writing stderr");
    }

    #[test]
    fn freed_bytes_parses_the_real_nix_collect_garbage_summary() {
        assert_eq!(freed_bytes("1935 store paths deleted, 3423.35 MiB freed"), (3423.35 * 1024.0 * 1024.0) as u64);
        assert_eq!(freed_bytes("0 store paths deleted, 0.00 MiB freed"), 0);
        assert_eq!(freed_bytes("nothing to see here"), 0);
    }
}
