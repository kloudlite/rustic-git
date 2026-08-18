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

/// A lapsed clock must not take a repo from the node still holding it. The leader is the only node
/// that can renew, so its own downtime is precisely when leases lapse innocently — declining here
/// closes a database that is serving fine.
#[test]
fn renew_of_a_lapsed_entry_by_the_holder_extends_it() {
    let cur = entry("rustic-git-1", 1_000);
    let renewed = decide_renew(Some(&cur), "rustic-git-1", 2_000).unwrap();
    assert_eq!(renewed.node, "rustic-git-1");
    assert_eq!(renewed.expires_ms, 2_000 + LEASE_TTL.as_millis() as u64);
}

/// The prune loop may have reaped the entry while the leader was away; the holder still holds the
/// database, so the lease follows the handle back.
#[test]
fn renew_of_a_pruned_entry_regrants_it_to_the_holder() {
    let renewed = decide_renew(None, "rustic-git-1", 2_000).unwrap();
    assert_eq!(renewed.node, "rustic-git-1");
}

/// Safety is unchanged: once the map names somebody else, the asker has genuinely lost it and must
/// close — expired or not.
#[test]
fn renew_is_declined_once_the_map_names_another_node() {
    let cur = entry("rustic-git-2", 1_000);
    assert!(decide_renew(Some(&cur), "rustic-git-1", 2_000).is_none());
    let live = entry("rustic-git-2", 9_000);
    assert!(decide_renew(Some(&live), "rustic-git-1", 2_000).is_none());
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
    assert_eq!(least_loaded(&s, &held, &[], now), Some("rustic-git-2".to_string()));
}

#[test]
fn least_loaded_skips_a_draining_node_even_though_it_looks_emptiest() {
    let now = 1_000;
    let held = vec![(
        "a/1".to_string(),
        Entry { node: "rustic-git-2".into(), expires_ms: now + 5_000 },
    )];
    let s = servers("rustic-git-0", 3);
    // rustic-git-1 holds nothing — it just released everything on its way out.
    assert_eq!(
        least_loaded(&s, &held, &["rustic-git-1".to_string()], now),
        Some("rustic-git-2".to_string()),
        "an emptied, departing node must not be preferred"
    );
    // With everyone draining, naming someone still beats naming nobody.
    assert!(least_loaded(&s, &held, &s, now).is_some());
}

// ---- forced claims: the asker could not reach the holder ----

/// Catches: a forced claim refusing an unheld repo, which would make recovery useless in the very
/// case it exists for (the entry was pruned while the owner was gone).
#[test]
fn force_claim_on_absent_entry_grants() {
    match decide_force_claim(None, "rustic-git-2", 10_000) {
        Grant::Granted(e) => assert_eq!(e.node, "rustic-git-2"),
        g => panic!("absent entry must grant: {g:?}"),
    }
}

/// The whole point: an entry that is still LIVE on the clock but whose holder cannot be reached is
/// taken over now, not in ten seconds. Catches a forced claim that still honours the lease.
#[test]
fn force_claim_on_a_live_but_unreachable_holder_grants() {
    // Written at 1_000 (expiry 11_000), so it is live at 5_000 and well past FORCE_MIN_AGE.
    let cur = entry("rustic-git-1", 1_000 + LEASE_TTL.as_millis() as u64);
    match decide_force_claim(Some(&cur), "rustic-git-2", 5_000) {
        Grant::Granted(e) => assert_eq!(e.node, "rustic-git-2"),
        g => panic!("a live entry whose holder is unreachable must be forced over: {g:?}"),
    }
}

/// An expired entry is granted with or without force. Catches a forced path that got stricter than
/// the ordinary one.
#[test]
fn force_claim_on_a_stale_entry_grants() {
    let cur = entry("rustic-git-1", 1_000);
    match decide_force_claim(Some(&cur), "rustic-git-2", 20_000) {
        Grant::Granted(e) => assert_eq!(e.node, "rustic-git-2"),
        g => panic!("expired entry must grant: {g:?}"),
    }
}

/// Catches an anti-flap rule that fires on the asker's own entry — a node re-forcing what it
/// already holds must stay idempotent, not be told it lost its own repo.
#[test]
fn force_claim_by_the_current_holder_grants() {
    let cur = entry("rustic-git-1", 10_500);
    match decide_force_claim(Some(&cur), "rustic-git-1", 10_000) {
        Grant::Granted(e) => {
            assert_eq!(e.node, "rustic-git-1");
            assert_eq!(e.expires_ms, 10_000 + LEASE_TTL.as_millis() as u64);
        }
        g => panic!("re-claim by the holder must be idempotent: {g:?}"),
    }
}

/// Anti-flap. Catches the ping-pong: two nodes recovering from the same dead owner arrive a few
/// hundred milliseconds apart, and without this the second takes the repo straight off the first.
#[test]
fn force_claim_refuses_an_entry_written_moments_ago() {
    // Written at 10_000 by node 3; node 2 asks 500ms later.
    let cur = entry("rustic-git-3", 10_000 + LEASE_TTL.as_millis() as u64);
    match decide_force_claim(Some(&cur), "rustic-git-2", 10_500) {
        Grant::HeldBy(e) => assert_eq!(e, cur, "must name the winner so the caller forwards there"),
        g => panic!("a just-granted entry must not be forced over: {g:?}"),
    }
    // And exactly at the threshold it is fair game again.
    let now = 10_000 + FORCE_MIN_AGE.as_millis() as u64;
    match decide_force_claim(Some(&cur), "rustic-git-2", now) {
        Grant::Granted(e) => assert_eq!(e.node, "rustic-git-2"),
        g => panic!("past FORCE_MIN_AGE a forced claim must grant: {g:?}"),
    }
}
