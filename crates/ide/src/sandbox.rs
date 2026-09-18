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
        "--".into(),
    ]);
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

/// Whether the wrapper can be used at all: `bwrap` on PATH, and every bind source present.
/// Answered once per process — both are facts about the image and the pod's mounts, not per-exec
/// coin flips, and the line that says so must not repeat per command.
///
/// False means every exec runs UNWRAPPED, with `paths::confine` as its only fence. That is worse
/// than the wrapper and far better than the alternative this replaced: refusing to run anything.
/// `ide.sandbox.unavailable` is how the fleet sees which of the two states a pod is in.
pub fn available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let found = std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !found {
            // Once, loudly: every exec after this runs with `confine` as its only fence, which is
            // the state the fleet has to be able to see it is in.
            tracing::warn!(reason = "no-bwrap", "ide.sandbox.unavailable");
            return false;
        }
        if let Some(path) = missing_bind() {
            tracing::warn!(reason = %format!("missing-bind:{path}"), "ide.sandbox.unavailable");
            return false;
        }
        true
    })
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
        let sources: Vec<&String> = argv.windows(3).filter(|w| w[0] == "--ro-bind").map(|w| &w[1]).collect();
        assert_eq!(sources, vec!["/nix", "/etc/passwd", "/etc/resolv.conf"], "{argv:?}");
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
        let sources: Vec<String> = argv.windows(3).filter(|w| w[0] == "--ro-bind").map(|w| w[1].clone()).collect();
        assert_eq!(sources, BINDS.map(str::to_string).to_vec());
    }
}
