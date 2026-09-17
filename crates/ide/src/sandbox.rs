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
use std::path::Path;

/// `bwrap`'s own arguments, up to and including the `--` that ends them. `cmd` is appended whole.
pub fn bwrap_argv(tree: &TreeCtx, profile: &Path, cmd: &[String]) -> Vec<String> {
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
        "--ro-bind".into(),
        "/nix".into(),
        "/nix".into(),
    ];
    a.extend(["--ro-bind".to_string(), profile.to_string_lossy().into_owned(), profile.to_string_lossy().into_owned()]);
    for f in ["/etc/passwd", "/etc/resolv.conf"] {
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

/// Whether `bwrap` can be used at all. Answered once per process: a missing binary is a fact about
/// the image, not a per-exec coin flip, and the log line that says so must not repeat per command.
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
        }
        found
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
        let argv = bwrap_argv(&x, Path::new("/home/kl/.nix-profile"), &["sh".into(), "-c".into(), "true".into()]);
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
    }
}
