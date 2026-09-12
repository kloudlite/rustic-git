//! Confinement: every path a tool touches resolves under the home. The root is the workspace dir;
//! a relative path is relative to it; an absolute path is allowed only under the home, symlinks
//! followed. A denial names the path, never the rule, because the caller can act on a path.
use crate::tools::ToolError;
use std::path::{Component, Path, PathBuf};

pub fn confine(root: &Path, home: &Path, given: &str) -> Result<PathBuf, ToolError> {
    let joined = if Path::new(given).is_absolute() { PathBuf::from(given) } else { root.join(given) };
    // Normalise lexically first (`..` never climbs above the home), then resolve every existing
    // prefix so a symlink cannot point out of the home either.
    let mut lexical = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    let resolved = resolve_existing_prefix(&lexical);
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if !resolved.starts_with(&home) {
        return Err(ToolError::Denied(format!("EACCES {}: outside {}", given, home.display())));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_resolve_under_the_root_and_escapes_are_refused_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(confine(&root, &home, "src/main.rs").unwrap(), root.join("src/main.rs"));
        assert_eq!(confine(&root, &home, &home.join(".config/x").to_string_lossy()).unwrap(), home.join(".config/x"));
        let e = confine(&root, &home, "../../../../etc/passwd").unwrap_err();
        assert!(matches!(e, ToolError::Denied(ref m) if m.contains("etc/passwd")), "{e:?}");
        assert!(confine(&root, &home, "/etc/passwd").is_err());
        // A symlink INSIDE the home resolves to its target: what was checked is what is opened.
        std::fs::create_dir_all(home.join("real")).unwrap();
        std::os::unix::fs::symlink(home.join("real"), root.join("link")).unwrap();
        assert_eq!(confine(&root, &home, "link/f.txt").unwrap(), home.join("real/f.txt"));
        // A symlink that leaves the home is followed and refused.
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("out")).unwrap();
        assert!(confine(&root, &home, "out/secret").is_err());
    }
}
