//! The merge worker.
//!
//! It merges again — but not the way it used to. Merging is a fetch, a three-way merge and a
//! push, all of it expressible over the git protocol, so it does NOT have to happen where the
//! repo's database is. It happens here, by running the real `git` binary
//! (`rustic_git_pulls::merge_worker`) against a bare cache clone, authenticated to the fleet as a
//! peer.
//!
//! That keeps two rules intact at once. The database still has exactly one opener — this process
//! never opens one; it asks the owner to claim the job and tells it the outcome over HTTP. And
//! BRANCH PROTECTION still holds, because the result reaches the repo as a PUSH through
//! `receive-pack`, judged by the same rule that judges anybody's push. What is bought is that an
//! unbounded tree merge no longer sits in front of the clones and pushes the owning node is
//! serving for that same repo.
//!
//! Three lanes' worth of work, on one stream:
//!
//!   * `MergeRequested` — claim the change from its owner, merge it, report back.
//!   * everything else — nudge the owner to re-check mergeability, and do the trial merge for
//!     whichever changes it says diverged (the cheap ancestry verdicts stay on the owner, which
//!     already has the graph).
//!   * the blob sweep, unrelated work that touches only the object store.
//!
//! The safety floor is still the owner's own periodic lanes (`App::check_owned_pulls`,
//! `App::announce_stranded_merges`), which need neither Redis nor Mongo: a nudge that never arrives, or
//! a worker that dies mid-merge, costs a change one lease of latency, never the work.

use rustic_git_core::err;
use rustic_git_core::Result;
use rustic_git_registry::uploads::UploadsExt;
use rustic_git_storage::config::{env, install_crypto_provider, open_store};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    rustic_git_core::log::init();
    rustic_git_core::metrics::init();
    rustic_git_core::metrics::serve_if_configured().await;
    if let Err(e) = run().await {
        tracing::error!("{e}");
        std::process::exit(2);
    }
}

/// How long to wait when there was nothing to do.
const IDLE: std::time::Duration = std::time::Duration::from_secs(2);

/// The one stream every repo's events multiplex onto (see `rustic_git_storage::events`), and the
/// one consumer group every merge-worker lane/replica competes on.
const EVENTS_STREAM: &str = "events";
const EVENTS_GROUP: &str = "merge-worker";
/// How often a lane reclaims entries a dead consumer left unacked.
const RECLAIM_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// How long an entry may sit claimed-but-unacked before `XAUTOCLAIM` hands it to a different
/// consumer — long enough that a slow-but-alive lane isn't fought over, short enough that a
/// lane that died doesn't strand a nudge for a whole reclaim interval.
const CLAIM_STALE_AFTER_MS: u64 = 30_000;

async fn run() -> Result<()> {
    install_crypto_provider();

    // `false`: compaction and garbage collection belong to the node that owns the
    // repository. This process only ever adds packs.
    let store = open_store(false).await?;
    let upstream = env("RUSTIC_GIT_UPSTREAM", "http://rustic-git:8081");
    let secret = std::env::var("RUSTIC_GIT_PEER_SECRET")
        .map_err(|_| err("RUSTIC_GIT_PEER_SECRET required"))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default();

    // Nudging is mostly waiting on the fleet, so one lane leaves the worker idle whenever a
    // node is slow to answer. Independent tasks, each reading the stream for itself — the
    // consumer group is what keeps them from delivering the same entry twice.
    let lanes: usize = env("RUSTIC_GIT_WORKER_CONCURRENCY", "4").parse().unwrap_or(4).clamp(1, 64);
    // Liveness for a process with no listener: every lane touches its OWN file at the top of each
    // iteration, and the Deployment's probe counts how many are fresh. One file per lane, not one
    // shared file: with a shared one a single live lane keeps the heartbeat young while the other
    // N-1 sit wedged, which is exactly the failure the probe exists to catch. A lane can be slow
    // (sixteen nudges at the client's 60s timeout is sixteen minutes) but not silent; the probe
    // window is wider than the slowest honest iteration, so it only fires for a truly stuck loop.
    let cache = std::path::PathBuf::from(env("RUSTIC_GIT_CACHE_DIR", "./.local/cache"));
    let _ = std::fs::create_dir_all(&cache);
    tracing::info!(lanes, %upstream, "merge worker ready");
    // Checked once, here, rather than discovered per merge: without git this process still nudges
    // and still sweeps blobs, so it looks healthy while refusing every merge it is handed. Loud at
    // startup is the difference between "the image is wrong" and "merges mysteriously fail".
    if !rustic_git_pulls::merge_worker::available() {
        tracing::error!(
            "merge worker: no `git` on PATH — every merge will be REFUSED. Install git in the \
             runtime image (bookworm's 2.39 is new enough); mergeability for diverged changes \
             will stay unanswered too"
        );
    }
    // Correctness never depended on Redis (see `Cache::connect`'s fail-open design) and still
    // does not — the floor is the owning node's own periodic lane, which needs neither Redis nor
    // Mongo. What is lost without Redis is only speed: no nudges reach this worker, so every
    // change waits for that lane's drift ceiling instead of being looked at within seconds. Loud
    // on purpose, so a missing `RUSTIC_GIT_REDIS_URL` shows up in logs rather than showing up as
    // "mergeability takes a minute to update now".
    if !store.cache.connected() {
        tracing::warn!(
            "merge worker: no Redis (RUSTIC_GIT_REDIS_URL unset or unreachable) — no live stream \
             nudges; mergeability checks fall back to each owning node's own sweep and will be \
             much slower to notice changes"
        );
    }

    // Identifies one lane of one process to the consumer group, so `XAUTOCLAIM` can tell a dead
    // consumer's pending entries from a live one's. Random, not hostname+index: two pods
    // restarted into the same name would otherwise share a consumer and steal each other's.
    let run: u64 = rand::random();

    // Idempotent (see the doc comment): every replica that boots calls this, and only the first
    // one in the fleet's history actually creates anything.
    store.cache.xgroup_create_mkstream(EVENTS_STREAM, EVENTS_GROUP).await;

    // The blob sweep is unrelated work — it touches the object store directly, never a repo's
    // refs or packs — so it gets its own lane rather than competing with merge lanes for a slot.
    let grace = rustic_git_registry::gc::RUSTIC_GIT_BLOB_GRACE_SECS;
    let gc_store = Arc::clone(&store);
    let gc_cache = cache.clone();
    let mut tasks =
        vec![tokio::spawn(async move { gc_lane(&gc_store, grace, &gc_cache).await })];
    for i in 0..lanes {
        let alive = cache.join(format!("worker-alive.{i}"));
        let w = Worker {
            store: Arc::clone(&store),
            client: client.clone(),
            upstream: upstream.clone(),
            secret: secret.clone(),
            cache: cache.clone(),
            me: format!("{run:016x}/{i}"),
        };
        tasks.push(tokio::spawn(async move { lane(&w, &alive).await }));
    }
    // Every lane loops forever, so the FIRST one to finish — panic or return — is a dead lane.
    // Awaiting the handles in order would only notice lane N after lanes 0..N had finished,
    // which is never; this resolves on any of them, and the `Err` exits the process so the pod
    // restarts at full capacity instead of quietly running short.
    Err(err(first_exit(tasks).await))
}

async fn first_exit(tasks: Vec<tokio::task::JoinHandle<()>>) -> String {
    let (result, index, _rest) = futures::future::select_all(tasks).await;
    match result {
        Ok(()) => format!("worker lane {index} returned"),
        Err(e) => format!("worker lane {index} died: {e}"),
    }
}

/// One lane: consume the stream, nudge the owner, repeat.
///
/// Neither kind of work is discovered here any more. Mergeability checking used to poll Mongo for
/// whatever change was looked at longest ago; merge jobs used to be claimed the same way. Both
/// scan pull requests, and a pull request now lives in its repo's own database — scanning would
/// mean opening databases this process does not own, which FENCES the node serving them.
///
/// So the split is across processes rather than across clocks:
///
///   * this lane forwards each stream entry to the node that owns the repo as a
///     `pulls/{n}/check` POST — the low-latency path, "go look at this one, now";
///   * the FLOOR is that node's own periodic lanes, which sweep the repos it owns whether or not
///     anything reached it. A dropped, evicted or never-delivered nudge costs a change one drift
///     ceiling of staleness, never a check or a merge — and unlike the old floor, that holds with
///     Mongo down too.
///
/// `XAUTOCLAIM` runs on its own slower clock to reclaim entries a dead consumer left unacked —
/// "delayed, never lost" applied to redelivery instead of discovery.
/// One lane's share of everything a lane needs. A struct rather than six parameters threaded
/// through four functions: the merge path needs all of them, and the next thing added would have
/// to be added in four places.
struct Worker {
    store: Arc<rustic_git_storage::store::Store>,
    client: reqwest::Client,
    upstream: String,
    secret: String,
    cache: std::path::PathBuf,
    /// Identifies this lane to the consumer group AND to the owner as a merge claimant.
    me: String,
}

async fn lane(w: &Worker, alive: &std::path::Path) {
    let (store, me) = (&w.store, w.me.as_str());
    let mut last_claim = std::time::Instant::now();
    loop {
        // This lane's own heartbeat; the probe counts fresh ones against the lane count, so a lane
        // that stops writing is noticed even while its siblings keep going. Errors ignored: a
        // probe that fails because the cache directory is unwritable is the right outcome, and
        // logging it every 2s is not.
        let _ = std::fs::write(alive, b"");
        // Reclaim work whose consumer died before it acked, so a crashed lane's nudges are not
        // stranded until the next full sweep pass.
        if last_claim.elapsed() >= RECLAIM_EVERY {
            last_claim = std::time::Instant::now();
            let claimed = store
                .cache
                .xautoclaim(EVENTS_STREAM, EVENTS_GROUP, me, CLAIM_STALE_AFTER_MS, 16)
                .await;
            let ids: Vec<String> = claimed.iter().map(|(id, _)| id.clone()).collect();
            store.cache.xack(EVENTS_STREAM, EVENTS_GROUP, &ids).await;
            for (_, fields) in claimed {
                let _ = std::fs::write(alive, b"");
                handle_event(w, &fields).await;
            }
        }

        let delivered = store.cache.xreadgroup(EVENTS_STREAM, EVENTS_GROUP, me, 16).await;
        if delivered.is_empty() {
            // `xreadgroup` never blocks (see `cache.rs` — a blocking read would park the shared
            // multiplexed connection and starve every other command), so this sleep is the ONLY
            // thing pacing the lane, on a live Redis just as much as a dead one. It also sets the
            // worst-case delay between an event landing and a lane noticing it.
            tokio::time::sleep(IDLE).await;
            continue;
        }
        // The whole batch is acked in ONE round trip BEFORE any of it is handled, and the
        // heartbeat touched per entry. The stream is a nudge, never the record (`CLAUDE.md`): a
        // merge's record is the owner's claim, so an entry acked-then-lost costs one lease of
        // latency, whereas an entry held unacked through a long merge was `XAUTOCLAIM`ed by a
        // sibling lane at 30s and merged twice. Per-entry heartbeats keep a lane draining sixteen
        // slow merges from looking wedged.
        let ids: Vec<String> = delivered.iter().map(|(id, _)| id.clone()).collect();
        store.cache.xack(EVENTS_STREAM, EVENTS_GROUP, &ids).await;
        for (_, fields) in delivered {
            let _ = std::fs::write(alive, b"");
            handle_event(w, &fields).await;
        }
    }
}

/// A repo-wide event is `HeadMoved` specifically (its `number: 0` is the marker — see
/// `browse_api::pulls::api_pull_outcome`'s publish), never just "any event whose number happens to be 0": a stray or
/// legacy `PullOpened`/`PullCommented` with `number: 0` must stay a (no-op) single-PR lookup,
/// not fan out to the whole repo. Pulled out as a pure predicate so this can be unit-tested
/// without a `Directory`/Mongo fixture.
fn targets_whole_repo(e: &rustic_git_storage::events::Event) -> bool {
    e.number == 0 && matches!(e.kind, rustic_git_storage::events::Kind::HeadMoved)
}

/// Turn one delivered stream entry into work.
///
/// Ack happens before this runs (see the caller). Nothing that fails here is lost work:
/// a merge stays claimed until its lease lapses and the owner re-announces it, and a check the
/// owner never heard about is redone by its own periodic sweep. That floor, not this path, is
/// what makes it safe for all of this to depend on Redis and on the fleet being reachable.
async fn handle_event(w: &Worker, fields: &[(String, String)]) {
    let Some(e) = rustic_git_storage::events::from_fields(fields) else { return };
    let Some((owner, name)) = e.repo.split_once('/') else { return };
    let (owner, name) = (owner.to_string(), name.to_string());
    // One merge cache per repo, and one lane in it at a time: two lanes fetching and merging in
    // the same directory would race on refs and on the single result ref. Held across the whole
    // of the work, not just the git part, because the claim is what the lock is really about.
    let lock = w.store.keyed_lock(&format!("merge/{owner}/{name}"));
    let _guard = lock.lock().await;

    if matches!(e.kind, rustic_git_storage::events::Kind::MergeRequested) {
        merge_one(w, &owner, &name, e.number).await;
        return;
    }

    // `HeadMoved` is repo-wide, not about one change: number 0 is the whole-repo form the check
    // route understands, and the fan-out (and its cap) belongs to the owner, which can list the
    // open changes without a round trip.
    let number = if targets_whole_repo(&e) { 0 } else { e.number };
    // Kinds with no mergeability effect cost the owner one cheap no-op, so they are not filtered
    // here — `pulls::check` returns without writing when nothing moved, and a closed change is
    // skipped outright.
    let deep: Vec<rustic_git_pulls::pulls::Deep> =
        match post(w, &owner, &name, number, "check", None).await {
            Ok(Some(v)) => v,
            Ok(None) => Vec::new(),
            Err(why) => {
                tracing::warn!(repo = %e.repo, number, %why, "checking change");
                return;
            }
        };
    // Whatever ancestry could not answer. The owner has already written "checking…" against each
    // of these, so a trial merge that never happens shows as pending rather than as a wrong verdict.
    //
    // ONE fetch for the whole fan-out: a `HeadMoved` can hand back `CHECK_LIMIT` changes, and every
    // one of them wants the same cache at the same tips. A fetch that fails is not fatal — the
    // cache may still hold usable tips from a previous sync, and `check_local` says `Unknown` when
    // it does not.
    if !deep.is_empty() {
        let mut branches: Vec<String> = Vec::new();
        for d in &deep {
            for b in [&d.base, &d.head] {
                if !branches.contains(b) {
                    branches.push(b.clone());
                }
            }
        }
        let (cache, upstream, secret) = (w.cache.clone(), w.upstream.clone(), w.secret.clone());
        let (o, n) = (owner.clone(), name.clone());
        let synced = tokio::task::spawn_blocking(move || {
            rustic_git_pulls::merge_worker::sync_branches(&cache, &upstream, &secret, &o, &n, &branches)
        })
        .await;
        if let Ok(Err(why)) = &synced {
            tracing::warn!(%owner, %name, %why, "syncing branches");
        }
    }
    for d in deep {
        check_one(w, &owner, &name, &d).await;
    }
}

/// POST one of the owner's peer-only pull routes and read its JSON answer, if it has one.
///
/// `Ok(None)` is a success with no body (204). `Err` carries a sentence for the log — never the
/// peer secret, which lives only in the header.
async fn post<T: serde::de::DeserializeOwned>(
    w: &Worker,
    owner: &str,
    name: &str,
    number: i64,
    tail: &str,
    body: Option<serde_json::Value>,
) -> std::result::Result<Option<T>, String> {
    let url = format!("{}/api/{owner}/{name}/pulls/{number}/{tail}", w.upstream);
    let mut req = w
        .client
        .post(url)
        .header(rustic_git_core::peer::PEER_HEADER, &w.secret)
        // The identity the owner authorizes these routes as. The repo's owner, because that is
        // whose repo the worker is acting on — see `browse_api::pulls::as_owner`.
        .header(rustic_git_core::peer::OWNER_HEADER, owner);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let r = req.send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(r.status().to_string());
    }
    let bytes = r.bytes().await.map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&bytes).map(Some).map_err(|e| e.to_string())
}

/// Claim one change's merge from its owner, perform it, and report how it went.
///
/// A 409 on the claim is the normal answer to a duplicate delivery — someone else has it, or it
/// already finished — so it is not logged as a failure. The outcome POST is the only thing that
/// ends the job: if this process dies between the merge and the report, the merge itself was a
/// push (idempotent, and already landed or not) and the lease brings the job back.
async fn merge_one(w: &Worker, owner: &str, name: &str, number: i64) {
    let claimed = post::<rustic_git_pulls::merge_worker::Job>(
        w,
        owner,
        name,
        number,
        &format!("claim?by={}", urlencoding(&w.me)),
        None,
    )
    .await;
    let job = match claimed {
        Ok(Some(j)) => j,
        Ok(None) => return,
        // Includes the 409 "someone else has it", which is the common case on a redelivery.
        Err(_) => return,
    };
    let (cache, upstream, secret) = (w.cache.clone(), w.upstream.clone(), w.secret.clone());
    let started = std::time::Instant::now();
    let done = tokio::task::spawn_blocking(move || {
        rustic_git_pulls::merge_worker::run(&job, &cache, &upstream, &secret)
    })
    .await;
    metrics::histogram!("merge_duration_seconds").record(started.elapsed().as_secs_f64());
    let outcome = match done {
        Ok(Ok(o)) => {
            let state = format!("{:?}", o.state).to_ascii_lowercase();
            metrics::counter!("merge_outcomes_total", "state" => state).increment(1);
            o
        }
        // Neither a merge nor an answer: leave the job claimed and let its lease bring it back,
        // rather than recording a failure this worker cannot stand behind.
        Ok(Err(e)) => {
            metrics::counter!("merge_outcomes_total", "state" => "error").increment(1);
            tracing::error!(%owner, %name, number, error = %e, "merging change");
            return;
        }
        Err(e) => {
            metrics::counter!("merge_outcomes_total", "state" => "error").increment(1);
            tracing::error!(%owner, %name, number, error = %e, "merging change");
            return;
        }
    };
    let body = serde_json::to_value(&outcome).unwrap_or_default();
    // `by` is the token this lane claimed with. The owner refuses the report if the job has since
    // been claimed by someone else — this lane's lease lapsed while it was merging, and the newer
    // claimant's answer is the one that counts.
    let tail = format!("outcome?by={}", urlencoding(&w.me));
    if let Err(why) = post::<serde_json::Value>(w, owner, name, number, &tail, Some(body)).await {
        tracing::error!(%owner, %name, number, %why, "reporting merge outcome");
    }
}

/// The trial merge for one diverged change, and the verdict sent back. Purely local: the caller
/// has already fetched every branch of the fan-out in one go.
async fn check_one(w: &Worker, owner: &str, name: &str, d: &rustic_git_pulls::pulls::Deep) {
    let job = rustic_git_pulls::merge_worker::Job {
        owner: owner.to_string(),
        name: name.to_string(),
        number: d.number,
        strategy: String::new(), // unused by a check: it never commits and never pushes
        base: d.base.clone(),
        head: d.head.clone(),
        title: String::new(),
        requested_by: String::new(),
    };
    let cache = w.cache.clone();
    let verdict =
        tokio::task::spawn_blocking(move || rustic_git_pulls::merge_worker::check_local(&job, &cache))
            .await;
    let verdict = match verdict {
        Ok(Ok(v)) => v,
        // Left as "checking…"; the owner's next sweep asks again. Better than writing a verdict
        // this worker could not actually reach the fleet to compute.
        Ok(Err(e)) => {
            tracing::warn!(%owner, %name, number = d.number, error = %e, "checking change");
            return;
        }
        Err(e) => {
            tracing::warn!(%owner, %name, number = d.number, error = %e, "checking change");
            return;
        }
    };
    let body = serde_json::to_value(&verdict).unwrap_or_default();
    if let Err(why) =
        post::<serde_json::Value>(w, owner, name, d.number, "mergeability", Some(body)).await
    {
        tracing::warn!(%owner, %name, number = d.number, %why, "reporting the check of change");
    }
}

/// The lane id goes into a query string, and it contains a `/`. Percent-encoding exactly the
/// characters that would otherwise change the URL's shape is smaller than a dependency for it.
fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            c => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

/// How long to rest between owners within a sweep pass, and between whole passes once every
/// owner has had a turn. One owner per cycle — not every owner at once — so the sweep never
/// shows up as a burst of object-store listing traffic on top of whatever pushes are in flight.
const GC_OWNER_GAP: std::time::Duration = std::time::Duration::from_secs(5);
const GC_PASS_GAP: std::time::Duration = std::time::Duration::from_secs(60);

/// Every owner with anything under any image prefix. `blobs/` alone misses an owner whose layers
/// were all deleted but whose manifests remain, and one whose image database exists with nothing
/// pushed yet — both still need their listing markers reconciled. A prefix that fails to list is
/// logged and skipped: the others still get their turn.
async fn image_owners(store: &rustic_git_storage::store::Store) -> std::collections::BTreeSet<String> {
    let mut owners = std::collections::BTreeSet::new();
    for prefix in ["blobs/", "manifests/", "repo/img/"] {
        owners.extend(owners_under(store, prefix).await);
    }
    owners
}

/// The owner names directly under one prefix. A prefix that fails to list warns and yields none:
/// the sweep is keep-biased, so a missing owner costs this pass and nothing more.
async fn owners_under(store: &rustic_git_storage::store::Store, prefix: &str) -> Vec<String> {
    match rustic_git_registry::list_dir_names(&store.os, prefix).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(%prefix, error = %e, "gc: listing prefix");
            vec![]
        }
    }
}

/// Sweep one owner at a time, forever. Reads every manifest before it deletes a single blob —
/// see `registry::gc` for why that order is load-bearing — so a wrong answer here destroys a
/// layer a live image still needs, which is why it runs on its own schedule instead of hurrying.
/// How long a repo's merge cache may sit unused before it is deleted. A cache is a pure
/// derivative of the fleet, so this only ever costs a re-fetch — but a worker that has served a
/// thousand repos would otherwise hold a bare clone of every one of them forever.
const CACHE_KEEP: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// The byte budget the merge caches are pruned to, least recently used first, whatever their age.
/// 60 % of the 20 Gi emptyDir in the deploy yaml, leaving room for the worktree a rebase checks
/// out beside the caches.
const RUSTIC_GIT_MERGE_CACHE_BYTES: u64 = 12 << 30;

async fn gc_lane(
    store: &rustic_git_storage::store::Store,
    grace: std::time::Duration,
    cache: &std::path::Path,
) {
    let upload_grace = rustic_git_registry::uploads::upload_grace();
    loop {
        // Cheap and local — no object store, no fleet — so it rides the sweep it cannot slow down.
        match rustic_git_pulls::merge_worker::prune(cache, CACHE_KEEP, RUSTIC_GIT_MERGE_CACHE_BYTES) {
            0 => {}
            n => tracing::info!(dropped = n, "gc: dropped idle merge cache(s)"),
        }
        let owners = image_owners(store).await;
        // Uploads are swept for their own owner set: a push can leave a staging object behind
        // before it ever lands a blob, so an owner with only abandoned sessions and no blobs yet
        // must still be visited, not just the owners `image_owners` finds.
        let upload_owners = owners_under(store, "uploads/").await;
        // Repo owners are their own set: an owner with code repos and no images appears under
        // neither `blobs/` nor `uploads/`. `img` is filtered out because `repo/img/...` is the
        // image keyspace, not an owner with repos — see `reconcile_repo_owner`.
        let repo_owners = owners_under(store, "repo/").await;
        if owners.is_empty() && upload_owners.is_empty() && repo_owners.is_empty() {
            tokio::time::sleep(GC_PASS_GAP).await;
            continue;
        }
        for owner in &owners {
            match rustic_git_registry::gc::sweep_owner(store, owner, grace).await {
                Ok(n) if n > 0 => tracing::info!(%owner, blobs = n, "gc: swept blob(s) for owner"),
                Ok(_) => {}
                Err(e) => tracing::warn!(%owner, error = %e, "gc: sweeping owner"),
            }
            match rustic_git_registry::gc::reconcile_owner(store, owner).await {
                Ok(n) if n > 0 => tracing::info!(%owner, markers = n, "gc: reconciled listing marker(s) for owner"),
                Ok(_) => {}
                Err(e) => tracing::warn!(%owner, error = %e, "gc: reconciling markers for owner"),
            }
            tokio::time::sleep(GC_OWNER_GAP).await;
        }
        for owner in repo_owners.iter().filter(|o| o.as_str() != "img") {
            match rustic_git_registry::gc::reconcile_repo_owner(store, owner).await {
                Ok(n) if n > 0 => tracing::info!(%owner, markers = n, "gc: reconciled repo listing marker(s) for owner"),
                Ok(_) => {}
                Err(e) => tracing::warn!(%owner, error = %e, "gc: reconciling repo markers for owner"),
            }
            tokio::time::sleep(GC_OWNER_GAP).await;
        }
        for owner in &upload_owners {
            match store.sweep_stale_uploads(owner, upload_grace).await {
                Ok(n) if n > 0 => tracing::info!(%owner, sessions = n, "gc: swept stale upload session(s) for owner"),
                Ok(_) => {}
                Err(e) => tracing::warn!(%owner, error = %e, "gc: sweeping uploads for owner"),
            }
            tokio::time::sleep(GC_OWNER_GAP).await;
        }
        tokio::time::sleep(GC_PASS_GAP).await;
    }
}

#[cfg(test)]
mod targets_whole_repo_tests {
    use super::targets_whole_repo;
    use rustic_git_storage::events::{Event, Kind};

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
mod first_exit_tests {
    use super::first_exit;

    /// The point of the worker's supervision: a panic in ANY lane is noticed while the others
    /// are still running — not after they finish, which for a lane is never.
    #[tokio::test]
    async fn a_panicking_lane_is_noticed_while_another_still_runs() {
        let forever = tokio::spawn(async { std::future::pending::<()>().await });
        let dies = tokio::spawn(async { panic!("lane died") });
        let reason =
            tokio::time::timeout(std::time::Duration::from_secs(2), first_exit(vec![forever, dies]))
                .await
                .expect("must resolve while the other lane is still running");
        assert!(reason.contains("lane 1"), "got {reason}");
    }

    /// A lane that RETURNS is just as dead as one that panics — it stops doing its share either
    /// way — so it must resolve `first_exit` too, not only the panicking case.
    #[tokio::test]
    async fn a_lane_that_returns_is_a_death_too() {
        let quits = tokio::spawn(async {});
        let forever = tokio::spawn(async { std::future::pending::<()>().await });
        let reason =
            tokio::time::timeout(std::time::Duration::from_secs(2), first_exit(vec![quits, forever]))
                .await
                .expect("must resolve while the other lane is still running");
        assert_eq!(reason, "worker lane 0 returned");
    }
}

#[cfg(test)]
mod image_owners_tests {
    use super::image_owners;
    use slatedb::object_store::{memory::InMemory, path::Path as OsPath, ObjectStoreExt, PutPayload};
    use std::sync::Arc;

    /// An owner is anyone with anything under ANY of the image prefixes: blobs-only (mid-push),
    /// manifests-only (blobs deleted), or a bare image directory (DB created, nothing pushed).
    #[tokio::test]
    async fn owners_are_the_union_of_blobs_manifests_and_image_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = rustic_git_storage::store::Store::open(Arc::new(InMemory::new()), tmp.path().join("cache"), false)
            .await
            .unwrap();
        for p in ["blobs/alpha/sha256/aa", "manifests/beta/nginx/sha256/bb", "repo/img/gamma/nginx/manifest/0.sst"] {
            store.os.put(&OsPath::from(p), PutPayload::from("x")).await.unwrap();
        }
        let owners: Vec<String> = image_owners(&store).await.into_iter().collect();
        assert_eq!(owners, vec!["alpha", "beta", "gamma"]);
    }
}
