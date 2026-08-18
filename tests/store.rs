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
    assert!(!authorize(Some("bob"), "alice", true), "public grants read, not identity");
}
