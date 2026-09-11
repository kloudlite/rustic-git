//! git as the workspace sees it, in Rust through gitoxide — no `git` process, so the routes are one
//! process end to end and testable against a tempdir. Everything here is blocking work behind
//! `spawn_blocking`; a directory that is not a repository is an ANSWER (`repo: false`), not an
//! error, because a fresh workspace before its first `git init` is a normal thing to render.
use gix::bstr::{BStr, ByteSlice};
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, InternedInput, UnifiedDiff};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// Ignored paths: a file, or a directory with a trailing `/` when all of it is ignored.
    #[serde(skip)]
    pub ignored: Vec<String>,
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

/// The workspace directory itself, never a parent: `discover` would climb to a repository in the
/// home and render the wrong tree.
fn open(root: &Path) -> Option<gix::Repository> {
    gix::open(root).ok()
}

async fn blocking<T: Send + 'static>(root: &Path, f: impl FnOnce(PathBuf) -> Result<T, String> + Send + 'static) -> Result<T, String> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || f(root)).await.map_err(|e| format!("git task: {e}"))?
}

fn s(b: &BStr) -> String {
    b.to_str_lossy().into_owned()
}

/// The blob `rel` names at `at` (`HEAD`, any rev spec, or `index`); `None` when absent there.
fn blob_at(repo: &gix::Repository, at: &str, rel: &str) -> Option<Vec<u8>> {
    if at == "index" {
        let index = repo.index_or_empty().ok()?;
        let entry = index.entry_by_path(rel.into())?;
        return repo.find_object(entry.id).ok().map(|o| o.data.clone());
    }
    let id = repo.rev_parse_single(format!("{at}:{rel}").as_bytes().as_bstr()).ok()?;
    repo.find_object(id).ok().filter(|o| o.kind == gix::object::Kind::Blob).map(|o| o.data.clone())
}

fn worktree_bytes(root: &Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(root.join(rel)).ok()
}

fn is_binary(b: &[u8]) -> bool {
    b.iter().take(8192).any(|x| *x == 0)
}

fn walk_status(repo: &gix::Repository, with_ignored: bool) -> Result<(Vec<Change>, Vec<String>), String> {
    use gix::diff::index::Change as Tree;
    use gix::dir::entry::{Kind as DiskKind, Status as DirStatus};
    use gix::status::index_worktree::Item as Wt;
    use gix::status::{index_worktree::iter::Summary, UntrackedFiles};

    let mut plat = repo.status(gix::progress::Discard).map_err(|e| format!("git status: {e}"))?.untracked_files(UntrackedFiles::Files);
    if with_ignored {
        plat = plat.dirwalk_options(|o| o.emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory)));
    }
    let mut rows: BTreeMap<String, Change> = BTreeMap::new();
    let mut ignored = Vec::new();
    fn row(rows: &mut BTreeMap<String, Change>, path: String) -> &mut Change {
        rows.entry(path.clone()).or_insert(Change { path, index: '.', worktree: '.', renamed_from: None })
    }
    for item in plat.into_iter(Vec::new()).map_err(|e| format!("git status: {e}"))? {
        let item = item.map_err(|e| format!("git status: {e}"))?;
        match item {
            gix::status::Item::TreeIndex(t) => match t {
                Tree::Addition { location, .. } => row(&mut rows, s(&location)).index = 'A',
                Tree::Deletion { location, .. } => row(&mut rows, s(&location)).index = 'D',
                Tree::Modification { location, .. } => row(&mut rows, s(&location)).index = 'M',
                Tree::Rewrite { source_location, location, copy, .. } => {
                    let r = row(&mut rows, s(&location));
                    r.index = if copy { 'C' } else { 'R' };
                    r.renamed_from = Some(s(&source_location));
                }
            },
            gix::status::Item::IndexWorktree(w) => {
                if let Wt::DirectoryContents { entry, .. } = &w {
                    if matches!(entry.status, DirStatus::Ignored(_)) {
                        let mut p = s(entry.rela_path.as_ref());
                        if entry.disk_kind == Some(DiskKind::Directory) {
                            p.push('/');
                        }
                        ignored.push(p);
                        continue;
                    }
                }
                // `None` is an entry that only needs its stat refreshed — not a change.
                let Some(sum) = w.summary() else { continue };
                match w {
                    Wt::Modification { rela_path, .. } => {
                        let r = row(&mut rows, s(rela_path.as_ref()));
                        match sum {
                            Summary::Removed => r.worktree = 'D',
                            Summary::TypeChange => r.worktree = 'T',
                            Summary::Conflict => r.worktree = 'U',
                            // Intent-to-add is an index fact with nothing in the tree yet: ` A`.
                            Summary::IntentToAdd => r.index = 'A',
                            _ => r.worktree = 'M',
                        }
                    }
                    Wt::DirectoryContents { entry, .. } => {
                        let r = row(&mut rows, s(entry.rela_path.as_ref()));
                        r.index = '?';
                        r.worktree = '?';
                    }
                    Wt::Rewrite { source, dirwalk_entry, copy, .. } => {
                        let r = row(&mut rows, s(dirwalk_entry.rela_path.as_ref()));
                        r.worktree = if copy { 'C' } else { 'R' };
                        r.renamed_from = Some(s(source.rela_path()));
                    }
                }
            }
        }
    }
    Ok((rows.into_values().collect(), ignored))
}

/// Branch, head, upstream and its distance, plus every change — the one listing everything
/// else here derives from.
pub async fn status(root: &Path, with_ignored: bool) -> Result<Status, String> {
    blocking(root, move |root| {
        let Some(repo) = open(&root) else { return Ok(Status::default()) };
        let mut st = Status { repo: true, ..Status::default() };
        let head = repo.head().map_err(|e| format!("HEAD: {e}"))?;
        st.head = head.id().map(|id| id.to_string());
        st.branch = head.referent_name().map(|n| s(n.shorten()));
        if let (Some(name), Some(head_id)) = (head.referent_name(), head.id()) {
            if let Some(Ok(up)) = repo.branch_remote_tracking_ref_name(name, gix::remote::Direction::Fetch) {
                if let Ok(mut r) = repo.find_reference(up.as_ref()) {
                    if let Ok(up_id) = r.peel_to_id() {
                        st.upstream = Some(s(up.shorten()));
                        let count = |from: gix::ObjectId, hide: gix::ObjectId| -> u32 {
                            repo.rev_walk([from]).with_hidden([hide]).all().map(|w| w.filter(Result::is_ok).count() as u32).unwrap_or(0)
                        };
                        st.ahead = count(head_id.detach(), up_id.detach());
                        st.behind = count(up_id.detach(), head_id.detach());
                    }
                }
            }
        }
        let (changes, ignored) = walk_status(&repo, with_ignored)?;
        st.changes = changes;
        st.ignored = ignored;
        Ok(st)
    })
    .await
}

/// Added and removed lines per changed path, worktree against HEAD; `None` is a binary file. An
/// untracked file counts every line, a deleted one every line it had.
pub async fn numstat(root: &Path) -> Result<Vec<(String, Option<(u32, u32)>)>, String> {
    blocking(root, move |root| {
        let Some(repo) = open(&root) else { return Ok(Vec::new()) };
        let (changes, _) = walk_status(&repo, false)?;
        Ok(changes
            .iter()
            .map(|c| {
                let before = blob_at(&repo, "HEAD", c.renamed_from.as_deref().unwrap_or(&c.path)).unwrap_or_default();
                let after = worktree_bytes(&root, &c.path).unwrap_or_default();
                if is_binary(&before) || is_binary(&after) {
                    return (c.path.clone(), None);
                }
                // Counted off the same hunks `/fs/diff` renders, so the two never disagree.
                match unified(&c.path, Some(&before), Some(&after)) {
                    Ok((text, _)) => {
                        let (mut add, mut del) = (0u32, 0u32);
                        for l in text.lines() {
                            if l.starts_with("+++") || l.starts_with("---") {
                                continue;
                            }
                            match l.as_bytes().first() {
                                Some(b'+') => add += 1,
                                Some(b'-') => del += 1,
                                _ => {}
                            }
                        }
                        (c.path.clone(), Some((add, del)))
                    }
                    Err(_) => (c.path.clone(), None),
                }
            })
            .collect())
    })
    .await
}

/// The bytes of `rel` at a ref (`index` for the staged copy). `None`: not there at that ref.
pub async fn show(root: &Path, at: &str, rel: &str) -> Result<Option<Vec<u8>>, String> {
    let (at, rel) = (at.to_string(), rel.to_string());
    blocking(root, move |root| {
        let Some(repo) = open(&root) else { return Ok(None) };
        Ok(blob_at(&repo, &at, &rel))
    })
    .await
}

/// One file's unified diff, both sides in memory: `--- /dev/null` for a file that did not exist
/// (an untracked file renders as all additions), `+++ /dev/null` for one that is gone.
fn unified(path: &str, before: Option<&[u8]>, after: Option<&[u8]>) -> Result<(String, bool), String> {
    let (b, a) = (before.unwrap_or(b""), after.unwrap_or(b""));
    let mut out = format!("diff --git a/{path} b/{path}\n");
    if is_binary(b) || is_binary(a) {
        out.push_str(&format!("Binary files a/{path} and b/{path} differ\n"));
        return Ok((out, true));
    }
    out.push_str(&if before.is_some() { format!("--- a/{path}\n") } else { "--- /dev/null\n".to_string() });
    out.push_str(&if after.is_some() { format!("+++ b/{path}\n") } else { "+++ /dev/null\n".to_string() });
    let input = InternedInput::new(b, a);
    let diff = gix::diff::blob::Diff::compute(Algorithm::Myers, &input);
    let hunks = UnifiedDiff::new(&diff, &input, ConsumeBinaryHunk::new(String::new(), "\n"), ContextSize::symmetrical(3)).consume().map_err(|e| format!("diff: {e}"))?;
    out.push_str(&hunks);
    Ok((out, false))
}

/// A unified diff of one path, or of every change when `rel` is `None`. `_untracked` is decided
/// here from the sides themselves and kept only for the caller's signature.
pub async fn diff(root: &Path, rel: Option<&str>, against: Against, _untracked: bool) -> Result<(String, bool), String> {
    let rel = rel.map(str::to_string);
    blocking(root, move |root| {
        let Some(repo) = open(&root) else { return Ok((String::new(), false)) };
        let sides = |path: &str| -> (Option<Vec<u8>>, Option<Vec<u8>>) {
            match against {
                Against::Head => (blob_at(&repo, "HEAD", path), worktree_bytes(&root, path)),
                Against::Index => (blob_at(&repo, "index", path), worktree_bytes(&root, path)),
                Against::Staged => (blob_at(&repo, "HEAD", path), blob_at(&repo, "index", path)),
            }
        };
        let paths: Vec<String> = match rel {
            Some(r) => vec![r],
            None => walk_status(&repo, false)?
                .0
                .into_iter()
                .filter(|c| match against {
                    Against::Head => true,
                    Against::Index => c.worktree != '.',
                    Against::Staged => c.index != '.' && c.index != '?',
                })
                .map(|c| c.path)
                .collect(),
        };
        let mut out = String::new();
        let mut binary = false;
        for p in paths {
            let (before, after) = sides(&p);
            if before.is_none() && after.is_none() {
                continue;
            }
            if before == after {
                continue;
            }
            let (text, bin) = unified(&p, before.as_deref(), after.as_deref())?;
            out.push_str(&text);
            binary |= bin;
        }
        Ok((out, binary))
    })
    .await
}

/// Every entry in `refs/stash`'s reflog is one stash.
pub async fn stash_count(root: &Path) -> u32 {
    blocking(root, move |root| {
        let Some(repo) = open(&root) else { return Ok(0) };
        let Ok(r) = repo.find_reference("refs/stash") else { return Ok(0) };
        Ok(r.log_iter().all().ok().flatten().map(|it| it.count() as u32).unwrap_or(0))
    })
    .await
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(root: &Path, args: &[&str]) {
        let o = std::process::Command::new("git").args(args).current_dir(root).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    #[test]
    fn against_parses_the_three_names_and_refuses_the_rest() {
        assert_eq!(Against::parse(None), Some(Against::Head));
        assert_eq!(Against::parse(Some("index")), Some(Against::Index));
        assert_eq!(Against::parse(Some("staged")), Some(Against::Staged));
        assert_eq!(Against::parse(Some("main")), None);
    }

    #[test]
    fn a_unified_diff_has_the_headers_git_prints_and_marks_binary() {
        let (t, bin) = unified("a.txt", Some(b"one\ntwo\n"), Some(b"one\nTWO\n")).unwrap();
        assert!(t.starts_with("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ "), "{t}");
        assert!(t.contains("-two\n+TWO\n") && !bin, "{t}");
        let (t, _) = unified("n.txt", None, Some(b"new\n")).unwrap();
        assert!(t.contains("--- /dev/null\n+++ b/n.txt\n") && t.contains("+new"), "{t}");
        let (t, bin) = unified("b.bin", Some(b"\x00a"), Some(b"\x00b")).unwrap();
        assert!(bin && t.contains("Binary files"), "{t}");
    }

    /// A real repository (git makes the fixture, gix reads it): not-a-repo, a commit, an edit, an
    /// untracked file, a staged rename, `show`, `diff`, `numstat`.
    #[tokio::test]
    async fn a_real_repository_answers_status_show_diff_and_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(!status(&root, false).await.unwrap().repo);
        assert_eq!(diff(&root, None, Against::Head, false).await.unwrap(), (String::new(), false));
        sh(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("old.txt"), "keep me\nas is\nplease\n").unwrap();
        std::fs::write(root.join(".gitignore"), "junk/\n").unwrap();
        sh(&root, &["add", "-A"]);
        sh(&root, &["commit", "-q", "-m", "one"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        std::fs::create_dir_all(root.join("junk")).unwrap();
        std::fs::write(root.join("junk/x"), "x").unwrap();
        sh(&root, &["mv", "old.txt", "renamed.txt"]);
        let st = status(&root, true).await.unwrap();
        assert!(st.repo);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert!(st.head.is_some() && st.upstream.is_none());
        let rows: Vec<(&str, char, char, Option<&str>)> = st.changes.iter().map(|c| (c.path.as_str(), c.index, c.worktree, c.renamed_from.as_deref())).collect();
        assert_eq!(rows, vec![("a.txt", '.', 'M', None), ("b.txt", '?', '?', None), ("renamed.txt", 'R', '.', Some("old.txt"))], "{rows:?}");
        assert_eq!(st.ignored, vec!["junk/"]);
        assert_eq!(show(&root, "HEAD", "a.txt").await.unwrap(), Some(b"one\n".to_vec()));
        assert_eq!(show(&root, "index", "renamed.txt").await.unwrap(), Some(b"keep me\nas is\nplease\n".to_vec()));
        assert_eq!(show(&root, "HEAD", "b.txt").await.unwrap(), None);
        let (patch, binary) = diff(&root, Some("a.txt"), Against::Head, false).await.unwrap();
        assert!(patch.contains("+two") && !binary, "{patch}");
        let (patch, _) = diff(&root, Some("b.txt"), Against::Head, true).await.unwrap();
        assert!(patch.contains("--- /dev/null") && patch.contains("+new"), "{patch}");
        let (patch, _) = diff(&root, Some("a.txt"), Against::Staged, false).await.unwrap();
        assert_eq!(patch, "", "nothing staged for a.txt");
        let (patch, _) = diff(&root, None, Against::Head, false).await.unwrap();
        assert!(patch.contains("b/a.txt") && patch.contains("b/b.txt"), "{patch}");
        assert_eq!(numstat(&root).await.unwrap(), vec![("a.txt".to_string(), Some((1, 0))), ("b.txt".to_string(), Some((1, 0))), ("renamed.txt".to_string(), Some((0, 0)))]);
        assert_eq!(stash_count(&root).await, 0);
    }

    #[tokio::test]
    async fn a_detached_head_has_no_branch_and_an_upstream_is_measured() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        sh(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        sh(&root, &["add", "-A"]);
        sh(&root, &["commit", "-q", "-m", "one"]);
        // A fake upstream: the remote-tracking ref points one commit behind.
        sh(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        sh(&root, &["config", "branch.main.remote", "origin"]);
        sh(&root, &["config", "branch.main.merge", "refs/heads/main"]);
        sh(&root, &["config", "remote.origin.url", "https://example.invalid/x.git"]);
        sh(&root, &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        sh(&root, &["commit", "-qam", "two"]);
        let st = status(&root, false).await.unwrap();
        assert_eq!((st.branch.as_deref(), st.upstream.as_deref(), st.ahead, st.behind), (Some("main"), Some("origin/main"), 1, 0));
        sh(&root, &["checkout", "-q", "--detach", "HEAD~1"]);
        let st = status(&root, false).await.unwrap();
        assert_eq!(st.branch, None);
        assert!(st.head.is_some());
    }
}
