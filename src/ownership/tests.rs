use super::*;

fn entry(node: &str, expires_ms: u64) -> Entry {
    Entry { node: node.to_string(), expires_ms }
}

#[test]
fn leader_of_picks_ordinal_zero() {
    assert_eq!(leader_of("rustic-git-0").unwrap(), "rustic-git-0");
    assert_eq!(leader_of("rustic-git-2").unwrap(), "rustic-git-0");
    assert_eq!(leader_of("a-b-12").unwrap(), "a-b-0");
}

#[test]
fn leader_of_rejects_names_without_an_ordinal() {
    assert!(leader_of("nodash").is_err());
    assert!(leader_of("x-notanumber").is_err());
}

#[test]
fn claim_on_absent_entry_grants() {
    match decide_claim(None, "rustic-git-1", 1_000) {
        Grant::Granted(e) => {
            assert_eq!(e.node, "rustic-git-1");
            assert_eq!(e.expires_ms, 1_000 + LEASE_TTL.as_millis() as u64);
        }
        Grant::HeldBy(_) => panic!("absent entry must grant"),
    }
}

#[test]
fn claim_on_live_entry_held_by_someone_else_returns_held_by() {
    let cur = entry("rustic-git-1", 5_000);
    match decide_claim(Some(&cur), "rustic-git-2", 1_000) {
        Grant::HeldBy(e) => assert_eq!(e, cur),
        Grant::Granted(_) => panic!("live entry held by another node must not grant"),
    }
}

#[test]
fn claim_on_expired_entry_grants() {
    let cur = entry("rustic-git-1", 1_000);
    match decide_claim(Some(&cur), "rustic-git-2", 2_000) {
        Grant::Granted(e) => assert_eq!(e.node, "rustic-git-2"),
        Grant::HeldBy(_) => panic!("expired entry must grant"),
    }
}

#[test]
fn reclaim_by_current_holder_grants_and_extends() {
    let cur = entry("rustic-git-1", 5_000);
    match decide_claim(Some(&cur), "rustic-git-1", 4_000) {
        Grant::Granted(e) => {
            assert_eq!(e.node, "rustic-git-1");
            assert_eq!(e.expires_ms, 4_000 + LEASE_TTL.as_millis() as u64);
        }
        Grant::HeldBy(_) => panic!("re-claim by the current holder must be idempotent"),
    }
}

#[test]
fn renew_by_holder_extends() {
    let cur = entry("rustic-git-1", 5_000);
    let renewed = decide_renew(Some(&cur), "rustic-git-1", 4_000).unwrap();
    assert_eq!(renewed.node, "rustic-git-1");
    assert_eq!(renewed.expires_ms, 4_000 + LEASE_TTL.as_millis() as u64);
}

#[test]
fn renew_by_non_holder_returns_none() {
    let cur = entry("rustic-git-1", 5_000);
    assert!(decide_renew(Some(&cur), "rustic-git-2", 4_000).is_none());
}

#[test]
fn renew_of_expired_entry_returns_none() {
    let cur = entry("rustic-git-1", 1_000);
    assert!(decide_renew(Some(&cur), "rustic-git-1", 2_000).is_none());
}

/// Release is a plain delete, and it runs only after the database is closed — so the guard that
/// matters is not timing but identity: a node may only drop an entry that still names it. A stale
/// release from a node that already lost the repo must not delete the new owner's entry.
#[test]
fn only_the_holder_may_release() {
    let cur = entry("rustic-git-1", 50_000);
    assert!(may_release(Some(&cur), "rustic-git-1"));
    assert!(!may_release(Some(&cur), "rustic-git-2"), "a stale release must not delete the owner");
    assert!(!may_release(None, "rustic-git-1"));
}

/// Once released, the repo is claimable at once by anyone — there is no tombstone and no drain
/// left to wait out, because the releasing node closed its database before releasing.
#[test]
fn a_released_repo_is_claimable_immediately() {
    match decide_claim(None, "rustic-git-2", 1_000) {
        Grant::Granted(e) => assert_eq!(e.node, "rustic-git-2"),
        g => panic!("a released repo must be claimable at once: {g:?}"),
    }
}

#[test]
fn servers_exclude_the_leader() {
    assert_eq!(servers("rustic-git-0", 3), vec!["rustic-git-1", "rustic-git-2"]);
    // Below two replicas there is no one else, so the leader serves rather than nothing serving.
    assert_eq!(servers("rustic-git-0", 1), vec!["rustic-git-0"]);
}

#[test]
fn least_loaded_picks_the_emptiest_and_ignores_lapsed_entries() {
    let now = 1_000;
    let live = |n: &str| Entry { node: n.to_string(), expires_ms: now + 5_000 };
    let held = vec![
        ("a/1".to_string(), live("rustic-git-1")),
        ("a/2".to_string(), live("rustic-git-1")),
        ("a/3".to_string(), live("rustic-git-2")),
        // Lapsed: the node that left it is not holding anything.
        ("a/4".to_string(), Entry { node: "rustic-git-2".into(), expires_ms: now - 1 }),
    ];
    let s = servers("rustic-git-0", 3);
    assert_eq!(least_loaded(&s, &held, now), Some("rustic-git-2".to_string()));
}
