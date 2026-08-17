//! Integration tests for the ownership store: a leader writes `cluster/ownership`, followers
//! read it via a `FollowLatest` reader.

use rustic_git::ownership::{Entry, OwnershipStore};
use slatedb::object_store::memory::InMemory;
use std::sync::Arc;
use std::time::Duration;

fn entry(node: &str, expires_ms: u64) -> Entry {
    Entry { node: node.to_string(), expires_ms }
}

#[tokio::test]
async fn leader_put_then_get() {
    let os = Arc::new(InMemory::new());
    let leader = OwnershipStore::open(os, true).await.unwrap();
    leader.put("alice/web", &entry("rustic-git-1", 1_000)).await.unwrap();
    let got = leader.get("alice/web").await.unwrap();
    assert_eq!(got, Some(entry("rustic-git-1", 1_000)));
}

#[tokio::test]
async fn follower_eventually_sees_leader_write() {
    let os = Arc::new(InMemory::new());
    let leader = OwnershipStore::open(os.clone(), true).await.unwrap();
    let follower = OwnershipStore::open(os, false).await.unwrap();

    leader.put("alice/web", &entry("rustic-git-1", 1_000)).await.unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut seen = None;
    while std::time::Instant::now() < deadline {
        seen = follower.get("alice/web").await.unwrap();
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(seen, Some(entry("rustic-git-1", 1_000)), "follower never saw the leader's write");
}

#[tokio::test]
async fn follower_put_errors() {
    let os = Arc::new(InMemory::new());
    let _leader = OwnershipStore::open(os.clone(), true).await.unwrap();
    let follower = OwnershipStore::open(os, false).await.unwrap();
    let res = follower.put("alice/web", &entry("rustic-git-1", 1_000)).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn all_returns_everything_written() {
    let os = Arc::new(InMemory::new());
    let leader = OwnershipStore::open(os, true).await.unwrap();
    leader.put("alice/web", &entry("rustic-git-1", 1_000)).await.unwrap();
    leader.put("bob/app", &entry("rustic-git-2", 2_000)).await.unwrap();

    let mut all = leader.all().await.unwrap();
    all.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        all,
        vec![
            ("alice/web".to_string(), entry("rustic-git-1", 1_000)),
            ("bob/app".to_string(), entry("rustic-git-2", 2_000)),
        ]
    );
}

/// The crashloop regression: only the leader creates `cluster/ownership`, and a StatefulSet rolls
/// in reverse ordinal order, so a follower routinely starts before the map exists. It must boot.
#[tokio::test]
async fn follower_opens_before_the_map_exists() {
    let os = Arc::new(InMemory::new());
    let follower = OwnershipStore::open(os, false).await.unwrap();
    assert_eq!(follower.get("alice/web").await.unwrap(), None);
    assert!(follower.all().await.unwrap().is_empty());
}

#[tokio::test]
async fn follower_opened_first_converges_once_the_leader_writes() {
    let os = Arc::new(InMemory::new());
    let follower = OwnershipStore::open(os.clone(), false).await.unwrap();
    assert_eq!(follower.get("alice/web").await.unwrap(), None);

    let leader = OwnershipStore::open(os, true).await.unwrap();
    leader.put("alice/web", &entry("rustic-git-1", 1_000)).await.unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut seen = None;
    while std::time::Instant::now() < deadline {
        seen = follower.get("alice/web").await.unwrap();
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(seen, Some(entry("rustic-git-1", 1_000)), "follower never converged");
}
