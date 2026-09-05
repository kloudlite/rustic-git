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
//! The peer secret reaches git as `-c http.extraHeader=…`, which puts it in the subprocess argv
//! and therefore in `/proc/<pid>/cmdline` for the duration of the fetch or push. Accepted, not
//! overlooked: the worker pod is single-tenant and runs only this process, so anyone who can read
//! that procfs already has the pod's environment — where the secret lives anyway. The rule that
//! IS load-bearing is that it must never outlive the process: no log line, error or panic message
//! here may carry a networked command's argv (see `networked`). `git` has no way to take a header
//! off a file descriptor, which is why the argv is the only door.
//!
//! Nothing here opens a database, and nothing here is async: it is a sequence of subprocesses, so
//! callers run it on a blocking thread.

use kloudlite_core::{err, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
        Outcome {
            state: OutcomeState::Refused,
            detail: Some(why.into()),
            new_tip: None,
        }
    }
    fn conflicts(why: String) -> Outcome {
        Outcome {
            state: OutcomeState::Conflicts,
            detail: Some(why),
            new_tip: None,
        }
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
    /// The tips this verdict was computed from. The owner records mergeability against the two
    /// oids it read, and this is what lets it tell a fresh answer from a lapsed lane's stale one —
    /// there is no claim on a check to match against. Empty when this worker could not resolve
    /// either branch, which the owner reads as "unstamped" and accepts.
    #[serde(default)]
    pub base_oid: String,
    #[serde(default)]
    pub head_oid: String,
}

/// Is there a `git` to run at all? Checked at worker startup so a missing binary is a loud line in
/// the log, not a merge that mysteriously refuses an hour later.
pub fn available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

/// Delete caches nothing has touched in `age`, then the least recently used until what is left
/// fits in `budget` bytes. A cache is a pure derivative of the fleet, so losing one costs a fetch,
/// never data — whereas the cache directory is a bounded emptyDir, and one large monorepo merged
/// within the age window used to fill it and have the kubelet evict the pod mid-merge.
///
/// ponytail: a single repo bigger than `budget` still fills the disk; the upgrade path is a
/// `--filter=blob:none` clone, not more accounting here.
pub fn prune(cache: &Path, age: std::time::Duration, budget: u64) -> usize {
    let mut gone = 0;
    let Ok(owners) = std::fs::read_dir(cache.join("merge")) else {
        return 0;
    };
    let mut kept: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for owner in owners.flatten() {
        let Ok(repos) = std::fs::read_dir(owner.path()) else {
            continue;
        };
        for repo in repos.flatten() {
            // No stamp at all is a cache from before this existed, or a half-made one.
            let used = std::fs::metadata(repo.path().join(USED)).and_then(|m| m.modified()).ok();
            match used.filter(|t| t.elapsed().unwrap_or_default() <= age) {
                Some(t) => kept.push((t, dir_size(&repo.path()), repo.path())),
                None => {
                    if std::fs::remove_dir_all(repo.path()).is_ok() {
                        gone += 1;
                    }
                }
            }
        }
    }
    kept.sort_by_key(|k| k.0);
    let mut total: u64 = kept.iter().map(|k| k.1).sum();
    for (_, size, path) in kept {
        if total <= budget {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            gone += 1;
            total -= size;
        }
    }
    gone
}

/// Apparent size of everything under `p`; a directory that vanishes mid-walk counts as empty.
fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Running git.
//
// Two shapes, deliberately kept apart: a LOCAL command, whose argv is safe to put in an error
// message, and a NETWORKED one, whose argv carries the peer secret in `-c http.extraHeader` and
// must therefore never reach a log, an error or a panic.
// ---------------------------------------------------------------------------

/// The ceiling on ONE git subprocess, and on a whole job (`run`, `check`, `sync_branches`).
///
/// `networked` already fails a transfer that stalls, but a `merge-tree` or `rebase` that never
/// returns has nothing watching it: it held its lane, its per-repo lock and — once the lane's
/// heartbeat went stale — the whole pod, taking every other lane's merge down with it. With a
/// ceiling the job fails as a JOB: `run` returns `Err`, the claim's lease lapses, the owner
/// re-announces, and the restart stays the last resort. Per-job as well as per-command because a
/// job is a sequence of commands, and sixteen of them each just under the line is still a wedged
/// lane. Seconds, via env; the job default sits under the liveness probe's 30 minute window.
fn cmd_timeout() -> Duration {
    static T: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *T.get_or_init(|| secs("KLOUDLITE_MERGE_CMD_TIMEOUT", 15 * 60))
}
fn job_timeout() -> Duration {
    static T: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *T.get_or_init(|| secs("KLOUDLITE_MERGE_JOB_TIMEOUT", 25 * 60))
}
fn secs(var: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|s| *s > 0)
            .unwrap_or(default),
    )
}

thread_local! {
    /// When the job running on this thread must be done by. Thread-local rather than threaded
    /// through every signature: a job is one blocking thread running subprocesses in sequence,
    /// so the thread IS the job, and `out` is the one place every subprocess passes through.
    static DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Run `f` as one job, under `job_timeout` — unless a job is already running on this thread, in
/// which case it is part of that job and keeps its deadline (`check` calls `sync_branches`).
fn as_job<T>(timeout: Duration, f: impl FnOnce() -> T) -> T {
    if DEADLINE.get().is_some() {
        return f();
    }
    DEADLINE.set(Some(Instant::now() + timeout));
    let r = f();
    DEADLINE.set(None);
    r
}

fn out(cmd: &mut Command) -> Result<std::process::Output> {
    use std::os::unix::process::CommandExt;
    let budget = match DEADLINE.get() {
        Some(by) if by <= Instant::now() => {
            return Err(err(format!(
                "merge job timed out after {}s",
                job_timeout().as_secs()
            )))
        }
        Some(by) => cmd_timeout().min(by - Instant::now()),
        None => cmd_timeout(),
    };
    // Its own process group, so the deadline can kill the whole tree: git forks helpers
    // (`remote-http`, `pack-objects`) that hold the output pipes, and killing only the leader
    // would leave `wait_with_output` waiting on a pipe an orphan still has open.
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| err(format!("git: {e}")))?;
    let pid = child.id() as i32;
    // `wait_timeout` is not in std: a watchdog thread sleeps on a channel that closes when the
    // child has been reaped, and only a timeout — not the close — fires the kill. The kill can
    // race a normal exit by the width of a `drop`, which at worst reports a finished command as
    // timed out; the pid cannot have been reused inside that window because the group is ours.
    let (done, wake) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if wake.recv_timeout(budget) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            return true;
        }
        false
    });
    let o = child.wait_with_output();
    drop(done);
    let killed = watchdog.join().unwrap_or(false);
    let o = o.map_err(|e| err(format!("git: {e}")))?;
    if killed {
        return Err(err(format!("git timed out after {}s", budget.as_secs())));
    }
    Ok(o)
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
        return Err(err(format!(
            "git {}: {}",
            args.join(" "),
            stderr_tail(&o)
        )));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// A git command that talks to the fleet.
///
/// The peer secret rides in `-c http.extraHeader`, so NOTHING here may put the argv into an error,
/// a log line or a panic message. `x-kloudlite-peer` admits the request on the peer listener;
/// `x-kloudlite-owner` is the identity it is served as, and the git routes authorize it exactly
/// as they would a token for that owner (see `http::open`).
fn networked(dir: &Path, secret: &str, owner: &str, args: &[&str]) -> Result<std::process::Output> {
    out(Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            &format!("http.extraHeader={}: {secret}", kloudlite_core::peer::PEER_HEADER),
        ])
        .args([
            "-c",
            &format!("http.extraHeader={}: {owner}", kloudlite_core::peer::OWNER_HEADER),
        ])
        // Fail a transfer that has moved less than 1 KiB/s for a minute. Without this a half-open
        // connection hangs the lane indefinitely: the lane's heartbeat goes stale and the pod is
        // restarted, which takes every OTHER lane's work with it. With it the merge fails as a
        // job — the claim's lease lapses, the owner re-announces, another lane picks it up — and
        // the restart stays what it is meant to be, the last resort rather than the mechanism.
        .args([
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=60",
        ])
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
    let (dir, url) = sync_branches(
        cache,
        upstream,
        secret,
        &job.owner,
        &job.name,
        &[job.base.clone(), job.head.clone()],
    )?;
    Ok((dir, url))
}

/// The same, for a whole fan-out: ONE fetch carrying every branch the caller is about to work on.
///
/// A `HeadMoved` fans out to up to `CHECK_LIMIT` diverged changes in one repo, and every one of
/// them wants the same cache brought up to date. Fetching per change was `CHECK_LIMIT` network
/// round trips, serialized under the same per-repo lock, for one repo's worth of refs — so the
/// caller fetches once here and then does purely local `merge-tree` work with `check_local`.
pub fn sync_branches(
    cache: &Path,
    upstream: &str,
    secret: &str,
    owner: &str,
    name: &str,
    branches: &[String],
) -> Result<(PathBuf, String)> {
    as_job(job_timeout(), || {
        sync_branches_inner(cache, upstream, secret, owner, name, branches)
    })
}

fn sync_branches_inner(
    cache: &Path,
    upstream: &str,
    secret: &str,
    owner: &str,
    name: &str,
    branches: &[String],
) -> Result<(PathBuf, String)> {
    let dir = cache_of(cache, owner, name);
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(&dir).map_err(|e| err(format!("{}: {e}", dir.display())))?;
        let o = out(Command::new("git").args(["init", "--bare", "-q"]).arg(&dir))?;
        if !o.status.success() {
            return Err(err(format!("git init --bare: {}", stderr_tail(&o))));
        }
    }
    let _ = std::fs::write(dir.join(USED), b"");
    let url = format!("{}/{owner}/{name}.git", upstream.trim_end_matches('/'));
    fetch(&dir, &url, secret, owner, branches)?;
    Ok((dir, url))
}

fn fetch(dir: &Path, url: &str, secret: &str, owner: &str, branches: &[String]) -> Result<()> {
    // Only the branches the caller named: every consumer of the cache (`run`, `check`, the
    // rebase worktree, `commit_tree`'s log read) operates on the base and head tips and their
    // history, so mirroring every branch was pure transfer. Forced and pruned per refspec, the
    // cache still never keeps a rewritten history of THESE refs; other cached branches go stale
    // harmlessly — nothing reads a ref a job did not name, and the next job naming one forces it.
    let specs: Vec<String> = branches
        .iter()
        .map(|b| format!("+refs/heads/{b}:refs/heads/{b}"))
        .collect();
    let mut args: Vec<&str> = vec!["fetch", "--quiet", "--prune", "--force", url];
    args.extend(specs.iter().map(String::as_str));
    let o = networked(dir, secret, owner, &args)?;
    if !o.status.success() {
        // A branch deleted upstream fails a named refspec where the mirror silently pruned it —
        // and `run`/`check` want to SEE the missing ref to refuse cleanly. One mirror fetch as
        // the fallback keeps that path; the fast path never pays for it.
        let o = networked(
            dir,
            secret,
            owner,
            &[
                "fetch",
                "--quiet",
                "--prune",
                "--force",
                url,
                "+refs/heads/*:refs/heads/*",
            ],
        )?;
        if !o.status.success() {
            // The URL is safe to name — it is the caller's own configuration; the argv is not.
            return Err(err(format!("fetching {url}: {}", stderr_tail(&o))));
        }
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
        let Some((meta, path)) = rec.split_once('\t') else {
            continue;
        };
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
    /// A path can be as long as git allows, and this sentence is stored on the job and rendered in
    /// a page. Truncated per path rather than on the whole string, so the second name cannot be
    /// pushed out of view by a pathological first one.
    const PATH_CAP: usize = 120;
    if paths.is_empty() {
        return "the branches conflict".to_string();
    }
    let head = paths
        .iter()
        .take(SHOWN)
        .map(|p| match p.char_indices().nth(PATH_CAP) {
            Some((at, _)) => format!("{}…", &p[..at]),
            None => p.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
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
    let o = local(
        dir,
        &["merge-tree", "--write-tree", "--messages", "-z", base, head],
    )?;
    if o.status.success() {
        let tree = o.stdout.split(|b| *b == 0).next().unwrap_or_default();
        return Ok(Ok(String::from_utf8_lossy(tree).trim().to_string()));
    }
    // Exit 1 is "they conflict"; anything else is git failing, which is not an answer.
    if o.status.code() != Some(1) {
        return Err(err(format!("merge-tree: {}", stderr_tail(&o))));
    }
    Ok(Err(Outcome::conflicts(conflict_detail(&conflicted_paths(
        &o.stdout,
    )))))
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
    as_job(job_timeout(), || run_inner(job, cache, upstream, secret))
}

fn run_inner(job: &Job, cache: &Path, upstream: &str, secret: &str) -> Result<Outcome> {
    if !available() {
        return Ok(Outcome::refused(NO_GIT));
    }
    let (dir, url) = sync(cache, upstream, secret, job)?;
    // One spawn resolves all three ids a merge can need; rev-parse exits non-zero if any rev
    // is unresolvable, which is exactly the "a branch is gone" answer.
    let resolved = local(
        &dir,
        &[
            "rev-parse",
            &format!("refs/heads/{}^{{commit}}", job.base),
            &format!("refs/heads/{}^{{commit}}", job.head),
            &format!("refs/heads/{}^{{tree}}", job.base),
        ],
    )?;
    if !resolved.status.success() {
        return Ok(Outcome::refused("one of the branches is gone"));
    }
    let ids: Vec<String> = String::from_utf8_lossy(&resolved.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect();
    let [base_oid, head_oid, base_tree] = ids.as_slice() else {
        return Err(err("rev-parse did not answer three ids"));
    };
    let (base_oid, head_oid) = (base_oid.clone(), head_oid.clone());

    // Already landed. A worker that merged and then lost the outcome POST — a crash, a network
    // blip — gets the job back when its lease lapses, and must not mint a second commit for work
    // that is already in the base. Checked before the strategy, because it is the same answer
    // whichever one was asked for: the base contains the head, so there is nothing to combine.
    // Catches fast-forward and merge, whose results keep the head as an ancestor. A squash's
    // retry is caught by the merged-tree-equals-base-tree guard in the strategy arm below (the
    // lease cannot see it: the retry re-resolves the base, so the lease holds). A rebase's retry
    // self-heals — git skips commits whose patches are already upstream, and the push is a no-op.
    if local(&dir, &["merge-base", "--is-ancestor", &head_oid, &base_oid])?
        .status
        .success()
    {
        return Ok(Outcome {
            state: OutcomeState::Merged,
            detail: Some("already merged".to_string()),
            new_tip: Some(base_oid),
        });
    }

    let new_tip = match job.strategy.as_str() {
        "fast-forward" => {
            if !local(&dir, &["merge-base", "--is-ancestor", &base_oid, &head_oid])?
                .status
                .success()
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
            // A squash rewrites, so the ancestry guard above cannot see its own retry: after a
            // pushed-but-unreported squash the base already carries the head's changes without
            // carrying the head. The merged tree equalling the base's tree is that state — and
            // also any genuinely empty change — and minting a commit for it would put junk in
            // someone's permanent history.
            if tree == *base_tree {
                return Ok(Outcome {
                    state: OutcomeState::Merged,
                    detail: Some("already merged".to_string()),
                    new_tip: Some(base_oid),
                });
            }
            let parents: &[&str] = if job.strategy == "squash" {
                &[&base_oid]
            } else {
                &[&base_oid, &head_oid]
            };
            commit_tree(
                &dir,
                &tree,
                parents,
                &head_oid,
                &format!("{} (#{})", job.title, job.number),
            )?
        }
        "rebase" => match rebase(&dir, &base_oid, &head_oid)? {
            Ok(t) => t,
            Err(o) => return Ok(o),
        },
        _ => {
            return Ok(Outcome::refused(
                "strategy must be fast-forward, squash, merge or rebase",
            ))
        }
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
        // A lost lease and a genuinely refused push look identical from here — git says "stale
        // info" either way. If the merge is already IN the base, another worker computed the same
        // result from the same base and won the race: recording Refused would show a merged
        // change as failed AND swallow HeadMoved/PullMerged, because both fire off Merged.
        if let Some(base) = landed_anyway(&dir, &url, secret, job, &head_oid) {
            return Ok(Outcome {
                state: OutcomeState::Merged,
                detail: Some("already merged".to_string()),
                new_tip: Some(base),
            });
        }
        // A protection rule, or a base that moved. Both are the fleet saying no to a merge that
        // was otherwise fine, and both are the person's to read, so git's own last word is kept.
        return Ok(Outcome::refused(stderr_tail(&o)));
    }
    Ok(Outcome {
        state: OutcomeState::Merged,
        detail: None,
        new_tip: Some(new_tip),
    })
}

/// Did this merge already land, despite our push being refused?
///
/// Only ever called after a `--force-with-lease` failure, which is the shape a lost race takes:
/// another worker computed the same merge from the same base and won. Re-resolves the base from
/// the fleet (ours is stale by definition — the lease failed because the ref moved) and asks the
/// two questions `run` already asks before merging: does the base now contain the head
/// (fast-forward, merge, rebase), or does merging the two now produce the base's own tree
/// (squash, which rewrites and so leaves no ancestry behind).
///
/// `Option`, not `Result`: every failure inside — a fetch that fails, a rev-parse that fails —
/// means "cannot prove it landed", which is the same answer as "it did not". Answering `None` is
/// the safe default, because it records the refusal git actually gave, which is what a protection
/// rule or a genuinely-moved base deserves.
fn landed_anyway(dir: &Path, url: &str, secret: &str, job: &Job, head_oid: &str) -> Option<String> {
    // An empty url means "already local" — the tests drive it that way; in production the fetch is
    // the whole point, since our refs are stale by the time we get here.
    if !url.is_empty() {
        fetch(
            dir,
            url,
            secret,
            &job.owner,
            &[job.base.clone(), job.head.clone()],
        )
        .ok()?;
    }
    let base = must(dir, &["rev-parse", &format!("refs/heads/{}^{{commit}}", job.base)]).ok()?;

    if job.strategy == "squash" {
        // A squash rewrites history, so the head is never an ancestor of what landed. The only
        // evidence left is the content: if merging head into the new base yields exactly the
        // base's own tree, the base already contains this work.
        let base_tree = must(dir, &["rev-parse", &format!("{base}^{{tree}}")]).ok()?;
        return match tree_merge(dir, &base, head_oid) {
            Ok(Ok(t)) if t == base_tree => Some(base),
            _ => None,
        };
    }
    local(dir, &["merge-base", "--is-ancestor", head_oid, &base])
        .ok()
        .filter(|o| o.status.success())
        .map(|_| base)
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
        lines
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("kloudlite"),
        lines
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("noreply@kloudlite.io"),
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
        return Err(err(format!("commit-tree: {}", stderr_tail(&o))));
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
        return Err(err(format!("worktree add: {}", stderr_tail(&o))));
    }
    let done = (|| -> Result<std::result::Result<String, Outcome>> {
        // No autostash (nothing to stash in a fresh worktree) and no signing, whatever the
        // ambient config says: a passphrase prompt here has nobody to answer it.
        // The replayed commits keep their original authors, but git still needs a COMMITTER
        // ident, and the pod has no git config — use the head's author, the same rule
        // `commit_tree` applies, so a retried rebase re-mints identical commits.
        let ident = must(&wt, &["log", "-1", "--format=%an%n%ae", "HEAD"])?;
        let (name, mail) = ident
            .split_once('\n')
            .unwrap_or(("kloudlite", "noreply@invalid"));
        let o = out(Command::new("git")
            .arg("-C")
            .arg(&wt)
            // `--committer-date-is-author-date` is what actually makes the claim above true: the
            // replayed commits already keep their authors, but an unpinned committer date comes
            // from the clock, so every replay minted NEW ids and a retried rebase pushed
            // duplicates instead of landing as a no-op.
            .args([
                "-c",
                "commit.gpgsign=false",
                "rebase",
                "--committer-date-is-author-date",
                base,
            ])
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", mail))?;
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
    as_job(job_timeout(), || {
        if !available() {
            return Ok(unknown(NO_GIT.to_string()));
        }
        sync(cache, upstream, secret, job)?;
        check_local(job, cache)
    })
}

fn unknown(why: String) -> Verdict {
    Verdict {
        state: crate::directory::MergeableState::Unknown,
        detail: Some(why),
        fast_forward: false,
        base_oid: String::new(),
        head_oid: String::new(),
    }
}

/// The trial merge alone, against a cache the caller has already brought up to date.
///
/// No network at all — that is the whole point: a fan-out syncs once with `sync_branches` and then
/// calls this per change, instead of re-fetching the same repo once per change.
pub fn check_local(job: &Job, cache: &Path) -> Result<Verdict> {
    use crate::directory::MergeableState;
    if !available() {
        return Ok(unknown(NO_GIT.to_string()));
    }
    let dir = cache_of(cache, &job.owner, &job.name);
    let refs = format!("refs/heads/{}", job.base);
    let head_ref = format!("refs/heads/{}", job.head);
    let tips = local(
        &dir,
        &[
            "rev-parse",
            &format!("{refs}^{{commit}}"),
            &format!("{head_ref}^{{commit}}"),
        ],
    )?;
    if !tips.status.success() {
        return Ok(unknown("one of the branches is gone".to_string()));
    }
    // `rev-parse` prints one oid per argument, in order — the two tips this verdict is about.
    let out = String::from_utf8_lossy(&tips.stdout);
    let mut lines = out.lines();
    let base_oid = lines.next().unwrap_or_default().trim().to_string();
    let head_oid = lines.next().unwrap_or_default().trim().to_string();
    // A verdict this worker could not actually compute is `Unknown` with the reason, never a guess
    // in either direction: "clean" would offer a button that fails, "dirty" would hide a merge
    // that works.
    Ok(match tree_merge(&dir, &refs, &head_ref) {
        Ok(Ok(_)) => Verdict {
            state: MergeableState::Clean,
            detail: Some(format!(
                "this can be merged into {}, but not fast-forwarded",
                job.base
            )),
            fast_forward: false,
            base_oid,
            head_oid,
        },
        Ok(Err(o)) => Verdict {
            state: MergeableState::Dirty,
            detail: o.detail,
            fast_forward: false,
            base_oid,
            head_oid,
        },
        Err(e) => Verdict {
            base_oid,
            head_oid,
            ..unknown(e.to_string())
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo with `main` and a `feature` branch off it. Returns the head oid.
    #[cfg(test)]
    fn repo_with_a_feature(dir: &Path) -> String {
        must(dir, &["init", "-q", "-b", "main"]).unwrap();
        // Pods have no git identity, and neither does a test runner's HOME.
        must(dir, &["config", "user.email", "t@example.com"]).unwrap();
        must(dir, &["config", "user.name", "t"]).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        must(dir, &["add", "."]).unwrap();
        must(dir, &["commit", "-qm", "base"]).unwrap();
        must(dir, &["checkout", "-q", "-b", "feature"]).unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        must(dir, &["add", "."]).unwrap();
        must(dir, &["commit", "-qm", "head"]).unwrap();
        let head = must(dir, &["rev-parse", "HEAD"]).unwrap();
        must(dir, &["checkout", "-q", "main"]).unwrap();
        head
    }

    fn job_for(strategy: &str) -> Job {
        Job {
            owner: "o".into(),
            name: "n".into(),
            number: 1,
            strategy: strategy.into(),
            base: "main".into(),
            head: "feature".into(),
            title: "t".into(),
            requested_by: String::new(),
        }
    }

    /// A fan-out fetches ONCE and then works locally. Proven by taking the upstream away after the
    /// single sync: every `check_local` still answers, which it could not if it re-fetched.
    #[test]
    fn a_fan_out_fetches_once_and_then_works_locally() {
        if !available() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        // The upstream `git` sees is `{upstream}/{owner}/{name}.git` — a local path works.
        let up = td.path().join("up").join("o");
        std::fs::create_dir_all(&up).unwrap();
        let src = up.join("n.git");
        std::fs::create_dir_all(&src).unwrap();
        repo_with_a_feature(&src);
        // Two more heads, so the fan-out really covers several changes.
        for b in ["f2", "f3"] {
            must(&src, &["checkout", "-q", "-b", b, "feature"]).unwrap();
            std::fs::write(src.join(format!("{b}.txt")), b).unwrap();
            must(&src, &["add", "."]).unwrap();
            must(&src, &["commit", "-qm", b]).unwrap();
        }
        must(&src, &["checkout", "-q", "main"]).unwrap();

        let cache = td.path().join("cache");
        let upstream = td.path().join("up");
        let branches: Vec<String> = ["main", "feature", "f2", "f3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sync_branches(&cache, upstream.to_str().unwrap(), "", "o", "n", &branches).unwrap();

        // No upstream left to fetch from: anything below that touches the network fails.
        std::fs::remove_dir_all(&upstream).unwrap();
        for head in ["feature", "f2", "f3"] {
            let mut job = job_for("merge");
            job.head = head.into();
            let v = check_local(&job, &cache).unwrap();
            assert_eq!(
                v.state,
                crate::directory::MergeableState::Clean,
                "{head} needed a fetch of its own"
            );
        }
    }

    /// A subprocess that outlives its budget is killed — the whole group, so a helper the leader
    /// forked cannot keep the pipes (and the lane) open — and the job sees an `Err`, which is what
    /// leaves it claimed for the lease to bring back. `sh -c 'sleep; true'` stands in for a
    /// wedged `merge-tree`: sh stays the leader and sleep is the helper.
    /// The audit's P-17: a cache within the age window is still evicted, least recently used
    /// first, once the caches together outgrow the byte budget.
    #[test]
    fn prune_evicts_the_least_recently_used_cache_past_the_byte_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now();
        for (name, age_secs, bytes) in [("old", 300, 100), ("mid", 200, 100), ("new", 100, 100)] {
            let dir = cache_of(tmp.path(), "acme", name);
            std::fs::create_dir_all(dir.join("objects")).unwrap();
            std::fs::write(dir.join("objects").join("pack"), vec![0u8; bytes]).unwrap();
            let stamp = std::fs::File::create(dir.join(USED)).unwrap();
            stamp.set_modified(now - std::time::Duration::from_secs(age_secs)).unwrap();
        }
        let week = std::time::Duration::from_secs(7 * 24 * 3600);
        // Everything fits: nothing goes.
        assert_eq!(prune(tmp.path(), week, 1_000), 0);
        // Budget for one and a bit: the two least recently used go, the newest stays.
        assert_eq!(prune(tmp.path(), week, 150), 2);
        assert!(!cache_of(tmp.path(), "acme", "old").exists());
        assert!(!cache_of(tmp.path(), "acme", "mid").exists());
        assert!(cache_of(tmp.path(), "acme", "new").exists());
    }

    #[test]
    fn a_hung_subprocess_is_killed_with_its_children_and_fails_the_job() {
        let td = tempfile::tempdir().unwrap();
        let pidfile = td.path().join("pid");
        let script = format!("echo $$ > {}; sleep 30; true", pidfile.display());
        let started = Instant::now();
        let got = as_job(Duration::from_millis(300), || {
            let first = out(Command::new("sh").args(["-c", &script]));
            // The deadline is the job's: the NEXT command is refused without being spawned.
            let second = local(Path::new("."), &["--version"]);
            (first, second)
        });
        assert!(started.elapsed() < Duration::from_secs(10), "the kill did not happen");
        let first = got.0.expect_err("a killed command is an Err, never an outcome");
        assert!(first.to_string().contains("timed out"), "{first}");
        let second = got.1.expect_err("a job past its deadline must not spawn more work");
        assert!(second.to_string().contains("merge job timed out"), "{second}");
        // Both processes gone: sh was the group leader, so signal 0 to its group finds nobody
        // once the orphaned sleep is dead too. Polled briefly — init reaps it asynchronously.
        let pgid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let gone = (0..20).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            (unsafe { libc::kill(-pgid, 0) }) == -1
        });
        assert!(gone, "an orphaned sleep survived the group kill");
    }

    /// A job that finishes inside its budget is untouched, and the deadline does not leak into
    /// the next job on the same thread.
    #[test]
    fn a_quick_job_is_unaffected_by_the_deadline() {
        if !available() {
            return;
        }
        let o = as_job(Duration::from_secs(30), || local(Path::new("."), &["--version"])).unwrap();
        assert!(o.status.success());
        assert!(DEADLINE.get().is_none(), "the deadline outlived its job");
    }

    #[test]
    fn landed_anyway_needs_the_head_in_the_new_base() {
        if !available() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let head = repo_with_a_feature(dir);
        let job = job_for("merge");

        // Nobody has merged it: a refused push here really is a refusal.
        assert_eq!(landed_anyway(dir, "", "", &job, &head), None);

        // The worker that won the race merged the same head into the same base.
        must(dir, &["merge", "-q", "--no-ff", "-m", "merged", &head]).unwrap();
        let new_base = must(dir, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(landed_anyway(dir, "", "", &job, &head), Some(new_base));
    }

    /// CLAUDE.md names the `local()`/`networked()` split as what keeps the peer secret out of error
    /// messages, and until this test nothing asserted it. The secret rides in an
    /// `-c http.extraHeader=` argv entry, so ANY code path that formats a networked command's argv
    /// into an error, a log line or a panic leaks a credential into wherever those go — and it would
    /// keep working perfectly while doing so, which is why only a test catches it.
    #[test]
    fn a_failed_networked_call_never_names_the_secret() {
        if !available() {
            return;
        }
        const SECRET: &str = "SUPER-SECRET-PEER-TOKEN-must-never-appear";
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        must(dir, &["init", "-q", "-b", "main"]).unwrap();

        // Port 1 is reserved and nothing listens: the fetch fails fast, which is the shape of every
        // real networked failure (a dead peer, a refused connection, a timeout).
        let o = networked(dir, SECRET, "alice", &["fetch", "http://127.0.0.1:1/repo.git", "main"])
            .expect("spawning git must succeed even when the fetch fails");
        assert!(!o.status.success(), "the fixture must actually fail, or it proves nothing");

        // Everything a caller can surface from here.
        let tail = stderr_tail(&o);
        let stderr = String::from_utf8_lossy(&o.stderr).to_string();
        let stdout = String::from_utf8_lossy(&o.stdout).to_string();
        for (what, text) in [("stderr_tail", &tail), ("stderr", &stderr), ("stdout", &stdout)] {
            assert!(!text.contains(SECRET), "{what} leaked the peer secret: {text}");
        }
    }

    #[test]
    fn a_rebase_is_byte_identical_when_replayed() {
        if !available() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let head = repo_with_a_feature(dir);
        // Move the base so the rebase actually replays something — a feature already on top of
        // its base is a no-op and would pass this test without proving anything.
        std::fs::write(dir.join("c.txt"), "c").unwrap();
        must(dir, &["add", "."]).unwrap();
        must(dir, &["commit", "-qm", "base moves"]).unwrap();

        let first = rebase(dir, "main", &head).unwrap().unwrap();
        // The committer date has one-second granularity, so without a wait the two runs can land
        // in the same second and pass by luck.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = rebase(dir, "main", &head).unwrap().unwrap();
        assert_eq!(
            first, second,
            "a replayed rebase must re-mint the same commit, or a retry pushes duplicates"
        );
    }

    #[test]
    fn a_squash_that_landed_is_recognised_by_its_tree() {
        if !available() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let head = repo_with_a_feature(dir);
        let job = job_for("squash");

        assert_eq!(landed_anyway(dir, "", "", &job, &head), None);

        // A squash rewrites, so the head is NEVER an ancestor of what landed — only the tree
        // matches. This is the arm that ancestry alone would get wrong.
        must(dir, &["merge", "-q", "--squash", &head]).unwrap();
        must(dir, &["commit", "-qm", "squashed"]).unwrap();
        let new_base = must(dir, &["rev-parse", "HEAD"]).unwrap();
        assert!(
            !local(dir, &["merge-base", "--is-ancestor", &head, &new_base]).unwrap().status.success(),
            "the fixture must really be a rewrite, or the test proves nothing"
        );
        assert_eq!(landed_anyway(dir, "", "", &job, &head), Some(new_base));
    }

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
        assert_eq!(
            conflict_detail(&p(&["a", "b", "c", "d"])),
            "conflicts in: a, b (+2 more)"
        );
        // Never an empty sentence: exit 1 with no parseable record still has to say something.
        assert_eq!(conflict_detail(&[]), "the branches conflict");
        // A path is arbitrary bytes and this sentence is stored and rendered: each name is capped
        // on its own, so a pathological first path cannot push the second out of view.
        let long = "x".repeat(400);
        let got = conflict_detail(&p(&[&long, "b"]));
        assert!(got.ends_with("…, b"), "{got}");
        assert_eq!(got.chars().count(), "conflicts in: ".len() + 120 + 1 + 3);
        // Cut on a CHARACTER boundary — slicing a multi-byte path by bytes would panic.
        let wide = "é".repeat(400);
        assert_eq!(
            conflict_detail(&p(&[&wide])).chars().count(),
            "conflicts in: ".len() + 121
        );
    }
}
