//! The tree a UI renders: one directory level at a time (lazy — a person expands on click), every
//! entry including hidden ones, with the two facts a file browser colours by: is it ignored, and
//! what does git say about it. Both come from ONE `git status` per request, never a process per
//! entry — a workspace directory has thousands.
use super::git::{self, Status};
use crate::paths::confine;
use crate::tools::ToolError;
use std::path::{Path, PathBuf};

pub const MAX_DEPTH: u8 = 3;
pub const MAX_ENTRIES: usize = 5_000;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    pub name: String,
    pub kind: &'static str,
    pub size: u64,
    pub mtime: u64,
    pub ignored: bool,
    pub git: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<Entry>>,
}

fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

/// One letter for a path: the worktree column when set, else the index column, `?` untracked;
/// a directory is `M` when anything below it is dirty and `?` when everything below it is new.
fn letter(st: &Status, rel: &str, is_dir: bool) -> String {
    let mut any = false;
    let mut all_new = true;
    for c in &st.changes {
        let hit = c.path == rel || (is_dir && c.path.starts_with(rel) && c.path[rel.len()..].starts_with('/'));
        if !hit {
            continue;
        }
        if !is_dir {
            let l = if c.worktree != '.' { c.worktree } else { c.index };
            return l.to_string();
        }
        any = true;
        if c.worktree != '?' {
            all_new = false;
        }
    }
    match (is_dir, any, all_new) {
        (true, true, true) => "?".into(),
        (true, true, false) => "M".into(),
        _ => String::new(),
    }
}

/// `--ignored=matching` names a file, or a whole directory with a trailing `/`.
fn ignored(st: &Status, rel: &str, is_dir: bool) -> bool {
    if rel == ".git" || rel.starts_with(".git/") {
        return true;
    }
    let as_dir = format!("{rel}/");
    st.ignored.iter().any(|i| *i == rel || (is_dir && *i == as_dir) || (i.ends_with('/') && rel.starts_with(i.as_str())))
}

fn read_level(dir: &Path, root: &Path, st: &Status, depth: u8, budget: &mut usize) -> std::io::Result<(Vec<Entry>, bool)> {
    let mut rows: Vec<(std::fs::DirEntry, std::fs::Metadata)> = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let m = e.metadata()?; // symlink_metadata semantics: `DirEntry::metadata` does not follow
        rows.push((e, m));
    }
    rows.sort_by(|a, b| b.1.is_dir().cmp(&a.1.is_dir()).then_with(|| a.0.file_name().cmp(&b.0.file_name())));
    let mut out = Vec::new();
    let mut truncated = false;
    for (e, m) in rows {
        if *budget == 0 {
            truncated = true;
            break;
        }
        *budget -= 1;
        let path = e.path();
        let rel = path.strip_prefix(root).ok().map(|r| r.to_string_lossy().into_owned());
        let kind = if m.is_symlink() { "symlink" } else if m.is_dir() { "dir" } else { "file" };
        let is_dir = kind == "dir";
        let (git, ign) = match &rel {
            Some(r) if st.repo => (letter(st, r, is_dir), ignored(st, r, is_dir)),
            Some(r) => (String::new(), r == ".git" || r.starts_with(".git/")),
            None => (String::new(), false),
        };
        let mut entry = Entry {
            name: e.file_name().to_string_lossy().into_owned(),
            kind,
            size: if is_dir { 0 } else { m.len() },
            mtime: mtime_secs(&m),
            ignored: ign,
            git,
            target: if kind == "symlink" { std::fs::read_link(&path).ok().map(|t| t.to_string_lossy().into_owned()) } else { None },
            entries: None,
        };
        if is_dir && depth > 1 && !ign {
            // An ignored directory (`.cache`, `graft`, `node_modules`) is shown but never descended
            // into unasked: it is where the thousands of entries live.
            let (kids, t) = read_level(&path, root, st, depth - 1, budget)?;
            truncated |= t;
            entry.entries = Some(kids);
        }
        out.push(entry);
    }
    Ok((out, truncated))
}

/// The entries under `path`, `depth` levels deep (1..=MAX_DEPTH). Answers the resolved directory,
/// the rows and whether the entry cap cut the answer short.
pub async fn tree(root: &Path, home: &Path, path: &str, depth: u8) -> Result<(PathBuf, Vec<Entry>, bool), ToolError> {
    if !(1..=MAX_DEPTH).contains(&depth) {
        return Err(ToolError::Invalid(format!("depth: 1..={MAX_DEPTH}")));
    }
    let dir = confine(root, home, path)?;
    if !dir.is_dir() {
        return Err(ToolError::Failed(format!("{}: not a directory", dir.display())));
    }
    let st = git::status(root, true).await.map_err(ToolError::Failed)?;
    let (root, dir2) = (root.to_path_buf(), dir.clone());
    let (entries, truncated) = tokio::task::spawn_blocking(move || {
        let mut budget = MAX_ENTRIES;
        read_level(&dir2, &root, &st, depth, &mut budget)
    })
    .await
    .map_err(|e| ToolError::Failed(e.to_string()))?
    .map_err(|e| ToolError::Failed(format!("{}: {e}", dir.display())))?;
    Ok((dir, entries, truncated))
}

/// One row for one path, or `None` when nothing is there.
pub async fn stat(root: &Path, home: &Path, path: &str) -> Result<Option<Entry>, ToolError> {
    let p = confine(root, home, path)?;
    let Ok(m) = std::fs::symlink_metadata(&p) else { return Ok(None) };
    let st = git::status(root, true).await.map_err(ToolError::Failed)?;
    let rel = p.strip_prefix(root).ok().map(|r| r.to_string_lossy().into_owned());
    let kind = if m.is_symlink() { "symlink" } else if m.is_dir() { "dir" } else { "file" };
    let is_dir = kind == "dir";
    let (git, ign) = match &rel {
        Some(r) if st.repo => (letter(&st, r, is_dir), ignored(&st, r, is_dir)),
        Some(r) => (String::new(), r == ".git"),
        None => (String::new(), false),
    };
    Ok(Some(Entry {
        name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        kind,
        size: if is_dir { 0 } else { m.len() },
        mtime: mtime_secs(&m),
        ignored: ign,
        git,
        target: if kind == "symlink" { std::fs::read_link(&p).ok().map(|t| t.to_string_lossy().into_owned()) } else { None },
        entries: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".cache/x")).unwrap();
        let sh = |args: &[&str]| {
            let o = std::process::Command::new("git").args(args).current_dir(&root).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join(".gitignore"), ".cache/\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "hi\n").unwrap();
        std::fs::write(root.join(".cache/x/blob"), "x").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "one"]);
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(root.join("src/new.rs"), "\n").unwrap();
        std::os::unix::fs::symlink("src/lib.rs", root.join("link")).unwrap();
        (tmp, root)
    }

    #[tokio::test]
    async fn one_level_is_dirs_first_with_ignored_flags_and_rolled_up_letters() {
        let (tmp, root) = repo();
        let home = tmp.path().canonicalize().unwrap();
        let (_, rows, truncated) = tree(&root, &home, ".", 1).await.unwrap();
        assert!(!truncated);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec![".cache", ".git", "src", ".gitignore", "README.md", "link"]);
        let by = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        assert!(by(".cache").ignored && by(".git").ignored && !by("src").ignored);
        assert_eq!(by("src").git, "M", "a dirty file below rolls up");
        assert_eq!(by("README.md").git, "");
        assert_eq!(by("link").kind, "symlink");
        assert_eq!(by("link").target.as_deref(), Some("src/lib.rs"));
        assert!(by("src").entries.is_none(), "depth 1 carries no children");
    }

    #[tokio::test]
    async fn depth_two_nests_children_skips_ignored_dirs_and_the_bound_holds() {
        let (tmp, root) = repo();
        let home = tmp.path().canonicalize().unwrap();
        let (_, rows, _) = tree(&root, &home, ".", 2).await.unwrap();
        let src = rows.iter().find(|r| r.name == "src").unwrap();
        let kids = src.entries.as_ref().unwrap();
        assert_eq!(kids.iter().map(|k| (k.name.as_str(), k.git.as_str())).collect::<Vec<_>>(), vec![("lib.rs", "M"), ("new.rs", "?")]);
        assert!(rows.iter().find(|r| r.name == ".cache").unwrap().entries.is_none(), "ignored dirs are not descended");
        assert!(matches!(tree(&root, &home, ".", 0).await, Err(ToolError::Invalid(_))));
        assert!(matches!(tree(&root, &home, ".", 4).await, Err(ToolError::Invalid(_))));
        assert!(matches!(tree(&root, &home, "/etc", 1).await, Err(ToolError::Denied(_))));
    }

    #[tokio::test]
    async fn stat_answers_one_row_or_none_and_outside_a_repo_nothing_is_ignored() {
        let (tmp, root) = repo();
        let home = tmp.path().canonicalize().unwrap();
        let e = stat(&root, &home, "src/lib.rs").await.unwrap().unwrap();
        assert_eq!((e.kind, e.git.as_str(), e.size), ("file", "M", 20));
        assert!(stat(&root, &home, "nope").await.unwrap().is_none());
        let plain = home.join("plain");
        std::fs::create_dir_all(plain.join("d")).unwrap();
        std::fs::write(plain.join("f"), "1").unwrap();
        let (_, rows, _) = tree(&plain, &home, ".", 1).await.unwrap();
        assert!(rows.iter().all(|r| !r.ignored && r.git.is_empty()));
    }
}
