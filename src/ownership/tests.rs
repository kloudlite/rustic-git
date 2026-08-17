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

/// The most important test in the task: release is not a delete. The released entry still
/// expires in the future, so a claim during the drain window must be told who holds it, not
/// granted — otherwise a node that gave up ownership before closing its database gets fenced
/// by whoever raced in during the drain.
#[test]
fn release_by_holder_yields_a_still_valid_entry_and_a_claim_during_drain_is_held_by() {
    let cur = entry("rustic-git-1", 50_000);
    let released = decide_release(Some(&cur), "rustic-git-1", 1_000).unwrap();
    assert_eq!(released.node, "rustic-git-1");
    assert_eq!(released.expires_ms, 1_000 + DRAIN.as_millis() as u64);
    assert!(!is_expired(&released, 1_000));

    match decide_claim(Some(&released), "rustic-git-2", 1_000) {
        Grant::HeldBy(e) => assert_eq!(e, released),
        Grant::Granted(_) => panic!("a claim during the drain must not be granted"),
    }
}

#[test]
fn release_by_non_holder_returns_none() {
    let cur = entry("rustic-git-1", 5_000);
    assert!(decide_release(Some(&cur), "rustic-git-2", 1_000).is_none());
}
