//! The merge worker.
//!
//! It used to run merges. It no longer does — and it no longer discovers work of any kind.
//!
//! Merging always finished on the node that owns the repo, because REFS have exactly one writer
//! per repo and that is what keeps BRANCH PROTECTION in force: a merge is refused by the same
//! rule that refuses a force push. What moved is everything around it. A pull request lives in
//! its repo's own database now, and no other process may open that without fencing the node
//! serving it, so claiming a merge and recording its outcome had to move to the owner too.
//!
//! What is left here is the LOW-LATENCY path and nothing else: consume the `events` stream and
//! nudge the node that owns the repo, so a push or a merge request is looked at within seconds
//! rather than within a sweep interval. Plus the blob sweep, which is unrelated work that touches
//! only the object store and never a repo's database.
//!
//! The safety floor is the owner's own periodic lanes (`App::check_owned_pulls`,
//! `App::merge_owned_pulls`): a nudge that never arrives costs a change one drift ceiling of
//! latency, never the work. That floor needs neither Redis nor Mongo, which the old one did — so
//! this process being down, or Redis being down, now slows the system rather than stopping it.

use rustic_git::config::{env, open_store};
use rustic_git::Result;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}"); // ponytail: eprintln
        std::process::exit(2);
    }
}

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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default();

    // Nudging is mostly waiting on the fleet, so one lane leaves the worker idle whenever a
    // node is slow to answer. Independent tasks, each reading the stream for itself — the
    // consumer group is what keeps them from delivering the same entry twice.
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

    // Identifies one lane of one process to the consumer group, so `XAUTOCLAIM` can tell a dead
    // consumer's pending entries from a live one's. Random, not hostname+index: two pods
    // restarted into the same name would otherwise share a consumer and steal each other's.
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
        let (client, upstream, secret, store) =
            (client.clone(), upstream.clone(), secret.clone(), Arc::clone(&store));
        let me = format!("{run:016x}/{i}");
        tasks.push(tokio::spawn(async move {
            lane(&store, &client, &upstream, &secret, &me).await;
        }));
    }
    // Every lane loops forever, so the FIRST one to finish — panic or return — is a dead lane.
    // Awaiting the handles in order would only notice lane N after lanes 0..N had finished,
    // which is never; this resolves on any of them, and the `Err` exits the process so the pod
    // restarts at full capacity instead of quietly running short.
    Err(rustic_git::err(first_exit(tasks).await))
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
async fn lane(
    store: &Arc<rustic_git::store::Store>,
    client: &reqwest::Client,
    upstream: &str,
    secret: &str,
    me: &str,
) {
    let mut last_claim = std::time::Instant::now();
    loop {
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
            // thing pacing the lane, on a live Redis just as much as a dead one. It also sets the
            // worst-case delay between an event landing and a lane noticing it.
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
/// `App::run_merge`'s publish), never just "any event whose number happens to be 0": a stray or
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

/// Every owner with anything under any image prefix. `blobs/` alone misses an owner whose layers
/// were all deleted but whose manifests remain, and one whose image database exists with nothing
/// pushed yet — both still need their listing markers reconciled. A prefix that fails to list is
/// logged and skipped: the others still get their turn.
async fn image_owners(store: &rustic_git::store::Store) -> std::collections::BTreeSet<String> {
    let mut owners = std::collections::BTreeSet::new();
    for prefix in ["blobs/", "manifests/", "repo/img/"] {
        match rustic_git::registry::list_dir_names(&store.os, prefix).await {
            Ok(o) => owners.extend(o),
            Err(e) => eprintln!("gc: listing {prefix}: {e}"), // ponytail: eprintln
        }
    }
    owners
}

/// Sweep one owner at a time, forever. Reads every manifest before it deletes a single blob —
/// see `registry::gc` for why that order is load-bearing — so a wrong answer here destroys a
/// layer a live image still needs, which is why it runs on its own schedule instead of hurrying.
async fn gc_lane(store: &rustic_git::store::Store, grace: std::time::Duration) {
    let upload_grace = rustic_git::registry::uploads::upload_grace();
    loop {
        let owners = image_owners(store).await;
        // Uploads are swept for their own owner set: a push can leave a staging object behind
        // before it ever lands a blob, so an owner with only abandoned sessions and no blobs yet
        // must still be visited, not just the owners `image_owners` finds.
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
        let store = rustic_git::store::Store::open(Arc::new(InMemory::new()), tmp.path().join("cache"), false)
            .await
            .unwrap();
        for p in ["blobs/alpha/sha256/aa", "manifests/beta/nginx/sha256/bb", "repo/img/gamma/nginx/manifest/0.sst"] {
            store.os.put(&OsPath::from(p), PutPayload::from("x")).await.unwrap();
        }
        let owners: Vec<String> = image_owners(&store).await.into_iter().collect();
        assert_eq!(owners, vec!["alpha", "beta", "gamma"]);
    }
}
