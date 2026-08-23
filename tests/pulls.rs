mod common;

use rustic_git::directory::{MergeState, MergeableState};
use rustic_git::pulls::{self, Comment, MergeJob, Mergeability, PullRequest, PullState};

fn pr(number: i64, state: PullState) -> PullRequest {
    PullRequest {
        id: format!("a/r#{number}"),
        repo: "a/r".into(),
        number,
        title: format!("change {number}"),
        body: "why".into(),
        base: "main".into(),
        head: "topic".into(),
        state,
        author: "alice".into(),
        created_at_ms: 1_700_000_000_000 + number,
        merged_at_ms: None,
        comments: Vec::new(),
        merge: None,
        mergeability: None,
        check_at_ms: None,
    }
}

#[tokio::test]
async fn put_get_round_trips_every_field() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();

    let bare = pr(1, PullState::Open);
    pulls::put(&db, &bare).await.unwrap();
    assert_eq!(pulls::get(&db, 1).await.unwrap(), Some(bare));

    let full = PullRequest {
        merged_at_ms: Some(1_700_000_009_000),
        check_at_ms: Some(1_700_000_008_000),
        comments: vec![Comment { author: "bob".into(), body: "ship it".into(), at_ms: 1_700_000_007_000 }],
        merge: Some(MergeJob {
            state: MergeState::Queued,
            strategy: "squash".into(),
            requested_by: "bob".into(),
            requested_at_ms: 1_700_000_006_000,
            claimed_at_ms: Some(1_700_000_005_000),
            claimed_by: Some("worker-1".into()),
            detail: Some("waiting".into()),
        }),
        mergeability: Some(Mergeability {
            state: MergeableState::Clean,
            fast_forward: true,
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
            checked_at_ms: 1_700_000_004_000,
            detail: None,
        }),
        ..pr(2, PullState::Merged)
    };
    pulls::put(&db, &full).await.unwrap();
    assert_eq!(pulls::get(&db, 2).await.unwrap(), Some(full));

    assert_eq!(pulls::get(&db, 3).await.unwrap(), None);
}

/// The whole point of zero-padding the key: lexical order over `pull/` must BE numeric order,
/// which a bare decimal breaks at every digit boundary (`10` sorts before `9`).
#[tokio::test]
async fn list_is_numeric_across_digit_boundaries() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    for n in [100, 9, 1, 99, 10, 2] {
        pulls::put(&db, &pr(n, PullState::Open)).await.unwrap();
    }
    let got: Vec<i64> = pulls::list(&db).await.unwrap().into_iter().map(|p| p.number).collect();
    assert_eq!(got, vec![1, 2, 9, 10, 99, 100]);
}

#[tokio::test]
async fn open_only_filters_and_limits() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &pr(1, PullState::Open)).await.unwrap();
    pulls::put(&db, &pr(2, PullState::Merged)).await.unwrap();
    pulls::put(&db, &pr(3, PullState::Closed)).await.unwrap();
    pulls::put(&db, &pr(4, PullState::Open)).await.unwrap();
    pulls::put(&db, &pr(5, PullState::Open)).await.unwrap();

    let all: Vec<i64> =
        pulls::open_only(&db, 10).await.unwrap().into_iter().map(|p| p.number).collect();
    assert_eq!(all, vec![1, 4, 5]);
    let capped: Vec<i64> =
        pulls::open_only(&db, 2).await.unwrap().into_iter().map(|p| p.number).collect();
    assert_eq!(capped, vec![1, 4]);
}

/// `next_number` is a read-increment-write; without the keyed lock concurrent callers read the
/// same value and two changes get the same number — and the number IS the key, so one overwrites
/// the other. Mirrors `concurrent_pulls_count_every_hit`.
#[tokio::test]
async fn concurrent_next_number_hands_out_distinct_numbers() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let n = 50usize;
    let mut tasks = Vec::new();
    for _ in 0..n {
        let store = e.store.clone();
        tasks.push(tokio::spawn(async move { pulls::next_number(&store, "a", "r").await }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap().unwrap());
    }
    got.sort();
    assert_eq!(got, (1..=n as i64).collect::<Vec<_>>(), "sequence starts at 1 and never repeats");
}

/// Guards ruling 4: no bson type may sneak back into the stored shape — a bson `DateTime`
/// round-trips through serde_json only by accident, and does not round-trip back.
#[test]
fn serde_json_round_trip_is_unchanged() {
    let p = PullRequest { merged_at_ms: Some(42), ..pr(7, PullState::Merged) };
    let bytes = serde_json::to_vec(&p).unwrap();
    assert_eq!(serde_json::from_slice::<PullRequest>(&bytes).unwrap(), p);
    // The wire names the web app reads must not shift with the rust field names.
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["createdAt"], serde_json::json!(1_700_000_000_007i64));
    assert_eq!(v["mergedAt"], serde_json::json!(42));
    assert!(v.get("_id").is_some(), "the web app keys its list on _id");
}

/// A row written by the OLD code holds a real bson `DateTime`, not a number. Task 6's migration
/// reads exactly such rows, so if this deserialization is wrong every pre-existing PR fails to
/// migrate — and no test involving a live Mongo would catch it before production does.
#[test]
fn a_bson_date_row_still_deserializes() {
    use mongodb::bson::{doc, DateTime};
    let d = doc! {
        "_id": "alice/web#1", "repo": "alice/web", "number": 1i64,
        "title": "t", "body": "", "base": "main", "head": "f",
        "state": "open", "author": "a@b.c",
        "createdAt": DateTime::from_millis(1_755_772_800_000),
        "comments": [],
    };
    let pr: rustic_git::pulls::PullRequest =
        mongodb::bson::from_document(d).expect("a bson DateTime row must still deserialize");
    assert_eq!(pr.created_at_ms, 1_755_772_800_000);
}

const MIGRATED: &[u8] = b"meta/pulls_migrated";
const NEXT: &[u8] = b"meta/next_pull";

async fn next_pull(db: &slatedb::Db) -> Option<i64> {
    db.get(NEXT).await.unwrap().map(|v| String::from_utf8_lossy(&v).parse().unwrap())
}

async fn migrated(db: &slatedb::Db) -> bool {
    db.get(MIGRATED).await.unwrap().as_deref() == Some(b"1".as_ref())
}

#[tokio::test]
async fn migration_copies_every_row_and_sets_the_next_number() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let rows = vec![pr(1, PullState::Merged), pr(2, PullState::Open), pr(3, PullState::Closed)];

    pulls::migrate_from(&e.store, "a", "r", || async { Ok(rows.clone()) }).await.unwrap();

    let db = e.store.db_for("a", "r").await.unwrap();
    for n in 1..=3 {
        assert_eq!(pulls::get(&db, n).await.unwrap().unwrap().number, n);
    }
    assert_eq!(next_pull(&db).await, Some(4));
    assert!(migrated(&db).await);
}

#[tokio::test]
async fn migrating_twice_changes_nothing() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let rows = vec![pr(1, PullState::Open), pr(2, PullState::Open)];
    pulls::migrate_from(&e.store, "a", "r", || async { Ok(rows.clone()) }).await.unwrap();

    // The second run must not even ASK for the rows: the fast path is one get.
    pulls::migrate_from(&e.store, "a", "r", || async { panic!("re-read the source") })
        .await
        .unwrap();

    let db = e.store.db_for("a", "r").await.unwrap();
    assert_eq!(pulls::list(&db).await.unwrap().len(), 2, "no duplicates");
    assert_eq!(next_pull(&db).await, Some(3));
}

/// The marker is written LAST, so a crash mid-copy leaves rows and `next_pull` behind without it.
/// Re-running must converge on exactly the same state rather than skipping or double-counting.
#[tokio::test]
async fn a_crash_before_the_marker_re_runs_cleanly() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    let rows = vec![pr(1, PullState::Open), pr(2, PullState::Open), pr(3, PullState::Open)];

    // Half a migration: two rows and a next_pull, no marker.
    pulls::put(&db, &rows[0]).await.unwrap();
    pulls::put(&db, &rows[1]).await.unwrap();
    db.put(NEXT, b"3").await.unwrap();
    assert!(!migrated(&db).await);

    pulls::migrate_from(&e.store, "a", "r", || async { Ok(rows.clone()) }).await.unwrap();

    assert_eq!(pulls::list(&db).await.unwrap().len(), 3);
    assert_eq!(next_pull(&db).await, Some(4));
    assert!(migrated(&db).await);
}

#[tokio::test]
async fn a_repo_with_no_rows_is_migrated_at_one() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    pulls::migrate_from(&e.store, "a", "r", || async { Ok(Vec::new()) }).await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    assert_eq!(next_pull(&db).await, Some(1));
    assert!(migrated(&db).await);
}

/// No Mongo configured is a fresh single-node deployment, not a failure.
#[tokio::test]
async fn no_directory_is_nothing_to_migrate() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    pulls::ensure_migrated(&e.store, &pulls::Source::Absent, "a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    assert_eq!(next_pull(&db).await, Some(1));
    assert!(migrated(&db).await);
}

/// Configured-but-unreachable is the opposite of absent: there may be changes we cannot see, so
/// recording the migration would hide them forever and restart numbering on top of them.
#[tokio::test]
async fn an_unreachable_directory_is_never_recorded_as_migrated() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let bad = pulls::ensure_migrated(&e.store, &pulls::Source::Unavailable, "a", "r").await;
    assert!(bad.is_err(), "an unreadable directory must fail, not migrate an empty repo");

    let db = e.store.db_for("a", "r").await.unwrap();
    assert!(!migrated(&db).await);
    assert_eq!(next_pull(&db).await, None);
}

/// ...and the failure is not remembered either: the next touch, once Mongo answers, migrates.
#[tokio::test]
async fn an_unreachable_directory_retries_on_the_next_touch() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    assert!(pulls::ensure_migrated(&e.store, &pulls::Source::Unavailable, "a", "r").await.is_err());

    let rows = vec![pr(1, PullState::Open), pr(7, PullState::Merged)];
    pulls::migrate_from(&e.store, "a", "r", || async { Ok(rows.clone()) }).await.unwrap();

    let db = e.store.db_for("a", "r").await.unwrap();
    assert_eq!(pulls::list(&db).await.unwrap().len(), 2);
    assert_eq!(next_pull(&db).await, Some(8));
    assert!(migrated(&db).await);
}

/// The most dangerous line in the migration: marking migrated after a failed read would erase
/// every existing PR for the repo from the only place anyone will look afterwards.
#[tokio::test]
async fn a_failed_read_leaves_the_repo_unmigrated() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let bad =
        pulls::migrate_from(&e.store, "a", "r", || async { Err(rustic_git::err("mongo down")) })
            .await;
    assert!(bad.is_err());

    let db = e.store.db_for("a", "r").await.unwrap();
    assert!(!migrated(&db).await, "a failed read must be retried, not remembered as done");
    assert_eq!(next_pull(&db).await, None);
}

// ---------------------------------------------------------------------------
// Mergeability discovery on the owning node (Task 8).
//
// The whole point of these is that they run with NOTHING central up: `common::env` has a
// disabled cache (no Redis) and no `Directory` at all (no Mongo). If any of them needs one to
// pass, the safety floor did not actually move.
// ---------------------------------------------------------------------------

/// A repo with `master` two commits deep and `base` parked on the first — so `base..master` is a
/// genuine fast-forward and `compare` has real objects to walk. Returns nothing; the caller reads
/// the refs it needs back out of the store.
async fn repo_with_a_ff(e: &common::TestEnv, owner: &str, name: &str) {
    let first = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let f = first.clone();
    let repo = common::push_built(e, owner, name, move |c| {
        std::fs::write(c.join("a.txt"), "one\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "one"]);
        std::fs::write(c.join("a.txt"), "two\n").unwrap();
        common::git(c, &["commit", "-qam", "two"]);
        *f.lock().unwrap() = common::git(c, &["rev-parse", "HEAD~1"]).trim().to_string();
    })
    .await;
    let oid = gix_hash::ObjectId::from_hex(first.lock().unwrap().as_bytes()).unwrap();
    e.store
        .update_refs(
            &repo,
            &[rustic_git::refs::RefUpdate { name: "refs/heads/base".into(), old: None, new: Some(oid) }],
        )
        .await
        .unwrap();
}

fn open_pr(number: i64) -> PullRequest {
    PullRequest { base: "base".into(), head: "master".into(), ..pr(number, PullState::Open) }
}

/// THE FLOOR. No Redis, no Mongo, no worker: the node that owns the repo finds the pending check
/// itself and writes the answer into the repo's own database.
#[tokio::test(flavor = "multi_thread")]
async fn the_owners_sweep_checks_a_pull_with_nothing_central_up() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    assert!(!e.store.cache.connected(), "this test is only meaningful with Redis down");
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &open_pr(1)).await.unwrap();

    let app = common::app(e.store.clone()).await;
    app.check_owned_pulls().await;

    let got = pulls::get(&db, 1).await.unwrap().unwrap();
    let m = got.mergeability.expect("the sweep must have written an answer");
    assert_eq!(m.state, MergeableState::Clean);
    assert!(m.fast_forward);
    assert!(!m.base_oid.is_empty() && !m.head_oid.is_empty());
    assert!(got.check_at_ms.is_some());
}

/// A change nothing has moved under must not be recomputed, or the lane spins on it forever.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_whose_tips_have_not_moved_is_not_rechecked() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &open_pr(1)).await.unwrap();

    assert!(pulls::check(&e.store, "a", "r", 1).await.unwrap(), "the first look is real work");
    let first = pulls::get(&db, 1).await.unwrap().unwrap();
    assert!(!pulls::check(&e.store, "a", "r", 1).await.unwrap(), "nothing moved; nothing to do");
    assert_eq!(pulls::get(&db, 1).await.unwrap().unwrap(), first, "a no-op check must not write");
}

/// A closed change is never work, however loudly an event names it.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_pull_is_not_checked() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &PullRequest { state: PullState::Closed, ..open_pr(1) }).await.unwrap();

    assert!(!pulls::check(&e.store, "a", "r", 1).await.unwrap());
    assert!(pulls::get(&db, 1).await.unwrap().unwrap().mergeability.is_none());
}

/// The low-latency path: a stream event makes the worker POST this route, and the OWNER — not the
/// worker — computes the answer. The worker sends no state; it only says "go look".
#[tokio::test(flavor = "multi_thread")]
async fn the_routed_check_endpoint_computes_on_the_owner() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &open_pr(1)).await.unwrap();
    pulls::put(&db, &open_pr(2)).await.unwrap();

    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);
    let post = |path: String| {
        let router = router.clone();
        async move {
            let req = Request::builder()
                .method("POST")
                .uri(path)
                .header(rustic_git::proxy::PEER_HEADER, "test-peer-secret")
                .header(rustic_git::proxy::OWNER_HEADER, "a")
                .body(axum::body::Body::empty())
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }
    };

    assert_eq!(post("/api/a/r/pulls/1/check".into()).await, StatusCode::NO_CONTENT);
    assert_eq!(
        pulls::get(&db, 1).await.unwrap().unwrap().mergeability.unwrap().state,
        MergeableState::Clean
    );
    assert!(pulls::get(&db, 2).await.unwrap().unwrap().mergeability.is_none());

    // Number 0 is the repo-wide form, matching the `HeadMoved` event that has no single PR.
    assert_eq!(post("/api/a/r/pulls/0/check".into()).await, StatusCode::NO_CONTENT);
    assert_eq!(
        pulls::get(&db, 2).await.unwrap().unwrap().mergeability.unwrap().state,
        MergeableState::Clean
    );
}

// ---------------------------------------------------------------------------
// Task 8b: merge jobs live in the repo that owns them.
//
// Same rule as the block above: nothing central is up. `common::env` has no Redis and no
// `Directory`, so a merge that lands here landed with the repo's own database as the only
// record there is.
// ---------------------------------------------------------------------------

const LEASE: std::time::Duration = std::time::Duration::from_secs(600);

fn queued(number: i64, strategy: &str) -> PullRequest {
    PullRequest {
        merge: Some(MergeJob {
            state: MergeState::Queued,
            strategy: strategy.into(),
            requested_by: "alice".into(),
            requested_at_ms: 1_700_000_000_000,
            claimed_at_ms: None,
            claimed_by: None,
            detail: None,
        }),
        ..open_pr(number)
    }
}

/// What Mongo's `find_one_and_update` used to buy. Repo-local it is bought by the repo's own
/// pull lock instead — and by there being exactly one node allowed to claim at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_merge_is_claimed_exactly_once() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &queued(1, "fast-forward")).await.unwrap();

    let mut set = tokio::task::JoinSet::new();
    for i in 0..8 {
        let store = e.store.clone();
        set.spawn(async move { pulls::claim_merge(&store, "a", "r", LEASE, &format!("w{i}")).await });
    }
    let mut won = 0;
    while let Some(r) = set.join_next().await {
        if r.unwrap().unwrap().is_some() {
            won += 1;
        }
    }
    assert_eq!(won, 1, "exactly one claimant may take a queued merge");

    let job = pulls::get(&db, 1).await.unwrap().unwrap().merge.unwrap();
    assert_eq!(job.state, MergeState::Running);
    assert!(job.claimed_by.is_some() && job.claimed_at_ms.is_some());
}

/// THE FLOOR for merges. No Redis, no Mongo, no worker: the owning node claims the job it was
/// asked for, performs the merge locally, and records the outcome in the repo's own database.
#[tokio::test(flavor = "multi_thread")]
async fn the_owners_lane_merges_with_nothing_central_up() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    assert!(!e.store.cache.connected(), "this test is only meaningful with Redis down");
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &queued(1, "fast-forward")).await.unwrap();

    let app = common::app(e.store.clone()).await;
    app.merge_owned_pulls().await;

    let got = pulls::get(&db, 1).await.unwrap().unwrap();
    assert_eq!(got.state, PullState::Merged);
    assert!(got.merged_at_ms.is_some());
    assert!(got.merge.is_none(), "a finished job is cleared, not left Queued");

    let repo = e.store.open_repo("a", "r").await.unwrap().unwrap();
    let base = e.store.get_ref(&repo, "refs/heads/base").await.unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap();
    assert_eq!(base, head, "the base branch must actually have moved");
}

/// The refusal path: `master` is not behind `base`, so landing it needs a real merge. The
/// outcome is recorded for the person waiting and the branch does not move.
#[tokio::test(flavor = "multi_thread")]
async fn a_conflicting_merge_records_conflicts_and_leaves_the_branch() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    // Reversed: base=master, head=base — the head is an ANCESTOR, so this is behind its base.
    pulls::put(
        &db,
        &PullRequest { base: "master".into(), head: "base".into(), ..queued(1, "fast-forward") },
    )
    .await
    .unwrap();
    let repo = e.store.open_repo("a", "r").await.unwrap().unwrap();
    let before = e.store.get_ref(&repo, "refs/heads/master").await.unwrap();

    common::app(e.store.clone()).await.merge_owned_pulls().await;

    let got = pulls::get(&db, 1).await.unwrap().unwrap();
    assert_eq!(got.state, PullState::Open, "a refused merge leaves the change open");
    let job = got.merge.expect("the job stays, with the reason on it");
    assert_eq!(job.state, MergeState::Conflicts);
    assert!(job.detail.unwrap().contains("already contained in its base"));
    assert_eq!(e.store.get_ref(&repo, "refs/heads/master").await.unwrap(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_merge_drops_the_job_and_leaves_the_pull_open() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &queued(1, "squash")).await.unwrap();

    pulls::clear_merge(&e.store, "a", "r", 1).await.unwrap();

    let got = pulls::get(&db, 1).await.unwrap().unwrap();
    assert!(got.merge.is_none());
    assert_eq!(got.state, PullState::Open);
}

/// The request handler runs on the owner, so a merge request lands without waiting for the
/// 15s `merge_owned_pulls` beat. Polls for up to 5s; the beat never runs in this test.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_request_lands_without_waiting_for_the_beat() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &pulls::PullRequest { merge: None, ..queued(1, "fast-forward") }).await.unwrap();

    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/a/r/pulls/1/merge?strategy=fast-forward&by=t@t")
        .header(rustic_git::proxy::PEER_HEADER, "test-peer-secret")
        .header(rustic_git::proxy::OWNER_HEADER, "a")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

    let started = std::time::Instant::now();
    loop {
        let got = pulls::get(&db, 1).await.unwrap().unwrap();
        if got.state == PullState::Merged { break; }
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "merge did not land: {:?}", got.merge);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

// ---------------------------------------------------------------------------
// Three-way merges. A diverged head used to be refused outright; now `merge` and
// `squash` combine the two trees for real.
// ---------------------------------------------------------------------------

/// A repo where `base` and `master` both moved on after a shared first commit — the only shape a
/// three-way merge is for. `on_base`/`on_head` are (path, contents) written on each side.
/// Returns the URL and the serving app — ONE app, because a second one opens the same ownership
/// database and fences the first, and a test that clones back what it merged needs the server it
/// pushed to to still be answering.
async fn push_diverged(
    e: &common::TestEnv,
    owner: &str,
    name: &str,
    on_base: &[(&str, &str)],
    on_head: &[(&str, &str)],
) -> (String, std::sync::Arc<rustic_git::App>) {
    let s = e.store.clone();
    s.create_repo(owner, name).await.unwrap();
    let token = s.create_token(owner).await.unwrap();
    let app = common::app(s.clone()).await;
    let port = common::serve(app.clone()).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/{owner}/{name}.git");

    let w = tempfile::tempdir().unwrap();
    let c = w.path().join("work");
    std::fs::create_dir(&c).unwrap();
    common::git(&c, &["init", "-q", "-b", "base"]);
    common::git(&c, &["config", "user.email", "t@t"]);
    common::git(&c, &["config", "user.name", "t"]);
    std::fs::write(c.join("shared.txt"), "one\ntwo\nthree\n").unwrap();
    common::git(&c, &["add", "."]);
    common::git(&c, &["commit", "-qm", "shared"]);

    let write_all = |files: &[(&str, &str)], msg: &str| {
        for (p, body) in files {
            let at = c.join(p);
            if let Some(d) = at.parent() {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(at, body).unwrap();
        }
        common::git(&c, &["add", "."]);
        common::git(&c, &["commit", "-qm", msg]);
    };
    common::git(&c, &["checkout", "-qb", "master"]);
    write_all(on_head, "on the head");
    common::git(&c, &["checkout", "-q", "base"]);
    write_all(on_base, "on the base");
    common::git(&c, &["push", "-q", &url, "base:refs/heads/base", "master:refs/heads/master"]);
    (url, app)
}

/// Clone `branch` fresh and hand back the work tree — the objects a merge invented are only
/// really there if a client can pull them back out.
fn cloned(url: &str, branch: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let w = tempfile::tempdir().unwrap();
    let at = w.path().join("clone");
    common::git(w.path(), &["clone", "-q", "-b", branch, url, at.to_str().unwrap()]);
    (w, at)
}

/// Number of parents of `branch`'s tip, plus the tip itself.
fn parents(at: &std::path::Path, branch: &str) -> usize {
    common::git(at, &["rev-list", "--parents", "-n", "1", branch]).split_whitespace().count() - 1
}

async fn run_diverged(strategy: &str) -> (common::TestEnv, String, pulls::PullRequest) {
    let e = common::env().await;
    let (url, app) = push_diverged(&e, "a", "r", &[("b.txt", "from base\n")], &[("a.txt", "from head\n")]).await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &queued(1, strategy)).await.unwrap();
    app.merge_owned_pulls().await;
    let got = pulls::get(&db, 1).await.unwrap().unwrap();
    (e, url, got)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diverged_merge_combines_both_trees() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let (e, url, got) = run_diverged("merge").await;
    assert_eq!(got.state, PullState::Merged, "{:?}", got.merge);

    let repo = e.store.open_repo("a", "r").await.unwrap().unwrap();
    let base = e.store.get_ref(&repo, "refs/heads/base").await.unwrap().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    assert_ne!(base, head, "a merge commit is not the head");

    let (_w, at) = cloned(&url, "base");
    assert_eq!(parents(&at, "HEAD"), 2, "a merge commit has both sides as parents");
    assert_eq!(std::fs::read_to_string(at.join("a.txt")).unwrap(), "from head\n");
    assert_eq!(std::fs::read_to_string(at.join("b.txt")).unwrap(), "from base\n");
    assert!(at.join("shared.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diverged_squash_combines_both_trees_under_one_parent() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let (_e, url, got) = run_diverged("squash").await;
    assert_eq!(got.state, PullState::Merged, "{:?}", got.merge);

    let (_w, at) = cloned(&url, "base");
    assert_eq!(parents(&at, "HEAD"), 1, "a squash keeps only the base as parent");
    assert_eq!(std::fs::read_to_string(at.join("a.txt")).unwrap(), "from head\n");
    assert_eq!(std::fs::read_to_string(at.join("b.txt")).unwrap(), "from base\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn both_sides_editing_one_line_is_a_conflict_and_the_branch_stays()  {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let (_url, app) =
        push_diverged(&e, "a", "r", &[("shared.txt", "base\ntwo\nthree\n")], &[("shared.txt", "head\ntwo\nthree\n")]).await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &queued(1, "merge")).await.unwrap();
    let repo = e.store.open_repo("a", "r").await.unwrap().unwrap();
    let before = e.store.get_ref(&repo, "refs/heads/base").await.unwrap();

    app.merge_owned_pulls().await;

    let got = pulls::get(&db, 1).await.unwrap().unwrap();
    assert_eq!(got.state, PullState::Open);
    let job = got.merge.expect("the job stays, with the paths on it");
    assert_eq!(job.state, MergeState::Conflicts);
    let detail = job.detail.unwrap();
    assert!(detail.contains("conflicts in:") && detail.contains("shared.txt"), "{detail}");
    assert_eq!(e.store.get_ref(&repo, "refs/heads/base").await.unwrap(), before, "nothing moves");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diverged_fast_forward_is_refused_with_the_way_out() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let (_e, _url, got) = run_diverged("fast-forward").await;
    assert_eq!(got.state, PullState::Open);
    let detail = got.merge.expect("the job stays").detail.unwrap();
    assert!(detail.contains("not a fast-forward"), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diverged_but_clean_pull_is_mergeable() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let (_url, app) = push_diverged(&e, "a", "r", &[("b.txt", "from base\n")], &[("a.txt", "from head\n")]).await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &open_pr(1)).await.unwrap();

    app.check_owned_pulls().await;

    let m = pulls::get(&db, 1).await.unwrap().unwrap().mergeability.unwrap();
    assert_eq!(m.state, MergeableState::Clean, "{:?}", m.detail);
    assert!(!m.fast_forward, "clean, but the base cannot simply move — the UI must not offer that");
}
