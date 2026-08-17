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
async fn node(
    os: Arc<dyn slatedb::object_store::ObjectStore>,
    name: &str,
    fleet: &[(String, String)],
) -> Node {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rustic_git::store::Store::open(os, tmp.path().join("cache"), false)
            .await
            .unwrap(),
    );
    let peers: Vec<Peer> = fleet
        .iter()
        .map(|(n, a)| Peer {
            name: n.clone(),
            addr: a.clone(),
        })
        .collect();
    let app = Arc::new(App::new(
        store.clone(),
        Arc::new(Membership::fixed(peers, name.into())),
        SECRET.into(),
    ));
    let pub_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public = pub_l.local_addr().unwrap().to_string();
    // The peer listener must be at the address the fleet was told, or probes go nowhere.
    let my_addr = fleet
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, a)| a.clone())
        .expect("node must be in its own fleet");
    let peer_l = tokio::net::TcpListener::bind(&my_addr).await.unwrap();
    let a2 = app.clone();
    tokio::spawn(async move { axum::serve(pub_l, rustic_git::http::router(a2)).await.unwrap() });
    tokio::spawn(async move {
        axum::serve(peer_l, rustic_git::http::peer_router(app))
            .await
            .unwrap()
    });
    Node {
        store,
        public,
        peer: my_addr,
        _tmp: tmp,
    }
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
    names
        .iter()
        .cloned()
        .map(String::from)
        .zip(reserve_ports(names.len()))
        .collect()
}

/// A repo whose top-ranked node in `fleet` is `want`.
fn repo_owned_by(fleet: &[(String, String)], want: &str) -> String {
    let names: Vec<String> = fleet.iter().map(|(n, _)| n.clone()).collect();
    (0..500)
        .map(|i| format!("alice/w{i}"))
        .find(|r| rustic_git::peers::rank(r, &names)[0] == want)
        .unwrap()
}

/// A repo whose full ranking (top N) is exactly `want`, so a test can pin every candidate's rank.
fn repo_ranked(fleet: &[(String, String)], want: &[&str]) -> String {
    let names: Vec<String> = fleet.iter().map(|(n, _)| n.clone()).collect();
    (0..5000)
        .map(|i| format!("alice/w{i}"))
        .find(|r| {
            rustic_git::peers::rank(r, &names)
                .iter()
                .take(want.len())
                .map(String::as_str)
                .eq(want.iter().copied())
        })
        .expect("some repo has that ranking")
}

async fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// A claimed identity on the public port must be ignored: this is the bypass a client would try.
#[tokio::test(flavor = "multi_thread")]
async fn the_public_listener_ignores_a_claimed_identity() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet_of(&["a"]);
    let a = node(e.store.os.clone(), "a", &f).await;
    let res = client()
        .await
        .get(format!(
            "http://{}/alice/web/info/refs?service=git-upload-pack",
            a.public
        ))
        .header(rustic_git::proxy::OWNER_HEADER, "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

/// A claimed hop count on the public port must be ignored: honouring it would let a client force
/// any node to open any repo and fence the owner. B ranks second behind a *reachable* A; if hops
/// were honoured B would serve, so B's pool warm means the bug.
#[tokio::test(flavor = "multi_thread")]
async fn the_public_listener_ignores_a_claimed_hop_count() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let b = node(e.store.os.clone(), "b", &f).await;
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            b.public
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(
            rustic_git::proxy::HOPS_HEADER,
            rustic_git::proxy::MAX_HOPS.to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.store.pool.warm_count(), 1, "A served");
    assert_eq!(
        b.store.pool.warm_count(),
        0,
        "B forwarded — hops from a client are stripped"
    );
}

/// The peer listener requires the secret. Missing or wrong → 403 before anything else.
#[tokio::test(flavor = "multi_thread")]
async fn the_peer_listener_requires_the_secret() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet_of(&["a"]);
    let a = node(e.store.os.clone(), "a", &f).await;
    for wrong in [None, Some("nope")] {
        let mut r = client()
            .await
            .get(format!(
                "http://{}/alice/web/info/refs?service=git-upload-pack",
                a.peer
            ))
            .header(rustic_git::proxy::OWNER_HEADER, "alice")
            .header("git-protocol", "version=2");
        if let Some(w) = wrong {
            r = r.header(rustic_git::proxy::PEER_HEADER, w);
        }
        assert_eq!(r.send().await.unwrap().status(), 403, "secret {wrong:?}");
    }
}

/// With the secret, the peer listener honours the forwarded identity, and its /healthz and /probe
/// answer — the probes routing depends on. Without the secret they refuse, so a probe that forgot
/// it would read every peer as down.
#[tokio::test(flavor = "multi_thread")]
async fn the_peer_listener_serves_probes_and_honours_identity() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet_of(&["a"]);
    let a = node(e.store.os.clone(), "a", &f).await;
    let c = client().await;
    let ok = |p: &str| format!("http://{}{p}", a.peer);
    assert_eq!(
        c.get(ok("/healthz"))
            .header(rustic_git::proxy::PEER_HEADER, SECRET)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(c.get(ok("/healthz")).send().await.unwrap().status(), 403);
    let res = c
        .get(ok("/alice/web/info/refs?service=git-upload-pack"))
        .header(rustic_git::proxy::OWNER_HEADER, "alice")
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    // /probe: a reports whether it can reach a named peer. It can reach itself.
    let body = c
        .get(ok("/probe"))
        .query(&[("peer", "a")])
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body.trim(), "up");
}

/// Hearsay, end to end. C is sent a request (hops=1) for a repo A owns, as if some node could not
/// reach A. C *can* reach A, so it must forward there — only A's pool opens the repo.
#[tokio::test(flavor = "multi_thread")]
async fn a_lower_ranked_node_forwards_up_when_the_owner_is_reachable() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "c"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let c = node(e.store.os.clone(), "c", &f).await;
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            c.peer
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(rustic_git::proxy::HOPS_HEADER, "1")
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.store.pool.warm_count(), 1, "A must have served it");
    assert_eq!(
        c.store.pool.warm_count(),
        0,
        "C must not have opened it — the whole rule"
    );
}

/// Out of hops with a reachable higher rank: NOT served here (that would be a knowing wrong open)
/// and NOT forwarded (that is the bound) — 503. Only a node whose own routing says Local serves at
/// the hop limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_out_of_hops_is_refused_unless_local() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let b = node(e.store.os.clone(), "b", &f).await;
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            b.peer
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(
            rustic_git::proxy::HOPS_HEADER,
            rustic_git::proxy::MAX_HOPS.to_string(),
        )
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        503,
        "out of hops but A is reachable: refuse, do not open"
    );
    assert_eq!(
        b.store.pool.warm_count(),
        0,
        "B must not open a repo it does not own"
    );
    assert_eq!(
        a.store.pool.warm_count(),
        0,
        "and must not forward either"
    );
}

/// One-sided partition, end to end: B cannot reach A (A's peer port is not listening from B's
/// point of view — we simulate by giving B a fleet where A's addr is a dead port) but a third node
/// C can. B must ask C, learn A is up, and NOT serve. Instead: 503, because from B's own view A is
/// down and B forwarding to a peer it cannot reach is impossible. The client retries elsewhere.
#[tokio::test(flavor = "multi_thread")]
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
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            b.public
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        503,
        "B: cannot reach A, but C can → B must not serve"
    );
    assert_eq!(a.store.pool.warm_count(), 0);
    assert_eq!(b.store.pool.warm_count(), 0, "B must NOT open the repo");
    assert_eq!(c.store.pool.warm_count(), 0);
}

/// Genuine outage, end to end: A is down from everyone. Whichever of B/C ranks second asks the
/// other, gets "down", and serves. Sent to the SECOND-ranked node explicitly, so this proves the
/// second candidate serves — not merely that something happened.
#[tokio::test(flavor = "multi_thread")]
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
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            second.public
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        200,
        "second candidate must serve a confirmed outage"
    );
    assert_eq!(
        second.store.pool.warm_count(),
        1,
        "the second candidate opened it"
    );
    assert_eq!(third.store.pool.warm_count(), 0, "the third did not");
}

/// A real git push and clone through a forwarding node. Push is over 1 MiB so Expect:
/// 100-continue and chunked framing are exercised.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_git_push_and_clone_work_through_a_forwarding_node() {
    if !common::have_git() {
        return;
    }
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
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("big"), vec![b'z'; 3 * 1024 * 1024]).unwrap();
    git(&work, &["add", "big"]);
    git(&work, &["commit", "-qm", "big"]);
    git(&work, &["push", "-q", &url, "main"]);
    let clone = tmp.path().join("clone");
    git(tmp.path(), &["clone", "-q", &url, clone.to_str().unwrap()]);
    assert_eq!(
        std::fs::metadata(clone.join("big")).unwrap().len(),
        3 * 1024 * 1024
    );
    assert_eq!(a.store.pool.warm_count(), 1);
    assert_eq!(b.store.pool.warm_count(), 0);
}

/// Fenced by a STRAY process (an admin command run against a live pod), routing still says this
/// node owns the repo: it must reopen and serve, not 503. This is the case on_fenced's `true`
/// branch exists for.
#[tokio::test(flavor = "multi_thread")]
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
    let stray = slatedb::Db::builder(rustic_git::pool::path(o, n), e.store.os.clone())
        .build()
        .await
        .unwrap();
    stray.put(b"k", b"v").await.unwrap();
    // wait for a's handle to observe the fence
    {
        let mut st = adb.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while st.borrow().close_reason.is_none() {
                st.changed().await.unwrap();
            }
        })
        .await
        .expect("a must observe the fence");
    }
    drop(adb);
    stray.close().await.unwrap();
    // a is still the sole owner by routing: the request must succeed after an in-handler reopen
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            a.public
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "still the owner: reopen and serve");
    assert_eq!(a.store.pool.warm_count(), 1);
}

/// A node fenced by a peer that ranks above it must NOT reopen the repo: `Pool::get` evicts and
/// reports, and nothing in the request path reopens because routing says the peer owns it.
#[tokio::test(flavor = "multi_thread")]
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
            while st.borrow().close_reason.is_none() {
                st.changed().await.unwrap();
            }
        })
        .await
        .expect("b's handle must observe the fence within 5s");
    }
    drop(bdb);
    // b's next get must report the fence and evict — never reopen.
    // `Arc<Db>` is not Debug, so expect_err is unavailable here.
    let e2 = match b.store.pool.get(o, n).await {
        Ok(_) => panic!("fenced handle must be reported, not reopened"),
        Err(e) => e,
    };
    assert!(rustic_git::pool::is_fenced(&e2), "got: {e2}");
    assert_eq!(
        b.store.pool.warm_count(),
        0,
        "b must have evicted and NOT reopened"
    );
    assert_eq!(a.store.pool.warm_count(), 1);
    // And a request to b for the repo is routed to a, not served from a reopened handle.
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            b.public
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(b.store.pool.warm_count(), 0, "still cold: b forwarded");
    let _ = token;
}

/// A push that hits an already-observed fence on a node that is STILL the owner: the fence
/// surfaces at `open()`, `on_fenced` says Local, the handle is evicted and reopened, and the push
/// must SUCCEED.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_after_a_stray_opener_succeeds() {
    if !common::have_git() {
        return;
    }
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let adb = a.store.pool.get(o, n).await.unwrap();
    let stray = slatedb::Db::builder(rustic_git::pool::path(o, n), e.store.os.clone())
        .build()
        .await
        .unwrap();
    stray.put(b"k", b"v").await.unwrap();
    {
        let mut st = adb.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while st.borrow().close_reason.is_none() {
                st.changed().await.unwrap();
            }
        })
        .await
        .expect("a must observe the fence");
    }
    drop(adb);
    stray.close().await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let url = format!("http://x:{token}@{}/{repo}.git", a.public);
    common::git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("f"), b"hello").unwrap();
    common::git(&work, &["add", "f"]);
    common::git(&work, &["commit", "-qm", "one"]);
    common::git(&work, &["push", "-q", &url, "main"]);
    let ls = common::git(&work, &["ls-remote", &url, "refs/heads/main"]);
    assert!(ls.contains("refs/heads/main"), "ls-remote: {ls}");
}

/// Best effort: a push racing a stray opener that is still holding the epoch. Either the fence was
/// handled (exit 0) or it was PROPAGATED as a request failure — never swallowed into a per-ref
/// `ng` line, which is what the `receive::serve` change prevents.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_racing_a_stray_opener_still_succeeds_or_reports_cleanly() {
    if !common::have_git() {
        return;
    }
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), "a", &f).await;
    let _adb = a.store.pool.get(o, n).await.unwrap();
    // the stray stays open for the whole push: whichever arm fires, nothing may be swallowed
    let stray = slatedb::Db::builder(rustic_git::pool::path(o, n), e.store.os.clone())
        .build()
        .await
        .unwrap();
    stray.put(b"k", b"v").await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let url = format!("http://x:{token}@{}/{repo}.git", a.public);
    common::git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("f"), b"hello").unwrap();
    common::git(&work, &["add", "f"]);
    common::git(&work, &["commit", "-qm", "one"]);
    let out = std::process::Command::new("git")
        .args(["push", "-q", &url, "main"])
        .current_dir(&work)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        assert!(
            err.contains("repository"),
            "a propagated fence must read as a repository error: {err}"
        );
        assert!(
            !err.contains("ng "),
            "a fence must not be swallowed as a per-ref failure: {err}"
        );
    }
    let _ = stray.close().await;
}
