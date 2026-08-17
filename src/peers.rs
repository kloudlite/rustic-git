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
/// The headless Service's A records give the set of live IPs, and each `<statefulset>-N` pod name
/// is forward-resolved and kept when its IP is in that set. The name is the hash key: a pod keeps
/// its name across restarts but not its IP, so hashing on IP would make every restart a new peer
/// and move nearly every repo twice per roll.
pub struct Membership {
    /// DNS name to resolve, e.g. `_peer._tcp.rustic-git.rustic-git.svc.cluster.local`. Empty
    /// when the set is fixed.
    dns: String,
    self_name: String,
    cache: Mutex<Option<(Instant, Vec<Peer>)>>,
    /// How long a resolved set is reused. This bounds how long two nodes can disagree, so it is
    /// short; the cost is one DNS query per node per interval.
    pub ttl: Duration,
}

impl Membership {
    /// How long a stale answer is trusted after DNS stops answering.
    pub const STALE_MAX: Duration = Duration::from_secs(30);

    pub fn new(dns: String, self_name: String) -> Membership {
        Membership {
            dns,
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
    /// Empty answers are logged: if the per-pod names stop resolving the set is empty and every
    /// request is 503 — that must be loud.
    pub async fn peers(&self) -> Vec<Peer> {
        if let Some((at, peers)) = self.cache.lock().unwrap().as_ref() {
            if self.dns.is_empty() || at.elapsed() < self.ttl {
                return peers.clone();
            }
        }
        match resolve_peers(&self.dns, &self.self_name).await {
            Ok(peers) if !peers.is_empty() => {
                *self.cache.lock().unwrap() = Some((Instant::now(), peers.clone()));
                peers
            }
            Ok(_) => {
                eprintln!("resolving {}: no peers found; keeping last answer", self.dns); // ponytail: eprintln
                self.cached()
            }
            Err(e) => {
                eprintln!("resolving {}: {e}; keeping last answer", self.dns); // ponytail: eprintln; swap for a logger when one exists
                self.cached()
            }
        }
    }

    fn cached(&self) -> Vec<Peer> {
        match self.cache.lock().unwrap().as_ref() {
            Some((at, p)) if at.elapsed() < Self::STALE_MAX => p.clone(),
            _ => Vec::new(),
        }
    }

    /// Whether this node appears in its own resolved set. Used at startup: a node must not become
    /// ready until it can see itself in DNS, or a resolver that cannot answer for the pod names would
    /// silently make every request take two hops and every repo forward away from its owner.
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

/// The pod-name prefix of a StatefulSet member: everything before the ordinal.
fn pod_prefix(self_name: &str) -> crate::Result<&str> {
    match self_name.rsplit_once('-') {
        Some((prefix, ord)) if !prefix.is_empty() && ord.parse::<u32>().is_ok() => Ok(prefix),
        _ => Err(crate::err(format!(
            "RUSTIC_GIT_SELF must look like <statefulset>-<ordinal>, got '{self_name}'"
        ))),
    }
}

/// Resolve peers by forward-resolving each StatefulSet pod's own DNS name.
///
/// The headless Service's A records give the set of live IPs. The names come from this node's own
/// name: `<statefulset>-<i>.<headless-svc>` is resolved for i = 0, 1, 2, … until one does not
/// resolve, and each name whose IP is in the live set is a peer.
///
/// Forward, not reverse: reverse resolution was the first design and failed on the first deploy.
/// Any additional Service selecting the same pods — the public LoadBalancer — makes CoreDNS
/// publish a second, IP-derived PTR per pod, and `getnameinfo` returns whichever comes first, so
/// a pod's "name" came back as `10-244-1-48` and it silently left every peer set.
///
/// A name that resolves but is not live (terminating, or unready with its A record still up) is
/// skipped, not a stop: only a name that does not resolve at all ends the scan.
async fn resolve_peers(dns: &str, self_name: &str) -> crate::Result<Vec<Peer>> {
    let (svc, port) = dns
        .strip_prefix("_peer._tcp.")
        .and_then(|rest| rest.rsplit_once(':'))
        .ok_or_else(|| crate::err("srv must look like _peer._tcp.<svc>.<ns>.svc.cluster.local:<port>"))?;
    // Trailing dot: fully qualified, so the resolver does not walk ndots search domains first.
    let fq_svc = if svc.ends_with('.') { svc.to_string() } else { format!("{svc}.") };
    let live: std::collections::HashSet<std::net::IpAddr> =
        tokio::net::lookup_host(format!("{fq_svc}:{port}")).await?.map(|a| a.ip()).collect();
    let prefix = pod_prefix(self_name)?;

    let mut out = Vec::new();
    // ponytail: 256 ordinals is plenty for any fleet this routes; raise it if one gets bigger.
    for i in 0u32..256 {
        let name = format!("{prefix}-{i}");
        let host = format!("{name}.{fq_svc}");
        match tokio::net::lookup_host(format!("{host}:{port}")).await {
            Ok(mut addrs) => match addrs.next() {
                Some(a) => {
                    if live.contains(&a.ip()) {
                        out.push(Peer { name, addr: a.to_string() });
                    }
                }
                None => break,
            },
            Err(_) => break,
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[cfg(test)]
mod tests;
