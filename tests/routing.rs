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
    app: Arc<App>,
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
    let (peer_l, stream_l) = take_reserved(&my_addr);
    let a2 = app.clone();
    tokio::spawn(async move { axum::serve(pub_l, rustic_git::http::router(a2)).await.unwrap() });
    let a4 = app.clone();
    tokio::spawn(async move {
        axum::serve(peer_l, rustic_git::http::peer_router(a4))
            .await
            .unwrap()
    });
    let a3 = app.clone();
    tokio::spawn(async move { rustic_git::proxy::serve_peer_streams(a3, stream_l).await });
    Node {
        store,
        app,
        public,
        peer: my_addr,
        _tmp: tmp,
    }
}

/// Reserve N loopback port PAIRS (p, p+1) up front so a fleet can be described before any node
/// starts, and the stream port (= peer port + 1) is reserved with it.
///
/// The listeners are PARKED, not released: between releasing a port and binding it again there is
/// a window in which any other bind-to-0 in this process (another node's public port, a client
/// socket) can be handed it, and the tests run concurrently. `node()` takes its pair back out of
/// the park, so a reserved port is never unbound.
type Parked = std::collections::HashMap<String, (std::net::TcpListener, std::net::TcpListener)>;
fn park() -> &'static std::sync::Mutex<Parked> {
    static P: std::sync::OnceLock<std::sync::Mutex<Parked>> = std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

fn reserve_ports(n: usize) -> Vec<String> {
    let mut out = Vec::new();
    while out.len() < n {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        let Ok(l2) = std::net::TcpListener::bind(("127.0.0.1", p + 1)) else {
            continue;
        };
        let addr = format!("127.0.0.1:{p}");
        park().lock().unwrap().insert(addr.clone(), (l, l2));
        out.push(addr);
    }
    out
}

/// The peer and stream listeners reserved for `addr`, as tokio listeners.
fn take_reserved(addr: &str) -> (tokio::net::TcpListener, tokio::net::TcpListener) {
    let (a, b) = park()
        .lock()
        .unwrap()
        .remove(addr)
        .expect("node's ports were reserved by fleet_of");
    let conv = |l: std::net::TcpListener| {
        l.set_nonblocking(true).unwrap();
        tokio::net::TcpListener::from_std(l).unwrap()
    };
    (conv(a), conv(b))
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

/// An unhealthy node must stop claiming its repos: its peers see /healthz fail and take them
/// over, and if it kept serving alongside them that is two writers. It still forwards what it
/// does not own (forwarding is safe). /probe also refuses, so its word is not a vantage; and
/// /probe answers "unknown" for a peer it has never heard of, which the asker treats as
/// no-evidence rather than "down".
#[tokio::test(flavor = "multi_thread")]
async fn an_unhealthy_node_stops_serving_but_still_forwards() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet_of(&["a", "b"]);
    let mine = repo_owned_by(&f, "a");
    let theirs = repo_owned_by(&f, "b");
    for r in [&mine, &theirs] { let (o, n) = r.split_once('/').unwrap(); e.store.create_repo(o, n).await.unwrap(); }
    let a = node(e.store.os.clone(), "a", &f).await;
    let b = node(e.store.os.clone(), "b", &f).await;
    let c = client().await;

    // Healthy: a serves its own repo, /healthz 200, /probe answers.
    let res = c.get(format!("http://{}/{mine}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.store.pool.warm_count(), 1);
    assert_eq!(c.get(format!("http://{}/healthz", a.peer)).header(rustic_git::proxy::PEER_HEADER, SECRET).send().await.unwrap().status(), 200);
    let unknown = c.get(format!("http://{}/probe", a.peer)).query(&[("peer", "nobody")])
        .header(rustic_git::proxy::PEER_HEADER, SECRET).send().await.unwrap().text().await.unwrap();
    assert_eq!(unknown.trim(), "unknown", "a peer we have never heard of is not 'down'");

    // Flip a unhealthy directly (the probe loop is not under test).
    a.store.healthy.store(false, std::sync::atomic::Ordering::Relaxed);

    // Its own repo: 503, not served. Pool count unchanged (no new open).
    let res = c.get(format!("http://{}/{mine}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 503, "unhealthy: must not claim its own repo");
    // /healthz and /probe both refuse — a's word is no longer a vantage.
    assert_eq!(c.get(format!("http://{}/healthz", a.peer)).header(rustic_git::proxy::PEER_HEADER, SECRET).send().await.unwrap().status(), 503);
    assert_eq!(c.get(format!("http://{}/probe", a.peer)).query(&[("peer", "b")]).header(rustic_git::proxy::PEER_HEADER, SECRET).send().await.unwrap().status(), 503);
    // A repo b owns: a still FORWARDS it (forwarding is safe), b serves.
    let res = c.get(format!("http://{}/{theirs}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 200, "unhealthy node still forwards what it does not own");
    assert_eq!(b.store.pool.warm_count(), 1);
    assert_eq!(a.store.pool.warm_count(), 1, "a opened nothing new");
}


async fn stream_listener(store: Arc<rustic_git::store::Store>) -> String {
    let app = common::app(store);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { rustic_git::proxy::serve_peer_streams(app, l).await });
    addr
}

/// A whole session on one stream: header, "ok", advertisement, then a command. hops=2 so this
/// node serves rather than routing again.
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_stream_reports_refusals_as_a_status_line() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let addr = stream_listener(e.store.clone()).await;
    for (hdr, want) in [
        ("test-peer-secret git-upload-pack alice/web mallory 2\n", "error: access denied"),
        // repository-not-found is reported AFTER "ok" on the git ERR channel, tested separately.
        ("test-peer-secret git-frobnicate alice/web alice 2\n", "error: unsupported service"),
        // owner with a space: splitn(5) yields owner="al", hops="ice" (unparseable -> MAX_HOPS);
        // "al" is a valid segment not authorised for alice/web -> access denied.
        ("test-peer-secret git-upload-pack alice/web al ice 2\n", "error: access denied"),
        ("test-peer-secret git-upload-pack alice/web ../x 2\n", "error: invalid owner"),
    ] {
        let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
        sock.write_all(hdr.as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut sock).read_line(&mut line).await.unwrap();
        assert!(line.starts_with(want), "{hdr:?} -> {line:?}, want {want:?}");
    }
}

/// A missing repo is reported after "ok", on the git ERR channel - the same channel a local
/// session uses - because "ok" must not wait for open_repo (which may download packs).
#[tokio::test(flavor = "multi_thread")]
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

/// Two hops: B (not owner) -> C (not owner, but C can reach A) -> A. C must relay A's "ok" back to
/// B, or B reads A's first git packet as a status line and fails the session.
#[tokio::test(flavor = "multi_thread")]
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
    // so C forwards to A and must relay A's status.
    let mut sock = tokio::net::TcpStream::connect(rustic_git::proxy::stream_addr(&c.peer)).await.unwrap();
    sock.write_all(format!("{SECRET} git-upload-pack {repo} alice 1\n").as_bytes()).await.unwrap();
    let mut r = BufReader::new(sock);
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim(), "ok", "the middle node must relay the owner's status, got {line:?}");
}

/// Wrong secret, over-long header, no newline: closed with nothing, so a stray pod learns nothing
/// and cannot hold a task.
#[tokio::test(flavor = "multi_thread")]
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

/// Host key via ssh-keygen (same reason as tests/ssh_e2e.rs: ssh-key's PrivateKey::random needs a
/// rand_core version this crate does not depend on).
fn gen_host_key(dir: &std::path::Path) -> russh::keys::PrivateKey {
    let p = dir.join("host_ed25519");
    assert!(std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", p.to_str().unwrap()])
        .status().unwrap().success());
    russh::keys::PrivateKey::from_openssh(std::fs::read_to_string(&p).unwrap()).unwrap()
}

/// A real ssh clone through a forwarding node: a multi-command session on one connection, and the
/// exit status reaches the client (needs the channel kept alive until it is sent).
#[tokio::test(flavor = "multi_thread")]
async fn a_real_ssh_clone_works_through_a_forwarding_node() {
    if !common::have_git() || !common::have_ssh() {
        eprintln!("skip: git/ssh missing");
        return;
    }
    let e = common::env().await;
    let f = fleet_of(&["a", "b"]);
    let repo = repo_owned_by(&f, "a");
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let token = e.store.create_token(o).await.unwrap();

    let kd = tempfile::tempdir().unwrap();
    let key = kd.path().join("id_ed25519");
    assert!(std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()])
        .status().unwrap().success());
    e.store
        .add_ssh_key(o, &std::fs::read_to_string(kd.path().join("id_ed25519.pub")).unwrap())
        .await
        .unwrap();

    let a = node(e.store.os.clone(), "a", &f).await;
    let b = node(e.store.os.clone(), "b", &f).await;

    // b also speaks SSH; b does not own the repo, so every session it accepts is forwarded to a.
    let host_key = gen_host_key(kd.path());
    let ssh_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ssh_port = ssh_l.local_addr().unwrap().port();
    let b_app = b.app.clone();
    tokio::spawn(async move { rustic_git::ssh::serve(b_app, ssh_l, host_key).await.unwrap() });

    // One commit, pushed over a's public HTTP port, so the repo has content.
    let w = tempfile::tempdir().unwrap();
    let http_url = format!("http://x:{token}@{}/{repo}.git", a.public);
    common::git(w.path(), &["clone", "-q", &http_url, "seed"]);
    let seed = w.path().join("seed");
    std::fs::write(seed.join("f.txt"), "one\n").unwrap();
    common::git(&seed, &["add", "."]);
    common::git(&seed, &["commit", "-qm", "one"]);
    common::git(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    let ssh_cmd = format!(
        "ssh -i {} -p {ssh_port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        key.display()
    );
    let ssh_url = format!("ssh://git@127.0.0.1/{repo}.git");
    let out = std::process::Command::new("git")
        .args(["clone", "-q", &ssh_url, "c1"])
        .current_dir(w.path())
        .env("GIT_SSH_COMMAND", &ssh_cmd)
        .output()
        .unwrap();
    assert!(out.status.success(), "clone: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(w.path().join("c1/f.txt")).unwrap(), "one\n");
    assert_eq!(a.store.pool.warm_count(), 1, "a served");
    assert_eq!(b.store.pool.warm_count(), 0, "b only forwarded");

    // With the repo gone, the refusal must reach the client as a reason and a non-zero exit -
    // which is what the status line and the ERR pkt-line buy.
    // Deleted through the owner's own store: the test env's store was fenced when a opened the repo.
    a.store.delete_repo(o, n).await.unwrap();
    a.store.pool.evict(o, n).await;
    let out = std::process::Command::new("git")
        .args(["ls-remote", &ssh_url])
        .current_dir(w.path())
        .env("GIT_SSH_COMMAND", &ssh_cmd)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "ls-remote on a deleted repo must fail: {err}");
    assert!(err.contains("repository not found"), "stderr: {err}");
}
