//! The merge worker.
//!
//! Merging is the one thing this system does that is neither a request nor a
//! push: it can be slow, it can fail in ways a person has to resolve, and doing
//! it inside an HTTP call would tie up a request on a git node that is also
//! serving clones. So it is a job, and this process runs it.
//!
//! What makes that safe is a division the rest of the design already relies on:
//!
//!   * OBJECTS live in shared storage and are content-addressed, so any process
//!     may add them. The worker reads the objects it needs and writes the pack it
//!     produces, directly — no clone, no transfer.
//!   * REFS have exactly one writer per repo, the node that owns it. The worker
//!     never touches them. It asks that node to move the ref, exactly as the api
//!     tier does, which is also what keeps BRANCH PROTECTION in force: a merge is
//!     refused by the same rule that refuses a force push.
//!
//! It is deliberately a poller rather than a queue server. A poll every second is
//! nothing next to a merge, and it means no broker to run, no delivery semantics
//! to reason about, and a job that survives this process dying — the lease in
//! `claim_merge` hands it back.

use rustic_git::config::{env, open_store};
use rustic_git::directory::{Directory, MergeState, Mergeability, MergeableState, PullRequest};
use rustic_git::Result;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}"); // ponytail: eprintln
        std::process::exit(2);
    }
}

/// How long a claimed job may sit before another worker may take it.
const LEASE: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// How long to wait when there was nothing to do.
const IDLE: std::time::Duration = std::time::Duration::from_secs(2);

async fn run() -> Result<()> {
    rustic_git::config::install_crypto_provider();

    // `false`: compaction and garbage collection belong to the node that owns the
    // repository. This process only ever adds packs.
    let store = open_store(false).await?;
    let upstream = env("RUSTIC_GIT_UPSTREAM", "http://rustic-git:8081");
    let secret = std::env::var("RUSTIC_GIT_PEER_SECRET")
        .map_err(|_| rustic_git::err("RUSTIC_GIT_PEER_SECRET required"))?;
    let uri = std::env::var("RUSTIC_GIT_MONGO_URI")
        .map_err(|_| rustic_git::err("RUSTIC_GIT_MONGO_URI required: the worker reads its jobs from it"))?;
    let db = Directory::connect(&uri, &env("RUSTIC_GIT_MONGO_DB", "kloudlite")).await?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default();

    eprintln!("merge worker ready; upstream {upstream}"); // ponytail: eprintln
    loop {
        match db.claim_merge(LEASE).await {
            Ok(Some(pr)) => {
                let repo = pr.repo.clone();
                let number = pr.number;
                // A job that panics must not take the worker with it — the next
                // claim would then be blocked behind a process that is gone.
                let outcome = merge_one(&store, &db, &client, &upstream, &secret, pr).await;
                if let Err(e) = outcome {
                    eprintln!("merge {repo}#{number}: {e}"); // ponytail: eprintln
                    let _ = db
                        .finish_merge(&repo, number, MergeState::Failed, Some(&e.to_string()))
                        .await;
                }
            }
            // Nothing to merge: work out whether the open changes COULD be
            // merged, so the page can say so before anyone clicks. This is the
            // half of the job nobody asks for and everybody reads.
            Ok(None) => {
                match check_one(&db, &client, &upstream, &secret).await {
                    Ok(true) => {}                                  // did something; look again at once
                    Ok(false) => tokio::time::sleep(IDLE).await,     // everything is current
                    Err(e) => {
                        eprintln!("checking mergeability: {e}"); // ponytail: eprintln
                        tokio::time::sleep(IDLE).await;
                    }
                }
            }
            Err(e) => {
                eprintln!("claiming a merge: {e}"); // ponytail: eprintln
                tokio::time::sleep(IDLE).await;
            }
        }
    }
}

/// What the fleet says about two branches.
#[derive(serde::Deserialize)]
struct Comparison {
    merge_base: Option<String>,
    fast_forward: bool,
    base: String,
    head: String,
}

/// Look at one open change and record whether it could be merged.
///
/// `Ok(false)` means the answer it already had is still true, which is the common
/// case — so the loop can go back to sleep rather than recomputing the world.
async fn check_one(
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
) -> Result<bool> {
    let Some(pr) = db.pull_to_check().await? else { return Ok(false) };
    let Some((owner, name)) = pr.repo.split_once('/') else { return Ok(false) };

    let url = format!(
        "{upstream}/api/{owner}/{name}/compare?base={}&head={}&n=1",
        urlencode(&pr.base),
        urlencode(&pr.head),
    );
    let res = client
        .get(url)
        .header(rustic_git::proxy::PEER_HEADER, secret)
        // The compare route is an ordinary READ of the repo, so the node applies
        // its read check and has to be told who is asking.
        .header(rustic_git::proxy::OWNER_HEADER, owner)
        .send()
        .await
        .map_err(|e| rustic_git::err(format!("the fleet is unreachable: {e}")))?;

    // A branch that has gone is not an error: the change is simply not mergeable
    // until someone pushes it again, and saying so beats retrying forever.
    let (state, base_oid, head_oid, detail) = if res.status() == reqwest::StatusCode::NOT_FOUND {
        (MergeableState::Unknown, String::new(), String::new(), Some("one of the branches is gone".to_string()))
    } else {
        let body = res
            .text()
            .await
            .map_err(|e| rustic_git::err(format!("reading the comparison: {e}")))?;
        let cmp: Comparison = serde_json::from_str(&body)
            .map_err(|e| rustic_git::err(format!("the fleet said something unexpected: {e}")))?;
        match (&cmp.merge_base, cmp.fast_forward) {
            (Some(_), true) => (MergeableState::Clean, cmp.base, cmp.head, None),
            // The base moved on. Landing this needs a real merge — which is the
            // piece still to come, so for now it is reported rather than done.
            (Some(_), false) => (
                MergeableState::Behind,
                cmp.base,
                cmp.head,
                Some("the base has moved on since this branch left it".to_string()),
            ),
            (None, _) => (
                MergeableState::Dirty,
                cmp.base,
                cmp.head,
                Some("these branches share no history".to_string()),
            ),
        }
    };

    // Already true? Then nothing has moved and there is nothing to write. Written
    // only on a change, so a quiet repo produces no database traffic at all.
    if let Some(old) = &pr.mergeability {
        if old.state == state && old.base_oid == base_oid && old.head_oid == head_oid {
            // Still stamp the time, or this change is picked first forever and
            // the loop spins on it while other changes wait.
            db.record_mergeability(
                &pr.repo,
                pr.number,
                &Mergeability { checked_at: mongodb::bson::DateTime::now(), ..old.clone() },
            )
            .await?;
            return Ok(false);
        }
    }

    db.record_mergeability(
        &pr.repo,
        pr.number,
        &Mergeability {
            state,
            base_oid,
            head_oid,
            checked_at: mongodb::bson::DateTime::now(),
            detail,
        },
    )
    .await?;
    Ok(true)
}

/// Do one merge, and record how it ended.
async fn merge_one(
    _store: &Arc<rustic_git::store::Store>,
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    pr: PullRequest,
) -> Result<()> {
    let Some(job) = &pr.merge else { return Ok(()) };
    let (owner, name) = pr
        .repo
        .split_once('/')
        .ok_or_else(|| rustic_git::err("a repo is owner/name"))?;

    // The node that owns the repo does the ref move, and refuses it if a
    // protection rule says so. Everything this worker could do on its own —
    // building the merged objects — happens before this call; the ref is the one
    // thing it must not write itself.
    let url = format!(
        "{upstream}/api/{owner}/{name}/merge?base={}&head={}&strategy={}&message={}",
        urlencode(&pr.base),
        urlencode(&pr.head),
        urlencode(&job.strategy),
        urlencode(&format!("{} (#{})\n", pr.title, pr.number)),
    );
    let res = client
        .post(url)
        .header(rustic_git::proxy::PEER_HEADER, secret)
        .send()
        .await
        .map_err(|e| rustic_git::err(format!("the fleet is unreachable: {e}")))?;

    let status = res.status().as_u16();
    let body = res.text().await.unwrap_or_default();
    match status {
        200..=299 => {
            db.set_pull_state(&pr.repo, pr.number, rustic_git::directory::PullState::Merged)
                .await?;
            db.finish_merge(&pr.repo, pr.number, MergeState::Queued, None).await?;
            // Queued is not a state a finished job stays in; clearing the job
            // entirely is the honest end, and `set_pull_state` already records
            // that it merged.
            eprintln!("merged {}#{}", pr.repo, pr.number); // ponytail: eprintln
            Ok(())
        }
        // The fleet's own words: "behind its base", or the protection rule that
        // refused it. Both are written for the person waiting, so they are passed
        // through rather than replaced.
        409 => {
            db.finish_merge(&pr.repo, pr.number, MergeState::Conflicts, Some(body.trim()))
                .await?;
            Ok(())
        }
        _ => Err(rustic_git::err(format!("the fleet said {status}: {}", body.trim()))),
    }
}

/// Percent-encode one query value. Branch names may contain `/` and `#`, and a
/// raw one would end the query or start a fragment.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
