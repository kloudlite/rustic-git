mod common;
use gix_hash::ObjectId;
use rustic_git::refs::RefUpdate;

#[tokio::test]
async fn repo_and_refs() {
    let e = common::env().await;
    let s = &e.store;
    assert!(!s.repo_exists("a", "r").await.unwrap());
    s.create_repo("a", "r").await.unwrap();
    assert!(s.repo_exists("a", "r").await.unwrap());
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(repo.pack_dir.is_dir());
    let oid1 = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
    let oid2 = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
    // create
    let r = s
        .update_refs(
            &repo,
            &[RefUpdate {
                name: "refs/heads/main".into(),
                old: None,
                new: Some(oid1),
            }],
        )
        .await
        .unwrap();
    assert_eq!(r, vec![None]);
    assert_eq!(
        s.get_ref(&repo, "refs/heads/main").await.unwrap(),
        Some(oid1)
    );
    // stale old -> rejected
    let r = s
        .update_refs(
            &repo,
            &[RefUpdate {
                name: "refs/heads/main".into(),
                old: Some(oid2),
                new: Some(oid2),
            }],
        )
        .await
        .unwrap();
    assert!(r[0].is_some());
    // correct old -> ok
    let r = s
        .update_refs(
            &repo,
            &[RefUpdate {
                name: "refs/heads/main".into(),
                old: Some(oid1),
                new: Some(oid2),
            }],
        )
        .await
        .unwrap();
    assert_eq!(r, vec![None]);
    // list
    s.update_refs(
        &repo,
        &[RefUpdate {
            name: "refs/tags/v1".into(),
            old: None,
            new: Some(oid1),
        }],
    )
    .await
    .unwrap();
    let l = s.list_refs(&repo).await.unwrap();
    assert_eq!(
        l.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec!["refs/heads/main", "refs/tags/v1"]
    );
    // delete
    s.update_refs(
        &repo,
        &[RefUpdate {
            name: "refs/tags/v1".into(),
            old: Some(oid1),
            new: None,
        }],
    )
    .await
    .unwrap();
    assert_eq!(s.list_refs(&repo).await.unwrap().len(), 1);
}

#[tokio::test]
async fn pack_sync_roundtrip() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let p = repo.pack_dir.join("pack-abc.pack");
    let i = repo.pack_dir.join("pack-abc.idx");
    std::fs::write(&p, b"PACKDATA").unwrap();
    std::fs::write(&i, b"IDX").unwrap();
    s.upload_pack_files(&repo, &p, &i).await.unwrap();
    // wipe cache, reopen -> files re-downloaded
    std::fs::remove_dir_all(&repo.pack_dir).unwrap();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    assert_eq!(
        std::fs::read(repo2.pack_dir.join("pack-abc.pack")).unwrap(),
        b"PACKDATA"
    );
    assert_eq!(
        std::fs::read(repo2.pack_dir.join("pack-abc.idx")).unwrap(),
        b"IDX"
    );
}

#[tokio::test]
async fn fork_copies_objects_and_refs() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    let src = s.open_repo("a", "r").await.unwrap().unwrap();
    let oid = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
    s.update_refs(
        &src,
        &[RefUpdate {
            name: "refs/heads/main".into(),
            old: None,
            new: Some(oid),
        }],
    )
    .await
    .unwrap();
    let p = src.pack_dir.join("pack-x.pack");
    let i = src.pack_dir.join("pack-x.idx");
    std::fs::write(&p, b"P").unwrap();
    std::fs::write(&i, b"I").unwrap();
    s.upload_pack_files(&src, &p, &i).await.unwrap();

    s.fork(&src, "b", "f").await.unwrap();
    let dst = s.open_repo("b", "f").await.unwrap().unwrap();

    // objects are COPIED, not shared: separate prefixes and separate local dirs
    assert_ne!(dst.pack_dir, src.pack_dir);
    assert_eq!(dst.s3_prefix(), "objects/b/f/pack");
    assert_eq!(src.s3_prefix(), "objects/a/r/pack");
    assert!(
        dst.pack_dir.join("pack-x.pack").exists(),
        "fork needs its own copy"
    );
    assert_eq!(s.get_ref(&dst, "refs/heads/main").await.unwrap(), Some(oid));

    // refs are independent
    let oid2 = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
    s.update_refs(
        &dst,
        &[RefUpdate {
            name: "refs/heads/main".into(),
            old: Some(oid),
            new: Some(oid2),
        }],
    )
    .await
    .unwrap();
    assert_eq!(s.get_ref(&src, "refs/heads/main").await.unwrap(), Some(oid));
    assert!(
        s.fork(&src, "b", "f").await.is_err(),
        "duplicate fork must be rejected"
    );

    // deleting the fork must not touch the source's objects
    s.delete_repo("b", "f").await.unwrap();
    let src = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(
        src.pack_dir.join("pack-x.pack").exists(),
        "source objects must survive"
    );
}

#[tokio::test]
async fn delete_repo_removes_refs_and_membership() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let oid = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
    s.update_refs(
        &repo,
        &[RefUpdate {
            name: "refs/heads/main".into(),
            old: None,
            new: Some(oid),
        }],
    )
    .await
    .unwrap();
    s.delete_repo("a", "r").await.unwrap();
    assert!(!s.repo_exists("a", "r").await.unwrap());
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(s.list_refs(&repo).await.unwrap().is_empty());
}

#[tokio::test]
async fn create_repo_rejects_duplicates() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    assert!(s.create_repo("a", "r").await.is_err());
}

/// Probing a repo that does not exist must not bring its database into being.
#[tokio::test]
async fn probing_an_unknown_repo_opens_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let s = rustic_git::store::Store::open(
        std::sync::Arc::new(slatedb::object_store::memory::InMemory::new()),
        tmp.path().join("c"),
        false,
    )
    .await
    .unwrap();
    s.create_repo("o", "r").await.unwrap();
    assert!(s.repo_exists("o", "r").await.unwrap());
    assert!(!s.repo_exists("o", "nope").await.unwrap());
    assert_eq!(s.pool.warm_count(), 1, "probing must not open a database");
}

#[tokio::test]
async fn visibility_defaults_private_and_round_trips() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    assert!(!e.store.is_public("alice", "web").await.unwrap());
    e.store.set_public("alice", "web", true).await.unwrap();
    assert!(e.store.is_public("alice", "web").await.unwrap());
    e.store.set_public("alice", "web", false).await.unwrap();
    assert!(!e.store.is_public("alice", "web").await.unwrap());
}

#[test]
fn authorize_allows_anonymous_reads_only_when_public() {
    use rustic_git::auth::authorize;
    assert!(!authorize(None, "alice", false));
    assert!(authorize(None, "alice", true));
    assert!(authorize(Some("alice"), "alice", false));
}

/// Catches the flaw where presenting a valid token made a caller LESS able to read a public repo
/// than presenting none: the `Some(_)` arm ignored `public_read` entirely, so every logged-in
/// stranger was locked out of every public repo (404 on browse, 403 on clone).
/// The write half catches the opposite mistake — public must grant read, never identity.
#[test]
fn public_grants_read_to_everyone_and_write_to_nobody_but_the_owner() {
    use rustic_git::auth::authorize;
    // reads on a public repo: callers pass public_read = true
    assert!(authorize(None, "alice", true), "anonymous may read a public repo");
    assert!(authorize(Some("bob"), "alice", true), "a stranger's token may read a public repo");
    assert!(authorize(Some("alice"), "alice", true), "the owner may read her public repo");
    // writes and admin: callers pass public_read = false whatever the visibility
    assert!(!authorize(None, "alice", false), "anonymous may not write a public repo");
    assert!(!authorize(Some("bob"), "alice", false), "a stranger may not write a public repo");
    assert!(authorize(Some("alice"), "alice", false), "the owner may write her repo");
}

/// `api` is the browse prefix, so it cannot also be an owner: `/api/alice/info/refs` would be both
/// the git route of `api/alice` and the browse route of `alice/info`. Rejected at the two creation
/// points, which is what `admin create-repo` and `admin fork` call.
#[tokio::test]
async fn api_is_a_reserved_owner_name() {
    let e = common::env().await;
    let s = &e.store;
    assert!(s.create_repo("api", "web").await.is_err(), "admin create-repo api/web");
    assert!(!s.repo_exists("api", "web").await.unwrap());
    s.create_repo("alice", "web").await.unwrap();
    let src = s.open_repo("alice", "web").await.unwrap().unwrap();
    assert!(s.fork(&src, "api", "web").await.is_err(), "admin fork ... api/web");
    // Only the OWNER is reserved; a repo may still be NAMED api.
    s.create_repo("alice", "api").await.unwrap();
}

// ── branch protection ───────────────────────────────────────────────────────

use rustic_git::refs::Protection;

fn rule(pattern: &str) -> Protection {
    Protection { pattern: pattern.into(), no_force: true, no_delete: true }
}

#[test]
fn a_pattern_matches_a_name_or_a_prefix() {
    assert!(rule("main").matches("main"));
    assert!(!rule("main").matches("mainline"), "an exact pattern is exact");
    assert!(rule("release/*").matches("release/1.0"));
    assert!(rule("release/*").matches("release/"));
    assert!(!rule("release/*").matches("releases/1.0"));
}

/// The rules a protected branch enforces, through the real push path.
#[tokio::test(flavor = "multi_thread")]
async fn a_protected_branch_refuses_a_delete_and_a_rewrite() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "web").await; // two commits on master
    let s = &e.store;

    let head = s.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let commits = rustic_git::browse::log(&repo.odb().unwrap(), head, 10).unwrap();
    let parent: gix_hash::ObjectId = commits[1].oid.parse().unwrap();

    // Unprotected: rewinding master to its parent is allowed.
    let rewind = vec![rustic_git::refs::RefUpdate {
        name: "refs/heads/master".into(),
        old: Some(head),
        new: Some(parent),
    }];
    assert_eq!(s.update_refs(&repo, &rewind).await.unwrap(), vec![None]);

    // Put it back, then protect it.
    let forward = vec![rustic_git::refs::RefUpdate {
        name: "refs/heads/master".into(),
        old: Some(parent),
        new: Some(head),
    }];
    assert_eq!(s.update_refs(&repo, &forward).await.unwrap(), vec![None], "a fast-forward is fine");
    s.set_protection("alice", "web", &rule("master")).await.unwrap();

    // The same rewind is now refused, and the ref is untouched.
    let refused = s.update_refs(&repo, &rewind).await.unwrap();
    assert!(refused[0].as_deref().is_some_and(|m| m.contains("force")), "got {refused:?}");
    assert_eq!(s.get_ref(&repo, "refs/heads/master").await.unwrap(), Some(head), "nothing moved");

    // So is deleting it.
    let delete = vec![rustic_git::refs::RefUpdate {
        name: "refs/heads/master".into(),
        old: Some(head),
        new: None,
    }];
    let refused = s.update_refs(&repo, &delete).await.unwrap();
    assert!(refused[0].as_deref().is_some_and(|m| m.contains("deleted")), "got {refused:?}");
    assert_eq!(s.get_ref(&repo, "refs/heads/master").await.unwrap(), Some(head), "still there");

    // An unprotected branch beside it is unaffected — rules are per pattern.
    let other = vec![rustic_git::refs::RefUpdate {
        name: "refs/heads/scratch".into(),
        old: None,
        new: Some(parent),
    }];
    assert_eq!(s.update_refs(&repo, &other).await.unwrap(), vec![None]);

    // Lifting the rule restores the old behaviour.
    s.remove_protection("alice", "web", "master").await.unwrap();
    assert_eq!(s.update_refs(&repo, &rewind).await.unwrap(), vec![None]);
}
