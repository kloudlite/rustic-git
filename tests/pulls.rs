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
