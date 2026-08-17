# Peer Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every node route a repo's traffic to the one node that owns it, so a plain round-robin load balancer can front the fleet and SSH clients route correctly too.

**Architecture:** Ownership is computed, not stored: rendezvous hash over peers resolved from the headless Service's DNS. A node that is not the owner forwards — HTTP as a reverse proxy to the peer's HTTP peer port, SSH as a raw byte pipe to the peer's stream port. Both peer ports are separate listeners published by no Service, and only they honour a forwarded identity.

**Tech Stack:** Rust, axum 0.8, tokio, russh 0.62, reqwest 0.13 (already in the lock file via object_store), SlateDB 0.15.

**Spec:** `docs/superpowers/specs/2026-08-17-peer-routing-design.md`

## Global Constraints

- A repo's database may be open on exactly **one** node. Two nodes opening it is safe (SlateDB fences) but costs a failed request — never design for it deliberately.
- The public listeners (`8080`, `2222`) must **never** honour `X-Rustic-Git-Owner` or any identity claim. Only the peer listeners (`8081`, `8082`) do.
- Failover happens **only** on connection-level failures. An HTTP error from a peer that answered is returned to the client unchanged.
- **A node may serve a repo only if it cannot itself reach any higher-ranked node.** A lower-ranked node never takes a repo from a higher-ranked one on someone else's word. This is the rule that keeps two nodes from holding one repo; every routing decision goes through it.
- A request is forwarded at most twice (`X-Rustic-Git-Hops`, or the hop count in the stream header). A request out of hops is served where it lands. **The public listener strips this header** — a client must not be able to force a node to serve a repo it does not own.
- The peer listeners require `X-Rustic-Git-Peer: <secret>` (HTTP) / the secret in the stream header, from a Kubernetes Secret. Wrong or missing secret → 403, close. **This is in addition to the separate port, not instead of it**: `kolomi-cluster` runs with `networkPolicy: none`, so a NetworkPolicy would be silently accepted and enforce nothing, and pod networking is flat.
- Reachability means the peer's application answers `GET /healthz`, not that its kernel accepts a TCP connection. A pod mid-shutdown accepts TCP and then dies.
- Existing behaviour must not regress: run `cargo test --release` before every commit; all 26 existing tests must stay green.
- Comments explain *why*, matching the existing codebase style. Mark deliberate shortcuts with a `ponytail:` comment naming the ceiling.
- No new dependency beyond `reqwest`, which is already a transitive dependency.

---

### Task 1: Rendezvous ranking

Pure computation: given a repo and a list of peers, produce the same ordered candidate list on every node. No I/O, no clock, no network — this is the correctness core, so it is isolated and tested alone.

**Files:**
- Create: `src/peers.rs`
- Modify: `src/lib.rs` (add `pub mod peers;`)

**Interfaces:**
- Consumes: nothing
- Produces:
  - `pub fn rank(repo: &str, peers: &[String]) -> Vec<String>` — all peers, best first
  - `pub const CANDIDATES: usize = 3`

- [ ] **Step 1: Write the failing tests**

Create `src/peers.rs` with only the tests (the module compiles once Step 3 adds the code):

```rust
//! Which node owns a repo.
//!
//! Rendezvous hashing: every node scores each peer for a repo and takes the highest. The point is
//! agreement without coordination — no lookup on the request path, nothing to renew, and no state
//! that can be stale between nodes. Changing the peer set moves only the repos whose top scorer
//! changed, about 1/N of them, where `hash % N` would reshuffle nearly all of them.

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("10.244.0.{i}:8081")).collect()
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
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib peers
```

Expected: FAIL to compile — `cannot find function 'rank' in this scope`.

- [ ] **Step 3: Write the implementation**

Insert above the `#[cfg(test)]` block in `src/peers.rs`:

```rust
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

/// Score one (repo, peer) pair. The separator keeps "ab" + "c" from colliding with "a" + "bc".
fn score(repo: &str, peer: &str) -> u64 {
    let mut buf = Vec::with_capacity(repo.len() + peer.len() + 1);
    buf.extend_from_slice(repo.as_bytes());
    buf.push(0xff);
    buf.extend_from_slice(peer.as_bytes());
    fnv1a(&buf)
}

/// Every peer, best first. Ties break on the peer's own name so the order cannot depend on how
/// DNS happened to order its answer — two nodes resolving the same Service must rank identically.
pub fn rank(repo: &str, peers: &[String]) -> Vec<String> {
    let mut scored: Vec<(u64, &String)> = peers.iter().map(|p| (score(repo, p), p)).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(a.1)));
    scored.into_iter().map(|(_, p)| p.clone()).collect()
}
```

Add to `src/lib.rs`, keeping the module list alphabetical:

```rust
pub mod peers;
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib peers
```

Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/peers.rs src/lib.rs
git commit -m "Rank the nodes that could own a repo

Rendezvous hashing so every node reaches the same answer with no lookup and
nothing to keep in sync. Ties break on the peer name, because DNS returns its
records in arbitrary order and two nodes must still rank identically."
```

---

### Task 2: Membership and routing decisions

Wraps the ranking with the two things that need a clock and a resolver: the DNS-derived peer set, and a short memory of peers that refused a connection. Kept separate from Task 1 so the ranking stays provable without mocking either.

**Files:**
- Modify: `src/peers.rs`

**Interfaces:**
- Consumes: `rank`, `CANDIDATES` (Task 1)
- Produces:
  - `pub enum Route { Local, Peer(String) }`
  - `pub struct Membership`
  - `pub fn Membership::new(dns: String, self_addr: String) -> Membership`
  - `pub fn Membership::fixed(peers: Vec<String>, self_addr: String) -> Membership`
  - `pub async fn Membership::candidates(&self, repo: &str) -> Vec<Route>` — the ranked list, self as `Local`, at most `CANDIDATES` deep, down peers skipped
  - `pub async fn Membership::decide(&self, repo: &str, reachable: impl Fn(&str) -> Fut) -> Route` — the rule: forward to the first higher-ranked reachable node, else serve locally
  - `pub fn Membership::mark_down(&self, peer: &str)`

- [ ] **Step 1: Write the failing tests**

Append inside the existing `mod tests` block in `src/peers.rs`:

```rust
    /// A node must serve the repos it owns itself rather than forwarding to its own address.
    #[tokio::test]
    async fn owned_repos_route_locally() {
        let p = peers(3);
        for (i, me) in p.iter().enumerate() {
            let m = Membership::fixed(p.clone(), me.clone());
            let mine = (0..100)
                .map(|n| format!("o/r{n}"))
                .filter(|r| rank(r, &p)[0] == *me)
                .count();
            assert!(mine > 0, "peer {i} owns none of 100 repos");
            for n in 0..100 {
                let repo = format!("o/r{n}");
                let first = m.candidates(&repo).await.into_iter().next().unwrap();
                let expected = if rank(&repo, &p)[0] == *me {
                    Route::Local
                } else {
                    Route::Peer(rank(&repo, &p)[0].clone())
                };
                assert_eq!(first, expected, "{repo}");
            }
        }
    }

    /// Three deep, never more: a longer list would keep trying nodes that the rest of the fleet
    /// does not consider owners of this repo.
    #[tokio::test]
    async fn candidates_are_capped() {
        let m = Membership::fixed(peers(10), "10.244.0.9:8081".into());
        assert_eq!(m.candidates("o/r").await.len(), CANDIDATES);
    }

    /// A peer that refused a connection is skipped, so consecutive requests agree with each other
    /// instead of each rediscovering the failure and flapping.
    #[tokio::test]
    async fn a_peer_marked_down_is_skipped() {
        let p = peers(4);
        let me = p[3].clone();
        let repo = (0..100)
            .map(|n| format!("o/r{n}"))
            .find(|r| rank(r, &p)[0] != me)
            .unwrap();
        let m = Membership::fixed(p.clone(), me);
        let first = rank(&repo, &p)[0].clone();

        m.mark_down(&first);
        let after = m.candidates(&repo).await;
        assert!(
            !after.contains(&Route::Peer(first.clone())),
            "a peer marked down must not be offered again"
        );
        assert_eq!(after[0], Route::Peer(rank(&repo, &p)[1].clone()));
    }

    /// A pod that is not yet ready is not in DNS, so it would not find itself in the resolved set
    /// and would forward every repo it owns one rank down — then take them all back once ready,
    /// fencing each. Self must always be a member regardless of what DNS says.
    #[tokio::test]
    async fn self_is_always_a_member_even_when_dns_omits_it() {
        let p = peers(3);
        let me = "10.244.0.99:8081".to_string(); // not in the resolved set
        let m = Membership::fixed(p.clone(), me.clone());
        let all: Vec<String> = m.peers().await;
        assert!(all.contains(&me), "self must be in the peer set: {all:?}");
    }

    /// The memory is short: a restarted pod must come back into service without waiting for this
    /// node to restart too.
    #[tokio::test]
    async fn down_peers_recover_after_the_window() {
        let p = peers(4);
        let me = p[3].clone();
        let repo = (0..100)
            .map(|n| format!("o/r{n}"))
            .find(|r| rank(r, &p)[0] != me)
            .unwrap();
        let mut m = Membership::fixed(p.clone(), me);
        m.down_for = std::time::Duration::ZERO;
        let first = rank(&repo, &p)[0].clone();
        m.mark_down(&first);
        assert_eq!(m.candidates(&repo).await[0], Route::Peer(first));
    }

    /// If every candidate is down, routing locally is better than failing: this node can serve the
    /// repo, and being wrong about ownership costs a fenced request, not data.
    #[tokio::test]
    async fn all_candidates_down_falls_back_to_local() {
        let p = peers(3);
        let m = Membership::fixed(p.clone(), p[0].clone());
        for peer in &p {
            m.mark_down(peer);
        }
        assert_eq!(m.candidates("o/r").await, vec![Route::Local]);
    }

    // ---- decide(): the precedence rule ----

    /// Pick a repo and a peer set where `me` holds the given rank, so each test states which
    /// position it is exercising.
    fn repo_where_i_rank(p: &[String], me: &str, want: usize) -> String {
        (0..1000)
            .map(|n| format!("o/r{n}"))
            .find(|r| rank(r, p).iter().position(|x| x == me) == Some(want))
            .expect("some repo ranks me at that position")
    }

    /// The top candidate serves without checking anything: it has nothing above it, and this is
    /// the ordinary path that must pay nothing for failover.
    #[tokio::test]
    async fn the_top_candidate_serves_without_probing() {
        let p = peers(3);
        let me = p[0].clone();
        let repo = repo_where_i_rank(&p, &me, 0);
        let m = Membership::fixed(p.clone(), me);
        let probed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pr = probed.clone();
        let route = m
            .decide(&repo, |_| {
                pr.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async { true }
            })
            .await;
        assert_eq!(route, Route::Local);
        assert_eq!(probed.load(std::sync::atomic::Ordering::Relaxed), 0, "top rank must not probe");
    }

    /// The rule itself: a second-ranked node that CAN reach the first forwards there. It does not
    /// serve just because someone else sent it the request. This is what stops two nodes from
    /// holding one repo when only one of them can see the owner.
    #[tokio::test]
    async fn a_lower_rank_forwards_to_a_reachable_higher_rank() {
        let p = peers(3);
        let me = p[1].clone();
        let repo = repo_where_i_rank(&p, &me, 1);
        let top = rank(&repo, &p)[0].clone();
        let m = Membership::fixed(p.clone(), me);
        let route = m.decide(&repo, |_| async { true }).await;
        assert_eq!(route, Route::Peer(top));
    }

    /// And only when every higher rank is unreachable *from here* does it serve.
    #[tokio::test]
    async fn a_lower_rank_serves_only_when_higher_ranks_are_unreachable() {
        let p = peers(3);
        let me = p[2].clone();
        let repo = repo_where_i_rank(&p, &me, 2);
        let m = Membership::fixed(p.clone(), me);
        assert_eq!(m.decide(&repo, |_| async { false }).await, Route::Local);
    }

    /// Third-ranked, first is down, second is up: go to second, not local. Precedence is a strict
    /// order — node 3 loses to node 2 exactly as node 2 loses to node 1.
    #[tokio::test]
    async fn third_rank_defers_to_a_reachable_second() {
        let p = peers(3);
        let me = p[2].clone();
        let repo = repo_where_i_rank(&p, &me, 2);
        let ranked = rank(&repo, &p);
        let (first, second) = (ranked[0].clone(), ranked[1].clone());
        let m = Membership::fixed(p.clone(), me);
        let f = first.clone();
        let route = m
            .decide(&repo, move |peer: &str| {
                let up = peer != f;
                async move { up }
            })
            .await;
        assert_eq!(route, Route::Peer(second));
    }

    /// A node outside the top three for a repo is never its owner; it must forward to the first
    /// reachable candidate and never serve, whatever it can or cannot reach.
    #[tokio::test]
    async fn a_node_outside_the_candidates_never_serves() {
        let p = peers(6);
        let me = p[5].clone();
        let repo = repo_where_i_rank(&p, &me, 5);
        let top = rank(&repo, &p)[0].clone();
        let m = Membership::fixed(p.clone(), me);
        assert_eq!(m.decide(&repo, |_| async { true }).await, Route::Peer(top));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib peers
```

Expected: FAIL to compile — `cannot find type 'Membership' in this scope`.

- [ ] **Step 3: Write the implementation**

Add to `src/peers.rs`, above the test module:

```rust
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Where a request for a repo belongs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// This node owns it. Serve it here.
    Local,
    /// Another node owns it. Forward to this address.
    Peer(String),
}

/// The peer set, and which of them are currently worth talking to.
///
/// Membership comes from the headless Service's DNS rather than configuration, because the
/// dangerous state in this design is disagreement: if two nodes both believe they own a repo they
/// fence each other in turn, one reopen per flip. A peer list baked into the environment
/// guarantees that for the length of a rolling restart, since pods start with different lists.
/// Resolving DNS bounds the disagreement to `ttl` instead, and scaling then needs no restart.
pub struct Membership {
    /// `host:port` to resolve. Empty when the peer set is fixed (tests, single-node runs).
    dns: String,
    /// This node's own address as it appears in the resolved set.
    self_addr: String,
    cache: Mutex<Option<(Instant, Vec<String>)>>,
    down: Mutex<HashMap<String, Instant>>,
    /// How long a resolved peer set is reused. This bounds how long two nodes can disagree.
    pub ttl: Duration,
    /// How long a peer that refused a connection is skipped.
    pub down_for: Duration,
}

impl Membership {
    pub fn new(dns: String, self_addr: String) -> Membership {
        Membership {
            dns,
            self_addr,
            cache: Mutex::new(None),
            down: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(5),
            down_for: Duration::from_secs(5),
        }
    }

    /// A fixed peer set that is never resolved. Used by tests, and by a single-node deployment
    /// where there is nothing to discover.
    pub fn fixed(peers: Vec<String>, self_addr: String) -> Membership {
        let m = Membership::new(String::new(), self_addr);
        *m.cache.lock().unwrap() = Some((Instant::now(), m.with_self(peers)));
        m
    }

    /// Self is always a member. A pod that is not yet ready is absent from DNS, and without this it
    /// would forward every repo it owns one rank down, then take them all back once ready — one
    /// fence per repo on every scale-up.
    fn with_self(&self, mut peers: Vec<String>) -> Vec<String> {
        if !peers.contains(&self.self_addr) {
            peers.push(self.self_addr.clone());
        }
        peers.sort();
        peers.dedup();
        peers
    }

    /// The current peer set, resolved at most every `ttl`.
    ///
    /// A failed resolve keeps serving the previous answer: DNS being briefly unavailable is not a
    /// reason to decide the fleet has no members and take every repo locally.
    pub async fn peers(&self) -> Vec<String> {
        if let Some((at, peers)) = self.cache.lock().unwrap().as_ref() {
            if self.dns.is_empty() || at.elapsed() < self.ttl {
                return peers.clone();
            }
        }
        match tokio::net::lookup_host(&self.dns).await {
            Ok(addrs) => {
                let peers = self.with_self(addrs.map(|a| a.to_string()).collect());
                *self.cache.lock().unwrap() = Some((Instant::now(), peers.clone()));
                peers
            }
            Err(e) => {
                eprintln!("resolving {}: {e}", self.dns); // ponytail: eprintln; swap for a logger when one exists
                self.cached_or_self()
            }
        }
    }

    fn cached_or_self(&self) -> Vec<String> {
        match self.cache.lock().unwrap().as_ref() {
            Some((_, peers)) => peers.clone(),
            None => vec![self.self_addr.clone()],
        }
    }

    /// Where to send this repo, best first, at most `CANDIDATES` deep.
    ///
    /// Peers that recently refused a connection are skipped. If that leaves nothing, the answer is
    /// `Local`: this node can serve the repo, and being wrong about ownership costs one fenced
    /// request rather than an outage.
    pub async fn candidates(&self, repo: &str) -> Vec<Route> {
        let peers = self.peers().await;
        let now = Instant::now();
        let down = self.down.lock().unwrap();
        let out: Vec<Route> = rank(repo, &peers)
            .into_iter()
            .filter(|p| {
                *p == self.self_addr
                    || down
                        .get(p)
                        .is_none_or(|at| now.duration_since(*at) >= self.down_for)
            })
            .take(CANDIDATES)
            .map(|p| {
                if p == self.self_addr {
                    Route::Local
                } else {
                    Route::Peer(p)
                }
            })
            .collect();
        if out.is_empty() {
            return vec![Route::Local];
        }
        out
    }

    /// Remember that a peer refused a connection, so the next request does not rediscover it.
    pub fn mark_down(&self, peer: &str) {
        self.down
            .lock()
            .unwrap()
            .insert(peer.to_string(), Instant::now());
    }

    /// Where this request goes, by the one rule that keeps two nodes from holding one repo:
    ///
    /// > A node may serve a repo only if it cannot itself reach any higher-ranked node.
    ///
    /// Rank is agreed by every node; reachability is each node's own observation. A node that
    /// serves because *someone else* could not reach the owner is acting on an observation it did
    /// not make, and that is how two nodes end up holding one repo. So each candidate re-checks
    /// the nodes above it from its own vantage point, and forwards up if any answers.
    ///
    /// The top candidate has nothing above it and probes nothing — ordinary traffic pays no cost.
    /// A node outside the top `CANDIDATES` is never an owner and always forwards to the first
    /// reachable candidate.
    ///
    /// `reachable` is passed in rather than performed here so the rule can be tested with no
    /// network at all.
    pub async fn decide<F, Fut>(&self, repo: &str, reachable: F) -> Route
    where
        F: Fn(&str) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for candidate in self.candidates(repo).await {
            match candidate {
                Route::Local => return Route::Local,
                Route::Peer(peer) => {
                    if reachable(&peer).await {
                        return Route::Peer(peer);
                    }
                    self.mark_down(&peer);
                }
            }
        }
        // Everything ranked above us — or every candidate, if we are not one — is unreachable
        // from here. Serve rather than fail: being wrong costs one fenced request, refusing costs
        // the client everything.
        Route::Local
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib peers
```

Expected: PASS, 17 tests.

- [ ] **Step 5: Commit**

```bash
git add src/peers.rs
git commit -m "Resolve the peer set from DNS and decide who serves a repo

Membership comes from the headless Service rather than each pod's environment.
A baked-in list guarantees the nodes disagree for the length of a rolling
restart, and disagreement is what costs requests here: two nodes that both
think they own a repo fence each other in turn. DNS bounds that to the cache
interval, and scaling stops needing a restart.

The routing rule is that a node may serve a repo only if it cannot itself
reach any higher-ranked node. Rank is global but reachability is each node's
own view, and a node that serves because someone else could not reach the
owner is acting on an observation it did not make. Each candidate re-checks
from its own vantage point, so a lower rank never takes a repo from a higher
one on hearsay. The top candidate probes nothing, so ordinary traffic pays
nothing for this."
```

---

### Task 3: HTTP forwarding

Forwards one request to a peer and streams the response back. Separate from the routing decision so it can be tested against a stub server with no DNS or hashing involved.

**Files:**
- Create: `src/proxy.rs`
- Modify: `src/lib.rs` (add `pub mod proxy;`), `Cargo.toml`
- Test: `tests/proxy.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks
- Produces:
  - `pub const OWNER_HEADER: &str = "x-rustic-git-owner"`
  - `pub const HOPS_HEADER: &str = "x-rustic-git-hops"`
  - `pub const PEER_HEADER: &str = "x-rustic-git-peer"` — the shared secret
  - `pub const MAX_HOPS: u32 = 2`
  - `pub struct Forwarder { pub client: reqwest::Client, pub secret: String }`
  - `pub fn Forwarder::new(secret: String) -> Forwarder`
  - `pub async fn Forwarder::reachable(&self, peer: &str) -> bool` — `GET /healthz` on the peer answered 200
  - `pub async fn Forwarder::forward(&self, peer: &str, owner: &str, hops: u32, req: axum::extract::Request) -> Result<axum::response::Response, crate::Error>` — sets `HOPS_HEADER` to `hops + 1` and `PEER_HEADER` to the secret

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, under `[dependencies]`:

```toml
reqwest = { version = "0.13", default-features = false, features = ["stream"] }
```

`reqwest` is already in `Cargo.lock` as a transitive dependency of `object_store`, so this adds no new code to the build. Peer traffic is plain HTTP inside the cluster, so no TLS feature is needed.

- [ ] **Step 2: Write the failing test**

Create `tests/proxy.rs`:

```rust
//! Forwarding one request to a peer. The peer here is a stub server, so these tests cover the
//! proxy mechanics only — routing decisions are tested in src/peers.rs.
use axum::{routing::any, Router};
use rustic_git::proxy::{Forwarder, HOPS_HEADER, OWNER_HEADER, PEER_HEADER};

/// A stub peer that echoes back what it received, so the test can assert what crossed the wire.
/// It answers /healthz like a real node, because that is what reachability probes.
async fn stub_peer() -> String {
    let app = Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route(
        "/{*rest}",
        any(|headers: axum::http::HeaderMap, body: String| async move {
            let h = |k: &str| {
                headers
                    .get(k)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_string()
            };
            format!(
                "owner={} hops={} peer={} body={body}",
                h(OWNER_HEADER),
                h(HOPS_HEADER),
                h(PEER_HEADER)
            )
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// The forwarded request must carry the identity the edge authenticated, because the credential
/// was checked there and is not presented again.
#[tokio::test]
async fn forwarding_carries_body_and_authenticated_owner() {
    let peer = stub_peer().await;
    let f = Forwarder::new("s3cret".into());
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/alice/web/git-upload-pack")
        .body(axum::body::Body::from("0000"))
        .unwrap();

    let res = f.forward(&peer, "alice", 0, req).await.unwrap();
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    // hops is what we were given plus one: the peer knows how far this request has travelled
    assert_eq!(
        String::from_utf8_lossy(&body),
        "owner=alice hops=1 peer=s3cret body=0000"
    );
}

/// Reachability must mean "the application answers", not "the kernel accepts a connection". A pod
/// mid-shutdown accepts TCP and then dies, and treating that as reachable is how two nodes end up
/// holding one repo: B probes A (TCP ok), forwards, A dies; C probes A (refused), serves locally.
#[tokio::test]
async fn reachable_requires_the_application_to_answer() {
    let f = Forwarder::new(String::new());
    assert!(f.reachable(&stub_peer().await).await);
    // port 1 on loopback: reserved, nothing listens, connection is refused immediately
    assert!(!f.reachable("127.0.0.1:1").await);
    // A listener that accepts TCP but never speaks HTTP: must NOT count as reachable.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            drop(s); // accept, then close without answering
        }
    });
    assert!(!f.reachable(&addr).await, "TCP-accept-then-close is not reachable");
}
```

- [ ] **Step 3: Run the test to verify it fails**

```bash
cargo test --test proxy
```

Expected: FAIL to compile — `unresolved import 'rustic_git::proxy'`.

- [ ] **Step 4: Write the implementation**

Create `src/proxy.rs`:

```rust
//! Forwarding a request to the node that owns the repo.
//!
//! Two shapes, because the two client protocols are not the same shape. An HTTP request is one
//! request and one response, so it is reverse-proxied. An SSH session is a stream carrying an
//! advertisement and then repeated commands, so it is piped (see `stream_to_peer`).

use crate::Result;
use std::time::Duration;

/// Identity of the client the *forwarding* node authenticated. Honoured only on the peer
/// listener — the public listener never reads it, or a client could name any owner it liked.
pub const OWNER_HEADER: &str = "x-rustic-git-owner";

/// How many times this request has been forwarded. A receiving node may forward once more if it
/// can reach a higher-ranked node the sender could not; this bounds that rather than trusting the
/// routing to converge.
pub const HOPS_HEADER: &str = "x-rustic-git-hops";

/// Candidates are three deep, so a request never needs more than two forwards to reach the last
/// of them. Anything past this is served where it lands.
pub const MAX_HOPS: u32 = 2;

/// Shared secret carried on every forwarded request and in every peer stream header. The peer
/// listeners are on their own ports, published by no Service — but this cluster runs with
/// `networkPolicy: none`, so a NetworkPolicy would enforce nothing and any pod could reach them.
/// The secret is defence in depth on top of the separate port, not a replacement for it.
pub const PEER_HEADER: &str = "x-rustic-git-peer";

/// How long to wait for a peer to answer a health probe before treating it as down. Peers are in
/// the same cluster, so this is generous; the point is to fail over quickly rather than to hang.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

pub struct Forwarder {
    pub client: reqwest::Client,
    pub secret: String,
}

impl Forwarder {
    pub fn new(secret: String) -> Forwarder {
        Forwarder {
            client: reqwest::Client::builder()
                .connect_timeout(PROBE_TIMEOUT)
                // No total timeout: a clone of a large repo legitimately streams for a long time.
                .build()
                .expect("building an HTTP client cannot fail with these options"),
            secret,
        }
    }

    /// Whether a peer's *application* is answering right now.
    ///
    /// Probes `GET /healthz` rather than opening a bare TCP connection. A pod mid-shutdown still
    /// accepts TCP for a moment before it dies, and treating that as reachable is how two nodes end
    /// up holding one repo: one probes it (accepts), forwards, it dies; another probes it (refused)
    /// and serves locally. Requiring an HTTP 200 closes most of that window; the pod's `preStop`
    /// delay closes the rest by taking it out of DNS before it stops answering.
    ///
    /// Checked before forwarding rather than by retrying a failed forward: the request body is a
    /// stream that can only be consumed once, so there is nothing to retry with. One in-cluster
    /// GET is cheaper than buffering a push in memory to make it replayable.
    pub async fn reachable(&self, peer: &str) -> bool {
        let probe = self
            .client
            .get(format!("http://{peer}/healthz"))
            .timeout(PROBE_TIMEOUT)
            .send();
        matches!(probe.await, Ok(r) if r.status().is_success())
    }

    /// Send this request to `peer` and stream its response back, one hop further along.
    pub async fn forward(
        &self,
        peer: &str,
        owner: &str,
        hops: u32,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response> {
        use axum::body::Body;
        let (parts, body) = req.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let mut headers = parts.headers.clone();
        // The peer's host is not ours, and the body is re-framed by reqwest.
        headers.remove(axum::http::header::HOST);
        headers.remove(axum::http::header::CONTENT_LENGTH);
        headers.insert(OWNER_HEADER, owner.parse()?);
        headers.insert(HOPS_HEADER, (hops + 1).to_string().parse()?);
        headers.insert(PEER_HEADER, self.secret.parse()?);

        let upstream = self
            .client
            .request(parts.method, format!("http://{peer}{path}"))
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await?;

        let mut out = axum::response::Response::builder().status(upstream.status());
        for (k, v) in upstream.headers() {
            out = out.header(k, v);
        }
        Ok(out.body(Body::from_stream(upstream.bytes_stream()))?)
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod proxy;
```

- [ ] **Step 5: Run the test to verify it passes**

```bash
cargo test --test proxy
```

Expected: PASS, 2 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/proxy.rs src/lib.rs tests/proxy.rs
git commit -m "Forward an HTTP request to the node that owns the repo

Reachability means the peer's application answers a health probe, not that
its kernel accepts a connection: a pod mid-shutdown accepts TCP for a moment
and then dies, and treating that as reachable is how two nodes end up holding
one repo. It is checked before forwarding rather than by retrying a failure,
because the request body is a stream that can be consumed once.

Every forwarded request carries a shared secret. The peer port is already
separate and unpublished, but this cluster enforces no NetworkPolicy, so any
pod could reach it; the secret is defence in depth on top of the port."
```

---

### Task 4: Route HTTP requests before handling them

One decision point for all three git routes, and the split between the public router (routes, never trusts an identity header) and the peer router (serves, trusts it).

**Files:**
- Modify: `src/http.rs`, `src/lib.rs`
- Test: `tests/routing.rs`

**Interfaces:**
- Consumes: `peers::{Membership, Route}` (Task 2), `proxy::{Forwarder, OWNER_HEADER}` (Task 3)
- Produces:
  - `App` gains `pub peers: Arc<peers::Membership>` and `pub forwarder: Arc<proxy::Forwarder>`
  - `pub fn App::new(store, peers, peer_secret: String) -> App`
  - `pub fn http::router(app: Arc<App>) -> Router` — public, routes, trusts no identity
  - `pub fn http::peer_router(app: Arc<App>) -> Router` — internal, routes again by the precedence rule (bounded by hops), trusts the forwarded identity
  - `#[derive(Clone)] pub struct http::Trusted(pub Option<String>)`

- [ ] **Step 1: Write the failing test**

Create `tests/routing.rs`:

```rust
//! Which node ends up serving a repo, and who is allowed to claim an identity.
mod common;

use rustic_git::peers::Membership;
use rustic_git::App;
use std::sync::Arc;

const SECRET: &str = "test-peer-secret";

/// One node's own Store over a shared object store, so each node has its own pool and the test can
/// see which node opened a repo. Sharing one Store between two "nodes" would share one pool, and
/// "exactly one opener" could then never fail.
async fn own_store(os: Arc<dyn slatedb::object_store::ObjectStore>) -> Arc<rustic_git::store::Store> {
    let tmp = tempfile::tempdir().unwrap();
    let s = rustic_git::store::Store::open(os, tmp.path().join("cache"), false)
        .await
        .unwrap();
    std::mem::forget(tmp); // keep the cache dir for the test's lifetime
    Arc::new(s)
}

/// Bring up one node's public and peer listeners. An empty `peers` list means "I am the only
/// node": the node's own peer address is used, so it ranks first for everything.
async fn node(store: Arc<rustic_git::store::Store>, peers: Vec<String>, me: String) -> (String, String) {
    let pub_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (pub_a, peer_a) = (
        pub_l.local_addr().unwrap().to_string(),
        peer_l.local_addr().unwrap().to_string(),
    );
    let (peers, me) = if peers.is_empty() {
        (vec![peer_a.clone()], peer_a.clone())
    } else {
        (peers, me)
    };
    let app = Arc::new(App::new(store, Arc::new(Membership::fixed(peers, me)), SECRET.into()));
    let a2 = app.clone();
    tokio::spawn(async move { axum::serve(pub_l, rustic_git::http::router(a2)).await.unwrap() });
    tokio::spawn(async move { axum::serve(peer_l, rustic_git::http::peer_router(app)).await.unwrap() });
    (pub_a, peer_a)
}

/// The bypass a client would actually try: assert an owner on the public port and see if the
/// server believes it. It must not.
#[tokio::test]
async fn the_public_listener_ignores_a_claimed_identity() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let (public, _peer) = node(e.store.clone(), vec![], String::new()).await;

    let res = reqwest::Client::new()
        .get(format!("http://{public}/alice/web/info/refs?service=git-upload-pack"))
        .header(rustic_git::proxy::OWNER_HEADER, "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401, "a claimed owner must not authenticate a client");
}

/// A client must not be able to force a node to serve a repo it does not own by claiming the
/// request is out of hops. That would open the repo here and fence the real owner — an
/// unauthenticated way to disrupt any repo.
#[tokio::test]
async fn the_public_listener_ignores_a_claimed_hop_count() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();
    // This node ranks second for everything behind an unreachable first; with hops honoured it
    // would serve; with hops stripped it must fail over properly (first unreachable → serve
    // locally anyway) — so instead pin the ranking: first candidate IS reachable (a stub) and this
    // node is second. Honouring hops=2 would make it serve; stripping makes it forward.
    let os = e.store.os.clone();
    let (_a_pub, a_peer) = node(own_store(os.clone()).await, vec![], String::new()).await;
    let peers = vec![a_peer.clone(), "127.0.0.2:9".to_string()];
    let repo = (0..100)
        .map(|n| format!("alice/w{n}"))
        .find(|r| rustic_git::peers::rank(r, &peers)[0] == a_peer)
        .unwrap();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let b_store = own_store(os).await;
    let (b_pub, _b_peer) = node(b_store.clone(), peers, "127.0.0.2:9".into()).await;

    let res = reqwest::Client::new()
        .get(format!("http://{b_pub}/{repo}/info/refs?service=git-upload-pack"))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(rustic_git::proxy::HOPS_HEADER, rustic_git::proxy::MAX_HOPS.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(b_store.pool.warm_count(), 0, "B must forward, not serve: hops came from a client");
}

/// The peer listener requires the shared secret. Without it — or with the wrong one — the request
/// is refused outright, so a pod elsewhere in the cluster cannot forge an identity on this port.
#[tokio::test]
async fn the_peer_listener_requires_the_secret() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let (_public, peer) = node(e.store.clone(), vec![], String::new()).await;
    for wrong in [None, Some("not-the-secret")] {
        let mut req = reqwest::Client::new()
            .get(format!("http://{peer}/alice/web/info/refs?service=git-upload-pack"))
            .header(rustic_git::proxy::OWNER_HEADER, "alice")
            .header("git-protocol", "version=2");
        if let Some(w) = wrong {
            req = req.header(rustic_git::proxy::PEER_HEADER, w);
        }
        let res = req.send().await.unwrap();
        assert_eq!(res.status(), 403, "secret {wrong:?} must be refused");
    }
}

/// The same header on the peer listener is the whole point of that listener.
#[tokio::test]
async fn the_peer_listener_honours_a_forwarded_identity() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let (_public, peer) = node(e.store.clone(), vec![], String::new()).await;

    let res = reqwest::Client::new()
        .get(format!("http://{peer}/alice/web/info/refs?service=git-upload-pack"))
        .header(rustic_git::proxy::OWNER_HEADER, "alice")
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

/// The precedence rule, end to end: node C is sent a request by a node that could not reach A.
/// C *can* reach A, so it must forward there rather than serve — and only A opens the database.
#[tokio::test]
async fn a_lower_ranked_node_forwards_up_when_the_owner_is_reachable() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();

    // Two nodes, two Stores, one object store. Node A ranks first for the chosen repo; C second.
    let os = e.store.os.clone();
    let a_store = own_store(os.clone()).await;
    let (_a_pub, a_peer) = node(a_store.clone(), vec![], String::new()).await;
    let peers = vec![a_peer.clone(), "127.0.0.2:9".to_string()];
    let repo = (0..100)
        .map(|n| format!("alice/w{n}"))
        .find(|r| rustic_git::peers::rank(r, &peers)[0] == a_peer)
        .unwrap();
    let (owner, name) = repo.split_once('/').unwrap();
    e.store.create_repo(owner, name).await.unwrap();
    let c_store = own_store(os).await;
    let (_c_pub, c_peer) = node(c_store.clone(), peers.clone(), "127.0.0.2:9".into()).await;

    // Send it to C's *peer* port with hops=1, as if a node that could not reach A had forwarded it.
    let res = reqwest::Client::new()
        .get(format!("http://{c_peer}/{repo}/info/refs?service=git-upload-pack"))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(rustic_git::proxy::HOPS_HEADER, "1")
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a_store.pool.warm_count(), 1, "A must have served it");
    assert_eq!(c_store.pool.warm_count(), 0, "C must not have opened it — that is the whole rule");
}

/// A request that has used up its hops is served where it lands, so a routing disagreement can
/// never bounce a request forever.
#[tokio::test]
async fn a_request_out_of_hops_is_served_locally() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();
    // A peer set naming a node that does not exist, ranked wherever — with hops exhausted it must
    // not matter.
    let (_pub, peer) = node(e.store.clone(), vec!["127.0.0.1:1".into(), "127.0.0.2:9".into()], "127.0.0.2:9".into()).await;
    let res = reqwest::Client::new()
        .get(format!("http://{peer}/alice/web/info/refs?service=git-upload-pack"))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(rustic_git::proxy::HOPS_HEADER, rustic_git::proxy::MAX_HOPS.to_string())
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "out of hops: serve, do not bounce");
}

/// A repo owned by another node is forwarded there, and only that node opens its database.
/// Asserted through the warm count, so the test fails if both nodes open it.
#[tokio::test]
async fn a_request_to_the_wrong_node_is_forwarded() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();

    // Two nodes, two Stores, one object store. B's peer set names A's peer port and B is not in
    // it, so every repo ranks to A and B must forward.
    let os = e.store.os.clone();
    let a_store = own_store(os.clone()).await;
    let (_a_pub, a_peer) = node(a_store.clone(), vec![], String::new()).await;
    let b_store = own_store(os).await;
    let (b_pub, _b_peer) = node(b_store.clone(), vec![a_peer.clone()], "127.0.0.2:9".into()).await;

    let res = reqwest::Client::new()
        .get(format!("http://{b_pub}/alice/web/info/refs?service=git-upload-pack"))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "node B should have forwarded to node A");
    let body = res.text().await.unwrap();
    assert!(body.contains("service=git-upload-pack"), "got: {body}");
    assert_eq!(a_store.pool.warm_count(), 1, "A must have opened it");
    assert_eq!(b_store.pool.warm_count(), 0, "B must not have opened it");
}

/// The transport is only proven by real git. A push through a forwarding node exercises the
/// streamed request body (which forward() re-frames as chunked) and the streamed response.
#[tokio::test]
async fn a_real_git_push_and_clone_work_through_a_forwarding_node() {
    if !common::have_git() {
        eprintln!("git not installed; skipping");
        return;
    }
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();

    let os = e.store.os.clone();
    let a_store = own_store(os.clone()).await;
    let (_a_pub, a_peer) = node(a_store.clone(), vec![], String::new()).await;
    let b_store = own_store(os).await;
    let (b_pub, _b_peer) = node(b_store.clone(), vec![a_peer.clone()], "127.0.0.2:9".into()).await;

    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let url = format!("http://x:{token}@{b_pub}/alice/web.git");
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("f"), "through B to A").unwrap();
    git(&work, &["add", "f"]);
    git(&work, &["commit", "-qm", "one"]);
    git(&work, &["push", "-q", &url, "main"]);

    let clone = tmp.path().join("clone");
    git(tmp.path(), &["clone", "-q", &url, clone.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(clone.join("f")).unwrap(), "through B to A");

    assert_eq!(a_store.pool.warm_count(), 1, "A served both");
    assert_eq!(b_store.pool.warm_count(), 0, "B forwarded both");
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test --test routing
```

Expected: FAIL to compile — `App::new` takes 1 argument, `peer_router` not found.

- [ ] **Step 3: Extend `App`**

In `src/lib.rs`, replace the `App` struct and its impl:

```rust
pub struct App {
    pub store: std::sync::Arc<store::Store>,
    /// Who owns which repo, and which peers are answering.
    pub peers: std::sync::Arc<peers::Membership>,
    /// Client used to forward to whichever peer owns a repo; carries the peer secret.
    pub forwarder: std::sync::Arc<proxy::Forwarder>,
}

impl App {
    pub fn new(
        store: std::sync::Arc<store::Store>,
        peers: std::sync::Arc<peers::Membership>,
        peer_secret: String,
    ) -> Self {
        App {
            store,
            peers,
            forwarder: std::sync::Arc::new(proxy::Forwarder::new(peer_secret)),
        }
    }
}
```

- [ ] **Step 4: Add routing to `src/http.rs`**

Replace the `router` function and add the routing middleware:

```rust
/// Identity established by a *peer*, not by this node. `None` on the public listener, always —
/// the public listener authenticates clients and nothing else.
#[derive(Clone)]
pub struct Trusted(pub Option<String>);

/// The repo a request is for, if this is a git route. `/{owner}/{name}/info/refs` and friends.
fn repo_of(path: &str) -> Option<String> {
    let mut it = path.trim_start_matches('/').split('/');
    let (owner, name) = (it.next()?, it.next()?);
    let rest = it.next()?;
    if !matches!(rest, "info" | "git-upload-pack" | "git-receive-pack") {
        return None;
    }
    let (owner, name) = crate::protocol::parse_repo_path(&format!("{owner}/{name}"))?;
    Some(format!("{owner}/{name}"))
}

/// Send this request to the node that should serve the repo, or handle it here if that is us.
///
/// Runs before authentication so a request never reaches this node's handlers for a repo it
/// should not serve — opening that repo's database here is exactly what fences the node that
/// owns it. Applied to the peer listener as well as the public one: a node that receives a
/// forwarded request re-checks the nodes ranked above it from its own vantage point, and forwards
/// up if one answers. That is the rule that keeps two nodes from holding one repo when only one
/// of them can see the owner. The hop count bounds the chain.
async fn route(
    State(app): State<Arc<App>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(repo) = repo_of(req.uri().path()) else {
        return next.run(req).await; // /healthz and anything else is served locally
    };
    let hops: u32 = req
        .headers()
        .get(crate::proxy::HOPS_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if hops >= crate::proxy::MAX_HOPS {
        return next.run(req).await; // out of hops: serve here rather than bounce forever
    }
    let forwarder = app.forwarder.clone();
    let route = app
        .peers
        .decide(&repo, |peer| {
            let f = forwarder.clone();
            let p = peer.to_string();
            async move { f.reachable(&p).await }
        })
        .await;
    let crate::peers::Route::Peer(peer) = route else {
        return next.run(req).await;
    };
    // The identity to carry is whatever a peer already established for this request (nothing, on
    // the public listener); the client's own credential also travels with it, so the receiving
    // node can authenticate from scratch if it needs to. Forwarding is transport, not
    // authentication.
    let owner = req
        .extensions()
        .get::<Trusted>()
        .and_then(|t| t.0.clone())
        .unwrap_or_default();
    match app.forwarder.forward(&peer, &owner, hops, req).await {
        Ok(res) => res,
        Err(e) => {
            app.peers.mark_down(&peer);
            eprintln!("forwarding {repo} to {peer}: {e}"); // ponytail: eprintln; swap for a logger when one exists
            (StatusCode::SERVICE_UNAVAILABLE, "peer unavailable").into_response()
        }
    }
}

/// Admit a request from another node: check the secret, then read the identity it established.
///
/// The secret is checked here, on the peer listener only, and a miss is a 403 before anything
/// else runs. The separate port is the primary boundary; this exists because the cluster enforces
/// no NetworkPolicy, so any pod can reach the port.
async fn trust_peer(
    State(app): State<Arc<App>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let presented = req
        .headers()
        .get(crate::proxy::PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Constant-time compare is not needed: the secret is long and random, and a timing oracle
    // on an in-cluster port that also requires network reach is not the threat here.
    // ponytail: swap for subtle::ConstantTimeEq if the peer port is ever exposed more widely.
    if presented.is_empty() || presented != app.forwarder.secret {
        return (StatusCode::FORBIDDEN, "peer secret").into_response();
    }
    let owner = req
        .headers()
        .get(crate::proxy::OWNER_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
    req.extensions_mut().insert(Trusted(owner));
    next.run(req).await
}

/// Never trust anything a client says about routing: this listener faces clients.
///
/// Strips the hop count as well as the identity. A client that could set hops to the maximum
/// would force this node to serve a repo it does not own — opening it here and fencing the real
/// owner — which is an unauthenticated way to disrupt any repo.
async fn trust_nobody(mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    req.headers_mut().remove(crate::proxy::HOPS_HEADER);
    req.headers_mut().remove(crate::proxy::OWNER_HEADER);
    req.headers_mut().remove(crate::proxy::PEER_HEADER);
    req.extensions_mut().insert(Trusted(None));
    next.run(req).await
}

fn git_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/{owner}/{name}/info/refs", get(info_refs))
        .route("/{owner}/{name}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{name}/git-receive-pack", post(receive_pack))
        .layer(axum::extract::DefaultBodyLimit::max(max_body()))
}

/// The listener clients reach. Trusts no identity header, then routes.
///
/// Layer order matters: axum runs the outermost layer first, so `route` (added last) runs before
/// `trust_nobody`... except that `route` reads the `Trusted` extension. Put the trust layer
/// outermost so it has run by the time `route` looks.
pub fn router(app: Arc<App>) -> Router {
    git_routes()
        .layer(axum::middleware::from_fn_with_state(app.clone(), route))
        .layer(axum::middleware::from_fn(trust_nobody))
        .with_state(app)
}

/// The listener only other nodes reach. Honours the forwarded identity, then routes again by the
/// same rule — a receiving node may reach a higher-ranked owner the sender could not — bounded by
/// the hop count.
pub fn peer_router(app: Arc<App>) -> Router {
    git_routes()
        .layer(axum::middleware::from_fn_with_state(app.clone(), route))
        .layer(axum::middleware::from_fn_with_state(app.clone(), trust_peer))
        .with_state(app)
}
```

- [ ] **Step 5: Honour `Trusted` in `open`**

In `src/http.rs`, change `open` to take the trusted identity, replacing its authentication preamble:

```rust
async fn open(
    app: &App,
    headers: &HeaderMap,
    trusted: &Trusted,
    owner: &str,
    name: &str,
) -> Result<Repo, Response> {
    // A peer already authenticated this client; the credential is not presented again.
    let auth_owner = match &trusted.0 {
        Some(o) => Some(o.clone()),
        None => {
            let token = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Basic "))
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .and_then(|d| String::from_utf8(d).ok())
                .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()));
            let Some(token) = token else {
                return Err(unauthorized());
            };
            app.store.owner_for_token(&token).await.map_err(internal)?
        }
    };
    if auth_owner.is_none() {
        return Err(unauthorized());
    }
    if !crate::auth::authorize(auth_owner.as_deref(), owner) {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    let (owner, name) =
        crate::protocol::parse_repo_path(&format!("{owner}/{name}")).unwrap_or_default();
    match app.store.open_repo(&owner, &name).await {
        Ok(Some(repo)) => Ok(repo),
        Ok(None) => Err((StatusCode::NOT_FOUND, "repository not found").into_response()),
        Err(e) => {
            eprintln!("open_repo {owner}/{name}: {e}"); // ponytail: eprintln; swap for a logger when one exists
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
        }
    }
}
```

In each of the three handlers (`info_refs`, `upload_pack`, `receive_pack`), add the extractor and pass it through — for example in `info_refs`:

```rust
async fn info_refs(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
) -> Response {
    let service = q.get("service").cloned().unwrap_or_default();
    let repo = match open(&app, &headers, &trusted, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
```

Apply the same two changes to `upload_pack` and `receive_pack`. In both, `Extension` must come **before** the body extractor, since `Bytes` consumes the request.

- [ ] **Step 6: Fix the existing call sites**

`App::new` now takes two arguments. Update `tests/common/mod.rs`:

```rust
pub async fn env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Arc::new(InMemory::new()), tmp.path().join("cache"), false)
        .await
        .unwrap();
    TestEnv {
        store: Arc::new(store),
        _tmp: tmp,
    }
}

/// An App for a single node that owns everything — what every test that is not about routing
/// wants.
pub fn app(store: Arc<Store>) -> Arc<rustic_git::App> {
    Arc::new(rustic_git::App::new(
        store,
        Arc::new(rustic_git::peers::Membership::fixed(
            vec!["127.0.0.1:1".into()],
            "127.0.0.1:1".into(),
        )),
        "test-peer-secret".into(),
    ))
}
```

Then update `tests/http_e2e.rs` and `tests/ssh_e2e.rs` to build their `App` with `common::app(store)`.

- [ ] **Step 7: Run the tests**

```bash
cargo test --release
```

Expected: PASS — the 8 new routing tests plus every existing test. `a_real_git_push_and_clone_work_through_a_forwarding_node` is the one that proves the transport; if only it fails, the bug is in `Forwarder::forward`'s body handling.

- [ ] **Step 8: Commit**

```bash
git add src/http.rs src/lib.rs tests/routing.rs tests/common/mod.rs tests/http_e2e.rs tests/ssh_e2e.rs
git commit -m "Route each repo to its owner before handling the request

Routing runs ahead of authentication, because the damage is done by opening a
repo's database on the wrong node: that claims the writer epoch and fences the
node that owns it. Deciding first means a misrouted request never touches the
database at all.

Both listeners route. A node that receives a forwarded request re-checks the
nodes ranked above it from its own vantage point and forwards up if one
answers, so a lower rank never takes a repo from a higher one on another
node's word. A hop count bounds the chain.

The listeners differ in exactly one way: the peer listener honours the
identity its caller established, the public one never does. That is the whole
security boundary, so it is a property of which socket the request arrived on
rather than a header the code has to remember to check."
```

---

### Task 5: Peer stream listener and SSH forwarding

SSH sessions are piped rather than translated, because one SSH session is an advertisement plus repeated commands on a single stream, while HTTP is one command per request.

**Files:**
- Modify: `src/proxy.rs`, `src/ssh.rs`
- Test: `tests/routing.rs`

**Interfaces:**
- Consumes: `peers::{Membership, Route}`, `proxy::Forwarder`
- Produces:
  - `pub async fn proxy::stream_to_peer<S>(secret: &str, peer: &str, service: &str, repo: &str, owner: &str, hops: u32, stream: &mut S) -> crate::Result<()>` where `S: AsyncRead + AsyncWrite + Unpin` — header line is `<secret> <service> <repo> <owner> <hops+1>\n`. **Borrows** the stream so the caller keeps the SSH channel alive across the exit-status call.
  - `pub async fn proxy::serve_peer_streams(app: Arc<App>, listener: TcpListener) -> crate::Result<()>`

- [ ] **Step 1: Write the failing test**

Append to `tests/routing.rs`:

```rust
/// A multi-command session over one stream — ls-refs then fetch — is the case that a
/// single-request translation would have broken, so it is the case worth testing.
#[tokio::test]
async fn a_peer_stream_serves_a_whole_session() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();

    let app = common::app(e.store.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { rustic_git::proxy::serve_peer_streams(app, l).await });

    let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
    // hops=2: out of hops, so this node must serve rather than route again
    sock.write_all(b"test-peer-secret git-upload-pack alice/web alice 2\n").await.unwrap();

    // The advertisement comes first, exactly as it does for a local SSH client.
    let mut reader = BufReader::new(&mut sock);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.contains("version 2"), "expected a v2 advertisement, got: {line:?}");
}

/// An unauthorised owner in the header must be refused, or the stream port would be a way to
/// read any repo.
#[tokio::test]
async fn a_peer_stream_enforces_authorisation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();

    let app = common::app(e.store.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { rustic_git::proxy::serve_peer_streams(app, l).await });

    let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
    sock.write_all(b"test-peer-secret git-upload-pack alice/web mallory 2\n").await.unwrap();
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.unwrap();
    assert!(
        !String::from_utf8_lossy(&buf).contains("version 2"),
        "a client authorised as mallory must not be served alice's repo"
    );
}

/// The wrong secret is closed without a byte, so a stray pod cannot use the stream port at all.
#[tokio::test]
async fn a_peer_stream_requires_the_secret() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let app = common::app(e.store.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { rustic_git::proxy::serve_peer_streams(app, l).await });

    let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
    sock.write_all(b"wrong git-upload-pack alice/web alice 2\n").await.unwrap();
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.unwrap();
    assert!(buf.is_empty(), "wrong secret must get nothing back, got {} bytes", buf.len());
}

/// A real ssh clone through a forwarding node. This is the multi-command session (ls-refs, then
/// fetch, on one connection) that a single-request translation would have broken — and it checks
/// that the exit status reaches the client, which needs the channel kept alive until it is sent.
#[tokio::test]
async fn a_real_ssh_clone_works_through_a_forwarding_node() {
    if !common::have_git() || !common::have_ssh() {
        eprintln!("git or ssh not installed; skipping");
        return;
    }
    // Follow tests/ssh_e2e.rs for the harness: it already brings up rustic_git::ssh::serve on a
    // random port with a generated host key and a registered client key. Do exactly that for two
    // nodes A and B built with own_store() and node()-style Membership so that A ranks first and
    // B forwards. Then:
    //   git clone -q ssh://git@127.0.0.1:<B ssh port>/alice/web.git <dir>
    // with GIT_SSH_COMMAND pointing at the test key and StrictHostKeyChecking=no, exactly as
    // ssh_e2e.rs does. Assert the clone succeeds and contains the pushed file, and that
    // a_store.pool.warm_count() == 1 and b_store.pool.warm_count() == 0.
    //
    // This test is deliberately written against the existing SSH harness rather than duplicated
    // here: read tests/ssh_e2e.rs first and reuse its helper functions.
    unimplemented!("write against the tests/ssh_e2e.rs harness; see comment above");
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test --test routing a_peer_stream
```

Expected: FAIL to compile — `serve_peer_streams` not found.

- [ ] **Step 3: Implement the peer stream server**

Append to `src/proxy.rs`:

```rust
use crate::App;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Accept forwarded SSH sessions.
///
/// The first line names the service, the repo, and the owner the forwarding node authenticated;
/// everything after it is the git protocol, byte for byte. The socket is then handed to the same
/// `serve` a local SSH client would reach, so nothing about the protocol is reimplemented here —
/// which is the point of piping rather than translating.
pub async fn serve_peer_streams(app: Arc<App>, listener: TcpListener) -> Result<()> {
    loop {
        let (sock, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_peer_stream(app, sock).await {
                eprintln!("peer stream: {e}"); // ponytail: eprintln; swap for a logger when one exists
            }
        });
    }
}

async fn serve_peer_stream(app: Arc<App>, sock: tokio::net::TcpStream) -> Result<()> {
    let mut reader = BufReader::new(sock);
    let mut header = String::new();
    reader.read_line(&mut header).await?;
    let mut parts = header.trim_end().splitn(5, ' ');
    // Secret first, checked before anything else is parsed. Wrong secret: close without a byte,
    // so the port reveals nothing to a stray pod. See PEER_HEADER for why this exists on top of
    // the separate port.
    let presented = parts.next().unwrap_or_default();
    if presented.is_empty() || presented != app.forwarder.secret {
        return Err(crate::err("peer secret"));
    }
    let (service, repo, owner) = (
        parts.next().unwrap_or_default().to_string(),
        parts.next().unwrap_or_default().to_string(),
        parts.next().unwrap_or_default().to_string(),
    );
    let hops: u32 = parts.next().and_then(|h| h.parse().ok()).unwrap_or(0);
    let (ro, rn) = crate::protocol::parse_repo_path(&repo)
        .ok_or_else(|| crate::err("invalid repo path"))?;
    // The forwarding node authenticated the client; this node still decides what that identity is
    // allowed to reach. Trusting the caller's identity is not the same as skipping authorisation.
    if !crate::auth::authorize(Some(owner.as_str()), &ro) {
        return Err(crate::err("access denied"));
    }
    // Same rule as HTTP: re-check the nodes ranked above us from here, and forward up if one
    // answers, unless this request is out of hops.
    if hops < MAX_HOPS {
        let f = app.forwarder.clone();
        let route = app
            .peers
            .decide(&repo, |peer| {
                let f = f.clone();
                let p = peer.to_string();
                async move { f.reachable(&p).await }
            })
            .await;
        if let crate::peers::Route::Peer(peer) = route {
            let sock = reader.into_inner();
            return stream_to_peer(
                &app.forwarder.secret,
                &stream_addr(&peer),
                &service,
                &repo,
                &owner,
                hops,
                sock,
            )
            .await;
        }
    }
    let repo = app
        .store
        .open_repo(&ro, &rn)
        .await?
        .ok_or_else(|| crate::err("repository not found"))?;
    let sock = reader.into_inner();
    crate::ssh::serve_git(app.store.clone(), repo, &service, sock).await
}

/// The stream port sits one above the HTTP peer port on every node. Peers are addressed by their
/// HTTP peer port everywhere else, so derive rather than configure a second list.
/// ponytail: fixed offset; make it a second env var if the ports ever need to be independent.
pub fn stream_addr(http_peer: &str) -> String {
    match http_peer.rsplit_once(':') {
        Some((host, port)) => {
            let port: u16 = port.parse().unwrap_or(8081);
            format!("{host}:{}", port + 1)
        }
        None => http_peer.to_string(),
    }
}

/// Pipe an established stream to the node that owns the repo, one hop further along.
///
/// Takes the stream by `&mut` deliberately: the caller must keep it alive after this returns. On
/// the SSH path the stream *is* the channel, and dropping it closes the channel — but the exit
/// status has to go out first, or git sees the session end with no status. `run` in ssh.rs makes
/// the same point about its own bridges. Borrowing here makes it impossible to drop the channel by
/// accident inside the pipe.
pub async fn stream_to_peer<S>(
    secret: &str,
    peer_stream: &str,
    service: &str,
    repo: &str,
    owner: &str,
    hops: u32,
    stream: &mut S,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut sock = tokio::net::TcpStream::connect(peer_stream).await?;
    sock.write_all(format!("{secret} {service} {repo} {owner} {}\n", hops + 1).as_bytes())
        .await?;
    // Copying in both directions until either side finishes: a fetch ends when the client stops
    // asking, a push when the server has acknowledged.
    tokio::io::copy_bidirectional(stream, &mut sock).await?;
    Ok(())
}
```

- [ ] **Step 4: Extract the protocol runner in `src/ssh.rs`**

The blocking protocol body inside `run` becomes reusable so both a local SSH channel and a forwarded socket reach it. Add to `src/ssh.rs`:

```rust
/// Run one git service over an already-established byte stream.
///
/// Shared by the local SSH path and the peer stream path, so a forwarded session is served by
/// exactly the code a local one is.
pub async fn serve_git<S>(
    store: Arc<crate::store::Store>,
    repo: crate::store::Repo,
    service: &str,
    stream: S,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let upload = service == "git-upload-pack";
    let (rd, wr) = tokio::io::split(stream);
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    tokio::task::spawn_blocking(move || {
        let interrupt = interrupt;
        let mut input = std::io::BufReader::new(SyncIoBridge::new(rd));
        let mut output = SyncIoBridge::new(wr);
        use std::io::Write;
        if upload {
            upload::advertise(&mut output)?;
            upload::serve(&store, &repo, &mut input, &mut output, &interrupt)?;
        } else {
            receive::advertise(&store, &repo, &mut output)?;
            receive::serve(&store, &repo, &mut input, &mut output, &interrupt)?;
        }
        output.flush()?;
        Ok(())
    })
    .await?
}
```

- [ ] **Step 5: Forward in the SSH path**

In `src/ssh.rs`, inside `run`, immediately after the `authorize` check and before `open_repo`, add:

```rust
    // Route before opening: opening this repo's database here would claim the writer epoch and
    // fence whichever node owns it. Same rule as HTTP — decide() forwards to the first reachable
    // higher-ranked node, and serves locally only when none answers.
    let repo_path = format!("{owner}/{name}");
    let f = app.forwarder.clone();
    let route = app
        .peers
        .decide(&repo_path, |peer| {
            let f = f.clone();
            let p = peer.to_string();
            async move { f.reachable(&p).await }
        })
        .await;
    if let crate::peers::Route::Peer(peer) = route {
        let authed = auth_owner.clone().unwrap_or_default();
        // The channel stream stays alive in this scope until after the exit status is sent —
        // dropping it closes the SSH channel, and git needs the status before that. This is the
        // same ordering `run` already observes for the local path below.
        let mut stream = channel.into_stream();
        let piped = crate::proxy::stream_to_peer(
            &app.forwarder.secret,
            &crate::proxy::stream_addr(&peer),
            service,
            &repo_path,
            &authed,
            0,
            &mut stream,
        )
        .await;
        let code = match &piped {
            Ok(()) => 0,
            Err(e) => {
                // The channel was consumed by the attempt, so there is no second try here. Mark
                // the peer down so the *next* session picks another candidate.
                app.peers.mark_down(&peer);
                let _ = handle
                    .extended_data(id, 1, format!("rustic-git: forwarding to {peer}: {e}\n").into_bytes())
                    .await;
                1
            }
        };
        let _ = handle.exit_status_request(id, code).await;
        let _ = handle.eof(id).await;
        drop(stream); // now the channel may close
        return piped;
    }
```

- [ ] **Step 6: Run the tests**

```bash
cargo test --release
```

Expected: PASS, including the four new peer-stream tests. Add `pub fn have_ssh() -> bool` to `tests/common/mod.rs` mirroring `have_git()` (checks `ssh -V`) for the real-ssh test to gate on.

- [ ] **Step 7: Commit**

```bash
git add src/proxy.rs src/ssh.rs tests/routing.rs
git commit -m "Pipe forwarded SSH sessions to the owning node

An SSH session is not an HTTP request. Protocol v2 over SSH is one
advertisement followed by repeated command exchanges on a single stream, while
over HTTP each command is its own POST, so translating between them would mean
splitting a v2 session by hand and knowing exactly where each command ends.

The forwarding node instead sends one header line and copies bytes. The owner
hands that socket to the same serve() a local SSH client reaches, so the
protocol code is shared rather than reimplemented. The header names the
authenticated owner, and the owner still authorises it: trusting who the
caller says it is is not the same as skipping the check on what they may
reach."
```

---

### Task 6: Bind the listeners

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: everything above
- Produces: nothing further

- [ ] **Step 1: Wire up `serve`**

In `src/main.rs`, replace the body of `serve`:

```rust
/// Start the server. This node serves the repos it owns and forwards the rest, so the thing in
/// front of it can be a plain round-robin load balancer that knows nothing about git.
async fn serve() -> Result<()> {
    let store = Arc::new(
        Store::open(
            object_store()?,
            env("RUSTIC_GIT_CACHE_DIR", "./cache").into(),
            true,
        )
        .await?,
    );

    let peer_addr = env("RUSTIC_GIT_PEER_ADDR", "0.0.0.0:8081");
    let peer_port = peer_addr.rsplit(':').next().unwrap_or("8081").to_string();
    // Required whenever there is more than one node. A single node with no peers gets a random
    // one, so its peer port cannot be driven by anyone at all.
    let peer_secret = match std::env::var("RUSTIC_GIT_PEER_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            if std::env::var("RUSTIC_GIT_PEER_DNS").map(|d| !d.is_empty()).unwrap_or(false) {
                return Err(rustic_git::err(
                    "RUSTIC_GIT_PEER_SECRET is required when RUSTIC_GIT_PEER_DNS is set",
                ));
            }
            use rand::RngCore;
            let mut b = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut b);
            b.iter().map(|x| format!("{x:02x}")).collect()
        }
    };
    // Membership is the headless Service's endpoints. Kubernetes publishes only ready pods there,
    // so an unready peer leaves the candidate set without anyone having to notice it.
    let peers = match std::env::var("RUSTIC_GIT_PEER_DNS") {
        Ok(dns) if !dns.is_empty() => {
            let me = env("RUSTIC_GIT_SELF_IP", "127.0.0.1");
            rustic_git::peers::Membership::new(format!("{dns}:{peer_port}"), format!("{me}:{peer_port}"))
        }
        // No peers configured: a single node that owns everything.
        _ => rustic_git::peers::Membership::fixed(
            vec![format!("127.0.0.1:{peer_port}")],
            format!("127.0.0.1:{peer_port}"),
        ),
    };
    let app = Arc::new(rustic_git::App::new(store.clone(), Arc::new(peers), peer_secret));
    store.pool.spawn_sweeper();

    let http = tokio::net::TcpListener::bind(env("RUSTIC_GIT_HTTP_ADDR", "0.0.0.0:8080")).await?;
    let ssh = tokio::net::TcpListener::bind(env("RUSTIC_GIT_SSH_ADDR", "0.0.0.0:2222")).await?;
    let peer_http = tokio::net::TcpListener::bind(&peer_addr).await?;
    // The stream port is always one above the HTTP peer port; peers derive it the same way.
    let peer_stream =
        tokio::net::TcpListener::bind(rustic_git::proxy::stream_addr(&peer_addr)).await?;
    let key = host_key(&env("RUSTIC_GIT_HOST_KEY", "./host_key"))?;
    eprintln!(
        "http on {} ssh on {} — peers on {} and {}, up to {} warm databases",
        http.local_addr()?,
        ssh.local_addr()?,
        peer_http.local_addr()?,
        peer_stream.local_addr()?,
        store.pool.max_warm,
    );

    let (a2, a3, a4) = (app.clone(), app.clone(), app.clone());
    tokio::select! {
        r = axum::serve(http, rustic_git::http::router(a2)) => { r?; }
        r = axum::serve(peer_http, rustic_git::http::peer_router(a3)) => { r?; }
        r = rustic_git::proxy::serve_peer_streams(a4, peer_stream) => { r?; }
        r = rustic_git::ssh::serve(app, ssh, key) => { r?; }
    }
    store.pool.close().await;
    Ok(())
}
```

Also update `open_store` (used by `admin`) — it builds a `Store`, not an `App`, so it needs no change. Verify by compiling.

- [ ] **Step 2: Verify it builds and every test passes**

```bash
cargo build --release && cargo test --release && cargo clippy --all-targets
```

Expected: builds clean, all tests pass, no clippy warnings.

- [ ] **Step 3: Verify a single node still works end to end**

```bash
RUSTIC_GIT_S3_URL=mem:// ./target/release/rustic-git admin create-repo demo/x 2>&1 | head -2
```

Expected: no output, exit 0. (With `mem://` nothing persists; this only proves the binary runs.)

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "Bind the peer listeners alongside the public ones

Peer traffic gets its own sockets rather than a shared secret on the public
port. A secret would put one string between the internet and an authentication
bypass, checked on the same socket that serves clients; a separate listener
cannot be reached by a client at all, and forgetting to publish it fails as an
outage rather than a breach.

With no peer DNS configured the node owns everything, so a single-node
deployment needs no configuration and behaves exactly as before."
```

---

### Task 7: Deployment

**Files:**
- Modify: `deploy/rustic-git.yaml`
- Modify: `README.md`

- [ ] **Step 0: Confirm the cluster does not enforce NetworkPolicy**

```bash
az aks show -n kolomi-cluster -g kolomi-rg --query 'networkProfile.networkPolicy' -o tsv
```

Expected: `none`. This is why the plan requires a peer secret in addition to the separate port. If this ever changes to `azure` or `calico`, the NetworkPolicy below becomes real defence and the secret becomes belt-and-braces; keep both regardless.

- [ ] **Step 1: Create the peer secret**

```bash
kubectl --context kolomi-cluster -n rustic-git create secret generic rustic-git-peer \
  --from-literal=secret="$(openssl rand -hex 32)" \
  --dry-run=client -o yaml | kubectl --context kolomi-cluster apply -f -
```

- [ ] **Step 2: Replace the Ingress with a LoadBalancer, and add the peer ports**

In `deploy/rustic-git.yaml`, delete the `Ingress` and the `rustic-git-http` Service entirely. Add to the StatefulSet's container `env`:

```yaml
            # Membership is this Service's endpoints. Kubernetes publishes only ready pods there,
            # so an unready peer drops out of the candidate set on its own.
            - name: RUSTIC_GIT_PEER_DNS
              value: rustic-git.rustic-git.svc.cluster.local
            - name: RUSTIC_GIT_SELF_IP
              valueFrom: { fieldRef: { fieldPath: status.podIP } }
            # Required by every forwarded request. The peer ports are separate and unpublished, but
            # this cluster runs with networkPolicy: none, so any pod could reach them; the secret is
            # defence in depth on top of the port.
            - name: RUSTIC_GIT_PEER_SECRET
              valueFrom: { secretKeyRef: { name: rustic-git-peer, key: secret } }
```

Add a `lifecycle` block to the container, alongside `readinessProbe`:

```yaml
          # On termination, Kubernetes removes the pod from the Service endpoints and sends
          # SIGTERM at the same time. Without a delay the pod stops answering while peers still
          # resolve it, and a peer that probed it a moment ago forwards into a dying process.
          # Sleeping first lets the endpoint removal propagate through DNS, so every node agrees
          # this pod is gone before it actually goes. This is what makes shutdown a clean
          # handover rather than a race.
          lifecycle:
            preStop:
              exec:
                command: ["sleep", "10"]
```

And raise `terminationGracePeriodSeconds` from 60 to 75 to cover the sleep plus the pool flush.

Add to the container's `ports`:

```yaml
            - { name: peer-http, containerPort: 8081 }
            - { name: peer-stream, containerPort: 8082 }
```

Add the peer ports to the headless Service so they resolve, and append the public LoadBalancer:

```yaml
---
# Public entry point. Round robin is all it needs to be: whichever pod answers forwards the repo
# to its owner. Auth is a token over HTTP basic or an SSH key, checked before any repo work.
apiVersion: v1
kind: Service
metadata:
  name: rustic-git-lb
  namespace: rustic-git
spec:
  type: LoadBalancer
  selector: { app: rustic-git }
  ports:
    - { name: http, port: 80, targetPort: http }
    - { name: ssh, port: 2222, targetPort: ssh }
---
# The peer ports are published by no Service, but pod networking is flat, so anything else in the
# cluster could still reach them. They are the one place a caller's claimed identity is believed.
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: rustic-git-peers-only
  namespace: rustic-git
spec:
  podSelector:
    matchLabels: { app: rustic-git }
  policyTypes: [Ingress]
  ingress:
    - from:
        - podSelector:
            matchLabels: { app: rustic-git }
      ports:
        - { protocol: TCP, port: 8081 }
        - { protocol: TCP, port: 8082 }
    # The public ports stay open to everyone; they trust nothing.
    - ports:
        - { protocol: TCP, port: 8080 }
        - { protocol: TCP, port: 2222 }
```

- [ ] **Step 3: Apply and verify all three pods are ready**

```bash
kubectl --context kolomi-cluster apply -f deploy/rustic-git.yaml
kubectl --context kolomi-cluster -n rustic-git rollout status statefulset/rustic-git --timeout=240s
kubectl --context kolomi-cluster -n rustic-git get pods
```

Expected: `rustic-git-0/1/2` all `1/1 Running`.

- [ ] **Step 4: Verify routing works across pods**

Clone through the load balancer several times; every attempt must succeed regardless of which pod answers.

```bash
IP=$(kubectl --context kolomi-cluster -n rustic-git get svc rustic-git-lb -o jsonpath='{.status.loadBalancer.ingress[0].ip}')
kubectl --context kolomi-cluster -n rustic-git exec rustic-git-0 -- rustic-git admin create-repo kloudlite/routed
TOKEN=$(kubectl --context kolomi-cluster -n rustic-git exec rustic-git-0 -- rustic-git admin add-token kloudlite)
for i in 1 2 3 4 5 6; do git ls-remote http://x:$TOKEN@$IP/kloudlite/routed.git >/dev/null && echo "attempt $i ok"; done
```

Expected: six `ok` lines. With three pods behind round robin, at least some attempts are forwarded.

- [ ] **Step 5: Verify exactly one pod holds the repo**

```bash
for p in 0 1 2; do
  echo -n "rustic-git-$p: "
  kubectl --context kolomi-cluster -n rustic-git exec rustic-git-$p -- \
    sh -c 'wget -qO- localhost:8080/healthz' 2>/dev/null || echo "(no wget; check logs instead)"
done
```

Expected: exactly one pod reports a non-zero warm count. **If two pods report warm databases for the same repo, routing is broken** — stop and investigate before proceeding.

- [ ] **Step 6: Verify a rolling restart does not lose requests**

With a clone loop running against the load balancer, restart the StatefulSet and confirm no attempt fails. This is the `preStop` delay earning its keep.

```bash
IP=$(kubectl --context kolomi-cluster -n rustic-git get svc rustic-git-lb -o jsonpath='{.status.loadBalancer.ingress[0].ip}')
( for i in $(seq 1 60); do git ls-remote http://x:$TOKEN@$IP/kloudlite/routed.git >/dev/null 2>&1 && echo ok || echo FAIL; sleep 1; done ) > /tmp/roll.log &
kubectl --context kolomi-cluster -n rustic-git rollout restart statefulset/rustic-git
kubectl --context kolomi-cluster -n rustic-git rollout status statefulset/rustic-git --timeout=300s
wait; sort /tmp/roll.log | uniq -c
```

Expected: 60 `ok`, 0 `FAIL`. One or two `FAIL` during the roll means the `preStop` sleep is too short for this cluster's DNS propagation — raise it, do not accept it.

- [ ] **Step 7: Update the README**

Replace the "Running more than one node" section's routing paragraphs with the new model: a plain round-robin load balancer, ownership by rendezvous hash over the headless Service's endpoints, peer ports 8081/8082 published to no one, and `kubectl scale` with no restart. State the ceiling plainly: scaling moves about 1/N of repos, each costing one cold open, and an in-flight request on a moved repo fails once and is retried.

- [ ] **Step 8: Commit**

```bash
git add deploy/rustic-git.yaml README.md
git commit -m "Front the fleet with a plain load balancer

The nodes route repos to each other now, so the thing in front no longer needs
to understand git: the ingress and its hash-by-path annotations are gone, and a
round-robin Service replaces them. That also covers SSH, which no L4 or L7
balancer could route, because the repo name only appears inside an established
session.

Every forwarded request carries a shared secret. The peer ports are separate
and unpublished, but this cluster enforces no NetworkPolicy, so any pod could
reach them; the secret is defence in depth. A NetworkPolicy is included anyway
for a cluster that does enforce one.

Terminating pods sleep before exiting so their endpoint removal reaches every
node's DNS before they stop answering — shutdown becomes a handover, not a
race."
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| Ownership: rendezvous hash, three candidates deep | 1, 2 |
| Membership from DNS | 2, 6 |
| Failing over past an unreachable candidate — the precedence rule | 2 (`decide`), 4, 5 |
| Hop bound on re-forwarding | 3, 4, 5 |
| Data flow — HTTP reverse proxy | 3, 4 |
| Data flow — SSH byte pipe | 5 |
| Peer authentication, separate listeners | 4, 5, 6 |
| Failure handling | 2, 4, 5 |
| Components table | 1–5 |
| Testing | every task |
| Deployment changes | 7 |

**Known gap, deliberate:** the spec's "propagate cancellation to the owner" for forwarded HTTP requests is not separately implemented — dropping the axum response future drops the reqwest stream, which closes the peer connection and triggers the owner's existing disconnect handling. Verify this during Task 4 by aborting a clone mid-transfer and confirming the owner's log shows the request ending; if it does not, that is a follow-up task, not a silent omission.

**Type consistency:** `Membership::decide` takes a `Fn(&str) -> Future<Output = bool>` and returns `Route`; both call sites (Task 4 `route`, Task 5 `serve_peer_stream` and the SSH `run`) pass a closure over `Forwarder::reachable`. `Forwarder::forward` takes `(peer, owner, hops, Request)` and returns `Result<Response>`, matching its call site. `stream_to_peer` is generic over the stream so the SSH channel and a forwarded TCP socket both fit. `stream_addr` is defined in Task 5 and used in Task 5 and Task 6. `serve_git` is introduced in Task 5 and used by both `serve_peer_stream` and the local SSH path.

**On the precedence rule specifically:** the test `a_lower_ranked_node_forwards_up_when_the_owner_is_reachable` (Task 4) is the one that encodes the requirement that motivated the rule. If it is ever weakened, two nodes can end up holding one repo again.

**Review findings applied** (from the adversarial pass over the first draft):

1. *TCP-accept is not reachability* — a pod mid-shutdown accepts and then dies, reopening the two-writer window. → `reachable()` probes `/healthz`; `preStop` sleep takes a pod out of DNS before it stops answering; Task 7 Step 6 proves a roll loses nothing.
2. *SSH forwarding dropped the channel before sending exit status* — same trap `run` already documents. → `stream_to_peer` borrows the stream; the SSH path sends status, then drops.
3. *Hop count was client-controllable on the public port* — a client could force any node to open any repo. → `trust_nobody` strips it.
4. *Body re-framing untested against real git.* → `a_real_git_push_and_clone_work_through_a_forwarding_node`.
5. *`warm_count` assertions were tautological* with one shared Store. → two Stores over one object store, asserted separately.
6. *NetworkPolicy is not enforced on this cluster* (`networkPolicy: none`, verified). → shared secret on the peer ports, in addition to the separate port.
7. *A not-yet-ready pod is absent from its own DNS answer* and would forward all its repos one rank down, then fence them back. → self is always a member.
