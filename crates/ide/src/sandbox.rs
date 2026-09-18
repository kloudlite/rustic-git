//! The bubblewrap wrapper every `exec` runs under (spec §4.7).
//!
//! `paths::confine` guards what the TOOLS touch, but an `exec` is a shell and a shell can `cd ..`.
//! Wrapping each exec — never the server, which must see every tree to serve them — is what closes
//! that. What the command sees: its own tree read-write, the Nix store and profile read-only, a
//! fresh `/tmp`, the pod's network, and nothing else — not the workspace root, not another tree,
//! not the home, not the token, not `kl`.
//!
//! The tree is bound at THE SAME PATH inside and out. That is deliberate: §3.5 concedes that `pwd`
//! inside an exec leaks one string, and rewriting the path would make it two — the real one and a
//! fake one that every compiler error and every stack trace would disagree with.
//!
//! One function, and it answers an argv rather than running anything, so what is bound is a list a
//! test can read rather than a shape nobody checks. Whether bwrap's user-namespace and `/dev`
//! setup work under the workspace pods' runtime class is a fleet question, by the owner's ruling
//! (no spike): a missing or refused `bwrap` runs the command unwrapped and says so once.

use crate::trees::TreeCtx;

/// Every path the wrapper binds from the host, besides the tree itself. `/nix` is BOTH the store
/// and the profile: the profile is `/nix/profile/current` (`packages::PROFILE_LINK`), a symlink
/// into `/nix/store`, and the pod has exactly one `nix` volume mounted at `/nix` covering both.
///
/// This used to also bind `{home}/.nix-profile`, which is a path no workspace pod has — every exec
/// on the fleet died with `bwrap: Can't find source path /home/kl/.nix-profile` (2026-09-18).
/// The lesson is in `missing_bind` below, not in this list: a bind source that is not there must
/// never be the reason a person's command does not run.
const BINDS: [&str; 3] = ["/nix", "/etc/passwd", "/etc/resolv.conf"];

/// What a USERLAND needs from `/etc` beyond a name and a resolver, bound read-only when it exists.
///
/// Optional, every one: a workspace image that lays `/etc` out differently must cost the sandbox
/// nothing — and `bwrap` refuses to start on a missing `--ro-bind` source, so an unconditional
/// list here would be the same outage as `/home/kl/.nix-profile` wearing a different path.
///
/// `/etc/ssl` is the one that matters and the one that was missing: with only `passwd` and
/// `resolv.conf` inside, every HTTPS fetch in every workspace failed the moment the wrapper first
/// worked — `curl: (60) unable to get local issuer certificate`, and with it git-over-https, npm,
/// cargo, pip and go mod, fleet-wide (2026-09-18). The rest are the same class of thing: a host
/// alias, a resolver order, a group name, a timezone. Cheap, read-only, and each absent on some
/// image somewhere.
///
/// `/etc/kloudlite` is deliberately NOT here and must never be: the workspace token lives there,
/// and the whole point of the wrapper is that an exec cannot read it.
const OPTIONAL_BINDS: [&str; 7] = [
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/pki",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/group",
    "/etc/localtime",
];

/// The cert-bundle variables a userland reads, passed through when the image sets them. Nix images
/// point these at a store path rather than at `/etc/ssl`, and a store path is already bound with
/// `/nix` — so passing the variable is the whole of it.
const CERT_VARS: [&str; 3] = ["SSL_CERT_FILE", "NIX_SSL_CERT_FILE", "CURL_CA_BUNDLE"];

/// The optional binds this filesystem actually has. Resolved per call rather than cached: the
/// argv is built per exec anyway, and a list computed once at boot would miss a path that appears
/// when a mount lands.
fn present_optional() -> Vec<&'static str> {
    OPTIONAL_BINDS.into_iter().filter(|p| std::path::Path::new(p).exists()).collect()
}

/// `bwrap`'s own arguments, up to and including the `--` that ends them. `cmd` is appended whole.
pub fn bwrap_argv(tree: &TreeCtx, cmd: &[String]) -> Vec<String> {
    let root = tree.root.to_string_lossy().into_owned();
    let mut a: Vec<String> = vec![
        "--unshare-all".into(),
        // Everything but the network: ports are the pod's, and §4.6 divides them by convention
        // rather than by namespace, because a namespace per tree would be a pod restart.
        "--share-net".into(),
        // A detached process dies with the server, the way its ring already ends with it.
        "--die-with-parent".into(),
        "--new-session".into(),
        "--bind".into(),
        root.clone(),
        root.clone(),
    ];
    for f in BINDS {
        a.extend(["--ro-bind".to_string(), f.into(), f.into()]);
    }
    // Skip-if-absent, so an image that lays `/etc` out differently costs the sandbox nothing.
    for f in present_optional() {
        a.extend(["--ro-bind".to_string(), f.into(), f.into()]);
    }
    a.extend([
        "--tmpfs".to_string(),
        "/tmp".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        // Per tree, so git/npm/cargo config a subagent writes lands in the tree and travels with
        // it, instead of in a home the sandbox does not even bind.
        "--setenv".into(),
        "HOME".into(),
        tree.sandbox_home().to_string_lossy().into_owned(),
        "--chdir".into(),
        root,
    ]);
    // `--unshare-all` keeps the environment, but a caller that clears it would still want these:
    // pass them explicitly so the sandbox's view of the trust store never depends on inheritance.
    for k in CERT_VARS {
        if let Ok(v) = std::env::var(k) {
            a.extend(["--setenv".to_string(), k.into(), v]);
        }
    }
    a.push("--".into());
    a.extend(cmd.iter().cloned());
    a
}

/// The first bind source that is not on this filesystem, if any.
///
/// `bwrap` refuses to start when a `--ro-bind` source is missing, and that refusal is indisting-
/// uishable to a caller from the command itself failing — which is exactly how a wrong path in
/// this file became "every exec on the fleet exits 1". Checked here instead, so a layout this
/// code does not expect COSTS the sandbox and nothing else.
///
/// The tree's own root is deliberately not checked: it is the thing being served, `tree_of`
/// already refused a tree whose directory is gone, and a missing root is a real error rather than
/// a reason to run the command somewhere else.
pub fn missing_bind() -> Option<&'static str> {
    BINDS.into_iter().find(|p| !std::path::Path::new(p).exists())
}

/// Where the profile puts `bwrap`. The server is started by the pod's prelude and inherits a PATH
/// that does NOT carry the profile's bin (the shell's does, which is why `bwrap` on a terminal's
/// PATH proved nothing): a bare `Command::new("bwrap")` found nothing, `available()` answered
/// false, and every exec ran unwrapped with no line in the log because the warning was at WARN on
/// a path nobody read (2026-09-18).
const PROFILE_BWRAP: &str = "/nix/profile/current/bin/bwrap";

/// What the preflight RUNS inside the sandbox. It has to be a path that exists *in there*, and
/// inside the sandbox only `/nix` is bound — `/bin/true` does not exist, which is what the first
/// preflight on the fleet actually reported (`execvp /bin/true: No such file or directory`,
/// 2026-09-18). Coreutils is in the base profile, so the profile's own `true` is the one command
/// guaranteed to be there whenever `bwrap` itself is.
///
/// Its absence would be a real answer too: a sandbox whose profile carries no `true` carries no
/// shell either, and an exec in it could not run anything.
const PROFILE_TRUE: &str = "/nix/profile/current/bin/true";

/// The profile's `test`, used to ask a question INSIDE the sandbox rather than about it.
const PROFILE_TEST: &str = "/nix/profile/current/bin/test";

/// The trust store a userland looks for. Checked from inside the sandbox by the preflight, because
/// "bwrap started" and "a program in there can fetch over HTTPS" turned out to be different facts:
/// the wrapper worked on its first real roll and took every HTTPS fetch in the fleet down with it,
/// since `/etc` held only `passwd` and `resolv.conf` (2026-09-18).
///
/// The FIRST of these that the sandbox can read is enough — images disagree about where the bundle
/// lives, and the question is whether a bundle is reachable at all, not which one.
const CERT_BUNDLES: [&str; 3] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/certs/ca-bundle.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
];

/// The `bwrap` to run: the profile's, else whatever PATH finds. Answered once — which binary
/// exists is a fact about the image, not a per-exec coin flip.
///
/// `None` means every exec runs UNWRAPPED, with `paths::confine` as its only fence. That is worse
/// than the wrapper and far better than refusing to run anything.
pub fn binary() -> Option<&'static str> {
    static FOUND: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        let found = pick(&|p: &str| {
            std::process::Command::new(p)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        });
        match found {
            None => {
                // INFO, not WARN: the pods run at the default level, and a WARN nobody reads is
                // the reason this went ten minutes unnoticed. Once — it is a fact about the image.
                tracing::info!(reason = "no-bwrap", "ide.sandbox.unavailable");
                None
            }
            Some(p) => match missing_bind() {
                Some(path) => {
                    tracing::info!(reason = %format!("missing-bind:{path}"), "ide.sandbox.unavailable");
                    None
                }
                None => Some(p),
            },
        }
    })
}

/// Which candidate to take, given a way to ask whether one runs. Split from `binary` so the ORDER
/// — the profile's absolute path before a bare PATH lookup — has a test that does not depend on
/// what happens to be installed on the machine running it, and does not burn the `OnceLock`.
fn pick(runnable: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    // The profile first and by absolute path, because that is the one place the pod is guaranteed
    // to have it and the one place this process's PATH is guaranteed not to look.
    [PROFILE_BWRAP, "bwrap"].into_iter().find(|p| runnable(p))
}

/// Whether a `bwrap` binary exists at all. NOT enough to use it — see `usable`.
pub fn available() -> bool {
    binary().is_some()
}

/// The one-time RUNTIME proof, and the only thing that may be trusted.
///
/// A binary that exists and binds that exist do not mean the wrapper WORKS. Under the workspace
/// pods' runtime (gVisor) an unprivileged user namespace is refused to uid 1000, so the real argv
/// dies with `bwrap: setting up uid map: Operation not permitted` — a failure invisible to every
/// check that came before it, and the third outage of this exact shape would have been the one
/// that shipped it (2026-09-18, caught by hand before the roll).
///
/// So the sandbox proves it can start before it is trusted: the resolved binary, the REAL flags,
/// and `/bin/true` as the command. A preflight with fewer namespaces would pass where the thing it
/// stands for fails, which is the whole lesson of the three outages this file has now caused.
///
/// Cached for the life of the process: the runtime's answer does not change under a running pod,
/// and paying a fork per exec to re-ask would be its own defect.
///
/// `None` means every exec runs unwrapped, with `paths::confine` and the tree cwd as the fence —
/// a weaker boundary, and the only honest one available.
pub fn usable(tree: &TreeCtx) -> Option<&'static str> {
    static OK: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *OK.get_or_init(|| preflight(binary()?, tree))
}

/// The proof itself, uncached, so a test can run it against a fake `bwrap` without burning the
/// `OnceLock` — the caching is `usable`'s and the DECISION is this function's.
fn preflight(bwrap: &'static str, tree: &TreeCtx) -> Option<&'static str> {
    {
        // The HOME the real argv sets must exist before the real argv is run, preflight included.
        let _ = std::fs::create_dir_all(tree.sandbox_home());
        // Two questions in one run: does the wrapper START, and can something inside it reach a
        // trust store. The second is asked with the profile's own `test` against the bundles the
        // image might carry — `-r` on the first that answers, so an image with a different layout
        // passes on its own path rather than on this file's guess.
        //
        // Deliberately NOT a real HTTPS fetch: a preflight that dials the internet makes every
        // workspace's sandbox depend on a network that may be down for reasons of its own, and
        // turns an outage out there into the sandbox switching itself off in here.
        let probe_certs = CERT_BUNDLES.map(|b| format!("{PROFILE_TEST} -r {b}")).join(" || ");
        let argv = bwrap_argv(
            tree,
            &[
                "/nix/profile/current/bin/sh".to_string(),
                "-c".to_string(),
                // `true` first so a sandbox that starts but has no certs still names the reason.
                format!("{PROFILE_TRUE} && {{ {probe_certs}; }}"),
            ],
        );
        match std::process::Command::new(bwrap).args(&argv).output() {
            Ok(o) if o.status.success() => {
                // ONLY here. A passing preflight is the one thing that earns this line, so a log
                // carrying it means execs really are wrapped rather than that something was found
                // on disk.
                tracing::info!(bwrap, "ide.sandbox.active");
                Some(bwrap)
            }
            // The FIRST stderr line: bwrap says what it could not do on line one and usage after,
            // and the reason has to fit a log field a person reads at a glance.
            Ok(o) => {
                // A non-zero exit with NOTHING on stderr is the cert check failing: `test` is
                // silent. Say which, or a reader sees "preflight:no stderr" and learns nothing.
                let why = match first_line(&o.stderr).as_str() {
                    "no stderr" => format!("no readable trust store inside the sandbox (tried {})", CERT_BUNDLES.join(", ")),
                    line => line.to_string(),
                };
                tracing::info!(reason = %format!("preflight:{why}"), "ide.sandbox.unavailable");
                None
            }
            Err(e) => {
                tracing::info!(reason = %format!("preflight:{e}"), "ide.sandbox.unavailable");
                None
            }
        }
    }
}

/// The first non-empty line of a child's stderr, trimmed and bounded — a log field, not a dump.
fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no stderr")
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trees::Trees;

    #[test]
    fn the_wrapper_binds_the_tree_and_names_nothing_else_of_the_pod() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces/ws-1");
        std::fs::create_dir_all(root.join(".agents/x")).unwrap();
        let trees = Trees::new(root.clone(), None);
        let x = trees.resolve(Some("x")).unwrap();
        let argv = bwrap_argv(&x, &["sh".into(), "-c".into(), "true".into()]);
        let tree_path = x.root.to_string_lossy().into_owned();
        // Bound at the same path in and out — the one string §3.5 concedes stays one string.
        let bind = argv.windows(3).find(|w| w[0] == "--bind").expect("the tree is bound");
        assert_eq!((bind[1].as_str(), bind[2].as_str()), (tree_path.as_str(), tree_path.as_str()));
        // HOME is the tree's own, never the person's.
        let home = argv.windows(3).find(|w| w[0] == "--setenv" && w[1] == "HOME").expect("HOME is set");
        assert_eq!(home[2], format!("{tree_path}/.home"));
        // Nothing names the workspace root, so a `cd ..` has nowhere to land.
        assert!(!argv.iter().any(|s| *s == root.to_string_lossy()), "{argv:?}");
        // The command is last, whole, after the `--`.
        let dashdash = argv.iter().position(|s| s == "--").unwrap();
        assert_eq!(&argv[dashdash + 1..], &["sh".to_string(), "-c".into(), "true".into()]);
        // No path under a HOME is ever a bind SOURCE. The pod has no `~/.nix-profile`, and naming
        // one made bwrap refuse to start on every exec in the fleet (2026-09-18).
        // The REQUIRED ones, in order, first — the optional `/etc/*` follow and depend on what
        // this machine happens to have, which is the point of them being optional.
        let sources: Vec<&String> = argv.windows(3).filter(|w| w[0] == "--ro-bind").map(|w| &w[1]).collect();
        assert!(sources[..BINDS.len()].iter().zip(BINDS).all(|(got, want)| *got == want), "{argv:?}");
        assert!(sources[BINDS.len()..].iter().all(|s| OPTIONAL_BINDS.contains(&s.as_str())), "{sources:?}");
    }

    /// A `bwrap` that EXISTS but cannot START must not be trusted. Under the workspace pods'
    /// runtime an unprivileged user namespace is refused to uid 1000 and the real argv dies with
    /// `setting up uid map: Operation not permitted` — which every earlier check passed, because
    /// every earlier check asked something weaker than "does this argv run".
    ///
    /// The fake stands in for the runtime: a script that prints bwrap's own refusal and exits 1.
    #[test]
    fn a_bwrap_that_cannot_start_is_not_used_and_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let tree = Trees::new(root, None).resolve(None).unwrap();

        let fake = tmp.path().join("bwrap-fails");
        std::fs::write(&fake, "#!/bin/sh\necho 'bwrap: setting up uid map: Operation not permitted' >&2\nexit 1\n").unwrap();
        make_executable(&fake);
        // Leaked so the signature matches the cached form; a test binary's lifetime is the leak's.
        let path: &'static str = Box::leak(fake.to_string_lossy().into_owned().into_boxed_str());
        assert_eq!(preflight(path, &tree), None, "a wrapper that cannot start is not a wrapper");

        // And the other way: one that starts is taken.
        let ok = tmp.path().join("bwrap-works");
        std::fs::write(&ok, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&ok);
        let ok_path: &'static str = Box::leak(ok.to_string_lossy().into_owned().into_boxed_str());
        assert_eq!(preflight(ok_path, &tree), Some(ok_path));
    }

    fn make_executable(p: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The reason that reaches the log is bwrap's own first line, not its usage text — a person
    /// reading `ide.sandbox.unavailable` has to see WHY in the field, not be sent to the pod.
    #[test]
    fn the_reason_is_the_first_line_of_what_the_wrapper_said() {
        let stderr = b"bwrap: setting up uid map: Operation not permitted\nusage: bwrap [OPTIONS]\n  --unshare-all\n";
        assert_eq!(first_line(stderr), "bwrap: setting up uid map: Operation not permitted");
        assert_eq!(first_line(b"\n\n  indented and late\n"), "indented and late");
        assert_eq!(first_line(b""), "no stderr");
        // Bounded: a wrapper that printed a megabyte must not put a megabyte in a log field.
        assert_eq!(first_line(&vec![b'x'; 5000]).len(), 200);
    }

    /// The profile's absolute path wins, and a bare PATH lookup is only the fallback.
    ///
    /// This is the regression test for the second half of the 2026-09-18 outage: the server's own
    /// PATH does not carry `/nix/profile/current/bin`, so a bare `bwrap` resolved to nothing, the
    /// wrapper silently switched itself off, and the only line that would have said so was at a
    /// level the pods do not print. Order is the fix, so order is what is pinned.
    #[test]
    fn the_profiles_bwrap_is_preferred_and_a_bare_name_is_the_fallback() {
        // A "profile" that has it: the absolute path is taken even though the bare name would run.
        assert_eq!(pick(&|_| true), Some(PROFILE_BWRAP));
        // A pod whose profile has it and whose PATH does not — the real workspace pod.
        assert_eq!(pick(&|p| p == PROFILE_BWRAP), Some(PROFILE_BWRAP));
        // A developer machine: nothing at the profile path, `bwrap` on PATH.
        assert_eq!(pick(&|p| p == "bwrap"), Some("bwrap"));
        // Neither: unwrapped, and `binary()` says so once at INFO.
        assert_eq!(pick(&|_| false), None);
    }

    /// The candidate really is under the profile the pod mounts, not some other spelling of it:
    /// `packages::PROFILE_LINK` is where the agent points `current`, and `PATH` is built from it.
    /// A userland needs more from `/etc` than a name and a resolver. With only `passwd` and
    /// `resolv.conf` inside, every HTTPS fetch in every workspace failed the moment the wrapper
    /// first worked — git over https, npm, cargo, pip, go mod, all of it (2026-09-18).
    #[test]
    fn the_sandbox_carries_a_trust_store_and_never_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let x = Trees::new(root, None).resolve(None).unwrap();
        let argv = bwrap_argv(&x, &["true".into()]);
        let sources: Vec<&String> = argv.windows(3).filter(|w| w[0] == "--ro-bind").map(|w| &w[1]).collect();

        // The token dir is the one path under /etc that must NEVER be bound: reading it is
        // exactly what the wrapper exists to prevent.
        assert!(
            !sources.iter().any(|s| s.starts_with("/etc/kloudlite")),
            "the workspace token is inside the sandbox: {sources:?}"
        );
        // Every optional bind that exists on THIS machine is bound, and every one that does not is
        // skipped — a missing source is what bwrap refuses to start on.
        for p in OPTIONAL_BINDS {
            let there = std::path::Path::new(p).exists();
            assert_eq!(sources.iter().any(|s| *s == p), there, "{p} present={there}");
        }
        // `/etc/ssl` is in the list at all — the bind the outage was about.
        assert!(OPTIONAL_BINDS.contains(&"/etc/ssl"));
    }

    /// The preflight asks about the trust store from INSIDE, and the paths it asks about are ones
    /// a bind can actually reach. A check against a path outside every bind would pass on a
    /// developer machine and fail in a pod, which is how the last three of these went.
    #[test]
    fn the_cert_check_asks_about_paths_the_sandbox_binds() {
        for bundle in CERT_BUNDLES {
            let reachable = BINDS.iter().chain(OPTIONAL_BINDS.iter()).any(|b| bundle.starts_with(&format!("{b}/")));
            assert!(reachable, "{bundle} is under none of the binds, so the sandbox can never read it");
        }
        // And the tools the preflight runs come from the profile, which `/nix` carries.
        for tool in [PROFILE_TRUE, PROFILE_TEST] {
            assert!(tool.starts_with("/nix/"), "{tool}");
        }
    }

    /// The preflight's own command must exist INSIDE the sandbox, which is the one place it runs.
    ///
    /// The first preflight on the fleet failed with `execvp /bin/true: No such file or directory`
    /// — not because the sandbox was broken, but because only `/nix` is bound inside it and `/bin`
    /// is not (2026-09-18). A preflight that cannot start for a reason of its OWN reports the
    /// sandbox unusable when it may be fine: the same class of wrong answer as trusting a check
    /// that never ran, with the sign flipped.
    #[test]
    fn the_preflight_command_lives_under_a_path_the_sandbox_binds() {
        assert!(
            BINDS.iter().any(|b| PROFILE_TRUE.starts_with(&format!("{b}/"))),
            "{PROFILE_TRUE} is under none of {BINDS:?}, so it cannot exist inside the sandbox"
        );
        // And from the same profile as the wrapper: where `bwrap` is, coreutils is.
        let dir = |p: &str| p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        assert_eq!(dir(PROFILE_TRUE), dir(PROFILE_BWRAP));
    }

    #[test]
    fn the_candidate_is_under_the_profile_the_pod_mounts() {
        assert_eq!(PROFILE_BWRAP, "/nix/profile/current/bin/bwrap");
        assert!(PROFILE_BWRAP.starts_with("/nix/profile/current/"), "{PROFILE_BWRAP}");
        // And it is under a path the wrapper binds, or a wrapped exec could not see its own bwrap.
        assert!(BINDS.iter().any(|b| PROFILE_BWRAP.starts_with(&format!("{b}/"))), "{BINDS:?}");
    }

    /// Every source the argv names is one `missing_bind` speaks for, so a path added to `BINDS`
    /// without being checked cannot ship: the degradation is the whole safety of this file.
    #[test]
    fn every_bound_source_is_one_the_preflight_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let x = Trees::new(root, None).resolve(None).unwrap();
        let argv = bwrap_argv(&x, &["true".into()]);
        // Required first, then whichever optional ones exist here: `missing_bind` speaks for the
        // required list, and the optional ones are filtered by existence before they are named.
        let sources: Vec<String> = argv.windows(3).filter(|w| w[0] == "--ro-bind").map(|w| w[1].clone()).collect();
        assert_eq!(sources[..BINDS.len()], BINDS.map(str::to_string)[..]);
        assert_eq!(&sources[BINDS.len()..], &present_optional().iter().map(|s| s.to_string()).collect::<Vec<_>>()[..]);
    }
}
