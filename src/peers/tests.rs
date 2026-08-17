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

/// The peer set is derived from StatefulSet identity: replicas: N behind a headless Service is
/// exactly {app}-0 … {app}-(N-1), addressed by hostname and resolved at connect time.
#[test]
fn statefulset_names_every_replica_in_order() {
    let m = Membership::statefulset("rustic-git", 3, "rustic-git.ns.svc.cluster.local", 8081, "rustic-git-1".into());
    let want: Vec<Peer> = (0..3)
        .map(|i| Peer {
            name: format!("rustic-git-{i}"),
            addr: format!("rustic-git-{i}.rustic-git.ns.svc.cluster.local:8081"),
        })
        .collect();
    assert_eq!(m.peers(), want.as_slice());
}
