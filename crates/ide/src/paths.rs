//! Confinement: every path a tool touches resolves under ITS TREE's root, and every path it gives
//! back is relative to the same root (`relative`, re-exported from `trees`).
//!
//! Two rules, and they answer differently on purpose (spec §3.5, §4.4):
//!
//!   * An ABSOLUTE path is a 400 — "paths are relative to your working directory". There is
//!     nothing to deny, only a shape to correct, and a model told it was denied would go looking
//!     for a way in rather than re-spelling what it already had. The home used to be an allowed
//!     absolute prefix here, so a model could edit dotfiles; that is the person's shell's job now,
//!     and the shell is a separate container.
//!   * A path that RESOLVES outside the tree is a 403 naming the path, because a caller can act on
//!     a path and cannot act on a rule.
//!
//! The main tree's root is the workspace, and it has the one exception in this file: it may not
//! look under `.agents/`, where the subagents' trees live. Inside a tree there is no exception —
//! its root is the fence.
use crate::tools::ToolError;
use crate::trees::{TreeCtx, TREES_DIR};
use std::path::{Component, Path, PathBuf};

/// What a model is told when it hands over an absolute path. One sentence, and it is the shape,
/// never the layout: naming what it may not reach would teach exactly what §3.5 keeps from it.
pub const RELATIVE_ONLY: &str = "paths are relative to your working directory";

pub fn confine(tree: &TreeCtx, given: &str) -> Result<PathBuf, ToolError> {
    if Path::new(given).is_absolute() {
        return Err(ToolError::Invalid(RELATIVE_ONLY.into()));
    }
    // Normalise lexically first (`..` never climbs above the root), then resolve every existing
    // prefix so a symlink cannot point out of the tree either.
    let mut lexical = PathBuf::new();
    for c in tree.root.join(given).components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    let resolved = resolve_existing_prefix(&lexical);
    let root = tree.root.canonicalize().unwrap_or_else(|_| tree.root.clone());
    if !resolved.starts_with(&root) {
        return Err(ToolError::Denied(format!("EACCES {given}: outside your working directory")));
    }
    // The workspace's own session may not read a subagent's files. Checked on the RESOLVED path,
    // so a symlink into `.agents/` is refused with the spelling it was reached by.
    if tree.is_main() && resolved.strip_prefix(&root).is_ok_and(|rest| rest.starts_with(TREES_DIR)) {
        return Err(ToolError::Denied(format!("EACCES {given}: that is another session's working directory")));
    }
    // The RESOLVED path, not the lexical one: the caller opens what was checked, so a symlink
    // swapped between the check and the open cannot point the write somewhere else (2026-09-12).
    Ok(resolved)
}

/// Canonicalise the longest existing prefix and re-append the rest, so a path that does not exist
/// yet (a `write` target) is still checked through whatever symlinks lead to it.
fn resolve_existing_prefix(p: &Path) -> PathBuf {
    let mut existing = p.to_path_buf();
    let mut tail = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for t in tail.iter().rev() {
        out.push(t);
    }
    out
}

pub use crate::trees::relative;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trees::Trees;

    fn fixture() -> (tempfile::TempDir, Trees) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        std::fs::create_dir_all(root.join(".agents/x")).unwrap();
        (tmp, Trees::new(root, None))
    }

    #[test]
    fn relative_paths_resolve_under_the_root_and_escapes_are_refused_by_name() {
        let (_t, trees) = fixture();
        let main = trees.resolve(None).unwrap();
        assert_eq!(confine(&main, "src/main.rs").unwrap(), main.root.join("src/main.rs"));
        let e = confine(&main, "../../../../etc/passwd").unwrap_err();
        assert!(matches!(e, ToolError::Denied(ref m) if m.contains("etc/passwd")), "{e:?}");
        // Absolute is a SHAPE error, whatever it points at — the tree's own root included.
        assert!(matches!(confine(&main, "/etc/passwd"), Err(ToolError::Invalid(_))));
        assert!(matches!(confine(&main, &main.root.to_string_lossy()), Err(ToolError::Invalid(_))));
        // A symlink inside the tree resolves to its target: what was checked is what is opened.
        std::fs::create_dir_all(main.root.join("real")).unwrap();
        std::os::unix::fs::symlink(main.root.join("real"), main.root.join("link")).unwrap();
        assert_eq!(confine(&main, "link/f.txt").unwrap(), main.root.join("real/f.txt"));
        // A symlink that leaves the tree is followed and refused.
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), main.root.join("out")).unwrap();
        assert!(confine(&main, "out/secret").is_err());
    }

    /// The main tree's one exception, by spelling and by symlink alike.
    #[test]
    fn the_main_tree_may_not_look_under_agents() {
        let (_t, trees) = fixture();
        let main = trees.resolve(None).unwrap();
        assert!(matches!(confine(&main, ".agents/x/f"), Err(ToolError::Denied(_))));
        std::os::unix::fs::symlink(main.root.join(".agents"), main.root.join("peek")).unwrap();
        assert!(matches!(confine(&main, "peek/x/f"), Err(ToolError::Denied(_))), "reached by a symlink, refused the same");
        // A file that merely STARTS with the name is not the directory.
        assert!(confine(&main, ".agents-notes.md").is_ok());
    }

    /// A tree's root is its whole world: it cannot climb into the workspace, and `.agents` under
    /// it is an ordinary name because there are no trees inside a tree.
    #[test]
    fn a_tree_is_fenced_by_its_own_root() {
        let (_t, trees) = fixture();
        let x = trees.resolve(Some("x")).unwrap();
        assert_eq!(confine(&x, "src/a.rs").unwrap(), x.root.join("src/a.rs"));
        assert!(matches!(confine(&x, "../../src/main.rs"), Err(ToolError::Denied(_))));
        assert!(confine(&x, ".agents/whatever").is_ok(), "no trees inside a tree");
    }

    #[test]
    fn a_path_out_is_relative_to_the_tree_and_the_root_is_a_dot() {
        let (_t, trees) = fixture();
        let main = trees.resolve(None).unwrap();
        assert_eq!(relative(&main, &main.root.join("src/main.rs")), "src/main.rs");
        assert_eq!(relative(&main, &main.root), ".");
        let x = trees.resolve(Some("x")).unwrap();
        assert_eq!(relative(&x, &x.root.join("src/main.rs")), "src/main.rs");
        // Never the prefix, whatever happens: a path from outside answers as its own name.
        assert_eq!(relative(&x, Path::new("/etc/passwd")), "passwd");
    }
}
