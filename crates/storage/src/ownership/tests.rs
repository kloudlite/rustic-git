use super::*;

fn entry(node: &str, expires_ms: u64) -> Entry {
    Entry { node: node.to_string(), expires_ms }
}

#[test]
fn claim_on_absent_entry_grants() {
    match decide_claim(None, "kloudlite-1", 1_000) {
        Grant::Granted(e) => {
            assert_eq!(e.node, "kloudlite-1");
            assert_eq!(e.expires_ms, 1_000 + LEASE_TTL.as_millis() as u64);
        }
        Grant::HeldBy(_) => panic!("absent entry must grant"),
    }
}

#[test]
fn claim_on_live_entry_held_by_someone_else_returns_held_by() {
    let cur = entry("kloudlite-1", 5_000);
    match decide_claim(Some(&cur), "kloudlite-2", 1_000) {
        Grant::HeldBy(e) => assert_eq!(e, cur),
        Grant::Granted(_) => panic!("live entry held by another node must not grant"),
    }
}

#[test]
fn claim_on_expired_entry_grants() {
    let cur = entry("kloudlite-1", 1_000);
    match decide_claim(Some(&cur), "kloudlite-2", 2_000) {
        Grant::Granted(e) => assert_eq!(e.node, "kloudlite-2"),
        Grant::HeldBy(_) => panic!("expired entry must grant"),
    }
}

#[test]
fn reclaim_by_current_holder_grants_and_extends() {
    let cur = entry("kloudlite-1", 5_000);
    match decide_claim(Some(&cur), "kloudlite-1", 4_000) {
        Grant::Granted(e) => {
            assert_eq!(e.node, "kloudlite-1");
            assert_eq!(e.expires_ms, 4_000 + LEASE_TTL.as_millis() as u64);
        }
        Grant::HeldBy(_) => panic!("re-claim by the current holder must be idempotent"),
    }
}

#[test]
fn renew_by_holder_extends() {
    let cur = entry("kloudlite-1", 5_000);
    let renewed = decide_renew(Some(&cur), "kloudlite-1", 4_000).unwrap();
    assert_eq!(renewed.node, "kloudlite-1");
    assert_eq!(renewed.expires_ms, 4_000 + LEASE_TTL.as_millis() as u64);
}

#[test]
fn renew_by_non_holder_returns_none() {
    let cur = entry("kloudlite-1", 5_000);
    assert!(decide_renew(Some(&cur), "kloudlite-2", 4_000).is_none());
}

/// A lapsed clock must not take a repo from the node still holding it. The leader is the only node
/// that can renew, so its own downtime is precisely when leases lapse innocently — declining here
/// closes a database that is serving fine.
#[test]
fn renew_of_a_lapsed_entry_by_the_holder_extends_it() {
    let cur = entry("kloudlite-1", 1_000);
    let renewed = decide_renew(Some(&cur), "kloudlite-1", 2_000).unwrap();
    assert_eq!(renewed.node, "kloudlite-1");
    assert_eq!(renewed.expires_ms, 2_000 + LEASE_TTL.as_millis() as u64);
}

/// The prune loop may have reaped the entry while the leader was away; the holder still holds the
/// database, so the lease follows the handle back.
#[test]
fn renew_of_a_pruned_entry_regrants_it_to_the_holder() {
    let renewed = decide_renew(None, "kloudlite-1", 2_000).unwrap();
    assert_eq!(renewed.node, "kloudlite-1");
}

/// Safety is unchanged: once the map names somebody else, the asker has genuinely lost it and must
/// close — expired or not.
#[test]
fn renew_is_declined_once_the_map_names_another_node() {
    let cur = entry("kloudlite-2", 1_000);
    assert!(decide_renew(Some(&cur), "kloudlite-1", 2_000).is_none());
    let live = entry("kloudlite-2", 9_000);
    assert!(decide_renew(Some(&live), "kloudlite-1", 2_000).is_none());
}

/// Release is a plain delete, and it runs only after the database is closed — so the guard that
/// matters is not timing but identity: a node may only drop an entry that still names it. A stale
/// release from a node that already lost the repo must not delete the new owner's entry.
#[test]
fn only_the_holder_may_release() {
    let cur = entry("kloudlite-1", 50_000);
    assert!(may_release(Some(&cur), "kloudlite-1"));
    assert!(!may_release(Some(&cur), "kloudlite-2"), "a stale release must not delete the owner");
    assert!(!may_release(None, "kloudlite-1"));
}

/// Once released, the repo is claimable at once by anyone — there is no tombstone and no drain
/// left to wait out, because the releasing node closed its database before releasing.
#[test]
fn a_released_repo_is_claimable_immediately() {
    match decide_claim(None, "kloudlite-2", 1_000) {
        Grant::Granted(e) => assert_eq!(e.node, "kloudlite-2"),
        g => panic!("a released repo must be claimable at once: {g:?}"),
    }
}

// ---- forced claims: the asker could not reach the holder ----

/// Catches: a forced claim refusing an unheld repo, which would make recovery useless in the very
/// case it exists for (the entry was pruned while the owner was gone).
#[test]
fn force_claim_on_absent_entry_grants() {
    match decide_force_claim(None, "kloudlite-2", 10_000) {
        Grant::Granted(e) => assert_eq!(e.node, "kloudlite-2"),
        g => panic!("absent entry must grant: {g:?}"),
    }
}

/// The whole point: an entry that is still LIVE on the clock but whose holder cannot be reached is
/// taken over now, not in ten seconds. Catches a forced claim that still honours the lease.
#[test]
fn force_claim_on_a_live_but_unreachable_holder_grants() {
    // Written at 1_000 (expiry 11_000), so it is live at 5_000 and well past FORCE_MIN_AGE.
    let cur = entry("kloudlite-1", 1_000 + LEASE_TTL.as_millis() as u64);
    match decide_force_claim(Some(&cur), "kloudlite-2", 5_000) {
        Grant::Granted(e) => assert_eq!(e.node, "kloudlite-2"),
        g => panic!("a live entry whose holder is unreachable must be forced over: {g:?}"),
    }
}

/// An expired entry is granted with or without force. Catches a forced path that got stricter than
/// the ordinary one.
#[test]
fn force_claim_on_a_stale_entry_grants() {
    let cur = entry("kloudlite-1", 1_000);
    match decide_force_claim(Some(&cur), "kloudlite-2", 20_000) {
        Grant::Granted(e) => assert_eq!(e.node, "kloudlite-2"),
        g => panic!("expired entry must grant: {g:?}"),
    }
}

/// Catches an anti-flap rule that fires on the asker's own entry — a node re-forcing what it
/// already holds must stay idempotent, not be told it lost its own repo.
#[test]
fn force_claim_by_the_current_holder_grants() {
    let cur = entry("kloudlite-1", 10_500);
    match decide_force_claim(Some(&cur), "kloudlite-1", 10_000) {
        Grant::Granted(e) => {
            assert_eq!(e.node, "kloudlite-1");
            assert_eq!(e.expires_ms, 10_000 + LEASE_TTL.as_millis() as u64);
        }
        g => panic!("re-claim by the holder must be idempotent: {g:?}"),
    }
}

/// Anti-flap. Catches the ping-pong: two nodes recovering from the same dead owner arrive a few
/// hundred milliseconds apart, and without this the second takes the repo straight off the first.
#[test]
fn force_claim_refuses_an_entry_written_moments_ago() {
    // Written at 10_000 by node 3; node 2 asks 500ms later.
    let cur = entry("kloudlite-3", 10_000 + LEASE_TTL.as_millis() as u64);
    match decide_force_claim(Some(&cur), "kloudlite-2", 10_500) {
        Grant::HeldBy(e) => assert_eq!(e, cur, "must name the winner so the caller forwards there"),
        g => panic!("a just-granted entry must not be forced over: {g:?}"),
    }
    // And exactly at the threshold it is fair game again.
    let now = 10_000 + FORCE_MIN_AGE.as_millis() as u64;
    match decide_force_claim(Some(&cur), "kloudlite-2", now) {
        Grant::Granted(e) => assert_eq!(e.node, "kloudlite-2"),
        g => panic!("past FORCE_MIN_AGE a forced claim must grant: {g:?}"),
    }
}

/// WAL objects must actually get collected under the leader's own settings.
///
/// Asserting the constants would prove nothing: the collector was already enabled, already
/// running on its 300s tick, and structurally unable to delete a single object — WAL GC only
/// considers entries before `replay_after_wal_id`, and that pointer only moves when the memtable
/// flushes to L0. A map of a few dozen tiny keys never trips the 1 GiB size trigger, and a leader
/// restarting more often than 4096 writes never trips the count trigger either. So this drives
/// the real loop: write past the flush threshold, then let the collector run.
#[tokio::test]
async fn the_leader_actually_reclaims_its_wal() {
    use slatedb::object_store::{memory::InMemory, path::Path as OsPath, ObjectStore};
    use std::sync::Arc;

    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    // The real settings, with the two knobs wound down so the test does not wait 5 minutes.
    let db = slatedb::Db::builder(PATH, os.clone())
        .with_settings(leader_settings(
            std::time::Duration::from_millis(50),
            std::time::Duration::ZERO,
        ))
        .build()
        .await
        .unwrap();

    // Written with a pause between each, so the 10ms flush interval seals a SEPARATE WAL object
    // per write rather than batching them into two — the backlog this guards against is built out
    // of one lease write every few seconds, and a test that lets them coalesce proves nothing.
    let row = "n".repeat(100);
    for i in 0..40u32 {
        db.put(format!("node/{i}").as_bytes(), row.as_bytes()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let count = |os: Arc<dyn ObjectStore>| async move {
        use futures::StreamExt;
        os.list(Some(&OsPath::from(format!("{PATH}/wal"))))
            .filter_map(|r| async move { r.ok() })
            .count()
            .await
    };
    let before = count(os.clone()).await;
    assert!(before > 10, "the test needs a real backlog to collect, got {before}");

    // The fix: without this the pointer never moves, and nothing below is collectable.
    db.flush_with_options(slatedb::config::FlushOptions {
        flush_type: slatedb::config::FlushType::MemTable,
    })
    .await
    .unwrap();

    // Give the collector a few of its 50ms ticks.
    let mut after = before;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        after = count(os.clone()).await;
        if after < before {
            break;
        }
    }

    assert!(
        after < before,
        "the leader must reclaim WAL objects: {before} before, {after} after — if this fails the \
         flush pointer is stuck again and the WAL will grow without bound"
    );
}

/// Checkpointing with NOTHING written must return, not block.
///
/// The leader holds no repos, so on a quiet fleet the map often has nothing to flush when the
/// timer fires, and the checkpoint runs on the task that renews leases — if it ever blocked, the
/// leader would stop renewing. (A production hang at exactly this point was once blamed on the
/// empty flush itself; the cause was L0 being full with no compactor, now fixed in settings. The
/// property is still worth holding.)
#[tokio::test]
async fn checkpointing_an_untouched_map_returns() {
    use slatedb::object_store::{memory::InMemory, ObjectStore};
    use std::sync::Arc;

    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let store = OwnershipStore::open(os);
    store.promote().await.unwrap();

    // No writes at all — exactly the quiet-fleet case.
    let r = tokio::time::timeout(std::time::Duration::from_secs(10), store.checkpoint()).await;
    assert!(r.is_ok(), "a checkpoint with nothing to flush must not block the lease loop");
    r.unwrap().unwrap();
}

/// And a checkpoint WITH something to flush must also return.
///
/// The pair matters: this is the path that actually moves the flush pointer and makes the WAL
/// collectable, and it is bounded for the same reason — it runs on the task that renews leases.
#[tokio::test]
async fn checkpointing_after_a_write_returns() {
    use slatedb::object_store::{memory::InMemory, ObjectStore};
    use std::sync::Arc;

    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let store = OwnershipStore::open(os);
    store.promote().await.unwrap();
    store.put("alice/web", &entry("kloudlite-1", 1)).await.unwrap();

    let r = tokio::time::timeout(std::time::Duration::from_secs(10), store.checkpoint()).await;
    assert!(r.is_ok(), "a checkpoint with work to do must not block the lease loop either");
    r.unwrap().unwrap();

    // Immediately again: nothing new was written, so this one takes the skip path.
    let r2 = tokio::time::timeout(std::time::Duration::from_secs(10), store.checkpoint()).await;
    assert!(r2.is_ok());
    r2.unwrap().unwrap();
}

/// Every object the map leaves behind, by directory. Counting the whole prefix is the point: a
/// collector that empties one directory while another grows forever has not bounded anything.
async fn objects_by_dir(os: &std::sync::Arc<dyn slatedb::object_store::ObjectStore>) -> std::collections::BTreeMap<String, usize> {
    use futures::StreamExt;
    let prefix = slatedb::object_store::path::Path::from(PATH);
    os.list(Some(&prefix))
        .filter_map(|r| async move { r.ok() })
        .fold(std::collections::BTreeMap::new(), |mut m, meta| async move {
            let rel = meta.location.as_ref().trim_start_matches(PATH).trim_start_matches('/');
            let dir = rel.split('/').next().unwrap_or("").to_string();
            *m.entry(dir).or_insert(0) += 1;
            m
        })
        .await
}

/// The map's object count must be BOUNDED, not merely slow-growing: compaction orphans its input
/// SSTs, and without collection they accumulate forever — the WAL's failure again, on a longer
/// fuse. This drives the real leader settings, with the real 300s `min_age`, under a clock the
/// test controls.
///
/// The clock matters because the compactor pins its inputs: before each manifest write it takes a
/// checkpoint with a 15-minute lifetime (so a scan still reading the old SSTs can finish), and the
/// collector treats everything a live checkpoint references as active. So nothing may be deleted
/// inside that window — asserted, since deleting early would be the follower-read breakage this
/// whole configuration exists to avoid — and everything orphaned must be deleted after it.
#[tokio::test]
async fn the_leader_actually_reclaims_its_compacted_objects() {
    use slatedb::object_store::{memory::InMemory, ObjectStore};
    use slatedb_common::clock::{MockSystemClock, SystemClock};
    use std::sync::Arc;
    use std::time::Duration;

    // Starts at the real time: SST ids carry wall-clock timestamps, and the collector compares
    // them against this clock. A clock at zero would make every object look newer than "now".
    let clock = Arc::new(MockSystemClock::with_time(chrono::Utc::now().timestamp_millis()));
    // Every background loop — WAL flusher, compactor, collector — sleeps on this clock, and a put
    // waits for the flusher. So the clock must run on its own, ahead of the test, or the first
    // write deadlocks. A mock second per real millisecond: twenty mock minutes in about a real
    // second, coarse enough that every sleeper still wakes each step.
    let driver = {
        let clock = clock.clone();
        tokio::spawn(async move {
            loop {
                clock.advance(Duration::from_secs(1)).await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };
    let until = |clock: Arc<MockSystemClock>, t: chrono::DateTime<chrono::Utc>| async move {
        while clock.now() < t {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };

    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let db = slatedb::Db::builder(PATH, os.clone())
        .with_settings(leader_settings(Duration::from_secs(5), Duration::from_secs(300)))
        .with_system_clock(clock.clone())
        .build()
        .await
        .unwrap();

    let row = "n".repeat(100);
    for i in 0..90u32 {
        db.put(format!("node/{i}").as_bytes(), row.as_bytes()).await.unwrap();
        db.flush_with_options(slatedb::config::FlushOptions {
            flush_type: slatedb::config::FlushType::MemTable,
        })
        .await
        .unwrap();
    }
    let t0 = clock.now();
    let peak = objects_by_dir(&os).await;
    let total = |m: &std::collections::BTreeMap<String, usize>| m.values().sum::<usize>();
    eprintln!("after writes: {peak:?}");
    assert!(peak.get("compacted").copied().unwrap_or(0) > 8, "the compactor never ran: {peak:?}");

    // Ten minutes on: still inside every compactor checkpoint's lifetime, so the orphans must all
    // still be there.
    until(clock.clone(), t0 + chrono::Duration::minutes(10)).await;
    let inside = objects_by_dir(&os).await;
    eprintln!("at +10min: {inside:?}");
    assert!(
        inside.get("compacted") >= peak.get("compacted"),
        "deleted inside the checkpoint window — a follower mid-scan would have broken: {peak:?} -> {inside:?}"
    );

    // Past 15 minutes the checkpoints expire, the collector prunes them, and the orphans they
    // pinned become collectable. Twenty is ample for the 5s interval.
    until(clock.clone(), t0 + chrono::Duration::minutes(20)).await;
    let after = objects_by_dir(&os).await;
    eprintln!("at +20min: {after:?}");
    driver.abort();
    assert!(
        after.get("compacted").copied().unwrap_or(0) < peak.get("compacted").copied().unwrap_or(0),
        "compacted orphans never collected: {peak:?} -> {after:?}"
    );
    assert!(
        total(&after) < total(&peak),
        "nothing reclaimed once the checkpoints expired: {peak:?} -> {after:?}"
    );
}

/// One renewal beat is ONE durable write, however many repos it renews: a put per repo was a WAL
/// flush per repo, serialised under the leader lock, and 64 warm repos of it outran the beat.
#[tokio::test]
async fn a_renew_beat_is_one_durable_write() {
    use std::sync::atomic::Ordering::SeqCst;
    let counting = std::sync::Arc::new(crate::index::tests::Counting::default());
    let os: std::sync::Arc<dyn slatedb::object_store::ObjectStore> = counting.clone();
    let s = OwnershipStore::open(os);
    s.promote().await.unwrap();
    let entries: Vec<(String, Entry)> = (0..16).map(|i| (format!("a/r{i}"), entry("n", 1))).collect();

    let before = counting.puts.load(SeqCst);
    s.put_many(&entries).await.unwrap();
    let batched = counting.puts.load(SeqCst) - before;

    let before = counting.puts.load(SeqCst);
    for (repo, e) in &entries {
        s.put(repo, e).await.unwrap();
    }
    let singly = counting.puts.load(SeqCst) - before;

    assert!(batched <= 2, "a batch is one WAL flush, saw {batched}");
    assert!(singly >= entries.len(), "the per-repo path this replaces flushed per put: {singly}");
    for (repo, e) in &entries {
        assert_eq!(s.get(repo).await.unwrap().as_ref(), Some(e));
    }
}

/// The role changes under a running node: follower → writer → follower, and the map is readable
/// through every state. `promote`/`demote` are idempotent because the election loop calls them
/// on every tick it believes something changed, not only on the tick it actually did.
#[tokio::test]
async fn promote_then_demote_reopens_as_a_reader() {
    use slatedb::object_store::{memory::InMemory, ObjectStore};
    use std::sync::Arc;
    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let s = OwnershipStore::open(os);
    assert!(!s.is_writer().await);
    assert!(s.put("alice/web", &entry("n", 1)).await.is_err(), "a follower never writes");

    s.promote().await.unwrap();
    s.promote().await.unwrap();
    assert!(s.is_writer().await);
    s.put("alice/web", &entry("n", 1)).await.unwrap();

    s.demote().await;
    s.demote().await;
    assert!(!s.is_writer().await);
    assert!(s.put("alice/web", &entry("n", 2)).await.is_err());
    let mut seen = None;
    for _ in 0..40 {
        seen = s.get("alice/web").await.unwrap();
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(seen, Some(entry("n", 1)), "the reopened reader must catch up to what the writer left");
}

/// The storage-level backstop the election leans on: a second writer on the same map fences the
/// first, and the first's next write says so (`pool::is_fenced`) rather than landing. Without
/// this property a stale leader that has not noticed losing the lease could keep granting.
#[tokio::test]
async fn a_second_writer_fences_the_first() {
    use slatedb::object_store::{memory::InMemory, ObjectStore};
    use std::sync::Arc;
    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let a = OwnershipStore::open(os.clone());
    a.promote().await.unwrap();
    a.put("alice/web", &entry("n", 1)).await.unwrap();
    let b = OwnershipStore::open(os);
    b.promote().await.unwrap();
    let e = a.put("alice/web", &entry("n", 2)).await.expect_err("the fenced writer must not succeed");
    assert!(crate::pool::is_fenced(&e), "not reported as a fence: {e}");
    b.put("alice/web", &entry("n", 3)).await.unwrap();
}
