//! Which node owns a repo.
//!
//! Rendezvous hashing: every node scores each peer for a repo and takes the highest. The point is
//! agreement without coordination — no lookup on the request path, nothing to renew, and no state
//! that can be stale between nodes. Changing the peer set moves only the repos whose top scorer
//! changed, about 1/N of them, where `hash % N` would reshuffle nearly all of them.

/// How many candidates deep to look before giving up. The peer set is static, so this is what
/// covers every pod that is a member but not reachable right now.
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
/// how a list happened to be ordered — two nodes with the same membership must rank identically.
pub fn rank(repo: &str, peers: &[String]) -> Vec<String> {
    let mut scored: Vec<(u64, &String)> = peers.iter().map(|p| (score(repo, p), p)).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(a.1)));
    scored.into_iter().map(|(_, p)| p.clone()).collect()
}

use std::collections::HashMap;

/// A node in the fleet. `name` is the stable pod name and is the hash key; `addr` is where its
/// peer HTTP listener is right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub addr: String,
}

/// Where a request for a repo belongs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// This node serves it.
    Local,
    /// Another node serves it. Forward there.
    Peer(Peer),
    /// Nobody may safely serve it right now — 503, and let the client retry.
    Unavailable,
}

/// The peer set.
///
/// Membership is configuration, not a lookup. The names are the hash keys and they outlive every
/// restart; who is *up* is a separate question with a separate answer (`Forwarder::reachable`).
pub struct Membership {
    peers: Vec<Peer>,
    self_name: String,
}

impl Membership {
    /// The peer set of a StatefulSet is its identity, not a lookup: `replicas: N` behind a headless
    /// Service means the peers are `{app}-0 … {app}-{N-1}`, and those names outlive every restart,
    /// reschedule and IP change. Resolving them is what a connection does, when it connects.
    ///
    /// This used to poll the Service's A records. That answered "who are the peers?" with a cached
    /// view of "which pods are Ready", so a restarting pod left the set entirely and the fleet
    /// re-ranked its repos while it still held them — fencing it, and costing a burst of 503s on
    /// every roll. Liveness is `Forwarder::reachable`'s job; membership is config.
    pub fn statefulset(app: &str, replicas: u32, svc: &str, port: u16, self_name: String) -> Membership {
        let peers = (0..replicas)
            .map(|i| Peer { name: format!("{app}-{i}"), addr: format!("{app}-{i}.{svc}:{port}") })
            .collect();
        Membership { peers, self_name }
    }

    /// A fixed set: tests, and a single-node run.
    pub fn fixed(peers: Vec<Peer>, self_name: String) -> Membership {
        Membership { peers, self_name }
    }

    /// The peer set. Static, so this is a borrow — no lock, no await, nothing to go stale.
    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    /// Where this request goes.
    ///
    /// > A node may serve a repo only if every higher-ranked node is unreachable from two vantage
    /// > points: its own probe, and one other reachable peer's probe.
    ///
    /// Two phases, and the split matters:
    ///
    /// 1. **Forward up needs no vantage.** Probe every higher-ranked candidate (concurrently — the
    ///    slow case is a blackholed pod, and serial probes would stack timeouts). The first that
    ///    answers is where this goes. This is the hearsay defence: we never serve on another
    ///    node's word, we check ourselves.
    /// 2. **Serving needs a vantage on EVERY node above.** Only if *nothing* above answers do we
    ///    ask other peers — any peer that is not us and not the target, lower-ranked candidates
    ///    included — to probe each higher-ranked node on our behalf. Any hard "up" about any of
    ///    them means we are the one cut off: `Unavailable`, not a forward to an address we just
    ///    failed. A target nobody could vouch about is a split fleet: `Unavailable`. Only a soft
    ///    "down" for every target lets us serve. A vantage that is itself above us and answers at
    ///    all has proven it is reachable, and we forward to it instead.
    ///
    /// Why the vantage may be a lower-ranked candidate: in a three-node fleet the third node has
    /// nobody but the first two, and the first is the target. If the second may not vouch, the
    /// third can never serve and half the surviving traffic is 503 during an outage. Any node that
    /// is not us and not the target is an independent observer of the target.
    ///
    /// What two vantages do NOT catch, stated so nobody over-trusts this: correlated slowness. If
    /// the owner is alive but slow, both probes can time out for one cause, and the owner — as top
    /// candidate — never checks anyone. Probes are therefore generous and retried, positive
    /// answers cached briefly, and candidates spread across physical nodes.
    ///
    /// The top candidate has nothing above it and probes nobody — ordinary traffic pays nothing.
    /// A node outside the top `CANDIDATES` is never an owner: phase 1 only. A node whose own
    /// health check is failing returns `Unavailable` (see `App::route`): an unhealthy node must
    /// not hold repos that healthy peers are about to take.
    ///
    /// Worst case latency, so it is written down: phase 1 is one probe budget (probes run
    /// concurrently), phase 2 is one vantage round trip (all targets × all vias concurrently),
    /// itself one probe budget plus margin. About two probe budgets, ~9 s with the defaults, and
    /// only on the failover path. Concurrent requests for the same dead owner each pay it — see
    /// `Forwarder::reachable` single-flight for why that is not N× the probes.
    ///
    /// `probe(peer)` — can *I* reach it? `second_vantage(via, target)` — can `via` reach `target`?
    /// `None` when `via` could not be asked or does not know the target. Both are parameters so
    /// the rule is tested with scripted reachability and no network.
    pub async fn decide<P, PF, V, VF>(&self, repo: &str, probe: P, second_vantage: V) -> Route
    where
        P: Fn(&Peer) -> PF,
        PF: std::future::Future<Output = bool>,
        V: Fn(&Peer, &Peer) -> VF,
        VF: std::future::Future<Output = Option<bool>>,
    {
        let peers = self.peers();
        let by_name: HashMap<&str, &Peer> = peers.iter().map(|p| (p.name.as_str(), p)).collect();
        let names: Vec<String> = peers.iter().map(|p| p.name.clone()).collect();
        // Top three BY RANK. Filtering first would let ranks four and five become owners as soon
        // as one and two are down, and the fleet would stop agreeing on who the candidates are.
        let ranked: Vec<&Peer> = rank(repo, &names)
            .iter()
            .take(CANDIDATES)
            .filter_map(|n| by_name.get(n.as_str()).copied())
            .collect();
        let my_rank = ranked.iter().position(|p| p.name == self.self_name);
        let above: &[&Peer] = match my_rank {
            Some(r) => &ranked[..r],
            None => &ranked[..], // not a candidate: never Local
        };
        if above.is_empty() {
            return match my_rank { Some(_) => Route::Local, None => Route::Unavailable };
        }

        // Phase 1: probe everything above, concurrently, and forward to the best that answers.
        let results = futures::future::join_all(above.iter().map(|p| probe(p))).await;
        if let Some(i) = results.iter().position(|up| *up) {
            return Route::Peer(above[i].clone());
        }
        // Not a candidate and nothing above answers: we cannot serve, and there is nothing to
        // vouch for us serving. Unavailable.
        if my_rank.is_none() {
            return Route::Unavailable;
        }

        // Phase 2: nothing above answers from here. Before serving, EVERY node above must be
        // confirmed unreachable by some other peer — not just the top one. Vouching only for the
        // top leaves the second candidate unchecked: with A dead and B down-from-C (one cut link,
        // or B in a GC pause), C would serve on D's word about A alone while B also serves. Each
        // target gets its own vantage set: every peer that is not us and not that target.
        //
        // Evidence is asymmetric. Some(true) is a 200 from the target — hard proof it is alive —
        // and ANY vantage saying so vetoes serving. Some(false) is a timeout — soft — and serving
        // needs one for every target. And a vantage that is itself in `above` and answers at all
        // has just proven it is reachable: we forward to it rather than serve past it.
        let others: Vec<&Peer> = peers
            .iter()
            .filter(|p| p.name != self.self_name)
            .collect();
        // Borrow, so the `async move` blocks below capture `&V` (Copy) rather than moving `V`
        // out of an FnMut closure — which would not compile.
        let second_vantage = &second_vantage;
        // For each target above, ask every non-target peer, concurrently.
        let per_target = futures::future::join_all(above.iter().map(|target| {
            let vias: Vec<&Peer> = others.iter().copied().filter(|v| v.name != target.name).collect();
            async move {
                let answers = futures::future::join_all(vias.iter().map(|via| second_vantage(via, target))).await;
                // (via, answer) pairs, so a via in `above` that answered can be identified
                vias.into_iter().zip(answers).collect::<Vec<(&Peer, Option<bool>)>>()
            }
        }))
        .await;

        // Two flatten scans over |above| × |others| answers; bounded by CANDIDATES = 3, so
        // O(9·n) — do not generalise this loop without revisiting.
        // A via in `above` that answered anything is reachable from here after all (our phase-1
        // probe of it timed out; its vantage answer did not). Forward to the highest such.
        for target in above.iter() {
            let answered = per_target.iter().flatten().any(|(via, a)| via.name == target.name && a.is_some());
            if answered {
                return Route::Peer((*target).clone());
            }
        }
        // Any hard "up" about any target: we are the one cut off from it. Unavailable.
        if per_target.iter().flatten().any(|(_, a)| *a == Some(true)) {
            return Route::Unavailable;
        }
        // Every target must have at least one soft "down"; a target nobody could vouch about is a
        // split fleet as far as that target is concerned.
        let all_confirmed = per_target.iter().all(|answers| answers.iter().any(|(_, a)| *a == Some(false)));
        if all_confirmed {
            Route::Local
        } else {
            Route::Unavailable
        }
    }
}

#[cfg(test)]
mod tests;
