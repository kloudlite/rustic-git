//! The write paths must invalidate what they invalidate. A disabled cache cannot observe a call,
//! so every test here runs against `Cache::memory()` and asserts on the entries themselves.
mod common;

const TTL: u64 = 60;

/// Catches: `update_refs` not dropping the ref entry — a push would keep serving the old ref list
/// for the whole TTL.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_drops_the_ref_entry() {
    if !common::have_git() {
        return;
    }
    let e = common::env_cached().await;
    e.store.cache.put("alice/web", "refs", b"stale", TTL).await;
    common::push_fixture(&e, "alice", "web").await;
    assert_eq!(e.store.cache.get("alice/web", "refs").await, None);
}

/// Catches: a push dropping more than the ref list. Object-keyed answers are immutable and must
/// survive, or every push cold-starts the cache.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_keeps_the_object_keyed_answers() {
    if !common::have_git() {
        return;
    }
    let e = common::env_cached().await;
    e.store.cache.put("alice/web", "tree:abc:src", b"kept", TTL).await;
    common::push_fixture(&e, "alice", "web").await;
    assert_eq!(e.store.cache.get("alice/web", "tree:abc:src").await.as_deref(), Some(&b"kept"[..]));
}

/// Catches: `set_public` not bumping the generation — the load-bearing one. A repo flipped to
/// private would keep serving every cached answer, including its cached visibility flag.
#[tokio::test(flavor = "multi_thread")]
async fn a_visibility_flip_orphans_every_entry() {
    let e = common::env_cached().await;
    e.store.create_repo("alice", "web").await.unwrap();
    e.store.set_public("alice", "web", true).await.unwrap();
    e.store.cache.put("alice/web", "refs", b"stale", TTL).await;
    e.store.cache.put("alice/web", rustic_git_api::META, b"1", TTL).await;
    e.store.cache.put("alice/web", "tree:abc:src", b"stale", TTL).await;

    e.store.set_public("alice", "web", false).await.unwrap();

    assert_eq!(e.store.cache.get("alice/web", "refs").await, None);
    assert_eq!(e.store.cache.get("alice/web", rustic_git_api::META).await, None);
    assert_eq!(e.store.cache.get("alice/web", "tree:abc:src").await, None);
    assert!(!e.store.is_public("alice", "web").await.unwrap());
}

/// Catches: `delete_repo` not bumping — a recreated name would inherit the dead repo's answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_orphans_every_entry() {
    let e = common::env_cached().await;
    e.store.create_repo("alice", "web").await.unwrap();
    e.store.cache.put("alice/web", "tree:abc:src", b"stale", TTL).await;
    e.store.delete_repo("alice", "web").await.unwrap();
    assert_eq!(e.store.cache.get("alice/web", "tree:abc:src").await, None);
    // Another repo's entries are untouched: the generation is per repo.
    e.store.cache.put("bob/web", "tree:abc:src", b"kept", TTL).await;
    assert!(e.store.cache.get("bob/web", "tree:abc:src").await.is_some());
}

/// Catches: a missing `admin purge-cache` arm, which falls through to the usage error.
#[test]
fn purge_cache_is_a_command() {
    let out = std::process::Command::new(common::bin_path("rustic-git"))
        .args(["admin", "purge-cache", "alice/web"])
        .env("RUSTIC_GIT_S3_URL", "mem://")
        .env("RUSTIC_GIT_CACHE_DIR", tempfile::tempdir().unwrap().keep())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "purge-cache failed: {err}");
    assert!(!err.contains("usage:"), "purge-cache fell through to usage: {err}");
}
