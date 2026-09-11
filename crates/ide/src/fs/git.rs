//! git as the workspace sees it, through the `git` binary in the workspace directory — never a
//! library: the worker's rule (libgit2 speaks no protocol v2) and one less thing to keep in step
//! with the `git` a person runs over ssh. Every call is one process, bounded to ten seconds; a
//! directory that is not a repository is an ANSWER (`repo: false`), not an error, because a fresh
//! workspace before its first `git init` is a normal thing to render.
use std::path::Path;
use std::process::Output;
use std::time::Duration;
use tokio::process::Command;

pub const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// The tree with nothing in it, so a repository with no commit yet still has a base to diff against.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Clone, serde::Serialize)]
pub struct Change {
    pub path: String,
    pub index: char,
    pub worktree: char,
    pub renamed_from: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    pub repo: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<Change>,
    /// `--ignored=matching` rows: a file, or a directory with a trailing `/` when all of it is ignored.
    #[serde(skip)]
    pub ignored: Vec<String>,
}

impl Default for Change {
    fn default() -> Self {
        Change { path: String::new(), index: '.', worktree: '.', renamed_from: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Against {
    /// Worktree against the last commit: what the person changed.
    Head,
    /// Worktree against the index.
    Index,
    /// Index against the last commit.
    Staged,
}

impl Against {
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s.unwrap_or("HEAD") {
            "HEAD" | "head" => Some(Against::Head),
            "index" => Some(Against::Index),
            "staged" => Some(Against::Staged),
            _ => None,
        }
    }
}

async fn git(root: &Path, args: &[&str]) -> Result<Output, String> {
    let run = Command::new("git").args(["--no-optional-locks", "-c", "core.quotepath=off"]).args(args).current_dir(root).env("GIT_TERMINAL_PROMPT", "0").output();
    match tokio::time::timeout(GIT_TIMEOUT, run).await {
        Ok(Ok(o)) => Ok(o),
        Ok(Err(e)) => Err(format!("git: {e}")),
        Err(_) => Err(format!("git {} took longer than {} s", args.first().unwrap_or(&""), GIT_TIMEOUT.as_secs())),
    }
}

fn not_a_repo(o: &Output) -> bool {
    // `status` says "fatal: not a git repository" (128); `diff` says "warning: Not a git
    // repository" and exits 129 — one predicate for both.
    !o.status.success() && String::from_utf8_lossy(&o.stderr).to_ascii_lowercase().contains("not a git repository")
}

/// `git status --porcelain=v2 --branch -z`, the one listing everything else here derives from.
pub async fn status(root: &Path, with_ignored: bool) -> Result<Status, String> {
    // `-uall`: an untracked directory is listed file by file, so a tree can letter each entry and
    // the changes panel can count each new file rather than showing one `src/` row.
    let mut args = vec!["status", "--porcelain=v2", "--branch", "-z", "-uall"];
    if with_ignored {
        args.push("--ignored=matching");
    }
    let o = git(root, &args).await?;
    if not_a_repo(&o) {
        return Ok(Status::default());
    }
    if !o.status.success() {
        return Err(format!("git status: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(parse_porcelain_v2(&o.stdout))
}

/// Pure: records are NUL-separated; a rename (`2 …`) carries its ORIGINAL path as the next record.
pub fn parse_porcelain_v2(bytes: &[u8]) -> Status {
    let mut st = Status { repo: true, ..Status::default() };
    let recs: Vec<&str> = bytes.split(|b| *b == 0).map(|r| std::str::from_utf8(r).unwrap_or("")).collect();
    let mut i = 0;
    while i < recs.len() {
        let r = recs[i];
        i += 1;
        if let Some(h) = r.strip_prefix("# ") {
            let (k, v) = h.split_once(' ').unwrap_or((h, ""));
            match k {
                "branch.oid" if v != "(initial)" => st.head = Some(v.to_string()),
                "branch.head" if v != "(detached)" => st.branch = Some(v.to_string()),
                "branch.upstream" => st.upstream = Some(v.to_string()),
                "branch.ab" => {
                    for part in v.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            st.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            st.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let mut f = r.splitn(2, ' ');
        let (kind, rest) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
        match kind {
            "?" => st.changes.push(Change { path: rest.to_string(), index: '?', worktree: '?', renamed_from: None }),
            "!" => st.ignored.push(rest.to_string()),
            "1" | "2" | "u" => {
                let fields: Vec<&str> = rest.splitn(if kind == "1" { 8 } else if kind == "2" { 9 } else { 10 }, ' ').collect();
                let xy = fields.first().copied().unwrap_or("..");
                let path = fields.last().copied().unwrap_or("").to_string();
                let mut xy = xy.chars();
                let (index, worktree) = (xy.next().unwrap_or('.'), xy.next().unwrap_or('.'));
                let renamed_from = if kind == "2" {
                    let from = recs.get(i).copied().unwrap_or("").to_string();
                    i += 1;
                    Some(from)
                } else {
                    None
                };
                st.changes.push(Change { path, index, worktree, renamed_from });
            }
            _ => {}
        }
    }
    st
}

/// Per-path line counts of the worktree against HEAD (the empty tree before the first commit).
/// `None` is a binary file. Renames come back under the NEW path.
pub async fn numstat(root: &Path) -> Result<Vec<(String, Option<(u32, u32)>)>, String> {
    let mut o = git(root, &["diff", "--numstat", "-M", "-z", "HEAD"]).await?;
    if not_a_repo(&o) {
        return Ok(Vec::new());
    }
    if !o.status.success() {
        o = git(root, &["diff", "--numstat", "-M", "-z", EMPTY_TREE]).await?;
        if !o.status.success() {
            return Err(format!("git diff --numstat: {}", String::from_utf8_lossy(&o.stderr).trim()));
        }
    }
    Ok(parse_numstat(&o.stdout))
}

pub fn parse_numstat(bytes: &[u8]) -> Vec<(String, Option<(u32, u32)>)> {
    let recs: Vec<&str> = bytes.split(|b| *b == 0).map(|r| std::str::from_utf8(r).unwrap_or("")).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < recs.len() {
        let r = recs[i];
        i += 1;
        if r.is_empty() {
            continue;
        }
        let mut parts = r.splitn(3, '\t');
        let a = parts.next().unwrap_or("");
        let d = parts.next().unwrap_or("");
        let mut path = parts.next().unwrap_or("").to_string();
        if path.is_empty() {
            // A rename: the old and the new path follow as two records; the new one is the file now.
            i += 1;
            path = recs.get(i).copied().unwrap_or("").to_string();
            i += 1;
        }
        let counts = match (a.parse::<u32>(), d.parse::<u32>()) {
            (Ok(a), Ok(d)) => Some((a, d)),
            _ => None,
        };
        out.push((path, counts));
    }
    out
}

/// The bytes of `rel` at a ref (`index` for the staged copy). `None`: not there at that ref.
pub async fn show(root: &Path, at: &str, rel: &str) -> Result<Option<Vec<u8>>, String> {
    if at.starts_with('-') {
        return Err("at: a ref never starts with -".into());
    }
    let spec = if at == "index" { format!(":{rel}") } else { format!("{at}:{rel}") };
    let o = git(root, &["show", &spec]).await?;
    if o.status.success() {
        Ok(Some(o.stdout))
    } else if o.status.code() == Some(128) {
        Ok(None)
    } else {
        Err(format!("git show: {}", String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// A unified diff. An untracked file diffs against `/dev/null`, so a new file renders as all
/// additions. Answers the patch and whether git called it binary.
pub async fn diff(root: &Path, rel: Option<&str>, against: Against, untracked: bool) -> Result<(String, bool), String> {
    let o = if untracked {
        let rel = rel.ok_or("an untracked diff needs a path")?;
        // `--no-index` exits 1 when the files differ — that is the success case here.
        git(root, &["diff", "--no-index", "--", "/dev/null", rel]).await?
    } else {
        let mut args = vec!["diff"];
        match against {
            Against::Head => args.push("HEAD"),
            Against::Index => {}
            Against::Staged => args.push("--cached"),
        }
        if let Some(r) = rel {
            args.push("--");
            args.push(r);
        }
        let mut o = git(root, &args).await?;
        if !o.status.success() && against != Against::Index && String::from_utf8_lossy(&o.stderr).contains("bad revision 'HEAD'") {
            // No commit yet: everything is new against the empty tree.
            let mut args = vec!["diff", if against == Against::Staged { "--cached" } else { "" }, EMPTY_TREE];
            args.retain(|a| !a.is_empty());
            if let Some(r) = rel {
                args.push("--");
                args.push(r);
            }
            o = git(root, &args).await?;
        }
        o
    };
    if not_a_repo(&o) {
        return Ok((String::new(), false));
    }
    match o.status.code() {
        Some(0) | Some(1) => {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            let binary = text.lines().any(|l| l.starts_with("Binary files ") && l.ends_with(" differ"));
            Ok((text, binary))
        }
        _ => Err(format!("git diff: {}", String::from_utf8_lossy(&o.stderr).trim())),
    }
}

pub async fn stash_count(root: &Path) -> u32 {
    match git(root, &["stash", "list", "-z"]).await {
        Ok(o) if o.status.success() => o.stdout.iter().filter(|b| **b == 0).count() as u32,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_v2_headers_changes_untracked_and_a_rename() {
        let raw = b"# branch.oid 2261c195\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +1 -2\0\
1 .M N... 100644 100644 100644 abc def Cargo.toml\0\
2 R. N... 100644 100644 100644 abc abc R100 new.rs\0old.rs\0\
? scratch.txt\0! .cache/\0";
        let s = parse_porcelain_v2(raw);
        assert!(s.repo);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.head.as_deref(), Some("2261c195"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (1, 2));
        assert_eq!(s.changes.len(), 3);
        assert_eq!((s.changes[0].path.as_str(), s.changes[0].index, s.changes[0].worktree), ("Cargo.toml", '.', 'M'));
        assert_eq!((s.changes[1].path.as_str(), s.changes[1].index, s.changes[1].renamed_from.as_deref()), ("new.rs", 'R', Some("old.rs")));
        assert_eq!((s.changes[2].path.as_str(), s.changes[2].worktree), ("scratch.txt", '?'));
        assert_eq!(s.ignored, vec![".cache/"]);
    }

    #[test]
    fn a_detached_head_has_no_branch_and_an_empty_listing_is_clean() {
        let s = parse_porcelain_v2(b"# branch.oid abc\0# branch.head (detached)\0");
        assert_eq!(s.branch, None);
        assert_eq!(s.head.as_deref(), Some("abc"));
        assert!(s.changes.is_empty());
        let s = parse_porcelain_v2(b"# branch.oid (initial)\0# branch.head main\0");
        assert_eq!(s.head, None);
    }

    #[test]
    fn numstat_counts_marks_binary_and_follows_a_rename_to_its_new_name() {
        let raw = b"3\t1\tCargo.toml\x00-\t-\tlogo.png\x005\t0\t\x00old.rs\x00new.rs\x00";
        let n = parse_numstat(raw);
        assert_eq!(n, vec![("Cargo.toml".to_string(), Some((3, 1))), ("logo.png".to_string(), None), ("new.rs".to_string(), Some((5, 0)))]);
    }

    #[test]
    fn against_parses_the_three_names_and_refuses_the_rest() {
        assert_eq!(Against::parse(None), Some(Against::Head));
        assert_eq!(Against::parse(Some("index")), Some(Against::Index));
        assert_eq!(Against::parse(Some("staged")), Some(Against::Staged));
        assert_eq!(Against::parse(Some("main")), None);
    }

    /// A real repository: not-a-repo, then a commit, an edit, an untracked file, `show` and `diff`.
    #[tokio::test]
    async fn a_real_repository_answers_status_show_and_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(!status(&root, false).await.unwrap().repo);
        assert_eq!(diff(&root, None, Against::Head, false).await.unwrap(), (String::new(), false));
        let sh = |args: &[&str]| {
            let o = std::process::Command::new("git").args(args).current_dir(&root).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        sh(&["add", "a.txt"]);
        sh(&["commit", "-q", "-m", "one"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        let s = status(&root, false).await.unwrap();
        assert!(s.repo);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.changes.iter().map(|c| (c.path.as_str(), c.worktree)).collect::<Vec<_>>(), vec![("a.txt", 'M'), ("b.txt", '?')]);
        assert_eq!(show(&root, "HEAD", "a.txt").await.unwrap(), Some(b"one\n".to_vec()));
        assert_eq!(show(&root, "HEAD", "b.txt").await.unwrap(), None);
        let (patch, binary) = diff(&root, Some("a.txt"), Against::Head, false).await.unwrap();
        assert!(patch.contains("+two") && !binary, "{patch}");
        let (patch, _) = diff(&root, Some("b.txt"), Against::Head, true).await.unwrap();
        assert!(patch.contains("+new"), "{patch}");
        assert_eq!(numstat(&root).await.unwrap(), vec![("a.txt".to_string(), Some((1, 0)))]);
    }
}
