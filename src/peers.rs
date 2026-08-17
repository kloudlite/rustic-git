//! Which node owns a repo.
//!
//! Rendezvous hashing: every node scores each peer for a repo and takes the highest. The point is
//! agreement without coordination — no lookup on the request path, nothing to renew, and no state
//! that can be stale between nodes. Changing the peer set moves only the repos whose top scorer
//! changed, about 1/N of them, where `hash % N` would reshuffle nearly all of them.

/// How many candidates deep to look before giving up. Kubernetes already drops unready pods from
/// the peer set, so this covers the narrower case of a peer that is ready but unreachable.
pub const CANDIDATES: usize = 3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Score one (repo, peer) pair.
///
/// Repo and peer are hashed independently and then mixed. Hashing their concatenation with FNV-1a
/// alone is biased: FNV's final xor-multiply lets the LAST byte — the peer name's suffix digit —
/// dominate the top bits, and rendezvous over that gives one peer half of every repo (measured:
/// [7501, 7499, 15000] over 30 000 repos at n=3). Mixing two independent hashes through a
/// murmur3-style finalizer spreads every byte of both inputs across every output bit.
fn score(repo: &str, peer: &str) -> u64 {
    let a = fnv1a(repo.as_bytes());
    let b = fnv1a(peer.as_bytes());
    let mut h = a ^ b.rotate_left(32);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

/// Every peer, best first. `peers` are stable pod names (`rustic-git-0`), never IPs — a restarted
/// pod must come back owning the same repos. Ties break on the name so the order cannot depend on
/// how DNS happened to order its answer — two nodes resolving the same Service must rank
/// identically.
pub fn rank(repo: &str, peers: &[String]) -> Vec<String> {
    let mut scored: Vec<(u64, &String)> = peers.iter().map(|p| (score(repo, p), p)).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(a.1)));
    scored.into_iter().map(|(_, p)| p.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Peers are identified by stable pod name, never by IP: a StatefulSet pod keeps its name across
    /// restarts but not its IP, and hashing on IP would make every restart a new peer.
    fn peers(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("rustic-git-{i}")).collect()
    }

    /// The property everything rests on: the same inputs give the same answer, so two nodes
    /// never disagree about who owns a repo.
    #[test]
    fn ranking_is_deterministic() {
        let p = peers(5);
        assert_eq!(rank("alice/web", &p), rank("alice/web", &p));
    }

    /// Order of the peer list must not matter: DNS returns records in arbitrary order, and two
    /// nodes resolving the same Service must still agree.
    #[test]
    fn ranking_ignores_peer_list_order() {
        let p = peers(5);
        let mut shuffled = p.clone();
        shuffled.reverse();
        assert_eq!(rank("alice/web", &p), rank("alice/web", &shuffled));
    }

    /// Every peer appears exactly once, so failover always has somewhere to go.
    #[test]
    fn ranking_is_a_permutation() {
        let p = peers(5);
        let mut ranked = rank("alice/web", &p);
        ranked.sort();
        let mut expected = p.clone();
        expected.sort();
        assert_eq!(ranked, expected);
    }

    /// Repos must not all pile onto one peer.
    #[test]
    fn ranking_spreads_repos() {
        let p = peers(3);
        let mut counts = std::collections::HashMap::new();
        for i in 0..300 {
            *counts.entry(rank(&format!("o/r{i}"), &p)[0].clone()).or_insert(0) += 1;
        }
        assert_eq!(counts.len(), 3, "every peer should own some repos");
        for (peer, n) in &counts {
            assert!((50..150).contains(n), "{peer} owns {n} of 300, expected roughly 100");
        }
    }

    /// Removing a peer must move only that peer's repos. This is why rendezvous hashing is used
    /// instead of a modulo: scaling costs one cold open per moved repo, and we move as few as
    /// possible.
    #[test]
    fn removing_a_peer_moves_only_its_repos() {
        let five = peers(5);
        let four: Vec<String> = five.iter().filter(|p| **p != five[2]).cloned().collect();
        let (mut moved, mut kept) = (0, 0);
        for i in 0..500 {
            let repo = format!("o/r{i}");
            let before = rank(&repo, &five)[0].clone();
            let after = rank(&repo, &four)[0].clone();
            if before == five[2] {
                moved += 1;
            } else {
                assert_eq!(before, after, "{repo} moved but its owner was still present");
                kept += 1;
            }
        }
        assert!(moved > 50, "expected roughly 100 repos on the removed peer, got {moved}");
        assert!(kept > 350);
    }

    /// The property failover depends on: the second choice with all peers present is the first
    /// choice once the winner is gone. Without this, failing over would send a repo somewhere the
    /// rest of the fleet does not consider its owner.
    #[test]
    fn second_candidate_is_the_next_owner() {
        let five = peers(5);
        for i in 0..200 {
            let repo = format!("o/r{i}");
            let ranked = rank(&repo, &five);
            let without: Vec<String> = five.iter().filter(|p| **p != ranked[0]).cloned().collect();
            assert_eq!(rank(&repo, &without)[0], ranked[1]);
        }
    }
}
