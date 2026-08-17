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
- **A node may serve a repo only if every higher-ranked node is unreachable from two vantage points: its own probe, and one other reachable peer's probe.** With no second vantage available it returns 503. A lower-ranked node never takes a repo on one node's word, including its own. This is the rule that keeps two nodes from holding one repo; every routing decision goes through `Membership::decide`.
- Only a failed **probe** (`GET /healthz` ≠ 200) marks a peer down. A failed **forward** never does — routing runs before authentication, so anything a forward failure could trigger, an unauthenticated client can trigger.
- The "down" memory only skips forward attempts. It never by itself promotes a node to serve; serving as a non-top candidate always re-probes.
- Candidates are the top three **by rank**, then filtered — never the top three that happen to be up.
- Hash on the peer's stable pod **name**, never its IP. A restarted pod must own the same repos.
- A request is forwarded at most twice (`X-Rustic-Git-Hops`, or the hop count in the stream header). A request out of hops is served where it lands. **The public listener strips this header** — a client must not be able to force a node to serve a repo it does not own.
- The peer listeners require `X-Rustic-Git-Peer: <secret>` (HTTP) / the secret in the stream header, from a Kubernetes Secret. Wrong or missing secret → 403, close. **This is in addition to the separate port, not instead of it**: `kolomi-cluster` runs with `networkPolicy: none`, so a NetworkPolicy would be silently accepted and enforce nothing, and pod networking is flat.
- Reachability means the peer's application answers `GET /healthz` **200, with the secret**, not that its kernel accepts a TCP connection. A pod mid-shutdown accepts TCP and then dies. A probe without the secret would be refused by the peer listener and read as "down" — routing would silently collapse to every node serving everything.
- `/healthz` reports the result of a recent object-store round trip. A node whose blob-store client is dead must fail it, or it keeps its repos and 500s forever.
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

/// Every peer, best first. `peers` are stable pod names (`rustic-git-0`), never IPs — a restarted
/// pod must come back owning the same repos. Ties break on the name so the order cannot depend on
/// how DNS happened to order its answer — two nodes resolving the same Service must rank
/// identically.
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

### Task 2: Membership and the routing rule

Wraps the ranking with what needs a clock and a resolver: the SRV-derived peer set (name → address), a short memory of failed probes, and `decide()` — the rule, with its probe functions passed in so every scenario is a unit test with scripted reachability.

**Files:**
- Modify: `src/peers.rs`

**Interfaces:**
- Consumes: `rank`, `CANDIDATES` (Task 1)
- Produces:
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub struct Peer { pub name: String, pub addr: String }` — `addr` is `ip:port` of the peer HTTP port
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub enum Route { Local, Peer(Peer), Unavailable }`
  - `pub struct Membership`
  - `pub fn Membership::new(srv: String, self_name: String) -> Membership`
  - `pub fn Membership::fixed(peers: Vec<Peer>, self_name: String) -> Membership`
  - `pub async fn Membership::peers(&self) -> Vec<Peer>` — current set, cached; **self is a member only if DNS lists it**
  - `pub async fn Membership::decide<P, PF, V, VF>(&self, repo: &str, probe: P, second_vantage: V) -> Route` where `P: Fn(&Peer) -> PF, PF: Future<Output = bool>` (can I reach it?) and `V: Fn(&Peer, &Peer) -> VF, VF: Future<Output = Option<bool>>` (can `via` reach `target`? `None` = could not ask `via`)
  - `pub fn Membership::mark_down(&self, name: &str)`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/peers.rs`:

```rust
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

    /// One-sided partition: first is down FROM HERE but the second vantage can reach it → do not
    /// serve; forward to it. Nothing this node can see by itself tells "down" from "I can't see it".
    #[tokio::test]
    async fn second_does_not_serve_when_a_second_vantage_reaches_the_first() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let n = names(&f);
        let first = rank(&repo, &n)[0].clone();
        let m = Membership::fixed(f.clone(), "rustic-git-1".into());
        // my probe: first is down. second vantage: first is up.
        let fc = first.clone();
        let route = m
            .decide(&repo,
                    move |p: &Peer| std::future::ready(p.name != fc),
                    |_: &Peer, _: &Peer| std::future::ready(Some(true)))
            .await;
        assert_eq!(route, Route::Peer(f.iter().find(|p| p.name == first).unwrap().clone()),
            "a second vantage reaching the owner means we forward, not serve");
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
    /// not become a candidate — the fleet would no longer agree on who the candidates are.
    #[tokio::test]
    async fn candidates_are_top_three_by_rank_not_top_three_that_are_up() {
        let f = fleet(5);
        let repo = repo_where_i_rank(&f, "rustic-git-3", 3);
        let m = Membership::fixed(f.clone(), "rustic-git-3".into());
        let n = names(&f);
        let ranked = rank(&repo, &n);
        for name in &ranked[..3] { m.mark_down(name); }
        // We are rank 4. All three real candidates are marked down. We must still not serve.
        let r = m.decide(&repo, |_: &Peer| std::future::ready(false), |_: &Peer, _: &Peer| std::future::ready(Some(false))).await;
        assert_eq!(r, Route::Unavailable);
    }

    /// The down memory skips forward attempts. It never by itself promotes us to serve: serving as
    /// a non-top candidate always requires a fresh probe and second vantage.
    #[tokio::test]
    async fn a_down_entry_never_promotes_without_a_fresh_probe() {
        let f = fleet(3);
        let repo = repo_where_i_rank(&f, "rustic-git-1", 1);
        let n = names(&f);
        let first = rank(&repo, &n)[0].clone();
        let m = Membership::fixed(f.clone(), "rustic-git-1".into());
        m.mark_down(&first);
        // fresh probe says first is UP → we must forward, memory notwithstanding
        let r = m.decide(&repo, |_: &Peer| std::future::ready(true), |_: &Peer, _: &Peer| std::future::ready(Some(true))).await;
        assert_eq!(r, Route::Peer(f.iter().find(|p| p.name == first).unwrap().clone()));
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
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib peers
```

Expected: FAIL to compile — `Peer`, `Route`, `Membership` not found.

- [ ] **Step 3: Write the implementation**

Add to `src/peers.rs` above the test module:

```rust
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

/// The peer set, and which of them recently failed a probe.
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
    down: Mutex<HashMap<String, Instant>>,
    /// How long a resolved set is reused. This bounds how long two nodes can disagree, so it is
    /// short; the cost is one DNS query per node per interval.
    pub ttl: Duration,
    /// How long a peer that failed a probe is skipped as a forward target.
    pub down_for: Duration,
}

impl Membership {
    pub fn new(srv: String, self_name: String) -> Membership {
        Membership {
            srv,
            self_name,
            cache: Mutex::new(None),
            down: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(2),
            down_for: Duration::from_secs(5),
        }
    }

    /// A fixed set that is never resolved: tests, and a single-node run.
    pub fn fixed(peers: Vec<Peer>, self_name: String) -> Membership {
        let m = Membership::new(String::new(), self_name);
        *m.cache.lock().unwrap() = Some((Instant::now(), peers));
        m
    }

    /// The current set, resolved at most every `ttl`. A failed resolve keeps the previous answer:
    /// DNS being briefly unavailable is not a reason to decide the fleet has no members.
    ///
    /// Self is in the set exactly when DNS says so. An unready pod is absent from DNS, gets no
    /// traffic from anyone, and has nothing to route — adding itself early would only create a
    /// window where it serves repos every other node still routes to the old owner.
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
            Ok(_) => self.cached(),
            Err(e) => {
                eprintln!("resolving {}: {e}", self.srv); // ponytail: eprintln; swap for a logger when one exists
                self.cached()
            }
        }
    }

    fn cached(&self) -> Vec<Peer> {
        self.cache.lock().unwrap().as_ref().map(|(_, p)| p.clone()).unwrap_or_default()
    }

    fn is_down(&self, name: &str) -> bool {
        self.down
            .lock()
            .unwrap()
            .get(name)
            .is_some_and(|at| at.elapsed() < self.down_for)
    }

    /// Remember that a peer failed a probe, so the next request does not re-probe it at once.
    /// This only ever skips forward attempts; see `decide`.
    pub fn mark_down(&self, name: &str) {
        self.down.lock().unwrap().insert(name.to_string(), Instant::now());
    }

    /// Where this request goes.
    ///
    /// > A node may serve a repo only if every higher-ranked node is unreachable from two vantage
    /// > points: its own probe, and one other reachable peer's probe.
    ///
    /// Rank is agreed by every node; reachability is each node's own observation. Two ways a single
    /// observation breaks the invariant, and what this does about each:
    ///
    /// * *Hearsay* — B could not reach A and sent the repo here. We do not serve on B's word: we
    ///   probe A ourselves and forward up if it answers.
    /// * *One-sided partition* — we genuinely cannot reach A, but everyone else can. Nothing we
    ///   observe alone distinguishes that from A being down, so we ask another reachable peer to
    ///   probe A for us, and serve only if it also fails.
    ///
    /// With no second vantage available at all we return `Unavailable`: that is a partition
    /// splitting the fleet, and serving through it is how two writers happen. Safety over
    /// availability, with fencing as the backstop for what two vantages still miss.
    ///
    /// The top candidate has nothing above it and probes nothing — ordinary traffic pays no cost.
    /// A node outside the top `CANDIDATES` is never an owner: it forwards to the first reachable
    /// candidate or reports `Unavailable`.
    ///
    /// `probe(peer)` — can *I* reach it? `second_vantage(via, target)` — can `via` reach `target`?
    /// `None` when `via` itself could not be asked. Both are parameters so the rule is tested with
    /// scripted reachability and no network.
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

        // Everyone ranked above us must be confirmed unreachable before we may serve. Walk down:
        // the first higher-ranked peer that answers is where this goes.
        let above = match my_rank {
            Some(r) => &ranked[..r],
            None => &ranked[..], // not a candidate: never Local
        };
        // Reachable peers we could ask for a second vantage: any candidate or non-candidate that
        // is not the target and answers our probe. Cheapest honest choice: probe as we go.
        let mut reachable_others: Vec<&Peer> = Vec::new();
        for p in peers.iter().filter(|p| p.name != self.self_name) {
            if !ranked.iter().any(|c| c.name == p.name) && !self.is_down(&p.name) && probe(p).await {
                reachable_others.push(p);
            }
        }
        for target in above {
            // A down entry skips the *forward*, but only a fresh probe may promote us past it.
            let up = probe(target).await;
            if up {
                return Route::Peer((*target).clone());
            }
            self.mark_down(&target.name);
            // Second vantage: ask some other reachable peer. Prefer another candidate (it is
            // already in the routing conversation), fall back to any reachable node.
            let mut asked = false;
            let mut confirmed_down = false;
            let candidates_others = ranked.iter().filter(|p| p.name != target.name && p.name != self.self_name);
            for via in candidates_others.chain(reachable_others.iter().copied()) {
                match second_vantage(via, target).await {
                    Some(true) => return Route::Peer((*target).clone()), // they can reach it: forward
                    Some(false) => { asked = true; confirmed_down = true; break; }
                    None => continue, // could not ask this one
                }
            }
            if !asked || !confirmed_down {
                return Route::Unavailable; // no second vantage: do not serve through a split
            }
            // confirmed down from two vantages: keep walking down the ranks
        }
        match my_rank {
            Some(_) => Route::Local,
            None => Route::Unavailable,
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
    // `<pod>.<svc>.<ns>.svc.cluster.local`, whose first label is the stable pod name.
    let (svc, port) = srv
        .strip_prefix("_peer._tcp.")
        .and_then(|rest| rest.rsplit_once(':'))
        .ok_or_else(|| crate::err("srv must look like _peer._tcp.<svc>.<ns>.svc.cluster.local:<port>"))?;
    for addr in tokio::net::lookup_host(format!("{svc}:{port}")).await? {
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
```

Add `dns-lookup = "2"` to `Cargo.toml` `[dependencies]` — a thin, dependency-free wrapper over `getnameinfo`. This is the one new dependency; it replaces a much larger SRV resolver.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib peers
```

Expected: PASS, 17 tests (6 from Task 1, 11 here).

- [ ] **Step 5: Commit**

```bash
git add src/peers.rs Cargo.toml Cargo.lock
git commit -m "Decide who serves a repo: two vantage points before a lower rank takes over

Membership comes from the headless Service, hashed on the stable pod name so a
restarted pod comes back owning the same repos. Self is a member only when DNS
lists it; an unready pod gets no traffic and has nothing to route.

The rule: a node may serve a repo only if every higher-ranked node is
unreachable from two vantage points, its own probe and one other reachable
peer's. One observation cannot tell 'A is down' from 'I cannot see A', and
acting on it is how a one-sided partition produces two writers. With no second
vantage available the answer is 503 rather than serve: that is a split fleet,
and serving through it is the failure this design exists to prevent. Fencing
stays as the backstop for what two vantages miss."
```

---

### Task 3: HTTP forwarding and probes

Forwards one request to a peer, streams the response, and provides the two probes `decide()` needs. Tested against a stub peer, no DNS or hashing involved.

**Files:**
- Create: `src/proxy.rs`
- Modify: `src/lib.rs` (add `pub mod proxy;`), `Cargo.toml`
- Test: `tests/proxy.rs`

**Interfaces:**
- Produces:
  - `pub const OWNER_HEADER: &str = "x-rustic-git-owner"`, `pub const HOPS_HEADER: &str = "x-rustic-git-hops"`, `pub const PEER_HEADER: &str = "x-rustic-git-peer"`, `pub const MAX_HOPS: u32 = 2`
  - `pub struct Forwarder { pub client: reqwest::Client, pub secret: String }`
  - `pub fn Forwarder::new(secret: String) -> Forwarder`
  - `pub async fn Forwarder::reachable(&self, addr: &str) -> bool` — `GET /healthz` **with the secret** returned 200
  - `pub async fn Forwarder::probe_via(&self, via_addr: &str, target_name: &str) -> Option<bool>` — `GET /probe?peer=<name>` on `via`; `Some(true/false)` from its body, `None` if `via` did not answer
  - `pub async fn Forwarder::forward(&self, addr: &str, owner: &str, hops: u32, req: axum::extract::Request) -> Result<axum::response::Response>`

- [ ] **Step 1: Add the dependency**

`Cargo.toml` `[dependencies]`: `reqwest = { version = "0.13", default-features = false, features = ["stream"] }` (already in the lock file via object_store; peer traffic is plain HTTP in-cluster, no TLS feature needed).

- [ ] **Step 2: Write the failing tests**

Create `tests/proxy.rs`:

```rust
//! Forwarding and probing against a stub peer, so these cover the mechanics only.
use axum::{routing::{any, get}, Router};
use rustic_git::proxy::{Forwarder, HOPS_HEADER, OWNER_HEADER, PEER_HEADER};

const SECRET: &str = "s3cret";

/// A stub peer: /healthz and /probe behave like a real peer listener (secret required); anything
/// else echoes what crossed the wire.
async fn stub_peer(probe_answer: bool) -> String {
    let guard = |h: &axum::http::HeaderMap| h.get(PEER_HEADER).and_then(|v| v.to_str().ok()) == Some(SECRET);
    let app = Router::new()
        .route("/healthz", get(move |h: axum::http::HeaderMap| async move {
            if guard(&h) { (axum::http::StatusCode::OK, "ok") } else { (axum::http::StatusCode::FORBIDDEN, "") }
        }))
        .route("/probe", get(move |h: axum::http::HeaderMap| async move {
            if guard(&h) { (axum::http::StatusCode::OK, if probe_answer { "up" } else { "down" }) } else { (axum::http::StatusCode::FORBIDDEN, "") }
        }))
        .route("/{*rest}", any(|h: axum::http::HeaderMap, body: axum::body::Bytes| async move {
            let g = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("none").to_string();
            format!("owner={} hops={} peer={} expect={} te={} len={}",
                g(OWNER_HEADER), g(HOPS_HEADER), g(PEER_HEADER), g("expect"), g("transfer-encoding"), body.len())
        }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// Forwarding carries the identity, the hop count, and the secret; strips hop-by-hop headers so
/// each hop frames its own body (git sends Expect: 100-continue on pushes over 1 MiB, and passing
/// it through to a peer that then frames differently is a mismatch a small test never hits).
#[tokio::test]
async fn forwarding_carries_identity_and_strips_hop_by_hop_headers() {
    let peer = stub_peer(true).await;
    let f = Forwarder::new(SECRET.into());
    let big = vec![b'x'; 2 * 1024 * 1024];
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/alice/web/git-receive-pack")
        .header("expect", "100-continue")
        .header("transfer-encoding", "chunked")
        .header("connection", "keep-alive")
        .body(axum::body::Body::from(big.clone()))
        .unwrap();
    let res = f.forward(&peer, "alice", 0, req).await.unwrap();
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&body),
        format!("owner=alice hops=1 peer={SECRET} expect=none te=none len={}", big.len()));
}

/// Reachability requires the application to answer 200 — with the secret, or a real peer listener
/// would refuse the probe and every peer would look down. A socket that accepts and closes is not
/// reachable: a pod mid-shutdown does exactly that.
#[tokio::test]
async fn reachable_requires_a_200_from_the_application() {
    let f = Forwarder::new(SECRET.into());
    assert!(f.reachable(&stub_peer(true).await).await);
    assert!(!Forwarder::new("wrong".into()).reachable(&stub_peer(true).await).await, "wrong secret must read as down, loudly");
    assert!(!f.reachable("127.0.0.1:1").await);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { loop { let (s, _) = l.accept().await.unwrap(); drop(s); } });
    assert!(!f.reachable(&addr).await, "accept-then-close is not reachable");
}

/// The second vantage: ask a peer whether it can reach a third. Distinguishes "they said no" from
/// "we could not ask them", because only the former is evidence.
#[tokio::test]
async fn probe_via_distinguishes_no_from_could_not_ask() {
    let f = Forwarder::new(SECRET.into());
    assert_eq!(f.probe_via(&stub_peer(true).await, "rustic-git-0").await, Some(true));
    assert_eq!(f.probe_via(&stub_peer(false).await, "rustic-git-0").await, Some(false));
    assert_eq!(f.probe_via("127.0.0.1:1", "rustic-git-0").await, None);
}
```

- [ ] **Step 3: Run to verify it fails**

`cargo test --test proxy` → FAIL to compile, `unresolved import rustic_git::proxy`.

- [ ] **Step 4: Implement**

Create `src/proxy.rs`:

```rust
//! Forwarding a request to the node that owns the repo, and the probes routing needs.
//!
//! Two forwarding shapes, because the two client protocols are not the same shape. An HTTP request
//! is one request and one response, so it is reverse-proxied. An SSH session is a stream carrying
//! an advertisement and then repeated commands, so it is piped (see `stream`).

use crate::Result;
use std::time::Duration;

/// Identity of the client the *forwarding* node authenticated. Honoured only on the peer listener.
pub const OWNER_HEADER: &str = "x-rustic-git-owner";
/// How many times this request has been forwarded. Bounds re-forwarding.
pub const HOPS_HEADER: &str = "x-rustic-git-hops";
/// Shared secret on every peer request. The peer ports are separate and unpublished, but this
/// cluster runs with `networkPolicy: none`, so any pod can reach them; this is defence in depth on
/// top of the port, not instead of it.
pub const PEER_HEADER: &str = "x-rustic-git-peer";
/// Candidates are three deep, so two forwards reach the last of them. Past this, serve here.
pub const MAX_HOPS: u32 = 2;

/// Probes are in-cluster; generous for that, tight enough to fail over quickly.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Headers that describe one hop, not the message. Forwarded verbatim they mislead the next hop:
/// git sends `Expect: 100-continue` on pushes over 1 MiB, and `Transfer-Encoding` describes *our*
/// framing, not the peer's. Stripped in both directions; each hop frames its own body.
const HOP_BY_HOP: &[&str] = &[
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer",
    "transfer-encoding", "upgrade", "expect", "content-length", "host",
];

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

    /// Whether a peer's *application* answers right now.
    ///
    /// `GET /healthz` with the secret, expecting 200. Not a bare TCP connect: a pod mid-shutdown
    /// still accepts TCP for a moment before it dies, and treating that as reachable is how two
    /// nodes end up holding one repo. The secret matters too — the peer listener refuses requests
    /// without it, and a refused probe would read as "down" for every peer, collapsing routing to
    /// every node serving everything.
    pub async fn reachable(&self, addr: &str) -> bool {
        let r = self
            .client
            .get(format!("http://{addr}/healthz"))
            .header(PEER_HEADER, &self.secret)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await;
        matches!(r, Ok(r) if r.status().is_success())
    }

    /// The second vantage: can `via` reach `target`? `None` if `via` itself did not answer — that
    /// is not evidence about `target` either way.
    pub async fn probe_via(&self, via_addr: &str, target_name: &str) -> Option<bool> {
        let r = self
            .client
            .get(format!("http://{via_addr}/probe"))
            .query(&[("peer", target_name)])
            .header(PEER_HEADER, &self.secret)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        match r.text().await.ok()?.trim() {
            "up" => Some(true),
            "down" => Some(false),
            _ => None,
        }
    }

    /// Send this request to `addr` and stream its response back, one hop further along.
    pub async fn forward(
        &self,
        addr: &str,
        owner: &str,
        hops: u32,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response> {
        use axum::body::Body;
        let (parts, body) = req.into_parts();
        let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let mut headers = parts.headers.clone();
        for h in HOP_BY_HOP {
            headers.remove(*h);
        }
        headers.insert(OWNER_HEADER, owner.parse()?);
        headers.insert(HOPS_HEADER, (hops + 1).to_string().parse()?);
        headers.insert(PEER_HEADER, self.secret.parse()?);

        let upstream = self
            .client
            .request(parts.method, format!("http://{addr}{path}"))
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await?;

        let mut out = axum::response::Response::builder().status(upstream.status());
        for (k, v) in upstream.headers() {
            if !HOP_BY_HOP.contains(&k.as_str()) {
                out = out.header(k, v);
            }
        }
        Ok(out.body(Body::from_stream(upstream.bytes_stream()))?)
    }
}
```

Add `pub mod proxy;` to `src/lib.rs`.

- [ ] **Step 5: Run to verify it passes** — `cargo test --test proxy` → PASS, 3 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/proxy.rs src/lib.rs tests/proxy.rs
git commit -m "Forward HTTP requests to a peer, and probe peers for routing

Reachability is a 200 from the peer's /healthz, carrying the secret. Not a
bare TCP connect: a pod mid-shutdown accepts for a moment and then dies. Not
without the secret: the peer listener would refuse the probe, every peer would
read as down, and routing would silently collapse to every node serving
everything.

probe_via asks one peer whether it can reach another — the second vantage that
tells 'A is down' from 'I cannot see A'. Hop-by-hop headers are stripped both
ways so each hop frames its own body; git sends Expect: 100-continue on pushes
over 1 MiB and forwarding it verbatim is a framing mismatch."
```

---

### Task 4: Route HTTP requests, both listeners

**Files:**
- Modify: `src/http.rs`, `src/lib.rs`, `src/store.rs`
- Test: `tests/routing.rs`
- Modify: `tests/common/mod.rs`, `tests/http_e2e.rs`, `tests/ssh_e2e.rs`

**Interfaces:**
- Consumes: `peers::{Membership, Peer, Route}`, `proxy::{Forwarder, ...}`
- Produces:
  - `App { store, peers: Arc<Membership>, forwarder: Arc<Forwarder> }`, `App::new(store, peers, peer_secret)`
  - `pub fn http::router(app) -> Router` — public: strip routing headers, then route
  - `pub fn http::peer_router(app) -> Router` — peer: require secret; `/healthz`, `/probe`; then route again (bounded by hops); honour identity
  - `#[derive(Clone)] pub struct http::Trusted(pub Option<String>)`
  - `pub async fn App::route(&self, repo: &str) -> Route` — `decide()` wired to the forwarder's probes; the one place routing is invoked
  - `Store::healthy(&self) -> bool` — result of a recent object-store round trip

- [ ] **Step 1: Write the failing tests**

Create `tests/routing.rs`. The helpers first:

```rust
//! Which node ends up serving a repo, and who may claim an identity.
mod common;

use rustic_git::peers::{Membership, Peer};
use rustic_git::App;
use std::sync::Arc;

const SECRET: &str = "test-peer-secret";

/// One node's own Store over a shared object store, so each node has its own pool and the test can
/// see which node opened a repo. One shared Store would mean one shared pool, and "exactly one
/// opener" could never fail.
struct Node {
    store: Arc<rustic_git::store::Store>,
    public: String,
    peer: String,
    _tmp: tempfile::TempDir,
}

/// Bring up a node named `name`. `fleet` is every node's (name, peer addr) — pass the same list to
/// every node so they agree; a node's own entry is what makes it Local for repos it ranks first on.
async fn node(os: Arc<dyn slatedb::object_store::ObjectStore>, name: &str, fleet: &[(String, String)]) -> Node {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(rustic_git::store::Store::open(os, tmp.path().join("cache"), false).await.unwrap());
    let peers: Vec<Peer> = fleet.iter().map(|(n, a)| Peer { name: n.clone(), addr: a.clone() }).collect();
    let app = Arc::new(App::new(store.clone(), Arc::new(Membership::fixed(peers, name.into())), SECRET.into()));
    let pub_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public = pub_l.local_addr().unwrap().to_string();
    // The peer listener must be at the address the fleet was told, or probes go nowhere.
    let my_addr = fleet.iter().find(|(n, _)| n == name).map(|(_, a)| a.clone()).expect("node must be in its own fleet");
    let peer_l = tokio::net::TcpListener::bind(&my_addr).await.unwrap();
    let a2 = app.clone();
    tokio::spawn(async move { axum::serve(pub_l, rustic_git::http::router(a2)).await.unwrap() });
    tokio::spawn(async move { axum::serve(peer_l, rustic_git::http::peer_router(app)).await.unwrap() });
    Node { store, public, peer: my_addr, _tmp: tmp }
}

/// Reserve N loopback ports up front so a fleet can be described before any node starts.
fn reserve_ports(n: usize) -> Vec<String> {
    (0..n).map(|_| {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    }).collect()
}

fn fleet_of(names: &[&str]) -> Vec<(String, String)> {
    names.iter().cloned().map(String::from).zip(reserve_ports(names.len())).collect()
}

/// A repo whose top-ranked node in `fleet` is `want`.
fn repo_owned_by(fleet: &[(String, String)], want: &str) -> String {
    let names: Vec<String> = fleet.iter().map(|(n, _)| n.clone()).collect();
    (0..500).map(|i| format!("alice/w{i}")).find(|r| rustic_git::peers::rank(r, &names)[0] == want).unwrap()
}

async fn client() -> reqwest::Client { reqwest::Client::new() }
```

Then the tests:

```rust
/// A claimed identity on the public port must be ignored: this is the bypass a client would try.
#[tokio::test]
async fn the_public_listener_ignores_a_claimed_identity() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet_of(&["a"]);
    let a = node(e.store.os.clone(), "a", &f).await;
    let res = client().await
        .get(format!("http://{}/alice/web/info/refs?service=git-upload-pack", a.public))
        .header(rustic_git::proxy::OWNER_HEADER, "alice")
        .send().await.unwrap();
    assert_eq!(res.status(), 401);
}

/// A claimed hop count on the public port must be ignored: honouring it would let a client force
/// any node to open any repo and fence the owner. B ranks second behind a *reachable* A; if hops
/// were honoured B would serve, so B's pool warm means the bug.
#[tokio::test]
async fn the_public_listener_ignores_a_claimed_hop_count() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let b = node(e.store.os.clone(), "b", &f).await;
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .header(rustic_git::proxy::HOPS_HEADER, rustic_git::proxy::MAX_HOPS.to_string())
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.store.pool.warm_count(), 1, "A served");
    assert_eq!(b.store.pool.warm_count(), 0, "B forwarded — hops from a client are stripped");
}

/// The peer listener requires the secret. Missing or wrong → 403 before anything else.
#[tokio::test]
async fn the_peer_listener_requires_the_secret() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet_of(&["a"]);
    let a = node(e.store.os.clone(), "a", &f).await;
    for wrong in [None, Some("nope")] {
        let mut r = client().await
            .get(format!("http://{}/alice/web/info/refs?service=git-upload-pack", a.peer))
            .header(rustic_git::proxy::OWNER_HEADER, "alice").header("git-protocol", "version=2");
        if let Some(w) = wrong { r = r.header(rustic_git::proxy::PEER_HEADER, w); }
        assert_eq!(r.send().await.unwrap().status(), 403, "secret {wrong:?}");
    }
}

/// With the secret, the peer listener honours the forwarded identity, and its /healthz and /probe
/// answer — the probes routing depends on. Without the secret they refuse, so a probe that forgot
/// it would read every peer as down.
#[tokio::test]
async fn the_peer_listener_serves_probes_and_honours_identity() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet_of(&["a"]);
    let a = node(e.store.os.clone(), "a", &f).await;
    let c = client().await;
    let ok = |p: &str| format!("http://{}{p}", a.peer);
    assert_eq!(c.get(ok("/healthz")).header(rustic_git::proxy::PEER_HEADER, SECRET).send().await.unwrap().status(), 200);
    assert_eq!(c.get(ok("/healthz")).send().await.unwrap().status(), 403);
    let res = c.get(ok("/alice/web/info/refs?service=git-upload-pack"))
        .header(rustic_git::proxy::OWNER_HEADER, "alice").header(rustic_git::proxy::PEER_HEADER, SECRET)
        .header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 200);
    // /probe: a reports whether it can reach a named peer. It can reach itself.
    let body = c.get(ok("/probe")).query(&[("peer", "a")]).header(rustic_git::proxy::PEER_HEADER, SECRET)
        .send().await.unwrap().text().await.unwrap();
    assert_eq!(body.trim(), "up");
}

/// Hearsay, end to end. C is sent a request (hops=1) for a repo A owns, as if some node could not
/// reach A. C *can* reach A, so it must forward there — only A's pool opens the repo.
#[tokio::test]
async fn a_lower_ranked_node_forwards_up_when_the_owner_is_reachable() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "c"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let c = node(e.store.os.clone(), "c", &f).await;
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", c.peer))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .header(rustic_git::proxy::HOPS_HEADER, "1").header(rustic_git::proxy::PEER_HEADER, SECRET)
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.store.pool.warm_count(), 1, "A must have served it");
    assert_eq!(c.store.pool.warm_count(), 0, "C must not have opened it — the whole rule");
}

/// Out of hops: served where it lands, so a disagreement can never bounce a request forever. The
/// higher-ranked peer IS reachable here, so only the hop bound explains B serving.
#[tokio::test]
async fn a_request_out_of_hops_is_served_locally() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let b = node(e.store.os.clone(), "b", &f).await;
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.peer))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .header(rustic_git::proxy::HOPS_HEADER, rustic_git::proxy::MAX_HOPS.to_string())
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(b.store.pool.warm_count(), 1, "out of hops: B serves, does not bounce");
    assert_eq!(a.store.pool.warm_count(), 0);
}

/// One-sided partition, end to end: B cannot reach A (A's peer port is not listening from B's
/// point of view — we simulate by giving B a fleet where A's addr is a dead port) but a third node
/// C can. B must ask C, learn A is up, and NOT serve. Instead: 503, because from B's own view A is
/// down and B forwarding to a peer it cannot reach is impossible. The client retries elsewhere.
#[tokio::test]
async fn a_one_sided_partition_yields_503_not_a_second_writer() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    // Real fleet: a, b, c all reachable. B's private view: a is at a dead port.
    let real = fleet_of(&["a", "b", "c"]);
    let repo = repo_owned_by(&real, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &real).await;
    let c = node(e.store.os.clone(), "c", &real).await;
    let mut b_view = real.clone();
    b_view.iter_mut().find(|(n, _)| n == "a").unwrap().1 = "127.0.0.1:1".into(); // B cannot reach A
    let b = node(e.store.os.clone(), "b", &b_view).await;
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 503, "B: cannot reach A, but C can → B must not serve");
    assert_eq!(a.store.pool.warm_count(), 0);
    assert_eq!(b.store.pool.warm_count(), 0, "B must NOT open the repo");
    assert_eq!(c.store.pool.warm_count(), 0);
}

/// Genuine outage, end to end: A is down from everyone. B (second) asks C, C agrees, B serves.
#[tokio::test]
async fn a_confirmed_outage_lets_the_second_candidate_serve() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b", "c"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    // A is never started: its reserved port is closed.
    let b = node(e.store.os.clone(), "b", &f).await;
    let _c = node(e.store.os.clone(), "c", &f).await;
    let names: Vec<String> = f.iter().map(|(n, _)| n.clone()).collect();
    let second = rustic_git::peers::rank(&repo, &names)[1].clone();
    let target = if second == "b" { &b } else { /* c is second; send there instead */ &b };
    // Send to whichever of b/c ranks second; the other is the second vantage.
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", target.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert!(res.status() == 200 || res.status() == 503,
        "either the second candidate served (200) or the third forwarded to it and it served");
    // Whatever the path, at most one node opened it.
    let opened = [b.store.pool.warm_count(), _c.store.pool.warm_count()].iter().sum::<usize>();
    assert!(opened <= 1, "at most one opener, got {opened}");
}

/// A real git push and clone through a forwarding node. Push is over 1 MiB so Expect:
/// 100-continue and chunked framing are exercised.
#[tokio::test]
async fn a_real_git_push_and_clone_work_through_a_forwarding_node() {
    if !common::have_git() { return; }
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let b = node(e.store.os.clone(), "b", &f).await;
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let url = format!("http://x:{token}@{}/{repo}.git", b.public);
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git").args(args).current_dir(dir)
            .env("GIT_AUTHOR_NAME","t").env("GIT_AUTHOR_EMAIL","t@t").env("GIT_COMMITTER_NAME","t").env("GIT_COMMITTER_EMAIL","t@t")
            .output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init","-q","-b","main"]);
    std::fs::write(work.join("big"), vec![b'z'; 3 * 1024 * 1024]).unwrap();
    git(&work, &["add","big"]); git(&work, &["commit","-qm","big"]);
    git(&work, &["push","-q",&url,"main"]);
    let clone = tmp.path().join("clone");
    git(tmp.path(), &["clone","-q",&url,clone.to_str().unwrap()]);
    assert_eq!(std::fs::metadata(clone.join("big")).unwrap().len(), 3 * 1024 * 1024);
    assert_eq!(a.store.pool.warm_count(), 1);
    assert_eq!(b.store.pool.warm_count(), 0);
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test --test routing` → compile errors (`App::new` arity, `peer_router`).

- [ ] **Step 3: `App` and `App::route`**

In `src/lib.rs`:

```rust
pub struct App {
    pub store: std::sync::Arc<store::Store>,
    pub peers: std::sync::Arc<peers::Membership>,
    pub forwarder: std::sync::Arc<proxy::Forwarder>,
}

impl App {
    pub fn new(store: std::sync::Arc<store::Store>, peers: std::sync::Arc<peers::Membership>, peer_secret: String) -> Self {
        App { store, peers, forwarder: std::sync::Arc::new(proxy::Forwarder::new(peer_secret)) }
    }

    /// The routing decision for a repo, with the real probes wired in. The one place `decide` is
    /// called, so every route — HTTP public, HTTP peer, SSH, peer stream — applies the same rule.
    pub async fn route(&self, repo: &str) -> peers::Route {
        let f = self.forwarder.clone();
        let f2 = self.forwarder.clone();
        self.peers
            .decide(
                repo,
                move |p: &peers::Peer| { let f = f.clone(); let a = p.addr.clone(); async move { f.reachable(&a).await } },
                move |via: &peers::Peer, t: &peers::Peer| { let f = f2.clone(); let a = via.addr.clone(); let n = t.name.clone(); async move { f.probe_via(&a, &n).await } },
            )
            .await
    }
}
```

- [ ] **Step 4: `Store::healthy`**

In `src/store.rs`, add a field and method. `/healthz` must mean healthy or a node with a dead object-store connection keeps its repos forever:

```rust
    /// Whether the object store answered recently. Sampled by a background task; read by /healthz.
    pub healthy: std::sync::atomic::AtomicBool,
```

Initialise `healthy: std::sync::atomic::AtomicBool::new(true)` in `Store::open`, and add:

```rust
    /// Probe the object store every few seconds and record the result. Reachability and liveness
    /// both key off /healthz, so a node whose blob-store client is dead must fail it — otherwise
    /// it keeps its repos and returns 500 to every client with no failover and no restart.
    pub fn spawn_health_probe(self: &Arc<Self>) {
        let s = self.clone();
        tokio::spawn(async move {
            loop {
                let ok = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    s.os.head(&OsPath::from("auth/.health")),
                )
                .await
                .map(|r| !matches!(r, Err(slatedb::object_store::Error::Generic { .. })))
                .unwrap_or(false);
                // NotFound is a healthy answer: the store spoke. Only transport-level failures count.
                s.healthy.store(ok, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }
    pub fn healthy(&self) -> bool { self.healthy.load(std::sync::atomic::Ordering::Relaxed) }
```

- [ ] **Step 5: `src/http.rs` — routers, middleware, handlers**

Replace `healthz` and `router`, and add:

```rust
/// Liveness/readiness, and what peers probe. 503 when the object store has stopped answering.
async fn healthz(State(app): State<Arc<App>>) -> Response {
    if !app.store.healthy() {
        return (StatusCode::SERVICE_UNAVAILABLE, "object store unreachable").into_response();
    }
    (StatusCode::OK, format!("ok ({} warm)", app.store.pool.warm_count())).into_response()
}

/// The second vantage: can *this* node reach the named peer? Peer listener only.
async fn probe(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(name) = q.get("peer") else { return (StatusCode::BAD_REQUEST, "peer=").into_response(); };
    let peers = app.peers.peers().await;
    let Some(p) = peers.iter().find(|p| &p.name == name) else { return (StatusCode::OK, "down").into_response(); };
    let up = app.forwarder.reachable(&p.addr).await;
    (StatusCode::OK, if up { "up" } else { "down" }).into_response()
}

/// Identity established by a *peer*. `None` on the public listener, always.
#[derive(Clone)]
pub struct Trusted(pub Option<String>);

fn repo_of(path: &str) -> Option<String> {
    let mut it = path.trim_start_matches('/').split('/');
    let (owner, name, rest) = (it.next()?, it.next()?, it.next()?);
    if !matches!(rest, "info" | "git-upload-pack" | "git-receive-pack") { return None; }
    let (owner, name) = crate::protocol::parse_repo_path(&format!("{owner}/{name}"))?;
    Some(format!("{owner}/{name}"))
}

/// Route before handling. Runs ahead of authentication: the damage is done by *opening* a repo's
/// database on the wrong node, so a misrouted request must never reach the handlers. Applied to
/// both listeners — a node receiving a forwarded request re-checks the nodes above it from its own
/// vantage point (and one other's), bounded by the hop count.
async fn route(State(app): State<Arc<App>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let Some(repo) = repo_of(req.uri().path()) else { return next.run(req).await; };
    let hops: u32 = req.headers().get(crate::proxy::HOPS_HEADER).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
        .unwrap_or(crate::proxy::MAX_HOPS); // unparseable = exhausted: serve here, do not bounce
    if hops >= crate::proxy::MAX_HOPS { return next.run(req).await; }
    match app.route(&repo).await {
        crate::peers::Route::Local => next.run(req).await,
        crate::peers::Route::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "no node may safely serve this repository right now; retry").into_response(),
        crate::peers::Route::Peer(peer) => {
            let owner = req.extensions().get::<Trusted>().and_then(|t| t.0.clone()).unwrap_or_default();
            match app.forwarder.forward(&peer.addr, &owner, hops, req).await {
                Ok(res) => res,
                // A failed forward is NOT a failed probe: do not mark the peer down. Routing runs
                // before auth, so anything a forward failure could trigger, anyone could trigger —
                // e.g. push half a body and abort to demote the owner.
                Err(e) => { eprintln!("forwarding {repo} to {}: {e}", peer.name); (StatusCode::BAD_GATEWAY, "peer error").into_response() }
            }
        }
    }
}

/// Peer listener admission: the secret, then the identity the caller established.
async fn trust_peer(State(app): State<Arc<App>>, mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let presented = req.headers().get(crate::proxy::PEER_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
    // ponytail: plain compare; the secret is 64 hex chars and the port needs network reach. Use
    // subtle::ConstantTimeEq if this port is ever exposed more widely.
    if presented.is_empty() || presented != app.forwarder.secret {
        return (StatusCode::FORBIDDEN, "peer secret").into_response();
    }
    let owner = req.headers().get(crate::proxy::OWNER_HEADER).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(str::to_string);
    req.extensions_mut().insert(Trusted(owner));
    next.run(req).await
}

/// Public listener: strip every routing header a client could set. Hops especially — a client
/// that could set it to the maximum would force this node to open a repo it does not own.
async fn trust_nobody(mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    for h in [crate::proxy::HOPS_HEADER, crate::proxy::OWNER_HEADER, crate::proxy::PEER_HEADER] { req.headers_mut().remove(h); }
    req.extensions_mut().insert(Trusted(None));
    next.run(req).await
}

fn git_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/{owner}/{name}/info/refs", get(info_refs))
        .route("/{owner}/{name}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{name}/git-receive-pack", post(receive_pack))
        .layer(axum::extract::DefaultBodyLimit::max(max_body()))
}

/// Client-facing. Layers run outermost-first, and the LAST `.layer()` call is outermost — so
/// `trust_nobody` (added last) runs first, then `route`, then the handler.
pub fn router(app: Arc<App>) -> Router {
    git_routes()
        .route("/healthz", get(healthz))
        .layer(axum::middleware::from_fn_with_state(app.clone(), route))
        .layer(axum::middleware::from_fn(trust_nobody))
        .with_state(app)
}

/// Peer-facing. `trust_peer` outermost (secret check first, on everything including probes), then
/// `route`, then handlers. `/healthz` and `/probe` are inside the secret check on purpose: a probe
/// without the secret must fail loudly (403), not silently succeed and hide a misconfiguration.
pub fn peer_router(app: Arc<App>) -> Router {
    git_routes()
        .route("/healthz", get(healthz))
        .route("/probe", get(probe))
        .layer(axum::middleware::from_fn_with_state(app.clone(), route))
        .layer(axum::middleware::from_fn_with_state(app.clone(), trust_peer))
        .with_state(app)
}
```

Then, in `open()`, take `trusted: &Trusted` and use it exactly as in the earlier draft: if `trusted.0` is `Some(o)`, that is `auth_owner`; otherwise decode Basic auth. Add `axum::Extension(trusted): axum::Extension<Trusted>` to `info_refs`, `upload_pack`, `receive_pack` (before the body extractor) and pass `&trusted` to `open`.

- [ ] **Step 6: Fenced handle re-routes instead of blindly reopening**

In `src/pool.rs::get`, the fenced-then-reopen path stays, but the *callers* that hit a fenced error must re-route. Simplest correct place: in `open()` in `src/http.rs`, after `open_repo` returns a fenced error (`e.to_string().contains("fenced")` — SlateDB's `ErrorKind::Fenced`), call `app.route(&repo)`; if `Local`, retry once; else return 503. Add a comment: "Under routing, fenced almost always means another node believes it owns this. Reopening blindly takes it straight back and is the amplifier that turns any disagreement into a flap."

- [ ] **Step 7: Fix call sites** — `tests/common/mod.rs` gets `pub fn app(store) -> Arc<App>` using `Membership::fixed(vec![Peer{name:"solo".into(), addr:"127.0.0.1:1".into()}], "solo".into())` and secret `"test-peer-secret"`; `tests/http_e2e.rs` and `tests/ssh_e2e.rs` use it.

- [ ] **Step 8: Run** — `cargo test --release` → PASS, 9 new routing tests plus all existing.

- [ ] **Step 9: Commit**

```bash
git add src/http.rs src/lib.rs src/store.rs src/pool.rs tests/routing.rs tests/common/mod.rs tests/http_e2e.rs tests/ssh_e2e.rs
git commit -m "Route each repo before handling it, on both listeners

Routing runs ahead of authentication because the damage is done by opening a
repo's database on the wrong node. Both listeners route: a node receiving a
forwarded request re-checks the nodes ranked above it from its own vantage
point and one other's, so a lower rank never takes a repo on one node's word.
A hop count bounds the chain; the public listener strips it, since a client
that could set it would force any node to open any repo.

A failed forward never marks a peer down — only a failed probe does. Routing
runs before auth, so anything a forward failure could trigger, an
unauthenticated client could trigger. /healthz reports the object store's
health, because reachability keys off it and a node with a dead store must
not keep its repos. On a fence the node re-routes rather than reopening: under
routing, fenced means someone else believes they own this."
```

---

### Task 5: Peer stream listener and SSH forwarding

**Files:**
- Modify: `src/proxy.rs`, `src/ssh.rs`
- Test: `tests/routing.rs`, `tests/common/mod.rs` (add `have_ssh()`)

**Interfaces:**
- Produces:
  - `pub async fn proxy::serve_peer_streams(app: Arc<App>, listener: TcpListener) -> Result<()>`
  - `pub async fn proxy::stream_to_peer<S>(secret, peer_stream_addr, service, repo, owner, hops, stream: &mut S) -> Result<()>` — sends header, **waits for the status line**, then pipes. `Err` carries the owner's `error:` reason.
  - `pub fn proxy::stream_addr(http_peer_addr: &str) -> String`
  - `pub async fn ssh::serve_git<S>(store, repo, service, stream: S) -> Result<()>`

- [ ] **Step 1: Failing tests** — append to `tests/routing.rs`:

```rust
async fn stream_listener(store: Arc<rustic_git::store::Store>) -> String {
    let app = common::app(store);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { rustic_git::proxy::serve_peer_streams(app, l).await });
    addr
}

/// A whole session on one stream: header, "ok", advertisement, then a command. hops=2 so this
/// node serves rather than routing again.
#[tokio::test]
async fn a_peer_stream_serves_a_whole_session() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let addr = stream_listener(e.store.clone()).await;
    let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
    sock.write_all(b"test-peer-secret git-upload-pack alice/web alice 2\n").await.unwrap();
    let mut r = BufReader::new(sock);
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim(), "ok", "status line first");
    line.clear(); r.read_line(&mut line).await.unwrap();
    assert!(line.contains("version 2"), "then the advertisement: {line:?}");
    // then a command round-trip: ls-refs
    let cmd = "0014command=ls-refs\n0000";
    r.get_mut().write_all(cmd.as_bytes()).await.unwrap();
    line.clear(); r.read_line(&mut line).await.unwrap();
    assert!(!line.is_empty(), "ls-refs must get a response on the same stream");
}

/// Refusals are reported as a status line, so the forwarding node can give the client a real exit
/// status and reason instead of a silent exit 0.
#[tokio::test]
async fn a_peer_stream_reports_refusals_as_a_status_line() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let addr = stream_listener(e.store.clone()).await;
    for (hdr, want) in [
        ("test-peer-secret git-upload-pack alice/web mallory 2\n", "error: access denied"),
        ("test-peer-secret git-upload-pack alice/nope alice 2\n", "error: repository not found"),
        ("test-peer-secret git-frobnicate alice/web alice 2\n", "error: unsupported service"),
        ("test-peer-secret git-upload-pack alice/web al ice 2\n", "error: invalid owner"),
    ] {
        let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
        sock.write_all(hdr.as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut sock).read_line(&mut line).await.unwrap();
        assert!(line.starts_with(want), "{hdr:?} → {line:?}, want {want:?}");
    }
}

/// Wrong secret, over-long header, no newline: closed with nothing, so a stray pod learns nothing
/// and cannot hold a task.
#[tokio::test]
async fn a_peer_stream_rejects_bad_headers_silently() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let e = common::env().await;
    let addr = stream_listener(e.store.clone()).await;
    for bad in [b"wrong git-upload-pack alice/web alice 2\n".to_vec(), vec![b'a'; 4096], b"test-peer-secret git-upload-pack".to_vec()] {
        let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
        sock.write_all(&bad).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), sock.read_to_end(&mut buf)).await;
        assert!(buf.is_empty(), "bad header must get nothing back, got {:?}", String::from_utf8_lossy(&buf));
    }
}

/// A real ssh clone through a forwarding node: a multi-command session on one connection, and the
/// exit status reaches the client (needs the channel kept alive until it is sent). Reuse the harness
/// in tests/ssh_e2e.rs — read it first; it brings up ssh::serve with a host key and a client key.
#[tokio::test]
async fn a_real_ssh_clone_works_through_a_forwarding_node() {
    if !common::have_git() || !common::have_ssh() { return; }
    // Build two nodes a and b (fleet_of, node()) so a owns the repo. Additionally start
    // rustic_git::ssh::serve for b on a random port with a generated host key (see ssh_e2e.rs for
    // the exact calls) and register a client key for "alice". Push one commit via a's HTTP public
    // port, then:
    //   GIT_SSH_COMMAND="ssh -i <key> -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p <b ssh port>"
    //   git clone -q ssh://git@127.0.0.1/<repo>.git <dir>
    // Assert: clone succeeded and contains the file; a.store.pool.warm_count()==1; b's ==0.
    // Then, with the repo deleted, assert `git ls-remote` over ssh via b exits NON-zero and prints
    // "repository not found" — this is what the status line buys.
    let _ = (fleet_of, repo_owned_by); // silence unused warnings until written
    // Write this against ssh_e2e.rs's helpers; do not leave it as a stub. If ssh_e2e.rs's harness
    // is not reusable as-is, refactor its helpers into tests/common/mod.rs first.
    todo!("implement against tests/ssh_e2e.rs harness — see comment");
}
```

`todo!()` panics; that is deliberate. This test **must be written** before Task 5's step 6 passes, and the plan's expected result for that step reflects it.

- [ ] **Step 2: Run** — `cargo test --test routing a_peer_stream` → compile error, `serve_peer_streams` not found.

- [ ] **Step 3: Peer stream server** — append to `src/proxy.rs`:

```rust
use crate::App;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const HEADER_MAX: usize = 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// The stream port sits one above the HTTP peer port on every node.
/// ponytail: fixed offset; make it configurable if the ports ever need to be independent.
pub fn stream_addr(http_peer: &str) -> String {
    match http_peer.rsplit_once(':') {
        Some((host, port)) => format!("{host}:{}", port.parse::<u16>().unwrap_or(8081) + 1),
        None => http_peer.to_string(),
    }
}

/// Accept forwarded SSH sessions.
///
/// One header line, then one status line back, then the git protocol byte for byte. The socket is
/// then handed to the same `serve_git` a local SSH client reaches, so nothing about the protocol is
/// reimplemented here — which is the point of piping rather than translating.
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
    // Bounded and timed: a stray connection that never sends a newline must not hold a task or
    // grow a buffer without limit.
    let mut header = Vec::new();
    let n = tokio::time::timeout(HEADER_TIMEOUT, (&mut reader).take(HEADER_MAX as u64).read_until(b'\n', &mut header)).await??;
    if n == 0 || header.last() != Some(&b'\n') {
        return Err(crate::err("peer stream: bad header")); // silently closed
    }
    let header = String::from_utf8_lossy(&header).trim_end().to_string();
    let mut parts = header.splitn(5, ' ');
    // Secret first, checked before anything else is parsed. Wrong: close without a byte.
    let presented = parts.next().unwrap_or_default();
    if presented.is_empty() || presented != app.forwarder.secret {
        return Err(crate::err("peer stream: secret"));
    }
    let service = parts.next().unwrap_or_default().to_string();
    let repo = parts.next().unwrap_or_default().to_string();
    let owner = parts.next().unwrap_or_default().to_string();
    // Unparseable hops = exhausted: serve here rather than bounce.
    let hops: u32 = parts.next().and_then(|h| h.parse().ok()).unwrap_or(MAX_HOPS);

    // From here on, refusals are reported as a status line: the forwarding node relays them so
    // the client sees a reason and a non-zero exit, as it would from a local session.
    let refuse = |reader: BufReader<tokio::net::TcpStream>, why: &str| async move {
        let mut s = reader.into_inner();
        let _ = s.write_all(format!("error: {why}\n").as_bytes()).await;
        Err::<(), crate::Error>(crate::err(why))
    };
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return refuse(reader, "unsupported service").await;
    }
    if !crate::store::valid_segment(&owner) {
        return refuse(reader, "invalid owner").await;
    }
    let Some((ro, rn)) = crate::protocol::parse_repo_path(&repo) else {
        return refuse(reader, "invalid repo path").await;
    };
    // The forwarding node authenticated the client; this node still decides what that identity may
    // reach. Trusting who the caller says it is is not the same as skipping authorisation.
    if !crate::auth::authorize(Some(owner.as_str()), &ro) {
        return refuse(reader, "access denied").await;
    }
    // Same rule as HTTP: re-check the nodes ranked above us from here (and one other vantage),
    // and forward up if one answers, unless out of hops.
    if hops < MAX_HOPS {
        match app.route(&format!("{ro}/{rn}")).await {
            crate::peers::Route::Local => {}
            crate::peers::Route::Unavailable => return refuse(reader, "no node may safely serve this repository; retry").await,
            crate::peers::Route::Peer(peer) => {
                // Keep the BufReader: any bytes it buffered past the header belong to git.
                let mut sock = reader;
                return stream_to_peer(&app.forwarder.secret, &stream_addr(&peer.addr), &service, &format!("{ro}/{rn}"), &owner, hops, &mut sock).await;
            }
        }
    }
    let repo = match app.store.open_repo(&ro, &rn).await? {
        Some(r) => r,
        None => return refuse(reader, "repository not found").await,
    };
    let mut sock = reader; // BufReader kept: see above
    sock.get_mut().write_all(b"ok\n").await?;
    crate::ssh::serve_git(app.store.clone(), repo, &service, sock).await
}

/// Pipe an established stream to the node that owns the repo, one hop further along.
///
/// Sends the header, waits for the owner's status line, then copies bytes both ways. Borrows the
/// stream so the caller keeps it alive afterwards: on the SSH path the stream *is* the channel, and
/// dropping it closes the channel — but the exit status has to go out first. `run` in ssh.rs makes
/// the same point about its own bridges.
pub async fn stream_to_peer<S>(secret: &str, peer_stream: &str, service: &str, repo: &str, owner: &str, hops: u32, stream: &mut S) -> Result<()>
where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let sock = tokio::net::TcpStream::connect(peer_stream).await?;
    let mut sock = BufReader::new(sock);
    sock.get_mut().write_all(format!("{secret} {service} {repo} {owner} {}\n", hops + 1).as_bytes()).await?;
    let mut status = String::new();
    tokio::time::timeout(HEADER_TIMEOUT, sock.read_line(&mut status)).await??;
    let status = status.trim_end();
    if status != "ok" {
        return Err(crate::err(status.strip_prefix("error: ").unwrap_or(status)));
    }
    // Both directions until either side finishes; copy_bidirectional half-closes the write side on
    // EOF, which is what git expects. NOTE: on an SSH channel stream, this shutdown already sends
    // the channel EOF (russh ChannelStream::poll_shutdown) — the caller must not send a second one.
    tokio::io::copy_bidirectional(stream, &mut sock).await?;
    Ok(())
}
```

- [ ] **Step 4: `serve_git` in `src/ssh.rs`** — the shared protocol runner. **Do not** refactor the existing local `run` onto it: `run` deliberately hands its I/O halves *out* of `spawn_blocking` and drops them after exit-status; `serve_git` drops them inside. Add alongside `run`, used only by the peer stream:

```rust
/// Run one git service over an established byte stream, to completion. Used by the peer stream
/// path; the local SSH path keeps its own ordering in `run` because it must send an exit status
/// before its stream closes.
pub async fn serve_git<S>(store: Arc<crate::store::Store>, repo: crate::store::Repo, service: &str, stream: S) -> Result<()>
where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let upload = service == "git-upload-pack";
    let (rd, wr) = tokio::io::split(stream);
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    tokio::task::spawn_blocking(move || {
        let interrupt = interrupt;
        let mut input = std::io::BufReader::new(SyncIoBridge::new(rd));
        let mut output = SyncIoBridge::new(wr);
        use std::io::Write;
        if upload { upload::advertise(&mut output)?; upload::serve(&store, &repo, &mut input, &mut output, &interrupt)?; }
        else { receive::advertise(&store, &repo, &mut output)?; receive::serve(&store, &repo, &mut input, &mut output, &interrupt)?; }
        output.flush()?;
        Ok(())
    }).await?
}
```

- [ ] **Step 5: Forward in the SSH `run`** — after the `!v2` check and **before** `open_repo`:

```rust
    let repo_path = format!("{owner}/{name}");
    match app.route(&repo_path).await {
        crate::peers::Route::Local => {} // fall through to the local path below
        crate::peers::Route::Unavailable => return Err(crate::err("no node may safely serve this repository right now; retry")),
        crate::peers::Route::Peer(peer) => {
            let authed = auth_owner.clone().unwrap_or_default();
            // The stream lives until after the exit status is sent: dropping it closes the channel.
            let mut stream = channel.into_stream();
            let piped = crate::proxy::stream_to_peer(&app.forwarder.secret, &crate::proxy::stream_addr(&peer.addr), service, &repo_path, &authed, 0, &mut stream).await;
            let code = match &piped {
                Ok(()) => 0,
                Err(e) => { let _ = handle.extended_data(id, 1, format!("rustic-git: {e}\n").into_bytes()).await; 1 }
            };
            let _ = handle.exit_status_request(id, code).await;
            // No explicit handle.eof(): copy_bidirectional's shutdown already sent the channel EOF
            // through ChannelStream::poll_shutdown, and a second EOF is a protocol error.
            drop(stream);
            return piped;
        }
    }
```

- [ ] **Step 6: Run** — `cargo test --release`. Expected: PASS **once `a_real_ssh_clone_works_through_a_forwarding_node` is written**. Until then that one test panics on `todo!()`, and that failure is the signal that Task 5 is not done. Add `pub fn have_ssh() -> bool` to `tests/common/mod.rs` mirroring `have_git()`.

- [ ] **Step 7: Commit**

```bash
git add src/proxy.rs src/ssh.rs tests/routing.rs tests/common/mod.rs
git commit -m "Pipe forwarded SSH sessions to the owning node

An SSH session is an advertisement and repeated commands on one stream, not an
HTTP request, so it is piped byte for byte rather than translated. The
forwarding node sends one header line, waits for the owner's status line, and
copies bytes; the owner hands the socket to the same serve() a local session
reaches. The status line is what lets a refusal on the owner become a reason
and a non-zero exit at the client instead of a silent exit 0.

The header is bounded and read under a timeout, every field is validated, an
unparseable hop count means serve-here, and the wrong secret gets nothing back.
The channel stream is kept alive until the exit status is sent, and no second
EOF is issued because the pipe's shutdown already sent one."
```

---

### Task 6: Bind the listeners, handle SIGTERM

**Files:** `src/main.rs`

- [ ] **Step 1: `serve`**

```rust
async fn serve() -> Result<()> {
    let store = Arc::new(Store::open(object_store()?, env("RUSTIC_GIT_CACHE_DIR", "./cache").into(), true).await?);
    store.spawn_health_probe();

    let peer_addr = env("RUSTIC_GIT_PEER_ADDR", "0.0.0.0:8081");
    let peer_port: u16 = peer_addr.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(8081);
    // Multi-node needs all three; a default for any of them fails silently (a phantom peer, an
    // open port), so refuse to start instead.
    let (peers, peer_secret) = match std::env::var("RUSTIC_GIT_PEER_DNS") {
        Ok(dns) if !dns.is_empty() => {
            let me = std::env::var("RUSTIC_GIT_SELF").ok().filter(|s| !s.is_empty())
                .ok_or_else(|| rustic_git::err("RUSTIC_GIT_SELF (this pod's name) is required with RUSTIC_GIT_PEER_DNS"))?;
            let secret = std::env::var("RUSTIC_GIT_PEER_SECRET").ok().filter(|s| !s.is_empty())
                .ok_or_else(|| rustic_git::err("RUSTIC_GIT_PEER_SECRET is required with RUSTIC_GIT_PEER_DNS"))?;
            (rustic_git::peers::Membership::new(format!("_peer._tcp.{dns}:{peer_port}"), me), secret)
        }
        _ => {
            // Single node: owns everything; random secret so nothing can drive the peer port.
            use rand::RngCore;
            let mut b = [0u8; 32]; rand::thread_rng().fill_bytes(&mut b);
            let secret: String = b.iter().map(|x| format!("{x:02x}")).collect();
            let solo = rustic_git::peers::Peer { name: "solo".into(), addr: format!("127.0.0.1:{peer_port}") };
            (rustic_git::peers::Membership::fixed(vec![solo], "solo".into()), secret)
        }
    };
    let app = Arc::new(rustic_git::App::new(store.clone(), Arc::new(peers), peer_secret));
    store.pool.spawn_sweeper();

    let http = tokio::net::TcpListener::bind(env("RUSTIC_GIT_HTTP_ADDR", "0.0.0.0:8080")).await?;
    let ssh = tokio::net::TcpListener::bind(env("RUSTIC_GIT_SSH_ADDR", "0.0.0.0:2222")).await?;
    let peer_http = tokio::net::TcpListener::bind(&peer_addr).await?;
    let peer_stream = tokio::net::TcpListener::bind(rustic_git::proxy::stream_addr(&peer_addr)).await?;
    let key = host_key(&env("RUSTIC_GIT_HOST_KEY", "./host_key"))?;
    eprintln!("http on {} ssh on {} — peers on {} and {}, up to {} warm databases",
        http.local_addr()?, ssh.local_addr()?, peer_http.local_addr()?, peer_stream.local_addr()?, store.pool.max_warm);

    // SIGTERM: stop accepting, let in-flight requests finish, close every warm database. Without
    // this the kubelet's SIGTERM kills the process outright — in-flight clones and pushes die, the
    // pool is never closed, and the next opener replays the WAL. terminationGracePeriodSeconds is
    // meaningless without a handler that uses it.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let (a2, a3, a4) = (app.clone(), app.clone(), app.clone());
    let http_srv = axum::serve(http, rustic_git::http::router(a2)).with_graceful_shutdown(async move { term.recv().await; });
    tokio::select! {
        r = http_srv => { r?; }
        r = axum::serve(peer_http, rustic_git::http::peer_router(a3)) => { r?; }
        r = rustic_git::proxy::serve_peer_streams(a4, peer_stream) => { r?; }
        r = rustic_git::ssh::serve(app, ssh, key) => { r?; }
    }
    // ponytail: only the public HTTP listener drains gracefully; the SSH and peer listeners stop
    // on select! exit. Add per-listener shutdown signals if SSH sessions being cut on roll matters.
    store.pool.close().await;
    Ok(())
}
```

- [ ] **Step 2: Verify** — `cargo build --release && cargo test --release && cargo clippy --all-targets` clean.

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "Bind the peer listeners and handle SIGTERM

Peer traffic gets its own sockets, and a multi-node deployment must name its
own pod and carry a peer secret or the server refuses to start; a default for
either would be a phantom peer or an open port, and both fail silently.

SIGTERM now drains the public HTTP listener and closes every warm database.
Without a handler the kubelet's SIGTERM kills the process outright — in-flight
requests die and the next opener replays the WAL — and the grace period buys
nothing."
```

---

### Task 7: Deployment

**Files:** `deploy/rustic-git.yaml`, `README.md`

- [ ] **Step 0** — `az aks show -n kolomi-cluster -g kolomi-rg --query networkProfile.networkPolicy -o tsv` → `none`. This is why the secret exists.
- [ ] **Step 1** — `kubectl -n rustic-git create secret generic rustic-git-peer --from-literal=secret="$(openssl rand -hex 32)"`.
- [ ] **Step 2** — Manifest. Delete the Ingress and `rustic-git-http`. On the container:

```yaml
          env:
            - name: RUSTIC_GIT_PEER_DNS
              value: rustic-git.rustic-git.svc.cluster.local
            - name: RUSTIC_GIT_SELF
              valueFrom: { fieldRef: { fieldPath: metadata.name } }   # the stable pod name — the hash key
            - name: RUSTIC_GIT_PEER_SECRET
              valueFrom: { secretKeyRef: { name: rustic-git-peer, key: secret } }
          ports:
            - { name: peer, containerPort: 8081 }
            - { name: peer-stream, containerPort: 8082 }
          # On termination Kubernetes removes the pod from endpoints and sends SIGTERM at once.
          # Sleep first so endpoint removal reaches every node's DNS (TTL 5s) plus every node's
          # membership cache (TTL 2s) plus margin, so no peer forwards into a dying pod. Then the
          # SIGTERM handler drains and closes the pool.
          lifecycle:
            preStop:
              exec:
                command: ["sleep", "15"]
```

`terminationGracePeriodSeconds: 90` (15 sleep + drain + flush). Add the SRV-bearing named port to the headless Service — SRV records need a **named** port:

```yaml
  ports:
    - { name: http, port: 8080, targetPort: http }
    - { name: ssh, port: 2222, targetPort: ssh }
    - { name: peer, port: 8081, targetPort: peer }   # SRV: _peer._tcp.rustic-git.rustic-git.svc
```

Add the LoadBalancer and NetworkPolicy exactly as in the previous draft.

- [ ] **Step 3** — Apply; `rollout status`; three pods `1/1`.
- [ ] **Step 4** — Six `git ls-remote` through the LB, all ok.
- [ ] **Step 5** — Exactly one pod's `/healthz` (via `kubectl exec ... wget -qO- --header="X-Rustic-Git-Peer: $SECRET" localhost:8081/healthz`) shows a non-zero warm count. Two → stop.
- [ ] **Step 6** — Roll under load: a 90-second `ls-remote` loop during `rollout restart`; expect 0 FAIL. Then a **long clone** (a repo with a >50 MB pack) started just before `rollout restart`; expect it to complete — this is the SIGTERM handler.
- [ ] **Step 7** — README: replace the multi-node section. State the rule and its trade plainly: safety over availability; a fleet split in halves returns 503 for the minority's repos rather than risk two writers.
- [ ] **Step 8** — Commit:

```bash
git add deploy/rustic-git.yaml README.md
git commit -m "Front the fleet with a plain load balancer

The nodes route repos to each other, so nothing in front needs to understand
git; the ingress and its hash annotations are gone. Peer ports carry a shared
secret because this cluster enforces no NetworkPolicy. Pods sleep in preStop
long enough for endpoint removal to reach every node's DNS and cache, then
drain on SIGTERM, so a roll is a handover rather than a race."
```

---

## Self-Review

| Spec section | Task |
|---|---|
| Ownership by stable name; rendezvous; three deep | 1, 2 |
| Membership from SRV; self only if listed | 2, 6, 7 |
| Precedence rule with second vantage; `Unavailable` | 2 (`decide`), 3 (`probe_via`), 4 (`/probe`, `App::route`) |
| Only probe failures mark down; forward failures never | 4 |
| Top three by rank, then filter | 2 |
| `/healthz` means healthy | 4 |
| Hop bound; public strips routing headers | 3, 4, 5 |
| Peer secret on both listeners | 3, 4, 5, 6, 7 |
| SSH byte pipe with status line; header validation | 5 |
| Hop-by-hop headers stripped | 3 |
| Fenced handle re-routes | 4 |
| SIGTERM drain | 6 |
| preStop ≥ DNS TTL + cache TTL | 7 |
| Refuse to start without SELF / SECRET | 6 |
| Testing section — every bullet | 2, 3, 4, 5, 7 |

**Second review findings applied:** (1) probes carry the secret — Task 3; (2) `mark_down` only on probe failure, down-memory never promotes, top-3-by-rank — Tasks 2, 4; (3) second vantage — Tasks 2, 3, 4; (4) hash on pod name via SRV, no self-inclusion — Tasks 1, 2, 6, 7; (5) tests rewritten: no `unimplemented!` shipped as passing, `todo!()` is a loud gate, hop test uses a reachable higher rank, ranking chosen by `repo_owned_by`, one Store per node, `TempDir` held not forgotten, layer comment fixed — Task 4, 5; (6) SIGTERM handler, preStop 15 s — Tasks 6, 7; (7) `SELF`/`SECRET` required — Task 6; (8) fence re-routes; admin documented — Task 4, 7; (9) `/healthz` reflects object store — Task 4; (10) stream header bounded/timed/validated, status line, no double EOF, BufReader kept, hops default exhausted, `run` not refactored — Task 5; (11) hop-by-hop stripped, >1 MiB push test — Tasks 3, 4; (12) `OWNER_HEADER` retained for the SSH path where it carries real identity, dead on HTTP but harmless; (13) addresses come from `SocketAddr::to_string()` on both sides via SRV resolution, so IPv6 formats consistently.

**Deliberate ceilings, marked `ponytail:` in code:** the SRV resolver is A-records + reverse lookup, not a real SRV query; only the public HTTP listener drains on SIGTERM; the peer secret compare is not constant-time.
