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

/// The one stream every repo's events multiplex onto (see `crate::events`), and the one
/// consumer group every merge-worker lane/replica competes on.
const EVENTS_STREAM: &str = "events";
const EVENTS_GROUP: &str = "merge-worker";
/// The fallback sweep cadence: even with Redis fully down, a lane must still discover pending
/// mergeability checks on its own, just less eagerly than a live nudge would. ~60s per the brief.
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// How long an entry may sit claimed-but-unacked before `XAUTOCLAIM` hands it to a different
/// consumer — long enough that a slow-but-alive lane isn't fought over, short enough that a
/// lane that died doesn't strand a nudge for the whole sweep interval.
const CLAIM_STALE_AFTER_MS: u64 = 30_000;

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
    // Correctness never depended on Redis (see `Cache::connect`'s fail-open design), but speed
    // now does: without it, `check_from_event` never fires and every PR waits for the
    // `SWEEP_EVERY` sweep alone, one PR per pass, per lane — a silent regression vs the pre-
    // stream poller. Loud on purpose so a missing `RUSTIC_GIT_REDIS_URL` shows up in logs
    // instead of just showing up as "PRs take longer to update now".
    if !store.cache.connected() {
        eprintln!(
            "merge worker: no Redis (RUSTIC_GIT_REDIS_URL unset or unreachable) — falling back \
             to the {}s sweep alone, no live stream nudges; mergeability checks will be much \
             slower to notice changes",
            SWEEP_EVERY.as_secs()
        ); // ponytail: eprintln
    }

    // Identifies one lane of one process, so a claim can be confirmed rather than
    // assumed. Random, not hostname+index: two pods restarted into the same name
    // would otherwise share a token and each believe it won the other's job.
    let run: u64 = rand::random();

    // Idempotent (see the doc comment): every replica that boots calls this, and only the first
    // one in the fleet's history actually creates anything.
    store.cache.xgroup_create_mkstream(EVENTS_STREAM, EVENTS_GROUP).await;

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
///
/// Mergeability checking used to be discovered ONE way: `pull_to_check` polling Mongo for
/// whatever change was checked longest ago. Now there are two, and the split is deliberate:
///
///   * The `events` stream is a NUDGE — "alice/web#3 changed, go look at it now" — so a lane
///     drains it first and reacts within `STREAM_BLOCK_MS`, not whenever that PR's turn comes
///     up in the sweep.
///   * `pull_to_check` stays exactly as it was, run on its own `SWEEP_EVERY` clock regardless of
///     what the stream is doing. This is load-bearing, not a leftover: Redis can drop the
///     stream, evict it, or be entirely down (`Cache::connect` degrades to a no-op — see
///     `crate::cache`'s and `crate::events`' doc comments), and a lane must still make progress
///     purely from Mongo. A missed or lost nudge costs a PR up to one sweep interval of extra
///     staleness, never a lost check.
///
/// `XAUTOCLAIM` runs on its own slower clock to reclaim entries a dead consumer left unacked —
/// the same "delayed, never lost" property applied to redelivery instead of discovery.
async fn lane(
    store: &Arc<rustic_git::store::Store>,
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    me: &str,
) {
    let mut last_sweep = std::time::Instant::now() - SWEEP_EVERY; // sweep once right away
    let mut last_claim = std::time::Instant::now();
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
                continue; // a merge just happened; look for the next one at once
            }
            Err(e) => {
                eprintln!("claiming a merge: {e}"); // ponytail: eprintln
                tokio::time::sleep(IDLE).await;
                continue;
            }
            Ok(None) => {} // fall through to the stream/sweep below
        }

        // Reclaim work whose consumer died before it acked, so a crashed lane's nudges are not
        // stranded until the next full sweep pass.
        if last_claim.elapsed() >= SWEEP_EVERY {
            last_claim = std::time::Instant::now();
            let claimed = store
                .cache
                .xautoclaim(EVENTS_STREAM, EVENTS_GROUP, me, CLAIM_STALE_AFTER_MS, 16)
                .await;
            for (id, fields) in claimed {
                handle_event(db, client, upstream, secret, &fields).await;
                store.cache.xack(EVENTS_STREAM, EVENTS_GROUP, &id).await;
            }
        }

        // The fallback sweep: fires on its own clock, unconditionally, so a stream that keeps
        // delivering entries never starves it — this used to live inside the
        // `delivered.is_empty()` branch below, which meant a busy stream (the exact case a lost
        // event needs the sweep to cover) could keep `pull_to_check` from ever running no
        // matter how long `last_sweep.elapsed()` said it had been.
        if last_sweep.elapsed() >= SWEEP_EVERY {
            last_sweep = std::time::Instant::now();
            match check_one(db, client, upstream, secret).await {
                Ok(_) => {}
                Err(e) => eprintln!("checking mergeability: {e}"), // ponytail: eprintln
            }
        }

        let delivered = store.cache.xreadgroup(EVENTS_STREAM, EVENTS_GROUP, me, 16).await;
        if delivered.is_empty() {
            // `xreadgroup` never blocks (see `cache.rs` — a blocking read would park the shared
            // multiplexed connection and starve every other command), so this sleep is the ONLY
            // thing pacing the lane, on a live Redis just as much as a dead one. Without it the
            // loop would spin `claim_merge` and the sweep as fast as Mongo answers. It also sets
            // the worst-case delay between an event landing and a lane noticing it.
            tokio::time::sleep(IDLE).await;
            continue;
        }
        for (id, fields) in delivered {
            handle_event(db, client, upstream, secret, &fields).await;
            store.cache.xack(EVENTS_STREAM, EVENTS_GROUP, &id).await;
        }
    }
}

/// A repo-wide event is `HeadMoved` specifically (its `number: 0` is the marker — see
/// `merge_one`'s publish), never just "any event whose number happens to be 0": a stray or
/// legacy `PullOpened`/`PullCommented` with `number: 0` must stay a (no-op) single-PR lookup,
/// not fan out to the whole repo. Pulled out as a pure predicate so this can be unit-tested
/// without a `Directory`/Mongo fixture.
fn targets_whole_repo(e: &rustic_git::events::Event) -> bool {
    e.number == 0 && matches!(e.kind, rustic_git::events::Kind::HeadMoved)
}

/// The most open PRs a single `HeadMoved` fan-out will re-check. Every merge publishes
/// `HeadMoved` (see `merge_one`), so this is the steady-state path, not an edge case — a repo
/// with hundreds of open PRs must not turn one merge into a hundreds-deep serial burst of
/// `GET .../refs` calls that stalls the lane for everything else.
/// ponytail: a flat cap, not a queue — a repo with more open PRs than this loses the tail of
/// them to this event and waits for the next one (another merge) or the `SWEEP_EVERY` sweep to
/// catch up. Upgrade to paging/backpressure if a repo's open-PR count regularly exceeds it.
const HEAD_MOVED_FANOUT_LIMIT: i64 = 25;

/// Turn one delivered stream entry into a targeted mergeability check. Ack happens regardless of
/// the outcome (see the caller): a check that fails here is not lost work, because `SWEEP_EVERY`
/// picks the same PR back up — see the module-level doc on `lane`. An entry this worker doesn't
/// care about (a kind with no mergeability effect, or one it can't parse) is simply skipped.
async fn handle_event(
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    fields: &[(String, String)],
) {
    let Some(e) = rustic_git::events::from_fields(fields) else { return };
    // `HeadMoved` is repo-wide, not about one PR: it means "re-check the open PRs against this
    // repo's base", not "check PR #0" (which does not exist).
    if targets_whole_repo(&e) {
        let pulls = match db.open_pulls_for(&e.repo, HEAD_MOVED_FANOUT_LIMIT).await {
            Ok(p) => p,
            Err(err) => {
                eprintln!("listing open pulls for {} from HeadMoved: {err}", e.repo); // ponytail: eprintln
                return;
            }
        };
        for pr in pulls {
            let (repo, number) = (pr.repo.clone(), pr.number);
            if let Err(err) = check_pr(db, client, upstream, secret, pr).await {
                eprintln!("checking {repo}#{number} from HeadMoved: {err}"); // ponytail: eprintln
            }
        }
        return;
    }
    // Every kind here can move mergeability (a new/updated PR, a comment that might carry a
    // retarget, a base branch moving). `PullMerged`/`PullClosed` also qualify: `check_from_event`
    // is a no-op once the PR is no longer open, so re-checking a just-closed PR costs nothing.
    if let Err(err) = check_from_event(db, client, upstream, secret, &e.repo, e.number).await {
        eprintln!("checking {}#{} from event: {err}", e.repo, e.number); // ponytail: eprintln
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
        // Repo owners are their own set: an owner with code repos and no images appears under
        // neither `blobs/` nor `uploads/`. `img` is filtered out because `repo/img/...` is the
        // image keyspace, not an owner with repos — see `reconcile_repo_owner`.
        let repo_owners = match rustic_git::registry::list_dir_names(&store.os, "repo/").await {
            Ok(o) => o,
            Err(e) => {
                eprintln!("gc: listing repo owners: {e}"); // ponytail: eprintln
                vec![]
            }
        };
        if owners.is_empty() && upload_owners.is_empty() && repo_owners.is_empty() {
            tokio::time::sleep(GC_PASS_GAP).await;
            continue;
        }
        for owner in &owners {
            match rustic_git::registry::gc::sweep_owner(store, owner, grace).await {
                Ok(n) if n > 0 => eprintln!("gc: swept {n} blob(s) for {owner}"), // ponytail: eprintln
                Ok(_) => {}
                Err(e) => eprintln!("gc: sweeping {owner}: {e}"), // ponytail: eprintln
            }
            match rustic_git::registry::gc::reconcile_owner(store, owner).await {
                Ok(n) if n > 0 => eprintln!("gc: reconciled {n} listing marker(s) for {owner}"), // ponytail: eprintln
                Ok(_) => {}
                Err(e) => eprintln!("gc: reconciling markers for {owner}: {e}"), // ponytail: eprintln
            }
            tokio::time::sleep(GC_OWNER_GAP).await;
        }
        for owner in repo_owners.iter().filter(|o| o.as_str() != "img") {
            match rustic_git::registry::gc::reconcile_repo_owner(store, owner).await {
                Ok(n) if n > 0 => eprintln!("gc: reconciled {n} repo listing marker(s) for {owner}"), // ponytail: eprintln
                Ok(_) => {}
                Err(e) => eprintln!("gc: reconciling repo markers for {owner}: {e}"), // ponytail: eprintln
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
    check_pr(db, client, upstream, secret, pr).await
}

/// The event-driven counterpart to `check_one`: an `events` stream entry names a repo#number
/// directly, so this looks that one PR up instead of asking Mongo for "whatever is oldest" —
/// the whole point of consuming the stream is to check the PR the event is ABOUT, promptly,
/// rather than waiting for it to cycle to the front of the sweep.
///
/// A PR that is gone or already closed/merged is not an error: the event just arrived after the
/// fact (a `PullClosed` for the same PR, a stale redelivery), and there is nothing left to check.
async fn check_from_event(
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    repo: &str,
    number: i64,
) -> Result<bool> {
    let Some(pr) = db.pull(repo, number).await? else { return Ok(false) };
    if pr.state != rustic_git::directory::PullState::Open {
        return Ok(false);
    }
    check_pr(db, client, upstream, secret, pr).await
}

async fn check_pr(
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    pr: PullRequest,
) -> Result<bool> {
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
                &Mergeability { checked_at_ms: rustic_git::ownership::now_ms() as i64, ..old.clone() },
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
            checked_at_ms: rustic_git::ownership::now_ms() as i64,
            detail,
        },
    )
    .await?;
    Ok(true)
}

/// Do one merge, and record how it ended.
async fn merge_one(
    store: &Arc<rustic_git::store::Store>,
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
            let now_ms = mongodb::bson::DateTime::now().timestamp_millis();
            // Two nudges, not one: PullMerged is the pull's own timeline event (Task 4's feed
            // renders it on the PR); HeadMoved is that the base branch's tip just changed, which
            // is what makes any OTHER open PR against the same base worth re-checking — exactly
            // the case this worker's own stream consumer exists to react to.
            rustic_git::events::publish(
                &store.cache,
                &rustic_git::events::Event {
                    kind: rustic_git::events::Kind::PullMerged,
                    repo: pr.repo.clone(),
                    number: pr.number,
                    actor: job.requested_by.clone(),
                    at_ms: now_ms,
                    // The feed (Task 4) renders `detail` off these — carried here since this
                    // worker has `pr` in hand and would otherwise cost the feed a second read.
                    title: pr.title.clone(),
                    base: pr.base.clone(),
                    head: pr.head.clone(),
                },
            )
            .await;
            rustic_git::events::publish(
                &store.cache,
                &rustic_git::events::Event {
                    kind: rustic_git::events::Kind::HeadMoved,
                    repo: pr.repo.clone(),
                    // Not tied to any one PR — a base branch tip moving is repo-wide, so there is
                    // no `number` to carry (see `crate::events`' `Event::number` doc if that gap
                    // needs closing: repo-scoped events would need a distinct number-less shape).
                    // Same reasoning empties `title`/`base`/`head`: this event is not about one
                    // PR, and the feed does not show `HeadMoved` at all (see `pull_event`).
                    number: 0,
                    actor: job.requested_by.clone(),
                    at_ms: now_ms,
                    title: String::new(),
                    base: String::new(),
                    head: String::new(),
                },
            )
            .await;
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
mod targets_whole_repo_tests {
    use super::targets_whole_repo;
    use rustic_git::events::{Event, Kind};

    fn event(kind: Kind, number: i64) -> Event {
        Event {
            kind,
            repo: "alice/web".into(),
            number,
            actor: String::new(),
            at_ms: 0,
            title: String::new(),
            base: String::new(),
            head: String::new(),
        }
    }

    #[test]
    fn head_moved_at_zero_targets_the_whole_repo() {
        assert!(targets_whole_repo(&event(Kind::HeadMoved, 0)));
    }

    #[test]
    fn a_pull_opened_at_zero_does_not() {
        // Keys on the KIND, not the value: a stray/legacy `number: 0` on any other kind must
        // stay a single-PR (no-op) lookup, never a repo-wide fan-out.
        assert!(!targets_whole_repo(&event(Kind::PullOpened, 0)));
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
