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
use rustic_git::directory::{Directory, MergeState, PullRequest};
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
/// How often a lane reclaims entries a dead consumer left unacked.
const RECLAIM_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// How long an entry may sit claimed-but-unacked before `XAUTOCLAIM` hands it to a different
/// consumer — long enough that a slow-but-alive lane isn't fought over, short enough that a
/// lane that died doesn't strand a nudge for a whole reclaim interval.
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
    // Correctness never depended on Redis (see `Cache::connect`'s fail-open design) and still
    // does not — the floor is the owning node's own periodic lane, which needs neither Redis nor
    // Mongo. What is lost without Redis is only speed: no nudges reach this worker, so every
    // change waits for that lane's drift ceiling instead of being looked at within seconds. Loud
    // on purpose, so a missing `RUSTIC_GIT_REDIS_URL` shows up in logs rather than showing up as
    // "mergeability takes a minute to update now".
    if !store.cache.connected() {
        eprintln!(
            "merge worker: no Redis (RUSTIC_GIT_REDIS_URL unset or unreachable) — no live stream \
             nudges; mergeability checks fall back to each owning node's own sweep and will be \
             much slower to notice changes"
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
/// Mergeability checking is no longer discovered here at all. It used to be, twice over —
/// `pull_to_check` polling Mongo for whatever change was looked at longest ago, plus the stream
/// as a nudge — and neither can survive a change living in its repo's own database: scanning
/// would mean opening databases this process does not own, which FENCES the node serving them.
///
/// So the split is now across processes rather than across clocks:
///
///   * this lane consumes the `events` stream and forwards each entry to the node that owns the
///     repo as a `pulls/{n}/check` POST — the low-latency path, "go look at this one, now";
///   * the FLOOR is that node's own periodic lane (`App::check_owned_pulls`), which sweeps the
///     repos it owns whether or not anything reached it. A dropped, evicted or never-delivered
///     nudge costs a change one drift ceiling of staleness, never a check — and unlike the old
///     floor, that holds with Mongo down too.
///
/// Merge JOBS are still claimed from Mongo here: moving merge execution to the owning node is a
/// separate change, and the claim is what keeps two lanes from running one merge twice.
///
/// `XAUTOCLAIM` runs on its own slower clock to reclaim entries a dead consumer left unacked —
/// "delayed, never lost" applied to redelivery instead of discovery.
async fn lane(
    store: &Arc<rustic_git::store::Store>,
    db: &Directory,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    me: &str,
) {
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
        if last_claim.elapsed() >= RECLAIM_EVERY {
            last_claim = std::time::Instant::now();
            let claimed = store
                .cache
                .xautoclaim(EVENTS_STREAM, EVENTS_GROUP, me, CLAIM_STALE_AFTER_MS, 16)
                .await;
            for (id, fields) in claimed {
                handle_event(client, upstream, secret, &fields).await;
                store.cache.xack(EVENTS_STREAM, EVENTS_GROUP, &id).await;
            }
        }

        let delivered = store.cache.xreadgroup(EVENTS_STREAM, EVENTS_GROUP, me, 16).await;
        if delivered.is_empty() {
            // `xreadgroup` never blocks (see `cache.rs` — a blocking read would park the shared
            // multiplexed connection and starve every other command), so this sleep is the ONLY
            // thing pacing the lane, on a live Redis just as much as a dead one. Without it the
            // loop would spin `claim_merge` as fast as Mongo answers. It also sets
            // the worst-case delay between an event landing and a lane noticing it.
            tokio::time::sleep(IDLE).await;
            continue;
        }
        for (id, fields) in delivered {
            handle_event(client, upstream, secret, &fields).await;
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

/// Turn one delivered stream entry into a nudge to the node that owns the repo.
///
/// The worker no longer computes mergeability and no longer goes looking for it: a change lives
/// in its repo's own database, and opening that here would fence the node serving it. So this
/// says only "go look at alice/web#3", and the owner — which holds the refs and the objects —
/// works the answer out locally and writes it.
///
/// Ack happens regardless of the outcome (see the caller). A nudge that fails here is not lost
/// work: the owning node's own periodic lane picks the same change up within its drift ceiling.
/// That lane, not this one, is the safety floor now — which is why it is fine for this whole path
/// to depend on Redis and on the fleet being reachable.
async fn handle_event(
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    fields: &[(String, String)],
) {
    let Some(e) = rustic_git::events::from_fields(fields) else { return };
    let Some((owner, name)) = e.repo.split_once('/') else { return };
    // `HeadMoved` is repo-wide, not about one change: number 0 is the whole-repo form the check
    // route understands, and the fan-out (and its cap) belongs to the owner, which can list the
    // open changes without a round trip.
    let number = if targets_whole_repo(&e) { 0 } else { e.number };
    // Kinds with no mergeability effect cost the owner one cheap no-op, so they are not filtered
    // here — `pulls::check` returns without writing when nothing moved, and a closed change is
    // skipped outright.
    let url = format!("{upstream}/api/{owner}/{name}/pulls/{number}/check");
    match client.post(url).header(rustic_git::proxy::PEER_HEADER, secret).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("checking {}#{number}: {}", e.repo, r.status()), // ponytail: eprintln
        Err(err) => eprintln!("checking {}#{number}: {err}", e.repo), // ponytail: eprintln
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
