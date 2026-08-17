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

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
/// Membership comes from the headless Service's SRV records rather than configuration. A peer list
/// baked into the environment guarantees the nodes disagree for the length of a rolling restart,
/// and disagreement is what costs requests: two nodes that both think they own a repo fence each
/// other in turn. Resolving DNS bounds the disagreement to `ttl` instead.
///
/// SRV, not A: SRV gives the stable pod name alongside the port, and the name is the hash key. A
/// pod keeps its name across restarts but not its IP, so hashing on IP would make every restart a
/// new peer and move nearly every repo twice per roll.
pub struct Membership {
    /// SRV name to resolve, e.g. `_peer._tcp.rustic-git.rustic-git.svc.cluster.local`. Empty
    /// when the set is fixed.
    srv: String,
    self_name: String,
    cache: Mutex<Option<(Instant, Vec<Peer>)>>,
    /// How long a resolved set is reused. This bounds how long two nodes can disagree, so it is
    /// short; the cost is one DNS query per node per interval.
    pub ttl: Duration,
}

impl Membership {
    pub fn new(srv: String, self_name: String) -> Membership {
        Membership {
            srv,
            self_name,
            cache: Mutex::new(None),
            ttl: Duration::from_secs(2),
        }
    }

    /// A fixed set that is never resolved: tests, and a single-node run.
    pub fn fixed(peers: Vec<Peer>, self_name: String) -> Membership {
        let m = Membership::new(String::new(), self_name);
        *m.cache.lock().unwrap() = Some((Instant::now(), peers));
        m
    }

    /// The current set, resolved at most every `ttl`. A failed resolve keeps the previous answer
    /// for up to `stale_max`: DNS being briefly unavailable is not a reason to decide the fleet
    /// has no members — but a node routing on a frozen view forever is a node that disagrees with
    /// everyone else forever, so past `stale_max` the set is empty and `decide` returns
    /// `Unavailable` until DNS answers again.
    ///
    /// Self is in the set exactly when DNS says so. An unready pod is absent from DNS, gets no
    /// traffic from anyone, and has nothing to route — adding itself early would only create a
    /// window where it serves repos every other node still routes to the old owner.
    ///
    /// Empty answers are logged: on a cluster whose reverse DNS zone is missing, every lookup
    /// yields no name, the set is empty, and every request is 503 — that must be loud.
    pub async fn peers(&self) -> Vec<Peer> {
        if let Some((at, peers)) = self.cache.lock().unwrap().as_ref() {
            if self.srv.is_empty() || at.elapsed() < self.ttl {
                return peers.clone();
            }
        }
        match resolve_srv(&self.srv).await {
            Ok(peers) if !peers.is_empty() => {
                *self.cache.lock().unwrap() = Some((Instant::now(), peers.clone()));
                peers
            }
            Ok(_) => {
                eprintln!("resolving {}: no peers found (reverse DNS missing?); keeping last answer", self.srv); // ponytail: eprintln
                self.cached()
            }
            Err(e) => {
                eprintln!("resolving {}: {e}; keeping last answer", self.srv); // ponytail: eprintln; swap for a logger when one exists
                self.cached()
            }
        }
    }

    /// How long a stale answer is trusted after DNS stops answering.
    pub const STALE_MAX: Duration = Duration::from_secs(30);

    fn cached(&self) -> Vec<Peer> {
        match self.cache.lock().unwrap().as_ref() {
            Some((at, p)) if at.elapsed() < Self::STALE_MAX => p.clone(),
            _ => Vec::new(),
        }
    }

    /// Whether this node appears in its own resolved set. Used at startup: a node must not become
    /// ready until it can see itself in DNS, or reverse DNS returning IP-derived names (which
    /// never match a pod name) would silently make every request take two hops and every repo
    /// forward away from its owner.
    pub async fn sees_self(&self) -> bool {
        self.peers().await.iter().any(|p| p.name == self.self_name)
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
        let peers = self.peers().await;
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

/// Resolve SRV records to (name, ip:port). Each SRV target is a pod's stable DNS name; its first
/// label is the pod name.
async fn resolve_srv(srv: &str) -> crate::Result<Vec<Peer>> {
    // ponytail: tokio has no SRV resolver; shell out to the system resolver via `lookup_host` on
    // each pod's A record after listing targets from a plain DNS query. Use the `hickory-resolver`
    // crate if this ever needs to be robust; for now the headless Service's A records plus reverse
    // lookup of each IP to its pod name is enough and needs no new dependency.
    let mut out = Vec::new();
    // The headless Service publishes one A record per ready pod. Reverse-resolving each gives
    // `<pod>.<svc>.<ns>.svc.cluster.local`, whose first label is the stable pod name — CoreDNS
    // publishes that PTR for endpoints that carry a hostname, which StatefulSet pods do. If a
    // cluster lacks the reverse zone the lookup fails and the peer is dropped; `peers()` logs the
    // empty result and `Membership::sees_self` blocks readiness at startup, so the failure is loud
    // rather than a silent 503 everywhere. Not real SRV: see the ponytail note above.
    let (svc, port) = srv
        .strip_prefix("_peer._tcp.")
        .and_then(|rest| rest.rsplit_once(':'))
        .ok_or_else(|| crate::err("srv must look like _peer._tcp.<svc>.<ns>.svc.cluster.local:<port>"))?;
    // Trailing dot: fully qualified, so the resolver does not walk ndots search domains first.
    let fq = if svc.ends_with('.') { svc.to_string() } else { format!("{svc}.") };
    for addr in tokio::net::lookup_host(format!("{fq}:{port}")).await? {
        let ip = addr.ip();
        // Reverse lookup via getnameinfo through std; blocking, but tiny and cached by the TTL.
        let name = tokio::task::spawn_blocking(move || {
            dns_lookup::lookup_addr(&ip).ok()
        })
        .await?
        .and_then(|fqdn| fqdn.split('.').next().map(str::to_string));
        if let Some(name) = name {
            out.push(Peer { name, addr: addr.to_string() });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    Ok(out)
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

    fn peer(name: &str) -> Peer {
        Peer { name: name.into(), addr: format!("10.244.0.{}:8081", name.len()) }
    }
    fn fleet(n: usize) -> Vec<Peer> {
        (0..n).map(|i| peer(&format!("rustic-git-{i}"))).collect()
    }
    fn names(f: &[Peer]) -> Vec<String> {
        f.iter().map(|p| p.name.clone()).collect()
    }
    /// A repo for which `me` holds rank `want` in this fleet, so each test says which position it
    /// is exercising.
    fn repo_where_i_rank(f: &[Peer], me: &str, want: usize) -> String {
        let n = names(f);
        (0..2000)
            .map(|i| format!("o/r{i}"))
            .find(|r| rank(r, &n).iter().position(|x| x == me) == Some(want))
            .expect("some repo ranks me at that position")
    }
    /// Scripted reachability: `up` is the set of names that answer probes.
    fn probe_where(up: &'static [&'static str]) -> impl Fn(&Peer) -> std::future::Ready<bool> {
        move |p: &Peer| std::future::ready(up.contains(&p.name.as_str()))
    }
    /// A second vantage that always agrees with the local probe (the honest case).
    fn vantage_agreeing(up: &'static [&'static str]) -> impl Fn(&Peer, &Peer) -> std::future::Ready<Option<bool>> {
        move |via: &Peer, target: &Peer| {
            std::future::ready(if up.contains(&via.name.as_str()) {
                Some(up.contains(&target.name.as_str()))
            } else {
                None // could not ask via
            })
        }
    }

    /// The ordinary path: the top candidate serves at once and probes nothing.
    #[tokio::test]
    async fn the_top_candidate_serves_without_probing() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-0", 0);
        let m = Membership::fixed(f, "rustic-git-0".into());
        let probed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pr = probed.clone();
        let route = m
            .decide(&repo, |_: &Peer| { pr.fetch_add(1, std::sync::atomic::Ordering::Relaxed); std::future::ready(true) },
                    |_: &Peer, _: &Peer| std::future::ready(Some(true)))
            .await;
        assert_eq!(route, Route::Local);
        assert_eq!(probed.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// Hearsay: second-ranked, first is reachable → forward up, do not serve. This is what stops a
    /// node taking a repo because some other node could not reach the owner.
    #[tokio::test]
    async fn second_forwards_up_when_first_answers() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let n = names(&f);
        let first = rank(&repo, &n)[0].clone();
        let m = Membership::fixed(f.clone(), "rustic-git-1".into());
        let up: &'static [&str] = &["rustic-git-0", "rustic-git-1", "rustic-git-2"];
        let route = m.decide(&repo, probe_where(up), vantage_agreeing(up)).await;
        assert_eq!(route, Route::Peer(f.iter().find(|p| p.name == first).unwrap().clone()));
    }

    /// Genuine outage: first is down from here AND from the second vantage → serve.
    #[tokio::test]
    async fn second_serves_when_first_is_down_from_two_vantages() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let n = names(&f);
        let first = rank(&repo, &n)[0].clone();
        // everyone except `first` is up; the third node is our second vantage
        let up: &'static [&str] = if first == "rustic-git-0" { &["rustic-git-1", "rustic-git-2"] } else { &["rustic-git-0", "rustic-git-1"] };
        let m = Membership::fixed(f, "rustic-git-1".into());
        assert_eq!(m.decide(&repo, probe_where(up), vantage_agreeing(up)).await, Route::Local);
    }

    /// One-sided partition: first is down FROM HERE but the second vantage can reach it → we are
    /// the one cut off. Do not serve, and do not forward to an address we just failed to reach:
    /// Unavailable, and the client retries (round robin lands it elsewhere).
    #[tokio::test]
    async fn second_returns_unavailable_when_a_second_vantage_reaches_the_first() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let n = names(&f);
        let first = rank(&repo, &n)[0].clone();
        let m = Membership::fixed(f.clone(), "rustic-git-1".into());
        let fc = first.clone();
        let route = m
            .decide(&repo,
                    move |p: &Peer| std::future::ready(p.name != fc),
                    |_: &Peer, _: &Peer| std::future::ready(Some(true)))
            .await;
        assert_eq!(route, Route::Unavailable, "they can reach it, we cannot: we are cut off");
    }

    /// The ordinary path must not probe non-candidates. With a 5-node fleet there are two
    /// non-candidates for every repo; a top-ranked node must still probe nobody.
    #[tokio::test]
    async fn the_top_candidate_probes_nobody_even_with_non_candidates_present() {
        let f = fleet(5);
        let repo = repo_where_i_rank(&f, "rustic-git-0", 0);
        let m = Membership::fixed(f, "rustic-git-0".into());
        let probed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pr = probed.clone();
        let route = m
            .decide(&repo, |_: &Peer| { pr.fetch_add(1, std::sync::atomic::Ordering::Relaxed); std::future::ready(true) },
                    |_: &Peer, _: &Peer| std::future::ready(Some(true)))
            .await;
        assert_eq!(route, Route::Local);
        assert_eq!(probed.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// Second-vantage peers are found lazily: a second-ranked node whose first IS reachable must
    /// probe exactly one peer (the first), not every non-candidate in the fleet.
    #[tokio::test]
    async fn a_reachable_first_costs_exactly_one_probe() {
        let f = fleet(6);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let m = Membership::fixed(f, "rustic-git-1".into());
        let probed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pr = probed.clone();
        let _ = m
            .decide(&repo, |_: &Peer| { pr.fetch_add(1, std::sync::atomic::Ordering::Relaxed); std::future::ready(true) },
                    |_: &Peer, _: &Peer| std::future::ready(Some(true)))
            .await;
        assert_eq!(probed.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// No second vantage available at all → 503, never serve. Serving here is exactly how a fleet
    /// split into halves gets two writers.
    #[tokio::test]
    async fn second_returns_unavailable_with_no_second_vantage() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let m = Membership::fixed(f, "rustic-git-1".into());
        let route = m
            .decide(&repo, |_: &Peer| std::future::ready(false), |_: &Peer, _: &Peer| std::future::ready(None))
            .await;
        assert_eq!(route, Route::Unavailable);
    }

    /// Third-ranked, first is confirmed down, second is up → go to second, not local.
    #[tokio::test]
    async fn third_defers_to_a_reachable_second() {
        let f = fleet(4);
        let repo = repo_where_i_rank(&f, "rustic-git-2", 2);
        let n = names(&f);
        let ranked = rank(&repo, &n);
        let (first, second) = (ranked[0].clone(), ranked[1].clone());
        let m = Membership::fixed(f.clone(), "rustic-git-2".into());
        let fc = first.clone();
        let fc2 = first.clone();
        let route = m
            .decide(&repo,
                    move |p: &Peer| std::future::ready(p.name != fc),
                    move |_: &Peer, t: &Peer| std::future::ready(Some(t.name != fc2)))
            .await;
        assert_eq!(route, Route::Peer(f.iter().find(|p| p.name == second).unwrap().clone()));
    }

    /// Outside the top three, a node never serves: it forwards to the first reachable candidate,
    /// and if none is reachable it says so rather than take a repo it can never own.
    #[tokio::test]
    async fn a_node_outside_the_candidates_never_serves() {
        let f = fleet(6);
        let repo = repo_where_i_rank(&f, "rustic-git-5", 5);
        let m = Membership::fixed(f.clone(), "rustic-git-5".into());
        let n = names(&f);
        let top = rank(&repo, &n)[0].clone();
        let r = m.decide(&repo, |_: &Peer| std::future::ready(true), |_: &Peer, _: &Peer| std::future::ready(Some(true))).await;
        assert_eq!(r, Route::Peer(f.iter().find(|p| p.name == top).unwrap().clone()));
        let r = m.decide(&repo, |_: &Peer| std::future::ready(false), |_: &Peer, _: &Peer| std::future::ready(Some(false))).await;
        assert_eq!(r, Route::Unavailable, "never serve a repo we are not a candidate for");
    }

    /// Candidates are the top three BY RANK, then filtered. If ranks 1 and 2 are down, rank 4 must
    /// not become a candidate — the fleet would no longer agree on who the candidates are. Rank 4
    /// with every candidate down: Unavailable, never Local.
    #[tokio::test]
    async fn candidates_are_top_three_by_rank_not_top_three_that_are_up() {
        let f = fleet(5);
        let repo = repo_where_i_rank(&f, "rustic-git-3", 3);
        let m = Membership::fixed(f.clone(), "rustic-git-3".into());
        let r = m.decide(&repo, |_: &Peer| std::future::ready(false), |_: &Peer, _: &Peer| std::future::ready(Some(false))).await;
        assert_eq!(r, Route::Unavailable);
    }

    /// Three-node fleet, owner dead, request lands on the THIRD candidate. Its only peers are the
    /// first (dead) and the second. Phase 1: second is up → forward there, no vantage needed. Then
    /// with second ALSO dead: nobody left to vouch → Unavailable, not Local.
    #[tokio::test]
    async fn in_a_three_node_fleet_the_third_forwards_to_second_or_reports_unavailable() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-2", 2);
        let n = names(&f);
        let ranked = rank(&repo, &n);
        let (first, second) = (ranked[0].clone(), ranked[1].clone());
        let m = Membership::fixed(f.clone(), "rustic-git-2".into());
        let f1 = first.clone();
        let r = m.decide(&repo,
            move |p: &Peer| std::future::ready(p.name != f1),
            |_: &Peer, _: &Peer| std::future::ready(None),
        ).await;
        assert_eq!(r, Route::Peer(f.iter().find(|p| p.name == second).unwrap().clone()), "second is up: forward, no vantage needed");
        let f1b = first.clone(); let s2b = second.clone();
        let r = m.decide(&repo,
            move |p: &Peer| std::future::ready(p.name != f1b && p.name != s2b),
            |_: &Peer, _: &Peer| std::future::ready(None),
        ).await;
        assert_eq!(r, Route::Unavailable, "both above dead, nobody to vouch: split fleet, do not serve");
    }

    /// Second candidate, owner dead from everyone: the third candidate may vouch, and does. In a
    /// three-node fleet this is the ONLY possible vantage for the second — a lower-ranked
    /// candidate — so it must be allowed.
    #[tokio::test]
    async fn a_lower_ranked_candidate_may_vouch_for_the_second() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let n = names(&f);
        let ranked = rank(&repo, &n);
        let (first, third) = (ranked[0].clone(), ranked[2].clone());
        let m = Membership::fixed(f.clone(), "rustic-git-1".into());
        let f1 = first.clone(); let f2 = first.clone(); let t3 = third.clone();
        let r = m.decide(&repo,
            move |p: &Peer| std::future::ready(p.name != f1),
            move |via: &Peer, t: &Peer| std::future::ready(if via.name == t3 && t.name == f2 { Some(false) } else { None }),
        ).await;
        assert_eq!(r, Route::Local, "third vouched that first is down: second serves");
    }

    /// Phase 2 must vouch for EVERY node above, not only the top. Third candidate: A dead from
    /// everyone, B down-from-me but alive (a cut link C↔B). D vouches "down" about A. Nobody has
    /// said anything about B — and B, asked as a via about A, answers (it is alive) — so B has
    /// proven reachable: forward to B. Serving here would be two writers (B serves too).
    #[tokio::test]
    async fn phase_two_forwards_to_a_higher_rank_that_answers_as_a_vantage() {
        let f = fleet(4);
        let repo = repo_where_i_rank(&f, "rustic-git-2", 2);
        let n = names(&f);
        let ranked = rank(&repo, &n);
        let (first, second) = (ranked[0].clone(), ranked[1].clone());
        let m = Membership::fixed(f.clone(), "rustic-git-2".into());
        let (f1, s1) = (first.clone(), second.clone());
        let (f2, s2) = (first.clone(), second.clone());
        let r = m.decide(&repo,
            // my probes: first down, second down (cut link), fourth up
            move |p: &Peer| std::future::ready(p.name != f1 && p.name != s1),
            // vantages: anyone asked about first says down; second, when asked as a via, ANSWERS
            move |via: &Peer, t: &Peer| std::future::ready(
                if t.name == f2 || via.name == s2 { Some(false) } else { None }),
        ).await;
        assert_eq!(r, Route::Peer(f.iter().find(|p| p.name == second).unwrap().clone()),
            "second answered as a vantage: it is reachable, forward there, never serve past it");
    }

    /// A target nobody could vouch about is Unavailable even if every OTHER target was confirmed
    /// down. Third candidate: A confirmed down by D, but every via asked about B returns None.
    #[tokio::test]
    async fn phase_two_requires_a_vantage_on_every_target() {
        let f = fleet(4);
        let repo = repo_where_i_rank(&f, "rustic-git-2", 2);
        let n = names(&f);
        let ranked = rank(&repo, &n);
        let (first, second) = (ranked[0].clone(), ranked[1].clone());
        let m = Membership::fixed(f.clone(), "rustic-git-2".into());
        let (f1, s1) = (first.clone(), second.clone());
        let (f2, s2) = (first.clone(), second.clone());
        let r = m.decide(&repo,
            move |p: &Peer| std::future::ready(p.name != f1 && p.name != s1),
            move |via: &Peer, t: &Peer| std::future::ready(
                if via.name == s2 { None }            // second is really down: cannot answer
                else if t.name == f2 { Some(false) }  // first confirmed down
                else { None }),                       // nobody can say anything about second
        ).await;
        assert_eq!(r, Route::Unavailable, "no vantage on the second: do not serve");
    }

    /// Any hard "up" vetoes. Second candidate, first down-from-me; C (asked concurrently) says
    /// down, D says up. D's answer is a 200 from the owner — proof it is alive. Unavailable.
    #[tokio::test]
    async fn any_positive_vantage_vetoes_serving() {
        let f = fleet(4);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let n = names(&f);
        let ranked = rank(&repo, &n);
        let (first, third) = (ranked[0].clone(), ranked[2].clone());
        let m = Membership::fixed(f.clone(), "rustic-git-1".into());
        let f1 = first.clone(); let t3 = third.clone();
        let r = m.decide(&repo,
            move |p: &Peer| std::future::ready(p.name != f1),
            move |via: &Peer, _t: &Peer| std::future::ready(if via.name == t3 { Some(false) } else { Some(true) }),
        ).await;
        assert_eq!(r, Route::Unavailable, "one vantage reached the owner: it is alive, we are cut off");
    }

    /// Phase 1 probes concurrently: a blackholed first must not delay a reachable second by a
    /// full timeout. All higher ranks must be issued before any resolves.
    #[tokio::test]
    async fn higher_ranks_are_probed_concurrently() {
        let f = fleet(4);
        let repo = repo_where_i_rank(&f, "rustic-git-3", 3);
        let m = std::sync::Arc::new(Membership::fixed(f.clone(), "rustic-git-3".into()));
        let started = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let st = started.clone();
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let nt = notify.clone();
        let m2 = m.clone();
        let h = tokio::spawn(async move {
            m2.decide(&repo,
                move |p: &Peer| { st.lock().unwrap().push(p.name.clone()); let nt = nt.clone(); async move { nt.notified().await; true } },
                |_: &Peer, _: &Peer| std::future::ready(Some(true)),
            ).await
        });
        for _ in 0..20 { tokio::task::yield_now().await; }
        assert_eq!(started.lock().unwrap().len(), 3, "all three higher ranks must be probed before any resolves");
        // notify_waiters wakes every registered waiter at once; notify_one would store at most one
        // permit for any not-yet-registered waiter and the test could hang.
        notify.notify_waiters();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h).await.expect("decide must complete");
    }

    /// Self is a member only if DNS lists it. A not-yet-ready pod receives no traffic, so it has
    /// nothing to route; forcing itself in early only creates a window where it serves repos every
    /// other node still routes to the old owner.
    #[tokio::test]
    async fn self_is_a_member_only_when_dns_lists_it() {
        let f = fleet(3);
        let m = Membership::fixed(f.clone(), "rustic-git-9".into());
        assert!(!m.peers().await.iter().any(|p| p.name == "rustic-git-9"));
        let repo = "o/r";
        // Not in the set → never Local. Forward to top, or Unavailable.
        let r = m.decide(repo, |_: &Peer| std::future::ready(true), |_: &Peer, _: &Peer| std::future::ready(Some(true))).await;
        assert!(matches!(r, Route::Peer(_)));
    }
}
