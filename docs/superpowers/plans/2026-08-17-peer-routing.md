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
- New dependencies: `reqwest` (already transitive via object_store; enabling `stream`+`query`) and `dns-lookup` (thin `getnameinfo` wrapper, no transitive deps). Nothing else.

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
  - (no `mark_down`: negative probe results are never cached — see `Forwarder::reachable`; a stale "down" memory would be a stale reason to demote a healthy peer)

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
                if t.name == f2 { Some(false) } else if via.name == s2 { Some(false) } else { None }),
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
```

Add `dns-lookup = "2"` to `Cargo.toml` `[dependencies]` — a thin, dependency-free wrapper over `getnameinfo`. This is the one new dependency; it replaces a much larger SRV resolver.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib peers
```

Expected: PASS, 23 tests (6 from Task 1, 17 here). `futures` is already a dependency; `join_all` comes from it.

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

`Cargo.toml` `[dependencies]`: `reqwest = { version = "0.13", default-features = false, features = ["stream", "query"] }` (already in the lock file via object_store; `query` is needed for `.query(&[..])` and is not enabled by object_store; peer traffic is plain HTTP in-cluster, no TLS feature needed).

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
    // accept-then-close is a connection error, not a timeout, so this is fast despite the retry
    assert!(!f.reachable(&addr).await, "accept-then-close is not reachable");
}

/// A positive probe is cached briefly, a negative one is not: a hot owner is probed once per
/// second per node rather than once per request, but only fresh evidence may demote a peer.
/// The stub counts hits so the cache is actually observed.
#[tokio::test]
async fn positive_probes_are_cached_negative_ones_are_not() {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route("/healthz", get(move |hd: axum::http::HeaderMap| { let h = h.clone(); async move {
        h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if hd.get(PEER_HEADER).and_then(|v| v.to_str().ok()) == Some(SECRET) { axum::http::StatusCode::OK } else { axum::http::StatusCode::FORBIDDEN }
    }}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let f = Forwarder::new(SECRET.into());
    assert!(f.reachable(&addr).await);
    assert!(f.reachable(&addr).await);
    assert!(f.reachable(&addr).await);
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 1, "three reachable() calls within the cache window = one real probe");
    let f2 = Forwarder::new(SECRET.into());
    assert!(!f2.reachable("127.0.0.1:1").await);
    assert!(!f2.reachable("127.0.0.1:1").await, "negative is never cached");
}

/// Concurrent probes of one address share a single in-flight probe. Ten callers, one hit.
#[tokio::test]
async fn concurrent_probes_of_one_address_are_single_flight() {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route("/healthz", get(move || { let h = h.clone(); async move {
        h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await; // slow enough to overlap
        "ok"
    }}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let f = std::sync::Arc::new(Forwarder::new(SECRET.into()));
    let calls: Vec<_> = (0..10).map(|_| { let f = f.clone(); let a = addr.clone(); tokio::spawn(async move { f.reachable(&a).await }) }).collect();
    for c in calls { assert!(c.await.unwrap()); }
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 1, "ten concurrent callers, one probe");
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

/// A probe must distinguish "down" from "slow". Both vantages time out for one cause if the owner
/// is merely busy — a GC pause, a burst of requests — and the owner, as top candidate, checks
/// nobody; two vantages agreeing on a timeout is not two independent observations. So probes are
/// generous and retried once, and a positive answer is cached briefly so a hot owner is probed at
/// most once per second per node, not once per request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_RETRIES: u32 = 1;
const PROBE_CACHE: Duration = Duration::from_secs(1);

/// Headers that describe one hop, not the message. Forwarded verbatim they mislead the next hop:
/// git sends `Expect: 100-continue` on pushes over 1 MiB, and `Transfer-Encoding` describes *our*
/// framing, not the peer's. Stripped in both directions; each hop frames its own body.
const HOP_BY_HOP: &[&str] = &[
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer",
    "transfer-encoding", "upgrade", "expect", "content-length", "host",
];

async fn probe_via_once(client: &reqwest::Client, secret: &str, via_addr: &str, target_name: &str) -> Option<bool> {
    let r = client
        .get(format!("http://{via_addr}/probe"))
        .query(&[("peer", target_name)])
        .header(PEER_HEADER, secret)
        .timeout(PROBE_TIMEOUT * (PROBE_RETRIES + 1) + Duration::from_secs(1))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None; // includes 503 = the via is unhealthy; its word is not a vantage
    }
    match r.text().await.ok()?.trim() {
        "up" => Some(true),
        "down" => Some(false),
        _ => None, // includes "unknown": the via does not know that peer (stale view)
    }
}

async fn probe_once_with_retry(client: &reqwest::Client, secret: &str, addr: &str) -> bool {
    for _ in 0..=PROBE_RETRIES {
        let r = client.get(format!("http://{addr}/healthz"))
            .header(PEER_HEADER, secret).timeout(PROBE_TIMEOUT).send().await;
        match r {
            Ok(r) if r.status().is_success() => return true,
            // A definite answer that is not 200 (403 = wrong secret, 503 = unhealthy) is "down"
            // without retry; only a timeout or connect failure earns the retry.
            Ok(_) => return false,
            Err(e) if e.is_timeout() || e.is_connect() => continue,
            Err(_) => return false,
        }
    }
    false
}

pub struct Forwarder {
    pub client: reqwest::Client,
    pub secret: String,
    /// Recent positive probes, so a hot owner is not probed on every request.
    up_cache: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// In-flight probes by address, so N concurrent requests for a dead owner's repos share one
    /// probe rather than issuing N. Negatives are still never cached — this only dedups probes
    /// that are happening right now.
    in_flight: std::sync::Mutex<std::collections::HashMap<String, futures::future::Shared<futures::future::BoxFuture<'static, bool>>>>,
    /// Same, for second-vantage requests, keyed "via|target".
    via_in_flight: std::sync::Mutex<std::collections::HashMap<String, futures::future::Shared<futures::future::BoxFuture<'static, Option<bool>>>>>,
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
            up_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            in_flight: std::sync::Mutex::new(std::collections::HashMap::new()),
            via_in_flight: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Whether a peer's *application* answers right now.
    ///
    /// `GET /healthz` with the secret, expecting 200. Not a bare TCP connect: a pod mid-shutdown
    /// still accepts TCP for a moment before it dies, and treating that as reachable is how two
    /// nodes end up holding one repo. The secret matters too — the peer listener refuses requests
    /// without it, and a refused probe would read as "down" for every peer, collapsing routing to
    /// every node serving everything.
    ///
    /// Retried once on timeout, because "slow" must not read as "down" (see PROBE_TIMEOUT). A
    /// positive answer is cached for PROBE_CACHE; a negative one never is — only fresh evidence may
    /// demote a peer.
    pub async fn reachable(&self, addr: &str) -> bool {
        if let Some(at) = self.up_cache.lock().unwrap().get(addr) {
            if at.elapsed() < PROBE_CACHE { return true; }
        }
        // Single-flight: if a probe of this address is already running, await that one.
        let fut = {
            let mut m = self.in_flight.lock().unwrap();
            if let Some(f) = m.get(addr) {
                f.clone()
            } else {
                use futures::FutureExt;
                let client = self.client.clone();
                let secret = self.secret.clone();
                let a = addr.to_string();
                let f = async move { probe_once_with_retry(&client, &secret, &a).await }.boxed().shared();
                m.insert(addr.to_string(), f.clone());
                f
            }
        };
        let up = fut.await;
        self.in_flight.lock().unwrap().remove(addr);
        if up {
            self.up_cache.lock().unwrap().insert(addr.to_string(), std::time::Instant::now());
        }
        up
    }

    // (helper for reachable's single-flight)
    // ponytail: free function so the shared future is 'static; a method would borrow self.

    /// The second vantage: can `via` reach `target`? `None` if `via` itself did not answer, or
    /// does not know the target — neither is evidence about `target` either way.
    ///
    /// Single-flight per (via, target), like `reachable`: a failover decision asks
    /// |above| × |others| vias concurrently, and N concurrent requests for a dead owner's repos
    /// would otherwise fan that out N×. The via side already dedups its real /healthz probe via
    /// its own `reachable`, so this only bounds the asking node's outbound HTTP.
    ///
    /// The timeout here must EXCEED the via's own probe budget: `/probe` runs `reachable()`, which
    /// is PROBE_TIMEOUT plus one retry when the target is blackholed (a crashed pod's IP, still in
    /// DNS for ~40 s — the exact case failover exists for). A shorter timeout here makes every
    /// vantage answer `None` on a genuinely dead owner, and failover never happens.
    pub async fn probe_via(&self, via_addr: &str, target_name: &str) -> Option<bool> {
        let key = format!("{via_addr}|{target_name}");
        let fut = {
            let mut m = self.via_in_flight.lock().unwrap();
            if let Some(f) = m.get(&key) {
                f.clone()
            } else {
                use futures::FutureExt;
                let client = self.client.clone();
                let secret = self.secret.clone();
                let (v, t) = (via_addr.to_string(), target_name.to_string());
                let f = async move { probe_via_once(&client, &secret, &v, &t).await }.boxed().shared();
                m.insert(key.clone(), f.clone());
                f
            }
        };
        let out = fut.await;
        self.via_in_flight.lock().unwrap().remove(&key);
        out
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

- [ ] **Step 5: Run to verify it passes** — `cargo test --test proxy` → PASS, 5 tests. Note the stub in `concurrent_probes...` does not check the secret; that is deliberate — it is measuring dedup, not auth.

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

/// Reserve N loopback port PAIRS (p, p+1) up front so a fleet can be described before any node
/// starts and the stream port (= peer port + 1) is known free too. Both listeners are held until
/// the vector is dropped, then released just before node() binds; the window is tiny.
fn reserve_ports(n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut held = Vec::new();
    while out.len() < n {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        if let Ok(l2) = std::net::TcpListener::bind(("127.0.0.1", p + 1)) {
            out.push(format!("127.0.0.1:{p}"));
            held.push((l, l2));
        }
    }
    drop(held);
    out
}

fn fleet_of(names: &[&str]) -> Vec<(String, String)> {
    names.iter().cloned().map(String::from).zip(reserve_ports(names.len())).collect()
}

/// A repo whose top-ranked node in `fleet` is `want`.
fn repo_owned_by(fleet: &[(String, String)], want: &str) -> String {
    let names: Vec<String> = fleet.iter().map(|(n, _)| n.clone()).collect();
    (0..500).map(|i| format!("alice/w{i}")).find(|r| rustic_git::peers::rank(r, &names)[0] == want).unwrap()
}

/// A repo whose full ranking (top N) is exactly `want`, so a test can pin every candidate's rank.
fn repo_ranked(fleet: &[(String, String)], want: &[&str]) -> String {
    let names: Vec<String> = fleet.iter().map(|(n, _)| n.clone()).collect();
    (0..5000).map(|i| format!("alice/w{i}"))
        .find(|r| rustic_git::peers::rank(r, &names).iter().take(want.len()).map(String::as_str).eq(want.iter().copied()))
        .expect("some repo has that ranking")
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

/// Out of hops with a reachable higher rank: NOT served here (that would be a knowing wrong open)
/// and NOT forwarded (that is the bound) — 503. Only a node whose own routing says Local serves at
/// the hop limit.
#[tokio::test]
async fn a_request_out_of_hops_is_refused_unless_local() {
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
    assert_eq!(res.status(), 503, "out of hops but A is reachable: refuse, do not open");
    assert_eq!(b.store.pool.warm_count(), 0, "B must not open a repo it does not own");
    assert_eq!(a.store.pool.warm_count(), 0, "and must not forward either");
}

/// One-sided partition, end to end: B cannot reach A (A's peer port is not listening from B's
/// point of view — we simulate by giving B a fleet where A's addr is a dead port) but a third node
/// C can. B must ask C, learn A is up, and NOT serve. Instead: 503, because from B's own view A is
/// down and B forwarding to a peer it cannot reach is impossible. The client retries elsewhere.
#[tokio::test]
async fn a_one_sided_partition_yields_503_not_a_second_writer() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    // Real fleet: a, b, c all reachable. B's private view: a is at a dead port. Ranking pinned to
    // [a, b, c] so B is definitely SECOND — as third, B would legitimately forward to C in phase 1.
    let real = fleet_of(&["a", "b", "c"]);
    let repo = repo_ranked(&real, &["a", "b", "c"]);
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

/// Genuine outage, end to end: A is down from everyone. Whichever of B/C ranks second asks the
/// other, gets "down", and serves. Sent to the SECOND-ranked node explicitly, so this proves the
/// second candidate serves — not merely that something happened.
#[tokio::test]
async fn a_confirmed_outage_lets_the_second_candidate_serve() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b", "c"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    // A is never started: its reserved port is closed from everyone's point of view.
    let b = node(e.store.os.clone(), "b", &f).await;
    let c = node(e.store.os.clone(), "c", &f).await;
    let names: Vec<String> = f.iter().map(|(n, _)| n.clone()).collect();
    let second_name = rustic_git::peers::rank(&repo, &names)[1].clone();
    let (second, third) = if second_name == "b" { (&b, &c) } else { (&c, &b) };
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", second.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 200, "second candidate must serve a confirmed outage");
    assert_eq!(second.store.pool.warm_count(), 1, "the second candidate opened it");
    assert_eq!(third.store.pool.warm_count(), 0, "the third did not");
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
    ///
    /// An unhealthy node never routes `Local`. Its peers see its /healthz fail and — with a second
    /// vantage — will serve its repos; if it kept serving them too, that is two writers. So it
    /// answers Unavailable and lets the fleet take over. Health has hysteresis (see
    /// `spawn_health_probe`) so one slow round trip does not flip the whole fleet's view.
    pub async fn route(&self, repo: &str) -> peers::Route {
        let f = self.forwarder.clone();
        let unhealthy = !self.store.healthy();
        let f2 = self.forwarder.clone();
        let route = self.peers
            .decide(
                repo,
                move |p: &peers::Peer| { let f = f.clone(); let a = p.addr.clone(); async move { f.reachable(&a).await } },
                move |via: &peers::Peer, t: &peers::Peer| { let f = f2.clone(); let a = via.addr.clone(); let n = t.name.clone(); async move { f.probe_via(&a, &n).await } },
            )
            .await;
        // An unhealthy node may still FORWARD what it does not own — that is safe and keeps its
        // 1/N of load-balancer traffic flowing — but never serves: its peers see its /healthz fail
        // and will take its repos, and serving alongside them is two writers.
        match (unhealthy, route) {
            (true, peers::Route::Local) => peers::Route::Unavailable,
            (_, r) => r,
        }
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
    ///
    /// Hysteresis: three consecutive failures to flip unhealthy, one success to flip back. Without
    /// it, one slow round trip during an object-store blip makes every node unhealthy at once and
    /// every node stops routing Local for one probe interval.
    pub fn spawn_health_probe(self: &Arc<Self>) {
        let s = self.clone();
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                // The store is healthy if it *answered the question*: Ok, or NotFound (the probe
                // key need not exist). Everything else — Generic (transport, 5xx), Unauthenticated
                // (401: rotated key), PermissionDenied (403) — is unhealthy. Treating auth failures
                // as healthy would keep a node with a revoked key holding its repos and returning
                // 500 forever, which is exactly what this exists to catch.
                let ok = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    s.os.head(&OsPath::from("auth/.health")),
                )
                .await
                .map(|r| matches!(r, Ok(_) | Err(slatedb::object_store::Error::NotFound { .. })))
                .unwrap_or(false);
                failures = if ok { 0 } else { failures + 1 };
                s.healthy.store(failures < 3, std::sync::atomic::Ordering::Relaxed);
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
///
/// Answers 503 when this node is unhealthy, mirroring /healthz — an unhealthy node's word is not a
/// vantage. Without this, an unhealthy second candidate would keep answering as a via, and the
/// "a higher rank that answers as a via is reachable → forward to it" rule would send traffic to a
/// node that then refuses to serve, stalling failover past it for as long as it stays unhealthy.
async fn probe(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Response {
    if !app.store.healthy() {
        return (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response();
    }
    let Some(name) = q.get("peer") else { return (StatusCode::BAD_REQUEST, "peer=").into_response(); };
    let peers = app.peers.peers().await;
    // Unknown is not "down": a via with a 2 s-stale view that lacks a just-added owner must not turn
    // its ignorance into evidence. The asker treats "unknown" as could-not-ask.
    let Some(p) = peers.iter().find(|p| &p.name == name) else { return (StatusCode::OK, "unknown").into_response(); };
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
    // Absent means fresh (0): the public listener strips this header, so every client request
    // arrives without it and MUST route. Present-but-unparseable means exhausted: a peer sent
    // garbage, and serving here beats bouncing. Conflating the two — "missing = exhausted" — makes
    // the public listener never route at all, and every node opens every repo it is sent.
    let hops: u32 = match req.headers().get(crate::proxy::HOPS_HEADER) {
        None => 0,
        Some(v) => v.to_str().ok().and_then(|v| v.parse().ok()).unwrap_or(crate::proxy::MAX_HOPS),
    };
    let route = app.route(&repo).await;
    // Out of hops: never forward again (that is the bound), but never knowingly open a repo we do
    // not own either — a chain that arrives here disagreeing with our own view, or arrives at an
    // unhealthy node, gets 503 rather than a second writer. Same bound, no wrong opens.
    if hops >= crate::proxy::MAX_HOPS {
        return match route {
            crate::peers::Route::Local => next.run(req).await,
            _ => (StatusCode::SERVICE_UNAVAILABLE, "routing disagreement at hop limit; retry").into_response(),
        };
    }
    match route {
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

Today `Pool::get` (`src/pool.rs`) notices a closed handle and *transparently* evicts and reopens it. Under routing that is the amplifier: "fenced" almost always means another node believes it owns this repo, and reopening takes it straight back — one flip per request, forever. The fence must surface to a caller that can re-run the routing decision. Fences appear in two places: at `Pool::get` (handle already closed) and later, inside the protocol handlers running in `spawn_blocking`, as a write error from `update_refs`. Cover both:

In `src/pool.rs`, replace the body of `get` so it **does not reopen by itself**:

```rust
    /// The database for a repo, opening it if this node does not already hold it warm.
    ///
    /// A closed handle is evicted and reported, NOT reopened. Under routing, "closed" almost always
    /// means "fenced": another node opened this repo because it believes it owns it. Reopening here
    /// would take it straight back and turn any disagreement into a flap. The caller decides — via
    /// the routing rule — whether this node should hold the repo, and only then reopens.
    pub async fn get(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        let h = self.get_once(owner, name).await?;
        if h.status().close_reason.is_none() {
            return Ok(h);
        }
        drop(h);
        self.evict(owner, name).await;
        Err(FencedError { repo: format!("{owner}/{name}") }.into())
    }
```

Add, in `src/pool.rs`:

```rust
/// This node's handle on a repo was closed under it — fenced by another opener.
#[derive(Debug)]
pub struct FencedError { pub repo: String }
impl std::fmt::Display for FencedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{} fenced", self.repo) }
}
impl std::error::Error for FencedError {}
/// Whether an error, anywhere in a request, is a fence: ours or SlateDB's own.
pub fn is_fenced(e: &crate::Error) -> bool {
    e.downcast_ref::<FencedError>().is_some()
        // slatedb 0.15: a fence surfaces as ErrorKind::Closed(CloseReason::Fenced). There is no
        // bare ErrorKind::Fenced.
        || e.downcast_ref::<slatedb::Error>().is_some_and(|e| matches!(e.kind(), slatedb::ErrorKind::Closed(slatedb::CloseReason::Fenced)))
}
```

Update the pool test `fenced_handle_is_replaced_on_next_get` (`src/pool.rs`) to the new contract and rename it `fenced_handle_is_evicted_and_reported`: after the usurper opens the repo, **wait for the fence to surface** — `db.subscribe()` and await `close_reason.is_some()` with a 5 s timeout, because SlateDB sets it asynchronously (manifest poll ~1 s) or on the next failed write; the existing test's own comment says the racing request "still sees one Fenced error before the status reflects it". Then: `p.get("alice","web")` returns `Err` with `is_fenced` true, `warm_count()` is 0, and a *second* `p.get` reopens successfully.

Also in `src/protocol/receive.rs`, `serve` currently swallows every `apply()` error into per-ref `ng` lines and returns `Ok(())` (`receive.rs:88-95`). A fence in `update_refs` — the primary place a push hits one — would therefore reach the client as `200` with `ng ... Closed error` and never reach the `is_fenced` arm. Change that block:

```rust
    if let Err(e) = apply(store, repo, input, &updates, &mut results, interrupt) {
        // A fence is not a per-ref failure to report and move on from: it means this node no
        // longer holds the repo, and the caller must re-route. Propagate it; the HTTP/SSH layer
        // turns it into a retry or a 503.
        if crate::pool::is_fenced(&e) {
            return Err(e);
        }
        let m = e.to_string().replace('\n', " ");
        ...unchanged...
```

And add to `tests/routing.rs` a test where a PUSH (not a read) hits a fence on a node that is STILL the owner — the stray-opener variant: single node `a`, `a` holds the repo (hold the handle, as above), a stray `Db::builder(...).build()` takes the writer epoch, wait for `a` to observe the fence, close the stray, then a real `git push` of one commit through `a`'s public port. **The push must SUCCEED.** Be precise about what this exercises: because the wait lets the fence be *observed* first, it surfaces at `open()` (`open_repo → repo_exists → Pool::get`), and it is the **`open()` arm** that calls `on_fenced` → Local → evict → reopen — `receive::serve` then runs against a fresh handle and never sees a fence. Assert exit 0 and that `git ls-remote` afterwards shows the pushed commit.

The *write-time* path — a fence that lands between `open()` and `update_refs`, which is where the `receive::serve` propagation change and the handler-level arm matter — is not deterministically testable, because which arm fires depends on when SlateDB's poller observes the fence. Add a best-effort variant that does NOT wait and leaves the stray open during the push (`a_push_racing_a_stray_opener_still_succeeds_or_reports_cleanly`): assert that git either exits 0 (one of the two arms handled it) or exits non-zero with stderr containing "repository" and NOT containing "ng " (i.e. it was propagated, not swallowed as a per-ref failure). That is the observable difference the `receive::serve` change makes.

In `src/lib.rs`, add to `App`:

```rust
    /// What to do when a request for `repo` hit a fence: re-run routing. `true` means this node
    /// still owns the repo (a stray admin process fenced us, or a peer has since released it) and
    /// the caller should reopen and retry the operation ONCE, in-handler — the HTTP handlers hold
    /// the body as `Bytes`, so a retry costs nothing. `false` means the fence was correct: answer
    /// 503. git does NOT retry a 503 by itself; the user re-runs. Acceptable for the rare "routed
    /// to a node that just lost the repo" case, and the reason the Local case retries in-handler
    /// rather than bouncing the client.
    pub async fn on_fenced(&self, owner: &str, name: &str) -> bool {
        if !matches!(self.route(&format!("{owner}/{name}")).await, peers::Route::Local) {
            return false;
        }
        // Pool::get never reopens a fenced handle by itself (that is the amplifier this exists to
        // remove). Routing says we still own it, so evict here — the retry's Pool::get then opens
        // fresh and takes the writer epoch back. Without this the retry gets a second FencedError.
        self.store.pool.evict(owner, name).await;
        true
    }
```

In `src/http.rs`, in each of `info_refs`, `upload_pack`, `receive_pack`: the `spawn_blocking` result is matched today as `Ok(bytes)` / `Err(e) => internal(e)`. Change the error arm to:

```rust
        Err(e) if crate::pool::is_fenced(&e) => {
            // See App::on_fenced. If routing still says we own it, reopen and run the request
            // again — the body is `Bytes`, so this is a plain second call. Otherwise 503.
            // NOT the raw Path `owner`/`name`: those still carry the `.git` suffix (every real URL
            // has it), which would hash to a different rank and name a database that does not
            // exist. `repo` holds the parsed names — bind them before `run_protocol` moves it:
            // `let (o, n) = (repo.owner.clone(), repo.name.clone());` at the top of the handler.
            if app.on_fenced(&o, &n).await {
                // Reopen: open_repo → Pool::get opens fresh now that the fenced handle is evicted.
                let repo = match app.store.open_repo(&o, &n).await {
                    Ok(Some(r)) => r,
                    _ => return (StatusCode::SERVICE_UNAVAILABLE, "repository moved; retry").into_response(),
                };
                match run_protocol(repo, body.clone()).await { // second, identical attempt
                    Ok(bytes) => success(bytes),
                    Err(e) => internal(e), // a second fence is a real error, not retried again
                }
            } else {
                (StatusCode::SERVICE_UNAVAILABLE, "repository is owned by another node; retry").into_response()
            }
        }
        Err(e) => internal(e),
```

**The fence surfaces in two places, and both need the arm.** The FIRST `Pool::get` on every request is inside `open()` → `open_repo()` → `repo_exists()` → `db_for()`, before any `spawn_blocking`. A fence already observed by then (which is what the tests wait for via `subscribe()`) surfaces there, and `open()` today maps every `Err` to 500. So in `open()`, replace the `Err(e) =>` arm of the `open_repo` match:

```rust
        Err(e) if crate::pool::is_fenced(&e) => {
            // Fenced at open time. Routing decides: still ours → evict (on_fenced does) and open
            // once more; not ours → 503 so the client retries against the owner.
            if app.on_fenced(&owner, &name).await {
                match app.store.open_repo(&owner, &name).await {
                    Ok(Some(repo)) => Ok(repo),
                    Ok(None) => Err((StatusCode::NOT_FOUND, "repository not found").into_response()),
                    Err(e) => { eprintln!("reopen after fence {owner}/{name}: {e}"); Err(internal(e)) }
                }
            } else {
                Err((StatusCode::SERVICE_UNAVAILABLE, "repository is owned by another node; retry").into_response())
            }
        }
        Err(e) => {
            eprintln!("open_repo {owner}/{name}: {e}"); // ponytail: eprintln; swap for a logger when one exists
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
        }
```

(`open()` returns `Result<Repo, Response>`, so `Err(...)` above is a `Response`; `internal(e)` already builds one.) The second place is the write-time fence inside the protocol handlers (`update_refs`), which is the `spawn_blocking` arm below.

Concretely, in each of the three handlers: after `open()` returns `repo`, bind `let (o, n) = (repo.owner.clone(), repo.name.clone());` — the parsed names, not the raw `Path` ones. The existing `spawn_blocking` becomes a local async closure `run_protocol` taking `(repo: Repo, body: Bytes)` — `Bytes` is `Clone`, and the closure re-creates its `Cursor`/`body_reader` from it — and a `success(bytes)` closure builds the existing 200 response. `Repo` must derive `Clone` (add `#[derive(Clone)]` in `src/store.rs`; it holds only `String`s and `PathBuf`s). Call `run_protocol(repo, body.clone())` once; on fenced+Local, reopen and call it again as shown. `info_refs` has no body: its `run_protocol` takes only `repo`. In SSH `run` and `serve_peer_stream`, a fenced error is reported to the client ("repository moved; retry") with exit 1 — the SSH stream cannot be replayed. Add two routing tests — the one below, and the Local case:

```rust
/// Fenced by a STRAY process (an admin command run against a live pod), routing still says this
/// node owns the repo: it must reopen and serve, not 503. This is the case on_fenced's `true`
/// branch exists for.
#[tokio::test]
async fn a_node_fenced_by_a_stray_process_reopens_when_it_is_still_the_owner() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let adb = a.store.pool.get(o, n).await.unwrap(); // a holds it — kept, see the not-owner test
    // a stray opener (an admin command, say) takes the writer epoch
    let stray = slatedb::Db::builder(rustic_git::pool::path(o, n), e.store.os.clone()).build().await.unwrap();
    stray.put(b"k", b"v").await.unwrap();
    // wait for a's handle to observe the fence
    {
        let mut st = adb.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while st.borrow().close_reason.is_none() { st.changed().await.unwrap(); }
        }).await.expect("a must observe the fence");
    }
    drop(adb);
    stray.close().await.unwrap();
    // a is still the sole owner by routing: the request must succeed after an in-handler reopen
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 200, "still the owner: reopen and serve");
    assert_eq!(a.store.pool.warm_count(), 1);
}
```

Add a routing test for the not-owner case:

```rust
/// A node fenced by a peer that ranks above it must NOT reopen the repo: `Pool::get` evicts and
/// reports, and nothing in the request path reopens because routing says the peer owns it. This
/// asserts the pool contract directly — a request to b for this repo never reaches b's handler
/// (routing forwards it or, at the hop limit, refuses it), so `on_fenced`'s false branch is not
/// exercised by HTTP here; it is three lines and is covered by the pool test in Step 6.
#[tokio::test]
async fn a_fenced_node_does_not_reopen_when_it_is_not_the_owner() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    // b opens the repo first (as if it had been the owner before a scale-up), then a comes up.
    let b = node(e.store.os.clone(), "b", &f).await;
    // HOLD b's handle across the fence: if it were dropped and re-fetched after a took the epoch,
    // a fast manifest poll could have already flagged it, the re-fetch would evict and return Err,
    // and the wait below would be skipped — then the expect_err after would see a fresh reopen.
    let bdb = b.store.pool.get(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let _ = a.store.pool.get(o, n).await.unwrap(); // a takes the writer epoch: b is now fenced
    // SlateDB observes the fence asynchronously (manifest poll, ~1 s) or on b's next write. Wait
    // for b's handle to report closed, or the assertions below see a stale-but-open handle.
    {
        let mut st = bdb.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while st.borrow().close_reason.is_none() { st.changed().await.unwrap(); }
        }).await.expect("b's handle must observe the fence within 5s");
    }
    drop(bdb);
    // b's next get must report the fence and evict — never reopen.
    let e2 = b.store.pool.get(o, n).await.expect_err("fenced handle must be reported, not reopened");
    assert!(rustic_git::pool::is_fenced(&e2), "got: {e2}");
    assert_eq!(b.store.pool.warm_count(), 0, "b must have evicted and NOT reopened");
    assert_eq!(a.store.pool.warm_count(), 1);
    // And a request to b for the repo is routed to a, not served from a reopened handle.
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(b.store.pool.warm_count(), 0, "still cold: b forwarded");
    let _ = token;
}
```

- [ ] **Step 7: Fix call sites** — `tests/common/mod.rs` gets `pub fn app(store) -> Arc<App>` using `Membership::fixed(vec![Peer{name:"solo".into(), addr:"127.0.0.1:1".into()}], "solo".into())` and secret `"test-peer-secret"`; `tests/http_e2e.rs` and `tests/ssh_e2e.rs` use it.

- [ ] **Step 8: Run** — `cargo test --release` → PASS, 10 new routing tests plus all existing (the pool test renamed in Step 6 included).

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
  - `pub async fn proxy::stream_to_peer<S>(secret, peer_stream_addr, service, repo, owner, hops, stream: &mut S, relay: bool) -> Result<()>` — sends header, **waits for the status line**, then pipes. With `relay`, writes the status upstream first (middle-node case). `Err` carries the owner's `error:` reason.
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
    // The advertisement is pkt-lines; read until the flush packet "0000" rather than by line, since
    // pkt-lines need not end in newline and an empty repo's ls-refs answer is a bare "0000".
    async fn read_until_flush(r: &mut BufReader<tokio::net::TcpStream>) -> String {
        use tokio::io::AsyncReadExt;
        let mut out = String::new();
        loop {
            let mut len = [0u8; 4];
            r.read_exact(&mut len).await.unwrap();
            let n = usize::from_str_radix(std::str::from_utf8(&len).unwrap(), 16).unwrap();
            if n == 0 { return out; }
            let mut body = vec![0u8; n - 4];
            r.read_exact(&mut body).await.unwrap();
            out.push_str(&String::from_utf8_lossy(&body));
        }
    }
    let advert = read_until_flush(&mut r).await;
    assert!(advert.contains("version 2"), "then the advertisement: {advert:?}");
    // then a command round-trip on the same stream: ls-refs on an empty repo answers with a bare
    // flush, which is still an answer.
    r.get_mut().write_all(b"0014command=ls-refs\n0000").await.unwrap();
    let mut flush = [0u8; 4];
    tokio::time::timeout(std::time::Duration::from_secs(5), tokio::io::AsyncReadExt::read_exact(&mut r, &mut flush)).await
        .expect("ls-refs must answer on the same stream").unwrap();
    assert_eq!(&flush, b"0000");
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
        // repository-not-found is reported AFTER "ok" on the git ERR channel (see serve_peer_stream),
        // because "ok" must not wait on open_repo; tested separately below.
        ("test-peer-secret git-frobnicate alice/web alice 2\n", "error: unsupported service"),
        // owner with a space: splitn(5) yields owner="al", hops="ice" (unparseable → MAX_HOPS);
        // "al" is a valid segment not authorised for alice/web → access denied. The point is that a
        // malformed line is refused, not which check catches it.
        ("test-peer-secret git-upload-pack alice/web al ice 2\n", "error: access denied"),
        ("test-peer-secret git-upload-pack alice/web ../x 2\n", "error: invalid owner"),
    ] {
        let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
        sock.write_all(hdr.as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut sock).read_line(&mut line).await.unwrap();
        assert!(line.starts_with(want), "{hdr:?} → {line:?}, want {want:?}");
    }
}

/// A missing repo is reported after "ok", on the git ERR channel — the same channel a local
/// session uses — because "ok" must not wait for open_repo (which may download packs).
#[tokio::test]
async fn a_peer_stream_reports_a_missing_repo_on_the_err_channel() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let e = common::env().await;
    let addr = stream_listener(e.store.clone()).await;
    let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
    sock.write_all(b"test-peer-secret git-upload-pack alice/nope alice 2\n").await.unwrap();
    let mut r = BufReader::new(sock);
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim(), "ok");
    let mut rest = Vec::new();
    r.read_to_end(&mut rest).await.unwrap();
    let rest = String::from_utf8_lossy(&rest);
    assert!(rest.contains("ERR repository not found"), "got {rest:?}");
}

/// Two hops: B (not owner) → C (not owner, but C can reach A) → A. C must relay A's "ok" back to B,
/// or B reads A's first git packet as a status line and fails the session.
#[tokio::test]
async fn a_two_hop_ssh_forward_relays_the_status_line() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let e = common::env().await;
    let f = fleet_of(&["a", "b", "c"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _a = node(e.store.os.clone(), "a", &f).await;
    let c = node(e.store.os.clone(), "c", &f).await;
    // Talk to C's STREAM port directly as if we were B, hops=1. C is not the owner and can reach A,
    // so C forwards to A and must relay A's status. (node() starts the stream listener too — Step 1b.)
    let mut sock = tokio::net::TcpStream::connect(rustic_git::proxy::stream_addr(&c.peer)).await.unwrap();
    sock.write_all(format!("{SECRET} git-upload-pack {repo} alice 1\n").as_bytes()).await.unwrap();
    let mut r = BufReader::new(sock);
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim(), "ok", "the middle node must relay the owner's status, got {line:?}");
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

- [ ] **Step 1b: `node()` starts the stream listener too** — in `tests/routing.rs`'s `node()`, after binding the peer listener, also bind `rustic_git::proxy::stream_addr(&my_addr)` and spawn `serve_peer_streams(app.clone(), l)` on it, so two-hop SSH tests can reach a node's stream port.

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
    // `&'static str`: an annotated `&str` here would be higher-ranked and the returned future could
    // not capture it. Every reason is a literal, so 'static is honest.
    let refuse = |reader: BufReader<tokio::net::TcpStream>, why: &'static str| async move {
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
    // Same rule as HTTP: re-check the nodes ranked above us from here (and vantages), forward up
    // if one answers unless out of hops — and at the hop limit, still refuse to serve what routing
    // says is not ours.
    let route = app.route(&format!("{ro}/{rn}")).await;
    if hops >= MAX_HOPS && !matches!(route, crate::peers::Route::Local) {
        return refuse(reader, "routing disagreement at hop limit; retry").await;
    }
    if hops < MAX_HOPS {
        match route {
            crate::peers::Route::Local => {}
            crate::peers::Route::Unavailable => return refuse(reader, "no node may safely serve this repository; retry").await,
            crate::peers::Route::Peer(peer) => {
                // Two-hop: we are the middle node. stream_to_peer reads the OWNER's status line
                // itself; with `relay = true` it writes a status line UPSTREAM to the node that
                // forwarded to us — "ok" once the owner said ok, or "error: …" if the owner refused
                // — BEFORE piping, so it can never write "error:" after "ok". Keep the BufReader:
                // any bytes it buffered past the header belong to git.
                let mut sock = reader;
                return stream_to_peer(&app.forwarder.secret, &stream_addr(&peer.addr), &service, &format!("{ro}/{rn}"), &owner, hops, &mut sock, true).await;
            }
        }
    }
    // "ok" goes out BEFORE open_repo. Opening a cold repo downloads its packs — seconds to
    // minutes for a big one — and the forwarding node is waiting on this line under a short
    // timeout meant for the header exchange, not for a pack download. Once "ok" is sent, a
    // missing repo is reported the way a local session reports it: on the git ERR channel with a
    // non-zero exit, which git prints as-is.
    let mut sock = reader; // BufReader kept: see above
    sock.get_mut().write_all(b"ok\n").await?;
    let repo = match app.store.open_repo(&ro, &rn).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            let _ = crate::pktline::write_err(&mut sock, "repository not found").await;
            return Err(crate::err("repository not found"));
        }
        Err(e) if crate::pool::is_fenced(&e) => {
            let _ = crate::pktline::write_err(&mut sock, "repository moved; retry").await;
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    crate::ssh::serve_git(app.store.clone(), repo, &service, sock).await
}

/// Pipe an established stream to the node that owns the repo, one hop further along.
///
/// Sends the header, waits for the owner's status line, then copies bytes both ways. With `relay`,
/// the caller is a middle node: the owner's status is written upstream (as "ok" or "error: …")
/// before piping starts, so an upstream node waiting on a status line gets one — and never gets
/// "error:" after "ok", because after "ok" nothing but git bytes is ever written upstream.
///
/// Borrows the stream so the caller keeps it alive afterwards: on the SSH path the stream *is* the
/// channel, and dropping it closes the channel — but the exit status has to go out first. `run` in
/// ssh.rs makes the same point about its own bridges.
pub async fn stream_to_peer<S>(secret: &str, peer_stream: &str, service: &str, repo: &str, owner: &str, hops: u32, stream: &mut S, relay: bool) -> Result<()>
where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let connect_and_status = async {
        let sock = tokio::net::TcpStream::connect(peer_stream).await?;
        let mut sock = BufReader::new(sock);
        sock.get_mut().write_all(format!("{secret} {service} {repo} {owner} {}\n", hops + 1).as_bytes()).await?;
        // The owner answers "ok" after validating the header, before it opens the repo, so this
        // wait is short in practice — but bounded generously rather than by HEADER_TIMEOUT, since
        // the owner may itself be routing (a few probes) before it answers.
        let mut status = String::new();
        tokio::time::timeout(Duration::from_secs(30), sock.read_line(&mut status)).await??;
        let status = status.trim_end().to_string();
        if status != "ok" {
            return Err(crate::err(status.strip_prefix("error: ").unwrap_or(&status).to_string()));
        }
        Ok::<_, crate::Error>(sock)
    };
    let mut sock = match connect_and_status.await {
        Ok(s) => s,
        Err(e) => {
            if relay {
                let _ = stream.write_all(format!("error: {e}\n").as_bytes()).await;
            }
            return Err(e);
        }
    };
    if relay {
        stream.write_all(b"ok\n").await?; // the node upstream is waiting on this
    }
    // Both directions until either side finishes; copy_bidirectional half-closes the write side on
    // EOF, which is what git expects. NOTE: on an SSH channel stream, this shutdown already sends
    // the channel EOF (russh ChannelStream::poll_shutdown) — the caller must not send a second one.
    tokio::io::copy_bidirectional(stream, &mut sock).await?;
    Ok(())
}
```

- [ ] **Step 3b: `pktline::write_err`** — `src/pktline.rs` has sync writers only. Add an async one for the peer stream's ERR line (git prints `ERR <msg>` pkt-lines verbatim and exits non-zero):

```rust
/// Write a git `ERR` pkt-line asynchronously. Used where a refusal must reach the client after the
/// stream is already committed to the git protocol.
pub async fn write_err<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, msg: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = format!("ERR {msg}\n");
    w.write_all(format!("{:04x}{body}", body.len() + 4).as_bytes()).await?;
    w.flush().await
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
    tokio::task::spawn_blocking(move || -> Result<()> {
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
            let piped = crate::proxy::stream_to_peer(&app.forwarder.secret, &crate::proxy::stream_addr(&peer.addr), service, &repo_path, &authed, 0, &mut stream, false).await;
            let code = match &piped {
                Ok(()) => 0,
                Err(e) => { let _ = handle.extended_data(id, 1, format!("rustic-git: {e}\n").into_bytes()).await; 1 }
            };
            let _ = handle.exit_status_request(id, code).await;
            // No explicit handle.eof(): copy_bidirectional's shutdown already sent the channel EOF
            // through ChannelStream::poll_shutdown, and a second EOF is a protocol error.
            drop(stream);
            // Ok, not `piped`: this arm has already reported the outcome to the client. Returning
            // Err would make exec_request's caller report it AGAIN — a second stderr line, a second
            // exit status, and a second EOF.
            return Ok(());
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
    let peers = Arc::new(peers);
    // Do NOT gate startup on seeing self in DNS. A pod enters the headless Service's DNS only when
    // it is READY, and readiness probes the HTTP listener bound below — so waiting here before
    // binding is a deadlock: never ready → never in DNS → never starts. Instead: bind, become
    // ready, and while self is unlisted `decide` returns Unavailable (self is not in the set, so
    // it never ranks Local). A background task warns if self stays absent well past readiness,
    // which is the reverse-DNS-returning-garbage case worth being loud about.
    if std::env::var("RUSTIC_GIT_PEER_DNS").map(|d| !d.is_empty()).unwrap_or(false) {
        let p = peers.clone();
        let me = std::env::var("RUSTIC_GIT_SELF").unwrap_or_default();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            loop {
                if !p.sees_self().await {
                    eprintln!("WARNING: {me} has been up 60s+ and does not appear in its own peer set {:?} — reverse DNS not returning pod names? every request from here is 503 until it does",
                        p.peers().await.iter().map(|x| x.name.clone()).collect::<Vec<_>>());
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }
    let app = Arc::new(rustic_git::App::new(store.clone(), peers, peer_secret));
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
    // Both HTTP listeners drain: for repos this node owns, most traffic arrives on the PEER
    // listener (forwarded from the other N-1 nodes), so draining only the public one would cut the
    // majority of in-flight requests. One SIGTERM, fanned out to both via a watch channel.
    let (term_tx, term_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("sigterm handler");
        term.recv().await;
        let _ = term_tx.send(true);
    });
    let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move { while !*rx.borrow() { if rx.changed().await.is_err() { break; } } };
    let (a2, a3, a4) = (app.clone(), app.clone(), app.clone());
    let http_srv = axum::serve(http, rustic_git::http::router(a2)).with_graceful_shutdown(wait(term_rx.clone()));
    let peer_srv = axum::serve(peer_http, rustic_git::http::peer_router(a3)).with_graceful_shutdown(wait(term_rx.clone()));
    // Both HTTP servers as ONE select arm: select! returns when its first arm resolves, and if
    // each server were its own arm the first to finish draining would end the select and
    // pool.close() would run under the other's in-flight requests. try_join waits for both.
    tokio::select! {
        r = async { tokio::try_join!(http_srv, peer_srv) } => { r?; }
        r = rustic_git::proxy::serve_peer_streams(a4, peer_stream) => { r?; }
        r = rustic_git::ssh::serve(app, ssh, key) => { r?; }
    }
    // ponytail: the SSH and peer-stream listeners stop on select! exit without draining; the
    // preStop delay is what makes that rare (the pod has left DNS before it stops). Add per-session
    // tracking if SSH sessions being cut on roll ever matters.
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

`terminationGracePeriodSeconds: 90` (15 sleep + drain + flush). Add pod anti-affinity so a repo's candidates do not share a physical node — two vantages on one node are one vantage when that node's link fails:

```yaml
      affinity:
        podAntiAffinity:
          preferredDuringSchedulingIgnoredDuringExecution:
            - weight: 100
              podAffinityTerm:
                labelSelector: { matchLabels: { app: rustic-git } }
                topologyKey: kubernetes.io/hostname
```

The headless Service needs the peer port **named** so its A records carry through and reverse lookups resolve (the name is also what a real SRV query would need, should the resolver be upgraded):

```yaml
  ports:
    - { name: http, port: 8080, targetPort: http }
    - { name: ssh, port: 2222, targetPort: ssh }
    - { name: peer, port: 8081, targetPort: peer }   # SRV: _peer._tcp.rustic-git.rustic-git.svc
```

Add the public LoadBalancer and the NetworkPolicy:

```yaml
---
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
# Kept for a cluster that enforces NetworkPolicy; this one (networkPolicy: none) does not, which
# is why the peer ports also require a secret.
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
    - ports:
        - { protocol: TCP, port: 8080 }
        - { protocol: TCP, port: 2222 }
```

- [ ] **Step 3** — Apply; `rollout status`; three pods `1/1`.
- [ ] **Step 4** — Six `git ls-remote` through the LB, all ok.
- [ ] **Step 5** — Exactly one pod's `/healthz` (via `kubectl exec ... wget -qO- --header="X-Rustic-Git-Peer: $SECRET" localhost:8081/healthz`) shows a non-zero warm count. Two → stop.
- [ ] **Step 6** — Roll under load: a 90-second `ls-remote` loop during `rollout restart`; expect 0 FAIL. Then a **long clone** (a repo with a >50 MB pack) started just before `rollout restart`; expect it to complete — this is the SIGTERM handler.
- [ ] **Step 7** — README: replace the multi-node section. State the rule and its trade plainly, and these three limits by name:
  - **`replicas >= 3` is required for failover.** With two nodes there is no second vantage: an unreachable owner's repos return 503 until Kubernetes drops it from DNS.
  - **A fleet split in halves** returns 503 for the minority's repos rather than risk two writers.
  - **Two vantages defeat one-sided partitions, not correlated slowness.** A slow-but-alive owner can time out from two peers for one cause; probes are generous and retried, and candidates are spread across nodes, but the top-ranked node never verifies that anyone can reach it. Fencing is the backstop.
  - **On SIGTERM the two HTTP listeners drain; SSH sessions do not.** An SSH session in flight on a terminating pod is cut when the drain ends; the preStop delay is what makes that rare (the pod has left DNS before it stops).
  - **Liveness is `/healthz`, which reflects the object store.** During an object-store outage longer than ~90 s every pod is restarted, which achieves nothing but is harmless — the pods come back into the same outage. Readiness-only for the store check is a reasonable follow-up.
  - **Reverse DNS must return pod names.** The image's `getnameinfo` needs a working NSS (`debian:bookworm-slim` has it); a cluster without the in-addr.arpa zone yields an empty peer set and a loud log line, and every request is 503 until fixed.
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

**Third review findings applied:** (1) no DNS gate before bind — startup deadlock removed; (2) `query` feature, `ErrorKind::Closed(CloseReason::Fenced)`; (3) `probe_via` timeout exceeds the via's own retry budget; (4) forward-up needs no vantage, and any non-target peer may vouch, so a 3-node fleet's third candidate is not stranded (R4 then tightened this: every node above needs a vantage, and any hard "up" vetoes); (5) middle node relays the owner's status line on two-hop SSH; (6) fence tests wait on `Db::subscribe`, and `receive::serve` propagates a fence instead of swallowing it as `ng`; (7) `route()` returns Unavailable when this node is unhealthy, health has 3-strike hysteresis; (8) `on_fenced` retries in-handler on Local — git does not retry 503; (9) both HTTP listeners drain on SIGTERM; (10) phase-1 probes run concurrently, worst-case latency stated; (11) `mark_down` deleted; (12) `/probe` answers "unknown" for a name it does not have; (13) LOW bundle: owner-with-space test corrected, positive-cache test observes hits, LB/NetworkPolicy inlined, dependencies stated honestly, liveness/NSS caveats in README.

**Fourth review findings applied:** (1) phase 2 vouches for EVERY node above, and a via in `above` that answers is treated as reachable — forward to it; (2) any `Some(true)` vetoes serving, regardless of order; (3) `on_fenced` evicts before returning `true` so the retry actually reopens, `Repo: Clone`, retry closure spelled out, Local-reopen test added; (4) both HTTP servers `try_join`ed as one `select!` arm so both drain; (5) at hop exhaustion routing is still consulted — Local serves, anything else 503, never a knowing wrong open; (6) one-sided-partition test pins the full ranking; (7) single-flight probes per address; (8) an unhealthy node still forwards what it does not own; (9) `stream_to_peer` gains `relay` and never writes `error:` after `ok`; (10) test ports reserved in pairs; (11) `notify_waiters`; (12) spec Components table and drain claims corrected.

**Sixth (final verify) findings applied:** (1) the handler-level fence arm uses `repo.owner`/`repo.name`, not the raw `Path` values that still carry `.git`; (2) both fence tests hold the first pool handle across the fence so the subscribe-wait cannot be skipped by a fast manifest poll; (3) the stray-push prose states which arm it exercises (`open()`), and a best-effort write-time variant is added that checks the fence is propagated rather than swallowed as `ng`.

**Targeted (fifth) review findings applied:** (1) the fence arm is in `open()` as well as on the protocol result — the first `Pool::get` of every request happens there; (2) `let second_vantage = &second_vantage;` so phase 2 compiles; (3) `/probe` answers 503 when unhealthy, so an unhealthy higher rank does not stall failover by answering as a via; (4) the not-owner fence test asserts the pool contract directly and that a request is forwarded; the push-fence test is the stray-owner variant and must SUCCEED; (5) the one-directional-cut limit is documented in the spec; (6) `probe_via` is single-flight per (via, target).

**Deliberate ceilings, marked `ponytail:` in code:** the SRV resolver is A-records + reverse lookup, not a real SRV query; SSH sessions do not drain on SIGTERM; the peer secret compare is not constant-time; the single-flight helpers are free functions so their futures are `'static`.
