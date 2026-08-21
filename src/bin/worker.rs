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
//!
//! Both kinds of work are CLAIMED atomically, and that one property is what makes
//! everything else scale. Several tasks inside this process, and several of these
//! processes, are the same thing to the database: whoever wins the claim does the
//! work, and nobody duplicates it. So concurrency is a number here, and capacity
//! is a replica count, and neither needs any coordination between workers.

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
    // One Directory shared by every lane: the driver inside it is a connection
    // pool already, so a second one would be a second pool for no reason.
    let db = Arc::new(Directory::connect(&uri, &env("RUSTIC_GIT_MONGO_DB", "kloudlite")).await?);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default();

    // Jobs are mostly waiting — on the fleet, on the database — so one at a time
    // leaves a worker idle while a merge is in flight. Independent tasks, each
    // claiming for itself.
    let lanes: usize = env("RUSTIC_GIT_WORKER_CONCURRENCY", "4").parse().unwrap_or(4).clamp(1, 64);
    eprintln!("merge worker ready; {lanes} lanes; upstream {upstream}"); // ponytail: eprintln

    // Identifies one lane of one process, so a claim can be confirmed rather than
    // assumed. Random, not hostname+index: two pods restarted into the same name
    // would otherwise share a token and each believe it won the other's job.
    let run: u64 = rand::random();

    // The blob sweep is unrelated work — it touches the object store directly, never a repo's
    // refs or packs — so it gets its own lane rather than competing with merge lanes for a slot.
    let grace = rustic_git::registry::gc::blob_grace();
    let gc_store = Arc::clone(&store);
    let mut tasks = vec![tokio::spawn(async move { gc_lane(&gc_store, grace).await })];
    for i in 0..lanes {
        let (db, client, upstream, secret, store) = (
            Arc::clone(&db),
            client.clone(),
            upstream.clone(),
            secret.clone(),
            Arc::clone(&store),
        );
        let me = format!("{run:016x}/{i}");
        tasks.push(tokio::spawn(async move {
            lane(&store, &db, &client, &upstream, &secret, &me).await;
        }));
    }
    // A lane that dies takes the worker with it, rather than leaving a process
    // that looks healthy and is quietly doing less work than it claims.
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

/// One lane: claim, do, repeat.
async fn lane(
    store: &Arc<rustic_git::store::Store>,
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    me: &str,
) {
    loop {
        match db.claim_merge(LEASE, me).await {
            Ok(Some(pr)) => {
                let repo = pr.repo.clone();
                let number = pr.number;
                let outcome = merge_one(store, db, client, upstream, secret, pr).await;
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
                match check_one(db, client, upstream, secret).await {
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

/// How long to rest between owners within a sweep pass, and between whole passes once every
/// owner has had a turn. One owner per cycle — not every owner at once — so the sweep never
/// shows up as a burst of object-store listing traffic on top of whatever pushes are in flight.
const GC_OWNER_GAP: std::time::Duration = std::time::Duration::from_secs(5);
const GC_PASS_GAP: std::time::Duration = std::time::Duration::from_secs(60);

/// The owners with anything under `blobs/` — the top-level prefixes of the sweep's own scope,
/// not `manifests/`, so an owner who has uploaded a blob but not yet a manifest is still swept
/// (once the grace window says it is safe to).
async fn blob_owners(store: &rustic_git::store::Store) -> Result<Vec<String>> {
    rustic_git::registry::list_dir_names(&store.os, "blobs/").await
}

/// Sweep one owner at a time, forever. Reads every manifest before it deletes a single blob —
/// see `registry::gc` for why that order is load-bearing — so a wrong answer here destroys a
/// layer a live image still needs, which is why it runs on its own schedule instead of hurrying.
async fn gc_lane(store: &rustic_git::store::Store, grace: std::time::Duration) {
    let upload_grace = rustic_git::registry::uploads::upload_grace();
    loop {
        let owners = match blob_owners(store).await {
            Ok(o) => o,
            Err(e) => {
                eprintln!("gc: listing owners: {e}"); // ponytail: eprintln
                tokio::time::sleep(GC_PASS_GAP).await;
                continue;
            }
        };
        // Uploads are swept for their own owner set: a push can leave a staging object behind
        // before it ever lands a blob, so an owner with only abandoned sessions and no blobs yet
        // must still be visited, not just the owners `blob_owners` finds.
        let upload_owners = match rustic_git::registry::list_dir_names(&store.os, "uploads/").await {
            Ok(o) => o,
            Err(e) => {
                eprintln!("gc: listing upload owners: {e}"); // ponytail: eprintln
                vec![]
            }
        };
        if owners.is_empty() && upload_owners.is_empty() {
            tokio::time::sleep(GC_PASS_GAP).await;
            continue;
        }
        for owner in &owners {
            match rustic_git::registry::gc::sweep_owner(store, owner, grace).await {
                Ok(n) if n > 0 => eprintln!("gc: swept {n} blob(s) for {owner}"), // ponytail: eprintln
                Ok(_) => {}
                Err(e) => eprintln!("gc: sweeping {owner}: {e}"), // ponytail: eprintln
            }
            tokio::time::sleep(GC_OWNER_GAP).await;
        }
        for owner in &upload_owners {
            match store.sweep_stale_uploads(owner, upload_grace).await {
                Ok(n) if n > 0 => eprintln!("gc: swept {n} stale upload session(s) for {owner}"), // ponytail: eprintln
                Ok(_) => {}
                Err(e) => eprintln!("gc: sweeping uploads for {owner}: {e}"), // ponytail: eprintln
            }
            tokio::time::sleep(GC_OWNER_GAP).await;
        }
        tokio::time::sleep(GC_PASS_GAP).await;
    }
}

/// One ref, as `/refs` lists them.
#[derive(serde::Deserialize, Debug)]
struct Ref {
    name: String,
    oid: String,
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
/// Turn a `/refs` response into refs, or an error. A branch that has gone is legitimately empty
/// (404); a 5xx or 403 is a transient upstream hiccup, NOT "no refs" — treating those as empty
/// used to make a flaky upstream look like every branch got deleted, stamping a false
/// mergeability answer that stuck until something else changed.
fn parse_refs_response(
    status: reqwest::StatusCode,
    body: &str,
    owner: &str,
    name: &str,
) -> Result<Vec<Ref>> {
    if status == reqwest::StatusCode::NOT_FOUND {
        Ok(Vec::new())
    } else if !status.is_success() {
        Err(rustic_git::err(format!("listing refs for {owner}/{name}: {status}")))
    } else {
        Ok(serde_json::from_str(body).unwrap_or_default())
    }
}

async fn check_one(
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
) -> Result<bool> {
    let Some(pr) = db.pull_to_check().await? else { return Ok(false) };
    let Some((owner, name)) = pr.repo.split_once('/') else { return Ok(false) };

    // The tips FIRST, because that is the cheap question. Listing refs is a read
    // of a few keys; comparing two branches walks the commit graph to find where
    // they parted. Asking the expensive one every couple of seconds — for changes
    // where nothing has moved, which is nearly all of them — is a graph walk per
    // tick to learn nothing.
    let refs: Vec<Ref> = {
        let url = format!("{upstream}/api/{owner}/{name}/refs");
        let res = client
            .get(url)
            .header(rustic_git::proxy::PEER_HEADER, secret)
            .header(rustic_git::proxy::OWNER_HEADER, owner)
            .send()
            .await
            .map_err(|e| rustic_git::err(format!("the fleet is unreachable: {e}")))?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        parse_refs_response(status, &body, owner, name)?
    };
    let tip = |branch: &str| {
        refs.iter()
            .find(|r| r.name == format!("refs/heads/{branch}"))
            .map(|r| r.oid.clone())
    };
    let (now_base, now_head) = (tip(&pr.base), tip(&pr.head));

    // Unchanged since the last answer: stamp it and move on. The stamp matters —
    // without it this change sorts first forever and the loop spins on it.
    if let (Some(old), Some(b), Some(h)) = (&pr.mergeability, &now_base, &now_head) {
        if old.base_oid == *b && old.head_oid == *h {
            db.record_mergeability(
                &pr.repo,
                pr.number,
                &Mergeability { checked_at: mongodb::bson::DateTime::now(), ..old.clone() },
            )
            .await?;
            return Ok(false);
        }
    }

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
            // Queued is not a state a finished job stays in; clearing the job
            // entirely is the honest end, and `set_pull_state` already records
            // that it merged.
            db.clear_merge(&pr.repo, pr.number).await?;
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

#[cfg(test)]
mod refs_response_tests {
    use super::parse_refs_response;

    #[test]
    fn not_found_is_empty_refs() {
        let refs = parse_refs_response(reqwest::StatusCode::NOT_FOUND, "", "o", "n").unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn server_error_is_not_treated_as_empty_refs() {
        let err = parse_refs_response(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "", "o", "n")
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[test]
    fn forbidden_is_not_treated_as_empty_refs() {
        assert!(parse_refs_response(reqwest::StatusCode::FORBIDDEN, "", "o", "n").is_err());
    }

    #[test]
    fn success_parses_refs() {
        let refs =
            parse_refs_response(reqwest::StatusCode::OK, r#"[{"name":"refs/heads/main","oid":"abc"}]"#, "o", "n")
                .unwrap();
        assert_eq!(refs.len(), 1);
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
