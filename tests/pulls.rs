mod common;

use kloudlite_pulls::directory::{MergeState, MergeableState};
use kloudlite_gitbase::refs::UpdateRefsExt;
use kloudlite_pulls::pulls::{self, Comment, MergeJob, Mergeability, PullRequest, PullState};

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
        comments: vec![Comment {
            author: "bob".into(),
            body: "ship it".into(),
            at_ms: 1_700_000_007_000,
        }],
        merge: Some(MergeJob {
            state: MergeState::Queued,
            strategy: "squash".into(),
            requested_by: "bob".into(),
            requested_at_ms: 1_700_000_006_000,
            claimed_at_ms: Some(1_700_000_005_000),
            claimed_by: Some("worker-1".into()),
            detail: Some("waiting".into()),
            announced_at_ms: Some(1_700_000_003_000),
        }),
        mergeability: Some(Mergeability {
            state: MergeableState::Clean,
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
            checked_at_ms: 1_700_000_004_000,
            detail: None,
            fast_forward: true,
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
    let got: Vec<i64> = pulls::list(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.number)
        .collect();
    assert_eq!(got, vec![1, 2, 9, 10, 99, 100]);
}

/// The list page: newest first, stopped at the limit, filtered on the wire state, and the
/// comments replaced by their count. Catches a descending scan that ignores the prefix or
/// the limit, and a count that never sees the bodies it is meant to count.
#[tokio::test]
async fn newest_is_descending_bounded_and_counts_comments() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    let mut two = pr(2, PullState::Merged);
    two.comments.push(pulls::Comment { author: "bob".into(), body: "lgtm".into(), at_ms: 1 });
    pulls::put(&db, &pr(1, PullState::Open)).await.unwrap();
    pulls::put(&db, &two).await.unwrap();
    pulls::put(&db, &pr(3, PullState::Closed)).await.unwrap();
    pulls::put(&db, &pr(10, PullState::Open)).await.unwrap();
    // A neighbouring prefix must not leak into a descending scan of `pull/`.
    db.put(b"pulls-counter", b"x").await.unwrap();

    let numbers = |v: &[serde_json::Value]| v.iter().map(|j| j["number"].as_i64().unwrap()).collect::<Vec<_>>();
    let all = pulls::newest(&db, None, usize::MAX).await.unwrap();
    assert_eq!(numbers(&all), vec![10, 3, 2, 1]);
    assert_eq!(numbers(&pulls::newest(&db, None, 2).await.unwrap()), vec![10, 3]);
    assert_eq!(numbers(&pulls::newest(&db, Some("open"), 10).await.unwrap()), vec![10, 1]);
    assert!(pulls::newest(&db, Some("Open"), 10).await.unwrap().is_empty(), "case-exact");

    let two = all.iter().find(|j| j["number"] == 2).unwrap();
    assert_eq!(two["commentCount"], 1);
    assert!(two.get("comments").is_none(), "bodies stay off the list");
    assert_eq!(all[0]["commentCount"], 0);
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

    let all: Vec<i64> = pulls::open_only(&db, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.number)
        .collect();
    assert_eq!(all, vec![1, 4, 5]);
    let capped: Vec<i64> = pulls::open_only(&db, 2)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.number)
        .collect();
    assert_eq!(capped, vec![1, 4]);
}

/// The prefilter must skip jobless rows without deserializing them, but still catch a jobless
/// row whose bytes happen to contain the literal `"merge":` — a comment body, say — as a false
/// positive that deserializes fine and is then dropped by the `is_some` filter.
#[tokio::test]
async fn with_merge_jobs_skips_jobless_rows_and_survives_a_decoy() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();

    pulls::put(&db, &pr(1, PullState::Open)).await.unwrap();
    pulls::put(&db, &queued(2, "fast-forward")).await.unwrap();
    let decoy = PullRequest {
        comments: vec![Comment {
            author: "eve".into(),
            body: "\"merge\": would be nice here".into(),
            at_ms: 1_700_000_000_000,
        }],
        ..pr(3, PullState::Open)
    };
    pulls::put(&db, &decoy).await.unwrap();

    let got: Vec<i64> = pulls::with_merge_jobs(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.number)
        .collect();
    assert_eq!(got, vec![2]);
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
        tasks.push(tokio::spawn(async move {
            pulls::next_number(&store, "a", "r").await
        }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap().unwrap());
    }
    got.sort();
    assert_eq!(
        got,
        (1..=n as i64).collect::<Vec<_>>(),
        "sequence starts at 1 and never repeats"
    );
}

/// Guards ruling 4: no bson type may sneak back into the stored shape — a bson `DateTime`
/// round-trips through serde_json only by accident, and does not round-trip back.
#[test]
fn serde_json_round_trip_is_unchanged() {
    let p = PullRequest {
        merged_at_ms: Some(42),
        ..pr(7, PullState::Merged)
    };
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
    let pr: kloudlite_pulls::pulls::PullRequest =
        mongodb::bson::from_document(d).expect("a bson DateTime row must still deserialize");
    assert_eq!(pr.created_at_ms, 1_755_772_800_000);
}

const MIGRATED: &[u8] = b"meta/pulls_migrated";
const NEXT: &[u8] = b"meta/next_pull";

async fn next_pull(db: &slatedb::Db) -> Option<i64> {
    db.get(NEXT)
        .await
        .unwrap()
        .map(|v| String::from_utf8_lossy(&v).parse().unwrap())
}

async fn migrated(db: &slatedb::Db) -> bool {
    db.get(MIGRATED).await.unwrap().as_deref() == Some(b"1".as_ref())
}

#[tokio::test]
async fn migration_copies_every_row_and_sets_the_next_number() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    let rows = vec![
        pr(1, PullState::Merged),
        pr(2, PullState::Open),
        pr(3, PullState::Closed),
    ];

    pulls::migrate_from(&e.store, "a", "r", || async { Ok(rows.clone()) })
        .await
        .unwrap();

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
    pulls::migrate_from(&e.store, "a", "r", || async { Ok(rows.clone()) })
        .await
        .unwrap();

    // The second run must not even ASK for the rows: the fast path is one get.
    pulls::migrate_from(&e.store, "a", "r", || async {
        panic!("re-read the source")
    })
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
    let rows = vec![
        pr(1, PullState::Open),
        pr(2, PullState::Open),
        pr(3, PullState::Open),
    ];

    // Half a migration: two rows and a next_pull, no marker.
    pulls::put(&db, &rows[0]).await.unwrap();
    pulls::put(&db, &rows[1]).await.unwrap();
    db.put(NEXT, b"3").await.unwrap();
    assert!(!migrated(&db).await);

    pulls::migrate_from(&e.store, "a", "r", || async { Ok(rows.clone()) })
        .await
        .unwrap();

    assert_eq!(pulls::list(&db).await.unwrap().len(), 3);
    assert_eq!(next_pull(&db).await, Some(4));
    assert!(migrated(&db).await);
}

#[tokio::test]
async fn a_repo_with_no_rows_is_migrated_at_one() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    pulls::migrate_from(&e.store, "a", "r", || async { Ok(Vec::new()) })
        .await
        .unwrap();
    let db = e.store.db_for("a", "r").await.unwrap();
    assert_eq!(next_pull(&db).await, Some(1));
    assert!(migrated(&db).await);
}

/// No Mongo configured is a fresh single-node deployment, not a failure.
#[tokio::test]
async fn no_directory_is_nothing_to_migrate() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    pulls::ensure_migrated(&e.store, &pulls::Source::Absent, "a", "r")
        .await
        .unwrap();
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
    assert!(
        bad.is_err(),
        "an unreadable directory must fail, not migrate an empty repo"
    );

    let db = e.store.db_for("a", "r").await.unwrap();
    assert!(!migrated(&db).await);
    assert_eq!(next_pull(&db).await, None);
}

/// ...and the failure is not remembered either: the next touch, once Mongo answers, migrates.
#[tokio::test]
async fn an_unreachable_directory_retries_on_the_next_touch() {
    let e = common::env().await;
    e.store.create_repo("a", "r").await.unwrap();
    assert!(
        pulls::ensure_migrated(&e.store, &pulls::Source::Unavailable, "a", "r")
            .await
            .is_err()
    );

    let rows = vec![pr(1, PullState::Open), pr(7, PullState::Merged)];
    pulls::migrate_from(&e.store, "a", "r", || async { Ok(rows.clone()) })
        .await
        .unwrap();

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
    let bad = pulls::migrate_from(&e.store, "a", "r", || async {
        Err(kloudlite_core::err("mongo down"))
    })
    .await;
    assert!(bad.is_err());

    let db = e.store.db_for("a", "r").await.unwrap();
    assert!(
        !migrated(&db).await,
        "a failed read must be retried, not remembered as done"
    );
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
            &[kloudlite_gitbase::refs::RefUpdate {
                name: "refs/heads/base".into(),
                old: None,
                new: Some(oid),
            }],
        )
        .await
        .unwrap();
}

fn open_pr(number: i64) -> PullRequest {
    PullRequest {
        base: "base".into(),
        head: "master".into(),
        ..pr(number, PullState::Open)
    }
}

/// THE FLOOR. No Redis, no Mongo, no worker: the node that owns the repo finds the pending check
/// itself and writes the answer into the repo's own database.
#[tokio::test(flavor = "multi_thread")]
async fn the_owners_sweep_checks_a_pull_with_nothing_central_up() {
    if !common::have_git() {
        eprintln!("skipping: no git");
        return;
    }
    let e = common::env().await;
    assert!(
        !e.store.cache.connected(),
        "this test is only meaningful with Redis down"
    );
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &open_pr(1)).await.unwrap();

    let app = common::app(e.store.clone()).await;
    kloudlite_server::lanes::check_owned_pulls(&app).await;

    let got = pulls::get(&db, 1).await.unwrap().unwrap();
    let m = got
        .mergeability
        .expect("the sweep must have written an answer");
    assert_eq!(m.state, MergeableState::Clean);
    assert!(!m.base_oid.is_empty() && !m.head_oid.is_empty());
    assert!(got.check_at_ms.is_some());
}

/// The cap on a sweep is on WORK, not on rows: with more open changes than `CHECK_LIMIT`, the
/// tail is reached on the next pass rather than never (the old row cap refilled with the same 25
/// lowest numbers every time, so #26 onward were never checked at all).
#[tokio::test(flavor = "multi_thread")]
async fn a_sweep_reaches_open_changes_past_the_cap() {
    if !common::have_git() {
        eprintln!("skipping: no git");
        return;
    }
    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    let last = pulls::CHECK_LIMIT as i64 + 5;
    for n in 1..=last {
        pulls::put(&db, &open_pr(n)).await.unwrap();
    }

    pulls::check_repo(&e.store, "a", "r").await.unwrap();
    assert!(
        pulls::get(&db, last).await.unwrap().unwrap().mergeability.is_none(),
        "the first pass stops at the cap"
    );
    pulls::check_repo(&e.store, "a", "r").await.unwrap();
    let m = pulls::get(&db, last).await.unwrap().unwrap().mergeability.expect("the next pass reaches the tail");
    assert_eq!(m.state, MergeableState::Clean);
}

/// A change nothing has moved under must not be recomputed, or the lane spins on it forever.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_whose_tips_have_not_moved_is_not_rechecked() {
    if !common::have_git() {
        eprintln!("skipping: no git");
        return;
    }
    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &open_pr(1)).await.unwrap();

    assert_eq!(
        pulls::check(&e.store, "a", "r", 1).await.unwrap(),
        pulls::Checked::Answered,
        "the first look is real work"
    );
    let first = pulls::get(&db, 1).await.unwrap().unwrap();
    assert_eq!(
        pulls::check(&e.store, "a", "r", 1).await.unwrap(),
        pulls::Checked::Unchanged,
        "nothing moved; nothing to do"
    );
    assert_eq!(
        pulls::get(&db, 1).await.unwrap().unwrap(),
        first,
        "a no-op check must not write"
    );
}

/// A closed change is never work, however loudly an event names it.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_pull_is_not_checked() {
    if !common::have_git() {
        eprintln!("skipping: no git");
        return;
    }
    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(
        &db,
        &PullRequest {
            state: PullState::Closed,
            ..open_pr(1)
        },
    )
    .await
    .unwrap();

    assert_eq!(
        pulls::check(&e.store, "a", "r", 1).await.unwrap(),
        pulls::Checked::Unchanged
    );
    assert!(pulls::get(&db, 1)
        .await
        .unwrap()
        .unwrap()
        .mergeability
        .is_none());
}

/// The low-latency path: a stream event makes the worker POST this route, and the OWNER — not the
/// worker — computes the answer. The worker sends no state; it only says "go look".
#[tokio::test(flavor = "multi_thread")]
async fn the_routed_check_endpoint_computes_on_the_owner() {
    if !common::have_git() {
        eprintln!("skipping: no git");
        return;
    }
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let e = common::env().await;
    repo_with_a_ff(&e, "a", "r").await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &open_pr(1)).await.unwrap();
    pulls::put(&db, &open_pr(2)).await.unwrap();

    let router = kloudlite_server::router::peer_router(common::app(e.store.clone()).await);
    let post = |path: String| {
        let router = router.clone();
        async move {
            let req = Request::builder()
                .method("POST")
                .uri(path)
                .header(kloudlite_core::peer::PEER_HEADER, "test-peer-secret")
                .header(kloudlite_core::peer::OWNER_HEADER, "a")
                .body(axum::body::Body::empty())
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }
    };

    assert_eq!(post("/api/a/r/pulls/1/check".into()).await, StatusCode::OK);
    assert_eq!(
        pulls::get(&db, 1)
            .await
            .unwrap()
            .unwrap()
            .mergeability
            .unwrap()
            .state,
        MergeableState::Clean
    );
    assert!(pulls::get(&db, 2)
        .await
        .unwrap()
        .unwrap()
        .mergeability
        .is_none());

    // Number 0 is the repo-wide form, matching the `HeadMoved` event that has no single PR.
    assert_eq!(post("/api/a/r/pulls/0/check".into()).await, StatusCode::OK);
    assert_eq!(
        pulls::get(&db, 2)
            .await
            .unwrap()
            .unwrap()
            .mergeability
            .unwrap()
            .state,
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
            announced_at_ms: None,
        }),
        ..open_pr(number)
    }
}

// ---------------------------------------------------------------------------
// Merging, end to end through the worker.
//
// The owner records and serves; `merge_worker::run` does the git work against the peer listener
// exactly as the worker process does, with the same peer secret. Each test drives the real three
// steps — claim, merge, report — so a break anywhere in that chain fails here.
//
// Every one of them skips when `git` is absent, as the rest of the suite does.
// ---------------------------------------------------------------------------

mod worker_merges {
    use super::*;
    use kloudlite_pulls::merge_worker::{self, Outcome, OutcomeState};

    /// A repo with `base` and `master` in whatever shape `build` leaves them, served on a peer
    /// listener. Returns that listener's base URL — the fleet, from the worker's point of view.
    async fn fleet(e: &common::TestEnv, build: impl FnOnce(&std::path::Path)) -> String {
        common::push_branches(e, "a", "r", |c| {
            // Named explicitly: `git init` picks `master` or `main` depending on the developer's
            // git, and `--all` pushes whatever the branch is actually called.
            common::git(c, &["checkout", "-q", "-b", "master"]);
            build(c);
        })
        .await
    }

    /// `base` is one commit behind `master`: the fast-forward case.
    async fn behind(e: &common::TestEnv) -> String {
        fleet(e, |c| {
            std::fs::write(c.join("a.txt"), "one\n").unwrap();
            common::git(c, &["add", "."]);
            common::git(c, &["commit", "-qm", "one"]);
            common::git(c, &["branch", "base"]);
            std::fs::write(c.join("a.txt"), "two\n").unwrap();
            common::git(c, &["commit", "-qam", "two"]);
        })
        .await
    }

    /// `base` and `master` each have a commit the other does not, touching DIFFERENT files.
    async fn diverged(e: &common::TestEnv) -> String {
        fleet(e, |c| {
            std::fs::write(c.join("a.txt"), "one\n").unwrap();
            common::git(c, &["add", "."]);
            common::git(c, &["commit", "-qm", "one"]);
            common::git(c, &["checkout", "-q", "-b", "base"]);
            std::fs::write(c.join("b.txt"), "from base\n").unwrap();
            common::git(c, &["add", "."]);
            common::git(c, &["commit", "-qm", "base side"]);
            common::git(c, &["checkout", "-q", "master"]);
            std::fs::write(c.join("m.txt"), "from master\n").unwrap();
            common::git(c, &["add", "."]);
            common::git(c, &["commit", "-qm", "master side"]);
        })
        .await
    }

    /// Diverged, and both sides rewrote the same line of the same file.
    async fn conflicting(e: &common::TestEnv) -> String {
        fleet(e, |c| {
            std::fs::write(c.join("a.txt"), "one\n").unwrap();
            common::git(c, &["add", "."]);
            common::git(c, &["commit", "-qm", "one"]);
            common::git(c, &["checkout", "-q", "-b", "base"]);
            std::fs::write(c.join("a.txt"), "from base\n").unwrap();
            common::git(c, &["commit", "-qam", "base side"]);
            common::git(c, &["checkout", "-q", "master"]);
            std::fs::write(c.join("a.txt"), "from master\n").unwrap();
            common::git(c, &["commit", "-qam", "master side"]);
        })
        .await
    }

    /// `base` has a commit; `master` (the job's head) was never committed, so it never reaches
    /// the remote at all — the fetch's named refspec for it fails, forcing the mirror fallback,
    /// which still finds nothing to mirror.
    async fn head_gone(e: &common::TestEnv) -> String {
        fleet(e, |c| {
            common::git(c, &["checkout", "-q", "-b", "base"]);
            std::fs::write(c.join("a.txt"), "one\n").unwrap();
            common::git(c, &["add", "."]);
            common::git(c, &["commit", "-qm", "one"]);
        })
        .await
    }

    async fn peer(base: &str, path: &str, body: Option<serde_json::Value>) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .post(format!("{base}{path}"))
            .header(kloudlite_core::peer::PEER_HEADER, "test-peer-secret")
            .header(kloudlite_core::peer::OWNER_HEADER, "a");
        if let Some(b) = body {
            req = req.json(&b);
        }
        req.send().await.unwrap()
    }

    /// The token this "lane" claims with; the owner refuses an outcome posted under any other.
    const LANE: &str = "test-lane";

    /// Claim the job from the owner, exactly as the worker does.
    async fn claim(base: &str, number: i64) -> merge_worker::Job {
        let r = peer(
            base,
            &format!("/api/a/r/pulls/{number}/claim?by={LANE}"),
            None,
        )
        .await;
        assert_eq!(r.status(), 200, "the job must be claimable");
        r.json().await.unwrap()
    }

    /// The whole worker path for one change: claim, merge with the `git` binary, report the outcome.
    async fn drive(base: &str, number: i64) -> (Outcome, tempfile::TempDir) {
        let job = claim(base, number).await;
        run(job, base).await
    }

    /// The two halves after the claim, so a test can do something between them.
    async fn run(job: merge_worker::Job, base: &str) -> (Outcome, tempfile::TempDir) {
        let number = job.number;
        let cache = tempfile::tempdir().unwrap();
        let (dir, url) = (cache.path().to_path_buf(), base.to_string());
        let out = tokio::task::spawn_blocking(move || {
            merge_worker::run(&job, &dir, &url, "test-peer-secret").unwrap()
        })
        .await
        .unwrap();
        let r = peer(
            base,
            &format!("/api/a/r/pulls/{number}/outcome?by={LANE}"),
            Some(serde_json::to_value(&out).unwrap()),
        )
        .await;
        assert_eq!(r.status(), 204);
        (out, cache)
    }

    async fn tip(e: &common::TestEnv, branch: &str) -> gix_hash::ObjectId {
        let repo = e.store.open_repo("a", "r").await.unwrap().unwrap();
        e.store
            .get_ref(&repo, &format!("refs/heads/{branch}"))
            .await
            .unwrap()
            .unwrap()
    }

    /// How many parents the commit at `branch` has, and whether `path` is in its tree — the two
    /// questions that tell merge, squash and rebase apart from each other.
    async fn shape(e: &common::TestEnv, branch: &str) -> (usize, Vec<String>) {
        let repo = e.store.open_repo("a", "r").await.unwrap().unwrap();
        let oid = tip(e, branch).await;
        tokio::task::spawn_blocking(move || {
            let odb = repo.odb().unwrap();
            let mut buf = Vec::new();
            let c = gix_object::FindExt::find_commit(&odb, &oid, &mut buf).unwrap();
            let (parents, tree) = (c.parents().count(), c.tree());
            let mut buf2 = Vec::new();
            let t = gix_object::FindExt::find_tree(&odb, &tree, &mut buf2).unwrap();
            let names = t.entries.iter().map(|e| e.filename.to_string()).collect();
            (parents, names)
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fast_forward_lands_and_the_change_is_merged() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = behind(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "fast-forward")).await.unwrap();

        let (out, _cache) = drive(&fleet, 1).await;
        assert_eq!(out.state, OutcomeState::Merged, "{:?}", out.detail);

        let got = pulls::get(&db, 1).await.unwrap().unwrap();
        assert_eq!(got.state, PullState::Merged);
        assert!(got.merged_at_ms.is_some());
        assert!(
            got.merge.is_none(),
            "a finished job is cleared, not left Running"
        );
        assert_eq!(
            tip(&e, "base").await,
            tip(&e, "master").await,
            "the base must have moved"
        );
    }

    /// The case the old owner-side merge could not do at all: two branches that both moved.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_diverged_merge_keeps_both_sides_and_both_parents() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "merge")).await.unwrap();

        let (out, _cache) = drive(&fleet, 1).await;
        assert_eq!(out.state, OutcomeState::Merged, "{:?}", out.detail);
        assert_eq!(
            pulls::get(&db, 1).await.unwrap().unwrap().state,
            PullState::Merged
        );

        let (parents, files) = shape(&e, "base").await;
        assert_eq!(parents, 2, "a merge commit keeps both histories");
        assert!(
            files.contains(&"b.txt".to_string()) && files.contains(&"m.txt".to_string()),
            "both sides' work must survive: {files:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_squash_lands_the_same_tree_under_one_parent() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "squash")).await.unwrap();

        let (out, _cache) = drive(&fleet, 1).await;
        assert_eq!(out.state, OutcomeState::Merged, "{:?}", out.detail);

        let (parents, files) = shape(&e, "base").await;
        assert_eq!(parents, 1, "a squash keeps only the base's history");
        assert!(files.contains(&"b.txt".to_string()) && files.contains(&"m.txt".to_string()));
    }

    /// The refusal that matters most: nothing is pushed, the change stays open, and the person
    /// waiting is told WHICH file to go and look at.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_same_line_conflict_is_reported_and_the_base_does_not_move() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = conflicting(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "merge")).await.unwrap();
        let before = tip(&e, "base").await;

        let (out, _cache) = drive(&fleet, 1).await;
        assert_eq!(out.state, OutcomeState::Conflicts);
        assert!(
            out.detail.as_deref().unwrap().contains("a.txt"),
            "{:?}",
            out.detail
        );

        let got = pulls::get(&db, 1).await.unwrap().unwrap();
        assert_eq!(got.state, PullState::Open, "a conflicted change stays open");
        let job = got.merge.expect("the job stays, carrying the reason");
        assert_eq!(job.state, MergeState::Conflicts);
        assert!(job.detail.unwrap().contains("a.txt"));
        assert_eq!(tip(&e, "base").await, before, "nothing may be pushed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rebase_lands_a_linear_history() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "rebase")).await.unwrap();
        let before = tip(&e, "base").await;

        let (out, _cache) = drive(&fleet, 1).await;
        assert_eq!(out.state, OutcomeState::Merged, "{:?}", out.detail);

        let (parents, files) = shape(&e, "base").await;
        assert_eq!(
            parents, 1,
            "a rebase replays commits; it does not merge them"
        );
        assert!(files.contains(&"b.txt".to_string()) && files.contains(&"m.txt".to_string()));
        assert_ne!(tip(&e, "base").await, before, "the base must have moved");
    }

    /// The base moved on, so there is nothing to fast-forward. Refused with an instruction, not a
    /// silent failure — and the job keeps the sentence for the page to show.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fast_forward_of_a_diverged_branch_is_refused() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "fast-forward")).await.unwrap();
        let before = tip(&e, "base").await;

        let (out, _cache) = drive(&fleet, 1).await;
        assert_eq!(out.state, OutcomeState::Refused);
        assert!(
            out.detail
                .as_deref()
                .unwrap()
                .contains("not a fast-forward"),
            "{:?}",
            out.detail
        );

        let got = pulls::get(&db, 1).await.unwrap().unwrap();
        assert_eq!(got.state, PullState::Open);
        let job = got.merge.expect("the reason is kept on the job");
        assert_eq!(job.state, MergeState::Failed);
        assert!(job.detail.unwrap().contains("not a fast-forward"));
        assert_eq!(tip(&e, "base").await, before);
    }

    /// The head branch was never pushed, so the worker's targeted fetch (base+head refspecs)
    /// fails on the missing head, falls back to a full mirror fetch, and still finds nothing —
    /// exercising both the fallback path and the "branch is gone" refusal it exists to preserve.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_gone_head_branch_is_refused_after_the_mirror_fallback() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = head_gone(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "merge")).await.unwrap();

        let (out, _cache) = drive(&fleet, 1).await;
        assert_eq!(out.state, OutcomeState::Refused, "{:?}", out.detail);
        assert!(
            out.detail.as_deref().unwrap_or("").contains("one of the branches is gone"),
            "{:?}",
            out.detail
        );
    }

    /// Someone else lands on the base between the claim and the merge.
    ///
    /// The worker resolves the branches when it MERGES, not when it claims, so the right outcome
    /// is that their commit is merged against rather than merged over — and `--force-with-lease`
    /// on the push is what keeps that true for the narrower window after the resolve. What is
    /// pinned here is the property that matters to the person who pushed: their work survives in
    /// the history the merge lands.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_base_that_moved_between_the_claim_and_the_merge_is_not_overwritten() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "merge")).await.unwrap();

        let job = claim(&fleet, 1).await;
        let theirs = land_on_base(&e).await;

        let (out, _cache) = run(job, &fleet).await;
        assert_eq!(out.state, OutcomeState::Merged, "{:?}", out.detail);
        assert_eq!(
            pulls::get(&db, 1).await.unwrap().unwrap().state,
            PullState::Merged
        );
        assert!(
            ancestors(&e, tip(&e, "base").await).await.contains(&theirs),
            "the commit that landed first must still be in the base's history"
        );
    }

    /// A commit on top of `base`, written straight through the store — the cheapest stand-in for
    /// "somebody else pushed while this was being merged". It reuses the base's own tree, because
    /// what is being tested is the ancestry, not the content.
    async fn land_on_base(e: &common::TestEnv) -> gix_hash::ObjectId {
        let repo = e.store.open_repo("a", "r").await.unwrap().unwrap();
        let base = tip(e, "base").await;
        let r = repo.clone();
        let tree = tokio::task::spawn_blocking(move || {
            let odb = r.odb().unwrap();
            let mut buf = Vec::new();
            let t = gix_object::FindExt::find_commit(&odb, &base, &mut buf)
                .unwrap()
                .tree();
            t
        })
        .await
        .unwrap();
        let oid = kloudlite_gitbase::objects::write_commit(
            &e.store,
            &repo,
            kloudlite_gitbase::objects::NewCommit {
                tree,
                parents: vec![base],
                message: "somebody else\n".into(),
                author_name: "other".into(),
                author_email: "other@t".into(),
                time: 1_700_000_000,
            },
        )
        .await
        .unwrap();
        e.store
            .update_refs(
                &repo,
                &[kloudlite_gitbase::refs::RefUpdate {
                    name: "refs/heads/base".into(),
                    old: Some(base),
                    new: Some(oid),
                }],
            )
            .await
            .unwrap();
        oid
    }

    /// Every commit reachable from `from`.
    async fn ancestors(e: &common::TestEnv, from: gix_hash::ObjectId) -> Vec<gix_hash::ObjectId> {
        let repo = e.store.open_repo("a", "r").await.unwrap().unwrap();
        tokio::task::spawn_blocking(move || {
            let odb = repo.odb().unwrap();
            gix_traverse::commit::Simple::new(Some(from), odb)
                .filter_map(|i| i.ok())
                .map(|i| i.id)
                .collect()
        })
        .await
        .unwrap()
    }

    /// Two workers, one nudge each: the second must not get the job. This is the whole reason the
    /// claim is a round trip to the owner rather than something the worker decides for itself.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_claimed_merge_cannot_be_claimed_again() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = behind(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "fast-forward")).await.unwrap();

        assert_eq!(
            peer(&fleet, "/api/a/r/pulls/1/claim?by=one", None)
                .await
                .status(),
            200
        );
        assert_eq!(
            peer(&fleet, "/api/a/r/pulls/1/claim?by=two", None)
                .await
                .status(),
            409
        );
    }

    /// A worker that took a job and died. The owner does not merge it — it says so again, and the
    /// next worker to hear the event claims it. Nothing is stranded and nothing is done twice.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_claim_is_re_announced_once_its_lease_lapses() {
        let e = common::env_cached().await;
        e.store.create_repo("a", "r").await.unwrap();
        let db = e.store.db_for("a", "r").await.unwrap();
        // Claimed so long ago that the lease cannot still be good.
        pulls::put(
            &db,
            &PullRequest {
                merge: Some(MergeJob {
                    state: MergeState::Running,
                    claimed_at_ms: Some(1),
                    claimed_by: Some("a worker that died".into()),
                    ..queued(1, "merge").merge.unwrap()
                }),
                ..queued(1, "merge")
            },
        )
        .await
        .unwrap();
        // Warm, so the owner's lane sees the repo at all.
        e.store.open_repo("a", "r").await.unwrap();

        let a = common::app(e.store.clone()).await;
        kloudlite_server::lanes::announce_stranded_merges(&a).await;

        // `xrevrange`, not `xreadgroup`: the in-process stand-in has no consumer groups, and what
        // is being asserted is that the event was PUBLISHED, not how it is delivered.
        let published = e.store.cache.xrevrange("events", 16).await;
        let kinds: Vec<String> = published
            .iter()
            .filter_map(|(_, f)| kloudlite_storage::events::from_fields(f))
            .map(|ev| format!("{:?}#{}", ev.kind, ev.number))
            .collect();
        assert!(
            kinds.contains(&"MergeRequested#1".to_string()),
            "got {kinds:?}"
        );

        // And it is still there to claim: re-announcing must not have consumed it.
        let still = pulls::get(&db, 1).await.unwrap().unwrap();
        assert_eq!(still.merge.unwrap().state, MergeState::Running);
        assert!(pulls::claim_merge_number(
            &e.store,
            "a",
            "r",
            1,
            std::time::Duration::from_secs(600),
            "next worker"
        )
        .await
        .unwrap()
        .is_some());
    }

    /// The mergeability half. The owner answers ancestry and stops at "diverged"; the worker's
    /// trial merge is what turns that into a yes or a no — and a yes must NOT offer fast-forward.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_diverged_change_is_answered_by_the_workers_trial_merge() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &open_pr(1)).await.unwrap();

        // The owner's cheap pass: it cannot answer, so it says so and names the change.
        let r = peer(&fleet, "/api/a/r/pulls/1/check", None).await;
        assert_eq!(r.status(), 200);
        let deep: Vec<pulls::Deep> = r.json().await.unwrap();
        assert_eq!(deep.len(), 1, "a diverged change is the worker's to answer");
        let pending = pulls::get(&db, 1)
            .await
            .unwrap()
            .unwrap()
            .mergeability
            .unwrap();
        assert_eq!(pending.state, MergeableState::Unknown);

        let job = merge_worker::Job {
            owner: "a".into(),
            name: "r".into(),
            number: 1,
            strategy: String::new(),
            base: deep[0].base.clone(),
            head: deep[0].head.clone(),
            title: String::new(),
            requested_by: String::new(),
        };
        let cache = tempfile::tempdir().unwrap();
        let (dir, url) = (cache.path().to_path_buf(), fleet.clone());
        let verdict = tokio::task::spawn_blocking(move || {
            merge_worker::check(&job, &dir, &url, "test-peer-secret").unwrap()
        })
        .await
        .unwrap();
        assert_eq!(verdict.state, MergeableState::Clean);
        assert!(!verdict.fast_forward);
        assert_eq!(
            verdict.base_oid, pending.base_oid,
            "the worker stamps the verdict with the tips it merged"
        );
        assert_eq!(verdict.head_oid, pending.head_oid);

        let r = peer(
            &fleet,
            "/api/a/r/pulls/1/mergeability",
            Some(serde_json::to_value(&verdict).unwrap()),
        )
        .await;
        assert_eq!(r.status(), 204);
        let m = pulls::get(&db, 1)
            .await
            .unwrap()
            .unwrap()
            .mergeability
            .unwrap();
        assert_eq!(m.state, MergeableState::Clean);
        assert!(
            !m.fast_forward,
            "a diverged branch is mergeable but not fast-forwardable"
        );
        assert_eq!(
            m.base_oid, pending.base_oid,
            "the tips the answer belongs to are kept"
        );
    }

    /// Two branches that cannot be combined at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_conflicting_change_is_answered_dirty() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = conflicting(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &open_pr(1)).await.unwrap();
        assert_eq!(
            peer(&fleet, "/api/a/r/pulls/1/check", None).await.status(),
            200
        );

        let job = merge_worker::Job {
            owner: "a".into(),
            name: "r".into(),
            number: 1,
            strategy: String::new(),
            base: "base".into(),
            head: "master".into(),
            title: String::new(),
            requested_by: String::new(),
        };
        let cache = tempfile::tempdir().unwrap();
        let (dir, url) = (cache.path().to_path_buf(), fleet.clone());
        let verdict = tokio::task::spawn_blocking(move || {
            merge_worker::check(&job, &dir, &url, "test-peer-secret").unwrap()
        })
        .await
        .unwrap();
        assert_eq!(verdict.state, MergeableState::Dirty);
        assert!(verdict.detail.unwrap().contains("a.txt"));
        let _ = db;
    }

    /// A cache nothing has touched in a week is a bare clone of a repo this worker may never see
    /// again. Deleting one costs a fetch; keeping every one of them costs the disk.
    #[test]
    fn idle_merge_caches_are_pruned_and_fresh_ones_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = merge_worker::cache_of(tmp.path(), "a", "fresh");
        let cold = merge_worker::cache_of(tmp.path(), "a", "cold");
        for d in [&fresh, &cold] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join(".last-used"), b"").unwrap();
        }
        // Nothing is old enough yet.
        assert_eq!(
            merge_worker::prune(tmp.path(), std::time::Duration::from_secs(60), u64::MAX),
            0
        );
        assert!(fresh.exists() && cold.exists());
        // A zero age makes every stamp "untouched for longer than that".
        assert_eq!(
            merge_worker::prune(tmp.path(), std::time::Duration::ZERO, u64::MAX),
            2
        );
        assert!(!fresh.exists() && !cold.exists());
    }

    /// A worker that merged and then lost its outcome POST gets the job back when the lease
    /// lapses. Running it again must NOT mint a second, empty merge commit on top of the first:
    /// the base already contains the head, so the honest answer is "already merged".
    #[tokio::test(flavor = "multi_thread")]
    async fn a_merge_that_already_landed_is_not_performed_twice() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "merge")).await.unwrap();

        let job = claim(&fleet, 1).await;
        let (first, _c1) = run(job.clone(), &fleet).await;
        assert_eq!(first.state, OutcomeState::Merged, "{:?}", first.detail);
        let landed = tip(&e, "base").await;

        // The same job again, exactly as a lapsed lease would hand it back — a fresh cache too,
        // so nothing about the answer can come from state the first run left behind.
        let cache = tempfile::tempdir().unwrap();
        let (dir, url) = (cache.path().to_path_buf(), fleet.clone());
        let second = tokio::task::spawn_blocking(move || {
            merge_worker::run(&job, &dir, &url, "test-peer-secret").unwrap()
        })
        .await
        .unwrap();

        assert_eq!(
            second.state,
            OutcomeState::Merged,
            "a retry reports the landing, not a failure"
        );
        assert_eq!(
            tip(&e, "base").await,
            landed,
            "no second commit may be minted"
        );
        assert_eq!(second.new_tip.unwrap(), landed.to_hex().to_string());
    }

    /// The squash variant of the retry: a squash rewrites, so the ancestry guard cannot see it —
    /// the merged-tree-equals-base-tree guard must, or the retry mints an empty commit.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_squash_that_already_landed_is_not_performed_twice() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "squash")).await.unwrap();

        let job = claim(&fleet, 1).await;
        let (first, _c1) = run(job.clone(), &fleet).await;
        assert_eq!(first.state, OutcomeState::Merged, "{:?}", first.detail);
        let landed = tip(&e, "base").await;

        let cache = tempfile::tempdir().unwrap();
        let (dir, url) = (cache.path().to_path_buf(), fleet.clone());
        let second = tokio::task::spawn_blocking(move || {
            merge_worker::run(&job, &dir, &url, "test-peer-secret").unwrap()
        })
        .await
        .unwrap();

        assert_eq!(
            second.state,
            OutcomeState::Merged,
            "a squash retry reports the landing"
        );
        assert_eq!(
            tip(&e, "base").await,
            landed,
            "no empty squash commit may be minted"
        );
        assert_eq!(second.new_tip.unwrap(), landed.to_hex().to_string());
    }

    /// A worker whose lease lapsed mid-merge may still be running. Its late report must not
    /// overwrite the state of the worker that holds the job now — "merged" on a change still being
    /// merged, or "failed" on one that just landed.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_outcome_from_a_worker_that_lost_the_claim_is_refused() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = behind(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &queued(1, "fast-forward")).await.unwrap();
        let _ = claim(&fleet, 1).await;
        let before = pulls::get(&db, 1).await.unwrap().unwrap();

        let r = peer(
            &fleet,
            "/api/a/r/pulls/1/outcome?by=a-worker-that-lost-the-race",
            Some(serde_json::json!({"state": "merged"})),
        )
        .await;
        assert_eq!(r.status(), 409);
        assert_eq!(
            pulls::get(&db, 1).await.unwrap().unwrap(),
            before,
            "nothing may be written"
        );

        // No `by` at all is the same answer: a claim always records a token.
        let r = peer(
            &fleet,
            "/api/a/r/pulls/1/outcome",
            Some(serde_json::json!({"state": "merged"})),
        )
        .await;
        assert_eq!(r.status(), 409);
        assert_eq!(pulls::get(&db, 1).await.unwrap().unwrap(), before);
    }

    /// The safety net must not become a firehose. A job nothing can claim is re-announced on a
    /// clock of its own, not on every 15s beat — the events stream is capped, and a job announcing
    /// itself forever would evict the activity feed everyone else reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stranded_job_is_re_announced_on_its_own_clock_not_every_beat() {
        let e = common::env_cached().await;
        e.store.create_repo("a", "r").await.unwrap();
        let db = e.store.db_for("a", "r").await.unwrap();
        e.store.open_repo("a", "r").await.unwrap();
        let app = common::app(e.store.clone()).await;

        // Just requested — `queued`'s fixed timestamp is years old, so this one carries the
        // clock. The merge handler has already announced it, so the beat must stay quiet.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let mut fresh = queued(1, "merge");
        fresh.merge.as_mut().unwrap().requested_at_ms = now;
        pulls::put(&db, &fresh).await.unwrap();
        kloudlite_server::lanes::announce_stranded_merges(&app).await;
        assert!(
            e.store.cache.xrevrange("events", 16).await.is_empty(),
            "a job announced a moment ago must not be announced again"
        );

        // Old enough to look lost.
        let stale = now - pulls::ANNOUNCE_EVERY.as_millis() as i64 * 4;
        pulls::modify(&e.store, "a", "r", 1, |pr| {
            pr.merge.as_mut().unwrap().requested_at_ms = stale;
            true
        })
        .await
        .unwrap();
        kloudlite_server::lanes::announce_stranded_merges(&app).await;
        assert_eq!(
            e.store.cache.xrevrange("events", 16).await.len(),
            1,
            "said once"
        );

        // And the stamp it left keeps the very next beat quiet.
        assert!(pulls::get(&db, 1)
            .await
            .unwrap()
            .unwrap()
            .merge
            .unwrap()
            .announced_at_ms
            .is_some());
        kloudlite_server::lanes::announce_stranded_merges(&app).await;
        assert_eq!(
            e.store.cache.xrevrange("events", 16).await.len(),
            1,
            "and not again yet"
        );
    }

    /// The outcome route guards a lapsed worker's late report by matching `?by=` against the claim.
    /// The verdict route has no claim to match, so it matches the TIPS instead: a slow lane's answer
    /// is only true of the branches it was computed from, and must not overwrite a newer lane's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_verdict_computed_from_other_tips_is_refused() {
        if !common::have_git() {
            eprintln!("skipping: no git");
            return;
        }
        let e = common::env().await;
        let fleet = diverged(&e).await;
        let db = e.store.db_for("a", "r").await.unwrap();
        pulls::put(&db, &open_pr(1)).await.unwrap();
        assert_eq!(peer(&fleet, "/api/a/r/pulls/1/check", None).await.status(), 200);
        let pending = pulls::get(&db, 1).await.unwrap().unwrap().mergeability.unwrap();
        assert_eq!(pending.state, MergeableState::Unknown);

        // A verdict stamped with a head the row never saw: the branch moved on since.
        let stale = serde_json::json!({
            "state": "clean",
            "detail": "from an older lane",
            "fastForward": false,
            "baseOid": pending.base_oid,
            "headOid": "0".repeat(40),
        });
        let r = peer(&fleet, "/api/a/r/pulls/1/mergeability", Some(stale)).await;
        assert_eq!(r.status(), 409, "a verdict about other tips is not this change's answer");
        assert_eq!(
            pulls::get(&db, 1).await.unwrap().unwrap().mergeability.unwrap().state,
            MergeableState::Unknown,
            "the row is untouched"
        );

        // The same verdict, honestly stamped, lands.
        let fresh = serde_json::json!({
            "state": "clean",
            "detail": "ok",
            "fastForward": false,
            "baseOid": pending.base_oid,
            "headOid": pending.head_oid,
        });
        let r = peer(&fleet, "/api/a/r/pulls/1/mergeability", Some(fresh)).await;
        assert_eq!(r.status(), 204);
        assert_eq!(
            pulls::get(&db, 1).await.unwrap().unwrap().mergeability.unwrap().state,
            MergeableState::Clean
        );

        // And an UNSTAMPED verdict still lands, so a worker older than this field keeps working
        // through a roll.
        let unstamped = serde_json::json!({"state": "dirty", "detail": "old worker", "fastForward": false});
        let r = peer(&fleet, "/api/a/r/pulls/1/mergeability", Some(unstamped)).await;
        assert_eq!(r.status(), 204);
    }

}
