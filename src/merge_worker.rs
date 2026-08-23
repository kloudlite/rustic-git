//! Merges, performed with the real `git` binary, off the node that owns the repo.
//!
//! Everything a merge needs is already spoken over the git protocol — fetch the two branches,
//! combine them, push the result — so the merge does not have to happen where the database is.
//! That matters because a repo's database has exactly one legitimate opener (see `CLAUDE.md`),
//! and merging is the one piece of pull-request work that is unbounded: a three-way merge of a
//! large tree is real CPU and real disk, and doing it on the node serving pushes for that repo
//! makes every push wait behind it.
//!
//! So the split is: the owner records state and serves the protocol, and this — running in the
//! worker — does the work against a bare clone cache over HTTP, authenticated as a peer. The push
//! goes back through `receive-pack`, which is what keeps BRANCH PROTECTION in force: a merge is
//! refused by exactly the rule that refuses a force push, because it *is* a push.
//!
//! `git` itself, not a library. An embedded libgit2 was tried and abandoned: this server serves
//! `upload-pack` over protocol **v2 only** (see `http::info_refs`), and libgit2 1.9 has no v2
//! support at all — it cannot fetch from us. The binary is also the only implementation whose
//! merge semantics are the ones everybody's local `git merge` produces, which for a result that
//! lands in someone's history is the whole point.
//!
//! Nothing here opens a database, and nothing here is async: it is a sequence of subprocesses, so
//! callers run it on a blocking thread.

use crate::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One merge to perform, as the owner handed it over on `claim`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub owner: String,
    pub name: String,
    pub number: i64,
    /// `fast-forward` | `squash` | `merge` | `rebase`.
    pub strategy: String,
    /// SHORT branch names, as the change stores them.
    pub base: String,
    pub head: String,
    pub title: String,
    #[serde(default)]
    pub requested_by: String,
}

/// How a merge ended, as the owner records it on `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeState {
    Merged,
    /// The trees could not be combined without a person deciding what wins.
    Conflicts,
    /// Nothing was wrong with the merge itself — the fleet would not take it (a protection rule,
    /// a base that moved), or the strategy does not apply to these branches.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub state: OutcomeState,
    /// Written for the person waiting, so it is git's own words wherever git had any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The commit the base now points at. Only ever set alongside `Merged`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_tip: Option<String>,
}

impl Outcome {
    fn refused(why: impl Into<String>) -> Outcome {
        Outcome { state: OutcomeState::Refused, detail: Some(why.into()), new_tip: None }
    }
    fn conflicts(why: String) -> Outcome {
        Outcome { state: OutcomeState::Conflicts, detail: Some(why), new_tip: None }
    }
}

/// What a mergeability check concluded, as the owner records it on `mergeability`.
///
/// A trial merge answers only "would this combine": `fast_forward` is the owner's cheap ancestry
/// verdict and is always `false` here, because the owner only asks for a trial merge once it has
/// established the branches diverged.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub state: crate::directory::MergeableState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub fast_forward: bool,
}

/// Is there a `git` to run at all? Checked at worker startup so a missing binary is a loud line in
/// the log, not a merge that mysteriously refuses an hour later.
pub fn available() -> bool {
    Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// The sentence a change gets when this worker has no `git`. A refusal rather than a silent
/// stall: the person waiting can see it, and it names the thing an operator has to fix.
const NO_GIT: &str = "this worker has no git binary installed; a merge cannot be performed";

/// Where a repo's bare clone cache lives under the worker's cache directory.
pub fn cache_of(cache: &Path, owner: &str, name: &str) -> PathBuf {
    cache.join("merge").join(owner).join(format!("{name}.git"))
}

/// The stamp the cache prune reads. Written on every use rather than trusting the directory's own
/// mtime, which a fetch that changes nothing does not move.
const USED: &str = ".last-used";

/// Delete caches nothing has touched in `age`. A cache is a pure derivative of the fleet, so
/// losing one costs a fetch, never data.
///
/// ponytail: one flat sweep of `merge/*/*.git`, no size accounting — a single repo bigger than the
/// disk still fills it. Upgrade path: sort by size and evict to a byte budget.
pub fn prune(cache: &Path, age: std::time::Duration) -> usize {
    let mut gone = 0;
    let Ok(owners) = std::fs::read_dir(cache.join("merge")) else { return 0 };
    for owner in owners.flatten() {
        let Ok(repos) = std::fs::read_dir(owner.path()) else { continue };
        for repo in repos.flatten() {
            let stale = std::fs::metadata(repo.path().join(USED))
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().unwrap_or_default() > age)
                // No stamp at all is a cache from before this existed, or a half-made one.
                .unwrap_or(true);
            if stale && std::fs::remove_dir_all(repo.path()).is_ok() {
                gone += 1;
            }
        }
    }
    gone
}

// ---------------------------------------------------------------------------
// Running git.
//
// Two shapes, deliberately kept apart: a LOCAL command, whose argv is safe to put in an error
// message, and a NETWORKED one, whose argv carries the peer secret in `-c http.extraHeader` and
// must therefore never reach a log, an error or a panic.
// ---------------------------------------------------------------------------

fn out(cmd: &mut Command) -> Result<std::process::Output> {
    cmd.output().map_err(|e| crate::err(format!("git: {e}")))
}

/// The last thing git said, for the person waiting. Its last non-empty line, because git puts the
/// actual reason there and the lines above it are progress.
fn stderr_tail(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr)
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("git refused it and said nothing")
        .trim()
        .to_string()
}

fn local(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    out(Command::new("git").arg("-C").arg(dir).args(args))
}

/// A local command that must succeed, with its stdout trimmed. Naming the argv is safe here.
fn must(dir: &Path, args: &[&str]) -> Result<String> {
    let o = local(dir, args)?;
    if !o.status.success() {
        return Err(crate::err(format!("git {}: {}", args.join(" "), stderr_tail(&o))));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// A git command that talks to the fleet.
///
/// The peer secret rides in `-c http.extraHeader`, so NOTHING here may put the argv into an error,
/// a log line or a panic message. `x-rustic-git-peer` admits the request on the peer listener;
/// `x-rustic-git-owner` is the identity it is served as, and the git routes authorize it exactly
/// as they would a token for that owner (see `http::open`).
fn networked(dir: &Path, secret: &str, owner: &str, args: &[&str]) -> Result<std::process::Output> {
    out(Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", &format!("http.extraHeader={}: {secret}", crate::proxy::PEER_HEADER)])
        .args(["-c", &format!("http.extraHeader={}: {owner}", crate::proxy::OWNER_HEADER)])
        .args(args)
        // Never let git ask a human anything: there is no terminal here and nobody to answer, so a
        // prompt would hang this lane until its heartbeat went stale.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", ""))
}

/// The bare cache for this repo, brought up to date with the fleet.
///
/// `init --bare` + `fetch` rather than `clone`-then-`fetch`: one code path instead of two, and a
/// bare clone has no `remote.origin.fetch` to reuse anyway. The refspec is forced and pruned, so
/// the cache is a MIRROR of the fleet's branches — never a merge of two histories of them, which
/// is what a plain fetch of a rewritten branch would leave behind.
fn sync(cache: &Path, upstream: &str, secret: &str, job: &Job) -> Result<(PathBuf, String)> {
    let dir = cache_of(cache, &job.owner, &job.name);
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(&dir).map_err(|e| crate::err(format!("{}: {e}", dir.display())))?;
        let o = out(Command::new("git").args(["init", "--bare", "-q"]).arg(&dir))?;
        if !o.status.success() {
            return Err(crate::err(format!("git init --bare: {}", stderr_tail(&o))));
        }
    }
    let _ = std::fs::write(dir.join(USED), b"");
    let url = format!("{}/{}/{}.git", upstream.trim_end_matches('/'), job.owner, job.name);
    fetch(&dir, &url, secret, &job.owner)?;
    Ok((dir, url))
}

fn fetch(dir: &Path, url: &str, secret: &str, owner: &str) -> Result<()> {
    let o = networked(
        dir,
        secret,
        owner,
        &["fetch", "--quiet", "--prune", "--force", url, "+refs/heads/*:refs/heads/*"],
    )?;
    if !o.status.success() {
        // The URL is safe to name — it is the caller's own configuration; the argv is not.
        return Err(crate::err(format!("fetching {url}: {}", stderr_tail(&o))));
    }
    Ok(())
}

/// The conflicted paths named by `git merge-tree`'s output, in order and without repeats.
///
/// The format (git ≥ 2.38, with `-z`) is NUL-separated records: the tree oid first, then one
/// `<mode> <object> <stage>\t<path>` record per conflicted path PER STAGE, then an empty record,
/// then informational prose. Parsing the prose would invent files (a message naming a filename is
/// not a conflicted file) and counting records would triple the count, which is why this stops at
/// the empty record and dedupes.
pub fn conflicted_paths(stdout: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for rec in stdout.split(|b| *b == 0).skip(1) {
        if rec.is_empty() {
            break;
        }
        let rec = String::from_utf8_lossy(rec);
        let Some((meta, path)) = rec.split_once('\t') else { continue };
        // Three fields, the last a single stage digit. Checked so a record shape we do not
        // recognise is skipped rather than yielding a nonsense path.
        let fields: Vec<&str> = meta.split(' ').collect();
        if fields.len() != 3 || !matches!(fields[2], "1" | "2" | "3") {
            continue;
        }
        if !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out
}

/// "conflicts in: a, b (+3 more)" — the sentence the person waiting is shown.
pub fn conflict_detail(paths: &[String]) -> String {
    const SHOWN: usize = 2;
    if paths.is_empty() {
        return "the branches conflict".to_string();
    }
    let head = paths.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    match paths.len().saturating_sub(SHOWN) {
        0 => format!("conflicts in: {head}"),
        n => format!("conflicts in: {head} (+{n} more)"),
    }
}

/// The three-way merge, as a tree and nothing else. `Ok(tree oid)` or the conflict outcome —
/// no object is written and no ref moves either way, so a conflict leaves nothing to clean up.
///
/// Also the mergeability check: `check` calls it for exactly the same answer, which is what makes
/// "can this merge" and "merge this" agree by construction rather than by two implementations
/// happening to match.
fn tree_merge(dir: &Path, base: &str, head: &str) -> Result<std::result::Result<String, Outcome>> {
    let o = local(dir, &["merge-tree", "--write-tree", "--messages", "-z", base, head])?;
    if o.status.success() {
        let tree = o.stdout.split(|b| *b == 0).next().unwrap_or_default();
        return Ok(Ok(String::from_utf8_lossy(tree).trim().to_string()));
    }
    // Exit 1 is "they conflict"; anything else is git failing, which is not an answer.
    if o.status.code() != Some(1) {
        return Err(crate::err(format!("merge-tree: {}", stderr_tail(&o))));
    }
    Ok(Err(Outcome::conflicts(conflict_detail(&conflicted_paths(&o.stdout)))))
}

/// Perform one merge and say how it went.
///
/// `Err` is reserved for "could not find out" — the cache is unwritable, or the fleet was
/// unreachable. Those must NOT be reported as an outcome: the job stays claimed, its lease lapses,
/// and the owner re-announces it. Everything the merge itself can decide comes back as an
/// `Outcome`, including a missing `git`, which is a refusal a person can read and act on.
///
/// Blocking: this shells out. Callers hold it off the runtime.
pub fn run(job: &Job, cache: &Path, upstream: &str, secret: &str) -> Result<Outcome> {
    if !available() {
        return Ok(Outcome::refused(NO_GIT));
    }
    let (dir, url) = sync(cache, upstream, secret, job)?;
    let oid = |branch: &str| -> Result<Option<String>> {
        let o = local(&dir, &["rev-parse", "--verify", &format!("refs/heads/{branch}^{{commit}}")])?;
        Ok(o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string()))
    };
    let (Some(base_oid), Some(head_oid)) = (oid(&job.base)?, oid(&job.head)?) else {
        return Ok(Outcome::refused("one of the branches is gone"));
    };

    let new_tip = match job.strategy.as_str() {
        "fast-forward" => {
            if !local(&dir, &["merge-base", "--is-ancestor", &base_oid, &head_oid])?.status.success()
            {
                return Ok(Outcome::refused(
                    "not a fast-forward — use merge or squash, or rebase and push again",
                ));
            }
            head_oid.clone()
        }
        "merge" | "squash" => {
            let tree = match tree_merge(&dir, &base_oid, &head_oid)? {
                Ok(t) => t,
                Err(o) => return Ok(o),
            };
            let parents: &[&str] =
                if job.strategy == "squash" { &[&base_oid] } else { &[&base_oid, &head_oid] };
            commit_tree(&dir, &tree, parents, &head_oid, &format!("{} (#{})", job.title, job.number))?
        }
        "rebase" => match rebase(&dir, &base_oid, &head_oid)? {
            Ok(t) => t,
            Err(o) => return Ok(o),
        },
        _ => return Ok(Outcome::refused("strategy must be fast-forward, squash, merge or rebase")),
    };

    // `--force-with-lease` against the tip this merge was computed FROM: a base that moved while
    // we were merging must lose the push rather than have this land on top of a state it never
    // saw. The lease is the only thing standing between a slow merge and someone else's commit
    // being buried — the fleet's own compare-and-swap sees a fast-forward and would allow it.
    let o = networked(
        &dir,
        secret,
        &job.owner,
        &[
            "push",
            "--quiet",
            &format!("--force-with-lease=refs/heads/{}:{base_oid}", job.base),
            &url,
            &format!("{new_tip}:refs/heads/{}", job.base),
        ],
    )?;
    if !o.status.success() {
        // A protection rule, or a base that moved. Both are the fleet saying no to a merge that
        // was otherwise fine, and both are the person's to read, so git's own last word is kept.
        return Ok(Outcome::refused(stderr_tail(&o)));
    }
    Ok(Outcome { state: OutcomeState::Merged, detail: None, new_tip: Some(new_tip) })
}

/// Write the merge commit, taking author AND committer from the head commit.
///
/// Not from the clock, deliberately: the commit id is then a pure function of the two branches
/// and the message, so a merge retried after a lost outcome produces the SAME commit and lands as
/// a no-op instead of a duplicate.
fn commit_tree(
    dir: &Path,
    tree: &str,
    parents: &[&str],
    head: &str,
    message: &str,
) -> Result<String> {
    let who = must(dir, &["log", "-1", "--format=%an%n%ae%n%at", head])?;
    let mut lines = who.lines();
    let (name, mail, at) = (
        lines.next().filter(|s| !s.is_empty()).unwrap_or("kloudlite"),
        lines.next().filter(|s| !s.is_empty()).unwrap_or("noreply@kloudlite.io"),
        lines.next().filter(|s| !s.is_empty()).unwrap_or("0"),
    );
    // A fixed zone, not the head commit's: the epoch second is what the id depends on, and
    // pinning the offset keeps a retry byte-identical wherever it runs.
    let when = format!("{at} +0000");
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(["commit-tree", tree]);
    for p in parents {
        cmd.args(["-p", p]);
    }
    cmd.args(["-m", message])
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", mail)
        .env("GIT_AUTHOR_DATE", &when)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", mail)
        .env("GIT_COMMITTER_DATE", &when);
    let o = out(&mut cmd)?;
    if !o.status.success() {
        return Err(crate::err(format!("commit-tree: {}", stderr_tail(&o))));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Replay the head's commits onto the base, in a throwaway worktree.
///
/// ponytail: a rebase needs an index and a checkout, so this is the one strategy that costs a full
/// working copy of the tree on disk and the IO to write it. Upgrade path: `merge-tree` per commit,
/// cherry-picking trees without a checkout, once there is a reason to pay for the extra machinery.
fn rebase(dir: &Path, base: &str, head: &str) -> Result<std::result::Result<String, Outcome>> {
    // Named for the pid so two lanes in one process cannot collide even if the per-repo lock is
    // ever relaxed; removed on every exit below.
    let wt = dir.with_extension(format!("wt.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&wt);
    let path = wt.to_string_lossy().to_string();
    let o = local(dir, &["worktree", "add", "--detach", "-f", &path, head])?;
    if !o.status.success() {
        return Err(crate::err(format!("worktree add: {}", stderr_tail(&o))));
    }
    let done = (|| -> Result<std::result::Result<String, Outcome>> {
        // No autostash (nothing to stash in a fresh worktree) and no signing, whatever the
        // ambient config says: a passphrase prompt here has nobody to answer it.
        let o = local(&wt, &["-c", "commit.gpgsign=false", "rebase", base])?;
        if !o.status.success() {
            let why = stderr_tail(&o);
            // Abort so no rebase state is left in the cache and the worktree can be removed.
            let _ = local(&wt, &["rebase", "--abort"]);
            return Ok(Err(Outcome::conflicts(format!("rebase stopped: {why}"))));
        }
        Ok(Ok(must(&wt, &["rev-parse", "HEAD"])?))
    })();
    let _ = local(dir, &["worktree", "remove", "--force", &path]);
    let _ = std::fs::remove_dir_all(&wt);
    let _ = local(dir, &["worktree", "prune"]);
    done
}

/// Would these two branches combine? The trial merge and nothing else — no commit, no push.
///
/// This is the deep half of a mergeability check: the owner answers ancestry itself and only asks
/// for this when the branches diverged (see `pulls::check`).
pub fn check(job: &Job, cache: &Path, upstream: &str, secret: &str) -> Result<Verdict> {
    use crate::directory::MergeableState;
    let unknown = |why: String| Verdict {
        state: MergeableState::Unknown,
        detail: Some(why),
        fast_forward: false,
    };
    if !available() {
        return Ok(unknown(NO_GIT.to_string()));
    }
    let (dir, _) = sync(cache, upstream, secret, job)?;
    let refs = format!("refs/heads/{}", job.base);
    let head_ref = format!("refs/heads/{}", job.head);
    if must(&dir, &["rev-parse", "--verify", "--quiet", &refs]).is_err()
        || must(&dir, &["rev-parse", "--verify", "--quiet", &head_ref]).is_err()
    {
        return Ok(unknown("one of the branches is gone".to_string()));
    }
    // A verdict this worker could not actually compute is `Unknown` with the reason, never a guess
    // in either direction: "clean" would offer a button that fails, "dirty" would hide a merge
    // that works.
    Ok(match tree_merge(&dir, &refs, &head_ref) {
        Ok(Ok(_)) => Verdict {
            state: MergeableState::Clean,
            detail: Some(format!("this can be merged into {}, but not fast-forwarded", job.base)),
            fast_forward: false,
        },
        Ok(Err(o)) => {
            Verdict { state: MergeableState::Dirty, detail: o.detail, fast_forward: false }
        }
        Err(e) => unknown(e.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `git merge-tree --write-tree --messages -z` actually emits (captured from git
    /// 2.47): tree oid, then one record per conflicted path PER STAGE, then an empty record, then
    /// prose. Parsing the prose would invent files; counting records would triple the count.
    #[test]
    fn conflicted_paths_are_deduped_and_stop_at_the_messages() {
        let out = b"7e3ea0b36c862be0a343d272648b6a16\
                    \x00100644 5626abf0f72e58d7a153368ba57db4c6 1\ta.txt\
                    \x00100644 df967b96a579e45a18b8251732d16804 2\ta.txt\
                    \x00100644 564b12f45becba5fb2f70e270af067c1 3\ta.txt\
                    \x00\x001\x00a.txt\x00Auto-merging\x00Auto-merging a.txt\n\x00";
        assert_eq!(conflicted_paths(out), vec!["a.txt"]);
    }

    #[test]
    fn every_conflicted_path_is_kept_in_order() {
        let out = b"tree\x00100644 aa 1\tz.txt\x00100644 bb 2\tz.txt\x00100644 cc 1\ta.txt\x00\x00prose\x00";
        assert_eq!(conflicted_paths(out), vec!["z.txt", "a.txt"]);
    }

    /// A clean merge writes the tree and nothing else, so there is no conflicted section to read —
    /// and the messages that follow must not be mistaken for one.
    #[test]
    fn a_clean_run_names_no_paths() {
        assert!(conflicted_paths(b"7e3ea0b3\x00Auto-merging a.txt\n\x00").is_empty());
    }

    #[test]
    fn the_detail_names_the_files_and_counts_the_rest() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(conflict_detail(&p(&["a"])), "conflicts in: a");
        assert_eq!(conflict_detail(&p(&["a", "b"])), "conflicts in: a, b");
        assert_eq!(conflict_detail(&p(&["a", "b", "c", "d"])), "conflicts in: a, b (+2 more)");
        // Never an empty sentence: exit 1 with no parseable record still has to say something.
        assert_eq!(conflict_detail(&[]), "the branches conflict");
    }
}
