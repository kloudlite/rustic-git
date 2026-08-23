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

/// The web app owns `/{owner}/activity` and five other names in the same
/// position as `/{owner}/{repo}`. A static segment beats a dynamic one, so a
/// repo with one of those names would be created and then be unreachable
/// forever — its page showing the namespace's feed instead of the repo.
#[test]
fn a_repo_cannot_be_named_after_a_page_in_the_namespace() {
    use rustic_git::store::reserved_repo_name;
    for name in rustic_git::store::RESERVED_REPO_NAMES {
        assert!(reserved_repo_name(name), "{name} must be refused");
        assert!(reserved_repo_name(&name.to_uppercase()), "{name} in any case");
    }
    // Everything else is still a name. The check must not creep.
    for ok in ["rustic-git", "api", "activities", "ci-runner", "settings-ui"] {
        assert!(!reserved_repo_name(ok), "{ok} must still be allowed");
    }
}

/// The web app keeps its own copy of the reserved names, because its chrome uses
/// them to tell `/{owner}/{repo}` from `/{owner}/{section}` without asking the
/// server. Two lists, one meaning: a name the web reserves and the server allows
/// shadows a real repo, and a name the server reserves and the web does not sends
/// a section to the repo router.
#[test]
fn the_web_and_the_server_reserve_the_same_names() {
    let ts = include_str!("../web/apps/web/src/lib/reserved.ts");
    let web: Vec<&str> = ts
        .split("export const RESERVED")
        .nth(1)
        .expect("RESERVED in reserved.ts")
        .split(']')
        .next()
        .expect("a list")
        .split('"')
        .skip(1)
        .step_by(2)
        .collect();
    let mut web = web;
    web.sort_unstable();
    let mut server: Vec<&str> = rustic_git::store::RESERVED_REPO_NAMES.to_vec();
    server.sort_unstable();
    assert_eq!(web, server, "web/lib/reserved.ts and RESERVED_REPO_NAMES must agree");
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

/// A commit the SERVER made ends up in the repo like any other.
///
/// The pack is written by hand — a 12-byte header, one zlib entry and a SHA-1
/// trailer — and the entry's size varint has a first group only 4 bits wide,
/// which is the part that is easy to get wrong. So the assertion is not "the
/// function returned Ok": it is that git's own indexer accepts the pack, the odb
/// can read the commit back, and a ref pointing at it resolves.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_the_server_writes_is_readable_afterwards() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "made").await;
    let s = &e.store;
    let head = s.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();

    // A squash: the head's tree, with the base as its only parent.
    let odb = repo.odb().unwrap();
    let mut headbuf = Vec::new();
    let tree = gix_object::FindExt::find_commit(&odb, &head, &mut headbuf).unwrap().tree();
    let squash = rustic_git::objects::NewCommit {
        tree,
        parents: vec![head],
        message: "squashed\n".into(),
        author_name: "kloudlite".into(),
        author_email: "noreply@kloudlite.io".into(),
        time: 1_700_000_000,
    };

    let oid = rustic_git::objects::write_commit(s, &repo, squash).await.unwrap();
    assert_ne!(oid, head, "a new commit, not the one we started from");

    // Readable through a FRESH handle, so this is the stored object rather than
    // anything cached from the write.
    let repo2 = s.open_repo("alice", "made").await.unwrap().unwrap();
    let odb2 = repo2.odb().unwrap();
    let mut buf = Vec::new();
    let read = gix_object::FindExt::find_commit(&odb2, &oid, &mut buf).unwrap();
    assert_eq!(read.message.to_string(), "squashed\n");
    assert_eq!(read.parents().collect::<Vec<_>>(), vec![head], "parented on the base");

    // And it can be pointed at, which is the whole purpose.
    let moved = s
        .update_refs(&repo2, &[rustic_git::refs::RefUpdate {
            name: "refs/heads/master".into(),
            old: Some(head),
            new: Some(oid),
        }])
        .await
        .unwrap();
    assert_eq!(moved, vec![None], "the ref moves to it");
    assert_eq!(s.get_ref(&repo2, "refs/heads/master").await.unwrap(), Some(oid));

    // Writing the same commit again is the same id and not an error: merges get
    // retried, and a retry must not fail or duplicate.
    let again = rustic_git::objects::write_commit(
        s,
        &repo2,
        rustic_git::objects::NewCommit {
            tree,
            parents: vec![head],
            message: "squashed\n".into(),
            author_name: "kloudlite".into(),
            author_email: "noreply@kloudlite.io".into(),
            time: 1_700_000_000,
        },
    )
    .await;
    assert!(matches!(again, Ok(id) if id == oid), "idempotent: {again:?}");
}

/// A patch is one commit over many files. These check the tree it builds is the
/// tree git would have built — including the parts that silently corrupt a repo
/// if they are wrong.
// multi_thread: push_built serves the push from a task on this runtime and
// then blocks the thread waiting for git, which one thread cannot do both of.
#[tokio::test(flavor = "multi_thread")]
async fn a_patch_edits_adds_and_deletes_in_one_commit() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    use rustic_git::objects::{apply_changes, Change, Staging};
    use std::collections::BTreeMap;

    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "patched").await;
    let s = &e.store;
    let head = s.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let odb = repo.odb().unwrap();
    let mut buf = Vec::new();
    let base = gix_object::FindExt::find_commit(&odb, &head, &mut buf).unwrap().tree();

    // What the fixture starts with, so the assertions below are about the patch.
    let before: Vec<String> = rustic_git::browse::files_at(&odb, head, "", 1000)
        .unwrap().into_iter().map(|e| e.name).collect();
    assert!(before.contains(&"src/main.rs".to_string()), "fixture has src/main.rs: {before:?}");

    let mut changes = BTreeMap::new();
    // An edit deep in the tree, a new file in a directory that does not exist
    // yet, and a delete -- all in ONE commit, which is the point of a patch.
    changes.insert("src/main.rs".to_string(), Change::Upsert { content: b"edited\n".to_vec(), executable: None });
    changes.insert("deep/nested/new.txt".to_string(), Change::Upsert { content: b"new\n".to_vec(), executable: None });
    changes.insert("README.md".to_string(), Change::Delete);

    let mut staging = Staging::default();
    let tree = apply_changes(&odb, Some(base), &changes, &mut staging).unwrap();
    assert_ne!(tree, base, "the tree changed");

    // Blobs and trees FIRST: a commit is validated against what is already
    // stored, so writing it before the tree it points at asks the indexer to
    // check an object against one that does not exist yet.
    staging.write(s, &repo).await.unwrap();
    let oid = rustic_git::objects::write_commit(s, &repo, rustic_git::objects::NewCommit {
        tree, parents: vec![head], message: "patch\n".into(),
        author_name: "K".into(), author_email: "k@example.com".into(), time: 1_700_000_000,
    }).await.unwrap();

    let fresh = s.open_repo("alice", "patched").await.unwrap().unwrap();
    let odb2 = fresh.odb().unwrap();
    let after: Vec<String> = rustic_git::browse::files_at(&odb2, oid, "", 1000)
        .unwrap().into_iter().map(|e| e.name).collect();
    assert!(after.contains(&"deep/nested/new.txt".to_string()), "new nested file: {after:?}");
    assert!(after.contains(&"src/main.rs".to_string()), "edited file still there: {after:?}");
    assert!(!after.contains(&"README.md".to_string()), "deleted file is gone: {after:?}");

    // The edit is really the new bytes, read back through a fresh handle.
    let blob = rustic_git::browse::blob_at(&odb2, oid, "src/main.rs", 1 << 20).unwrap();
    assert_eq!(blob.bytes, b"edited\n", "the edit landed");
}

// multi_thread: push_built serves the push from a task on this runtime and
// then blocks the thread waiting for git, which one thread cannot do both of.
#[tokio::test(flavor = "multi_thread")]
async fn a_patch_refuses_a_path_that_escapes_the_tree() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    use rustic_git::objects::{apply_changes, Change, Staging};
    use std::collections::BTreeMap;

    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "escapes").await;
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let odb = repo.odb().unwrap();
    let mut buf = Vec::new();
    let base = gix_object::FindExt::find_commit(&odb, &head, &mut buf).unwrap().tree();

    // A tree entry is a NAME, so these cannot be stored -- but a client checking
    // the tree out resolves them against the filesystem, which is a write
    // outside the worktree. They are refused rather than normalised.
    for path in ["../escape.txt", "a/../../escape.txt", ".git/config", "a//b.txt", "", "./x"] {
        let mut changes = BTreeMap::new();
        changes.insert(path.to_string(), Change::Upsert { content: b"x".to_vec(), executable: None });
        let mut staging = Staging::default();
        assert!(
            apply_changes(&odb, Some(base), &changes, &mut staging).is_err(),
            "{path:?} must be refused",
        );
    }
}

/// Editing a script must not quietly stop it being a script. The mode lives on
/// the tree entry, not in the bytes, so an edit that rebuilds the entry has to
/// carry it across -- and the path is nested, which is where reading the mode
/// from the tree editor rather than the base tree silently returned "not
/// executable" for everything.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_keeps_the_mode_the_file_already_had() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    use rustic_git::objects::{apply_changes, Change, Staging};
    use std::collections::BTreeMap;

    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "modes", |c| {
        std::fs::create_dir(c.join("bin")).unwrap();
        std::fs::write(c.join("bin/run.sh"), "#!/bin/sh\necho one\n").unwrap();
        std::fs::write(c.join("plain.txt"), "text\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["update-index", "--chmod=+x", "bin/run.sh"]);
        common::git(c, &["commit", "-qm", "one"]);
    })
    .await;

    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let odb = repo.odb().unwrap();
    let mut buf = Vec::new();
    let base = gix_object::FindExt::find_commit(&odb, &head, &mut buf).unwrap().tree();

    let mode_of = |odb: &gix_odb::Handle, tree: gix_hash::ObjectId, path: &str| -> u16 {
        let mut cur = (tree, gix_object::tree::EntryKind::Tree);
        for seg in path.split('/') {
            let mut b = Vec::new();
            let t = gix_object::FindExt::find_tree(odb, &cur.0, &mut b).unwrap();
            let e = t.entries.iter().find(|e| e.filename == seg.as_bytes()).unwrap();
            cur = (e.oid.to_owned(), e.mode.kind());
        }
        cur.1 as u16
    };
    assert_eq!(mode_of(&odb, base, "bin/run.sh"), 0o100755, "the fixture's script is executable");

    let mut changes = BTreeMap::new();
    changes.insert("bin/run.sh".to_string(), Change::Upsert { content: b"#!/bin/sh\necho two\n".to_vec(), executable: None });
    changes.insert("plain.txt".to_string(), Change::Upsert { content: b"edited\n".to_vec(), executable: None });
    let mut staging = Staging::default();
    let tree = apply_changes(&odb, Some(base), &changes, &mut staging).unwrap();
    // Staged objects are not in the odb until they are written, and the new trees
    // are what is being asserted on.
    staging.write(&e.store, &repo).await.unwrap();
    let repo = e.store.open_repo("alice", "modes").await.unwrap().unwrap();
    let odb = repo.odb().unwrap();

    assert_eq!(mode_of(&odb, tree, "bin/run.sh"), 0o100755, "an edited script is still executable");
    assert_eq!(mode_of(&odb, tree, "plain.txt"), 0o100644, "a plain file stays plain");

    // And it can be set deliberately, both ways.
    let mut changes = BTreeMap::new();
    changes.insert("plain.txt".to_string(), Change::Upsert { content: b"now a script\n".to_vec(), executable: Some(true) });
    changes.insert("bin/run.sh".to_string(), Change::Upsert { content: b"no longer\n".to_vec(), executable: Some(false) });
    let mut staging = Staging::default();
    let tree = apply_changes(&odb, Some(base), &changes, &mut staging).unwrap();
    staging.write(&e.store, &repo).await.unwrap();
    let repo2 = e.store.open_repo("alice", "modes").await.unwrap().unwrap();
    let odb = repo2.odb().unwrap();
    assert_eq!(mode_of(&odb, tree, "plain.txt"), 0o100755, "asked for executable");
    assert_eq!(mode_of(&odb, tree, "bin/run.sh"), 0o100644, "asked for not executable");
}

#[tokio::test]
async fn repo_meta_round_trips() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("alice", "web").await.unwrap();
    s.set_repo_meta("alice", "web", "a site", "alice", 1_700_000_000_000)
        .await
        .unwrap();
    s.set_public("alice", "web", true).await.unwrap();
    let m = s.repo_meta("alice", "web").await.unwrap().unwrap();
    assert_eq!(m.description, "a site");
    assert_eq!(m.created_by, "alice");
    assert_eq!(m.created_at_ms, 1_700_000_000_000);
    assert!(m.public);
}

#[tokio::test]
async fn repo_meta_is_none_until_written() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("alice", "web").await.unwrap();
    assert!(s.repo_meta("alice", "web").await.unwrap().is_none());
}

/// The `created_at` sentinel decides, not the visibility flag: `meta/public` predates this
/// namespace, so a repo carrying only that has still never had its metadata written.
#[tokio::test]
async fn repo_meta_ignores_the_public_flag() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("alice", "web").await.unwrap();
    s.set_public("alice", "web", true).await.unwrap();
    assert!(s.repo_meta("alice", "web").await.unwrap().is_none());
}

#[tokio::test]
async fn setting_the_description_keeps_the_rest() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("alice", "web").await.unwrap();
    s.set_repo_meta("alice", "web", "old", "alice", 42).await.unwrap();
    s.set_repo_description("alice", "web", "new").await.unwrap();
    let m = s.repo_meta("alice", "web").await.unwrap().unwrap();
    assert_eq!(m.description, "new");
    assert_eq!(m.created_by, "alice");
    assert_eq!(m.created_at_ms, 42);
}

/// A corrupt index row must not mean "size 0, re-download on every open" — it means the index
/// is untrustworthy, so the listing fallback runs and repairs it.
#[tokio::test]
async fn a_corrupt_pack_index_row_falls_back_to_the_listing() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let p = repo.pack_dir.join("pack-abc.pack");
    let i = repo.pack_dir.join("pack-abc.idx");
    std::fs::write(&p, b"PACKDATA").unwrap();
    std::fs::write(&i, b"IDX").unwrap();
    s.upload_pack_files(&repo, &p, &i).await.unwrap();
    s.db_for("a", "r").await.unwrap().put(b"pack/a/r/pack-abc.pack", b"junk").await.unwrap();

    let files = s.pack_index("a", "r").await.unwrap();
    let pack = files.iter().find(|(f, _)| f == "pack-abc.pack").unwrap();
    assert_eq!(pack.1, 8, "size came from the listing, not the corrupt row: {files:?}");
    let repaired = s.db_for("a", "r").await.unwrap().get(b"pack/a/r/pack-abc.pack").await.unwrap().unwrap();
    assert_eq!(&repaired[..], b"8", "and the row was rewritten");
}

/// `matches` only understands a trailing `*`; a pattern with one anywhere else would be stored,
/// match nothing, and leave its author believing the branch is protected.
#[tokio::test]
async fn a_non_trailing_star_is_refused() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("alice", "web").await.unwrap();
    let p = |pattern: &str| rustic_git::refs::Protection { pattern: pattern.into(), no_force: true, no_delete: true };
    assert!(s.set_protection("alice", "web", &p("rel*ease")).await.is_err());
    assert!(s.set_protection("alice", "web", &p("*/main")).await.is_err());
    assert!(s.set_protection("alice", "web", &p("release/*")).await.is_ok());
    assert!(s.set_protection("alice", "web", &p("main")).await.is_ok());
}

/// After a repo is repacked elsewhere and comes back, the superseded packs are still in this
/// node's cache — servable by gix-odb and never reclaimed. `open_repo` must drop what the index
/// no longer names, but not a fresh pack a push may still be uploading.
#[tokio::test]
async fn open_repo_prunes_packs_the_index_no_longer_names() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
    // The two temp shapes are here too: a killed process leaves them and nothing else sweeps them.
    for f in [
        "pack-stale.pack",
        "pack-stale.idx",
        "pack-fresh.pack",
        "pack-fresh.idx",
        ".pack-x.pack.99.0.tmp",
        "incoming-99-0.pack",
        ".pack-y.pack.99.1.tmp",
        "incoming-99-1.pack",
    ] {
        let p = repo.pack_dir.join(f);
        std::fs::write(&p, b"x").unwrap();
        if f.contains("stale") || f.ends_with("0.tmp") || f == "incoming-99-0.pack" {
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(old).unwrap();
        }
    }
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(!repo.pack_dir.join("pack-stale.pack").exists(), "stale pack pruned");
    assert!(!repo.pack_dir.join("pack-stale.idx").exists(), "stale idx pruned");
    assert!(repo.pack_dir.join("pack-fresh.pack").exists(), "a pack a push may still be uploading is kept");
    assert!(repo.pack_dir.join("pack-fresh.idx").exists());
    assert!(!repo.pack_dir.join(".pack-x.pack.99.0.tmp").exists(), "an abandoned download temp is reclaimed");
    assert!(!repo.pack_dir.join("incoming-99-0.pack").exists(), "an abandoned index temp is reclaimed");
    assert!(repo.pack_dir.join(".pack-y.pack.99.1.tmp").exists(), "a temp a live download is writing is kept");
    assert!(repo.pack_dir.join("incoming-99-1.pack").exists(), "a temp a live merge is indexing is kept");
}
