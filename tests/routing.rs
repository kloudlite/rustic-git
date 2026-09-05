//! Which node ends up serving a repo, and who may claim an identity.
mod common;

use kloudlite_storage::ownership::OwnershipStore;
use kloudlite_app::App;
use std::sync::Arc;

const SECRET: &str = "test-peer-secret";
/// The node every fleet starts FIRST, which is why it holds the lease: `node()` runs one election
/// beat before serving, and the first beat on an empty store wins. Nothing else is special about it.
const LEADER: &str = "kloudlite-0";
/// One node's own Store over a shared object store, so each node has its own pool and the test can
/// see which node opened a repo. One shared Store would mean one shared pool, and "exactly one
/// opener" could never fail.
struct Node {
    store: Arc<kloudlite_storage::store::Store>,
    app: Arc<App>,
    public: String,
    peer: String,
    _tmp: tempfile::TempDir,
}

/// Bring up a node named `name` (`kloudlite-N`). `fleet` is every node's (name, peer addr); it is
/// how a node resolves a name from the map to somewhere to forward. **Start `LEADER` first** — the
/// first node to tick takes the lease, and every later one reads who holds it.
async fn node(
    os: Arc<dyn slatedb::object_store::ObjectStore>,
    name: &str,
    fleet: &[(String, String)],
) -> Node {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        kloudlite_storage::store::Store::open(os.clone(), tmp.path().join("cache"), false)
            .await
            .unwrap(),
    );
    let ownership = OwnershipStore::open(os);
    let f: Vec<(String, String)> = fleet.to_vec();
    let app = Arc::new(App::new(
        store.clone(),
        Arc::new(ownership),
        name.into(),
        Arc::new(move |n: &str| {
            let addr = f
                .iter()
                .find(|(x, _)| x == n)
                .map(|(_, a)| a.clone())
                .unwrap_or_else(|| "127.0.0.1:1".into());
            // A blackholed address resolves to a refused port: how a test takes a node off the
            // network without stopping a listener other tests may be sharing the port space with.
            if blackholed().lock().unwrap().contains(&addr) {
                return "127.0.0.1:1".into();
            }
            if let Some(left) = flaky().lock().unwrap().get_mut(&addr) {
                if *left > 0 {
                    *left -= 1;
                    return "127.0.0.1:1".into();
                }
            }
            addr
        }),
        SECRET.into(),
        kloudlite_pulls::pulls::Source::Absent,
    ));
    // One election beat before serving, and then a renewal beat, because a lease that lapses
    // mid-test changes what the test measures. A test that needs a failover still advances a
    // follower's clock past LEADER_TTL and ticks it by hand — deterministic, and ten seconds
    // faster than waiting.
    app.election_tick().await.unwrap();
    // Production renews every held lease on a beat (`bins/server/src/lanes.rs`). Without it a
    // claim taken here is dead in LEASE_TTL (10 s) and any test whose middle runs longer — a
    // real git push, an ssh clone — silently stops testing forwarding and starts testing
    // claiming, which is the "only under load" flake. The 3 s cadence is LEASE_TTL/3, the same
    // ratio lanes.rs uses, so a single missed beat is survivable.
    let a5 = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let _ = a5.election_tick().await;
            let _ = a5.renew_once().await;
        }
    });
    // Eviction gives the lease back before it closes the database, exactly as `serve()` wires it.
    store.pool.set_release_hook(
        Arc::downgrade(&app) as std::sync::Weak<dyn kloudlite_storage::pool::ReleaseHook>
    );
    let pub_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public = pub_l.local_addr().unwrap().to_string();
    // The peer listener must be at the address the fleet was told, or forwards go nowhere.
    let my_addr = fleet
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, a)| a.clone())
        .expect("node must be in its own fleet");
    let (peer_l, stream_l) = take_reserved(&my_addr);
    let a2 = app.clone();
    tokio::spawn(async move { axum::serve(pub_l, kloudlite_server::router::router(a2)).await.unwrap() });
    let a4 = app.clone();
    tokio::spawn(async move {
        axum::serve(peer_l, kloudlite_server::router::peer_router(a4))
            .await
            .unwrap()
    });
    let a3 = app.clone();
    tokio::spawn(async move { kloudlite_vcs::proxy::serve_peer_streams(a3, stream_l).await });
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
/// The listeners are PARKED, not released: both listeners of each pair are stashed in the global
/// `park()` map (a `Mutex<HashMap<addr, (TcpListener, TcpListener)>>`) instead of being dropped.
/// Releasing a port and rebinding it later would leave a window in which any other bind-to-0 in
/// this process (another node's public port, a client socket) could be handed it, and the tests
/// run concurrently. `node()` takes a pair back out of the park by address when it actually
/// starts that node.
///
/// A reserved pair whose node is never started stays parked for the whole test binary's lifetime
/// — deliberately: it keeps the port unclaimable by anything else, and a connection to it is
/// accepted and then hangs rather than being refused.
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
        .expect("node's ports were reserved by fleet()");
    let conv = |l: std::net::TcpListener| {
        l.set_nonblocking(true).unwrap();
        tokio::net::TcpListener::from_std(l).unwrap()
    };
    (conv(a), conv(b))
}

/// Peer addresses that are pretending to be unreachable. Keyed by address, which is unique per
/// fleet, so blackholing one test's leader does not touch another's.
/// Addresses that fail their next N lookups and then heal — a transient blip, as opposed to
/// `blackholed`, which is a node that stays gone. Counts down per lookup, and `addr_of` is called
/// once per forward attempt, so `1` fails exactly the first attempt.
fn flaky() -> &'static std::sync::Mutex<std::collections::HashMap<String, u32>> {
    static F: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u32>>> =
        std::sync::OnceLock::new();
    F.get_or_init(Default::default)
}

fn blackholed() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static B: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    B.get_or_init(Default::default)
}

/// A fleet of `n` nodes, `kloudlite-0` first — start it first, and it leads.
fn fleet(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| format!("kloudlite-{i}"))
        .zip(reserve_ports(n))
        .collect()
}

async fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// A claimed identity on the public port must be ignored: this is the bypass a client would try.
#[tokio::test(flavor = "multi_thread")]
async fn the_public_listener_ignores_a_claimed_identity() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet(1);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let res = client()
        .await
        .get(format!(
            "http://{}/alice/web/info/refs?service=git-upload-pack",
            a.public
        ))
        .header(kloudlite_core::peer::OWNER_HEADER, "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

/// A claimed hop count on the public port must be ignored: honouring it would let a client force
/// any node to open any repo and fence the owner. The map says A holds this repo; if hops were
/// honoured B would serve it anyway, so B's pool going warm means the bug.
#[tokio::test(flavor = "multi_thread")]
async fn the_public_listener_ignores_a_claimed_hop_count() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(&repo).await.unwrap();
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            b.public
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(
            kloudlite_core::peer::HOPS_HEADER,
            kloudlite_core::peer::MAX_HOPS.to_string(),
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
    let f = fleet(1);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    for wrong in [None, Some("nope")] {
        let mut r = client()
            .await
            .get(format!(
                "http://{}/alice/web/info/refs?service=git-upload-pack",
                a.peer
            ))
            .header(kloudlite_core::peer::OWNER_HEADER, "alice")
            .header("git-protocol", "version=2");
        if let Some(w) = wrong {
            r = r.header(kloudlite_core::peer::PEER_HEADER, w);
        }
        assert_eq!(r.send().await.unwrap().status(), 403, "secret {wrong:?}");
    }
}

/// With the secret, the peer listener honours the forwarded identity and answers /healthz.
/// Without the secret it refuses — loudly, so a misconfigured peer cannot look merely unhealthy.
#[tokio::test(flavor = "multi_thread")]
async fn the_peer_listener_serves_healthz_and_honours_identity() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet(1);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let c = client().await;
    let ok = |p: &str| format!("http://{}{p}", a.peer);
    assert_eq!(
        c.get(ok("/healthz"))
            .header(kloudlite_core::peer::PEER_HEADER, SECRET)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(c.get(ok("/healthz")).send().await.unwrap().status(), 403);
    let res = c
        .get(ok("/alice/web/info/refs?service=git-upload-pack"))
        .header(kloudlite_core::peer::OWNER_HEADER, "alice")
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

/// A second node asking for a repo someone already holds is told the holder and forwards there.
/// C is sent a request (hops=1) for a repo A has claimed; C's own copy of the map may be behind,
/// so it asks the leader, is told "held by A", and forwards. Only A's pool opens the repo.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_node_is_told_the_holder_and_forwards_there() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let c = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(&repo).await.unwrap();
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            c.peer
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(kloudlite_core::peer::HOPS_HEADER, "1")
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
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

/// Out of hops for a repo the map says someone else holds: NOT served here (that would be a
/// knowing wrong open) and NOT forwarded (that is the bound) — 503. Only a node whose own routing
/// says Local serves at the hop limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_out_of_hops_is_refused_unless_local() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(&repo).await.unwrap();
    let res = client()
        .await
        .get(format!(
            "http://{}/{repo}/info/refs?service=git-upload-pack",
            b.peer
        ))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .header(
            kloudlite_core::peer::HOPS_HEADER,
            kloudlite_core::peer::MAX_HOPS.to_string(),
        )
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
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

/// A real git push and clone through a forwarding node. Push is over 1 MiB so Expect:
/// 100-continue and chunked framing are exercised.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_git_push_and_clone_work_through_a_forwarding_node() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    // The fixture FIRST: `renew_once` only renews repos this node has OPEN, so the window
    // between the claim and A's first forwarded request is not covered by the beat. Doing the
    // slow local work before the claim keeps that window at roughly one HTTP round trip.
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let url = format!("http://x:{token}@{}/{repo}.git", b.public);
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            // Hermetic: a developer with commit.gpgsign on would otherwise have
            // every fixture commit reach for a passphrase prompt that is not here.
            .args(["-c", "commit.gpgsign=false"])
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
    a.app.claim(&repo).await.unwrap(); // A holds it; every request to B is forwarded
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
    let f = fleet(1);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), LEADER, &f).await;
    a.app.claim(&repo).await.unwrap();
    let adb = a.store.pool.get(o, n).await.unwrap(); // a holds it — kept, see the not-owner test
    // a stray opener (an admin command, say) takes the writer epoch
    let stray = slatedb::Db::builder(kloudlite_storage::pool::path(o, n), e.store.os.clone())
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

/// A node fenced by a peer the map names as the owner must NOT reopen the repo: `Pool::get` evicts and
/// reports, and nothing in the request path reopens because routing says the peer owns it.
#[tokio::test(flavor = "multi_thread")]
async fn a_fenced_node_does_not_reopen_when_it_is_not_the_owner() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    // The map says A holds it, but B has the database open anyway — a stale opener, which is
    // exactly what fencing is the backstop for.
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(&repo).await.unwrap();
    // HOLD b's handle across the fence: if it were dropped and re-fetched after a took the epoch,
    // a fast manifest poll could have already flagged it, the re-fetch would evict and return Err,
    // and the wait below would be skipped — then the expect_err after would see a fresh reopen.
    let bdb = b.store.pool.get(o, n).await.unwrap();
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
    assert!(kloudlite_storage::pool::is_fenced(&e2), "got: {e2}");
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

/// The harness must renew what it holds, or any test whose middle takes longer than
/// `LEASE_TTL` silently stops testing forwarding and starts testing claiming — the failure
/// mode that made the real git/ssh tests flake only under load.
#[tokio::test]
async fn a_claim_outlives_an_operation_longer_than_the_lease() {
    let e = common::env().await;
    let f = fleet(3);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(&repo).await.unwrap();
    // Open it on A immediately, as a real request would in the same handler: `renew_once` only
    // renews repos this node has OPEN (`Pool::warm_repos`), so a claim with nothing behind it
    // yet is not the beat's job to save — it is a request never having arrived.
    let seed_url = format!("http://{}/{repo}.git/info/refs?service=git-upload-pack", a.public);
    let _ = reqwest::get(&seed_url).await.unwrap();

    // Longer than LEASE_TTL (10 s), which is what a loaded box's git fixture work costs.
    tokio::time::sleep(std::time::Duration::from_secs(13)).await;

    // B must still forward, not claim: it opens nothing.
    let url = format!("http://{}/{repo}.git/info/refs?service=git-upload-pack", b.public);
    let _ = reqwest::get(&url).await.unwrap();
    assert_eq!(b.store.pool.warm_count(), 0, "B claimed the repo — A's lease lapsed");
    assert_eq!(a.store.pool.warm_count(), 1, "A no longer holds the repo it claimed");
}

/// A push that hits an already-observed fence on a node that is STILL the owner: the fence
/// surfaces at `open()`, `on_fenced` says Local, the handle is evicted and reopened, and the push
/// must SUCCEED.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_after_a_stray_opener_succeeds() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(1);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), LEADER, &f).await;
    a.app.claim(&repo).await.unwrap();
    let adb = a.store.pool.get(o, n).await.unwrap();
    let stray = slatedb::Db::builder(kloudlite_storage::pool::path(o, n), e.store.os.clone())
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
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(1);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), LEADER, &f).await;
    a.app.claim(&repo).await.unwrap();
    let _adb = a.store.pool.get(o, n).await.unwrap();
    // the stray stays open for the whole push: whichever arm fires, nothing may be swallowed
    let stray = slatedb::Db::builder(kloudlite_storage::pool::path(o, n), e.store.os.clone())
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
    } else {
        let ls = common::git(&work, &["ls-remote", &url, "refs/heads/main"]);
        assert!(ls.contains("refs/heads/main"), "pushed ref must have landed: {ls}");
    }
    let _ = stray.close().await;
}

/// An unhealthy node must stop serving the repos it holds: it cannot reach the object store, so
/// answering would fail anyway and its lease is about to lapse for whoever takes over. It still
/// FORWARDS what it does not hold — forwarding needs nothing from the object store — and it must
/// not claim anything new.
#[tokio::test(flavor = "multi_thread")]
async fn an_unhealthy_node_stops_serving_but_still_forwards() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let (mine, theirs) = ("alice/mine".to_string(), "alice/theirs".to_string());
    for r in [&mine, &theirs] { let (o, n) = r.split_once('/').unwrap(); e.store.create_repo(o, n).await.unwrap(); }
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(&mine).await.unwrap();
    b.app.claim(&theirs).await.unwrap();
    // A reads the map through a follower's poll, so wait for b's entry to reach it: otherwise A
    // sees `theirs` as unowned and takes the claim path instead of the forward path under test.
    for _ in 0..50 {
        if a.app.owner(&theirs).await.unwrap().is_some() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(a.app.owner(&theirs).await.unwrap().is_some(), "A never saw B's entry");
    let c = client().await;

    // Healthy: a serves the repo it holds, and /healthz answers.
    let res = c.get(format!("http://{}/{mine}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.store.pool.warm_count(), 1);
    assert_eq!(c.get(format!("http://{}/healthz", a.peer)).header(kloudlite_core::peer::PEER_HEADER, SECRET).send().await.unwrap().status(), 200);

    // Flip a unhealthy directly (the probe loop is not under test).
    a.store.healthy.store(false, std::sync::atomic::Ordering::Relaxed);

    // The repo it holds: 503, not served. Pool count unchanged (no new open).
    let res = c.get(format!("http://{}/{mine}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 503, "unhealthy: must not serve even what it holds");
    assert_eq!(c.get(format!("http://{}/healthz", a.peer)).header(kloudlite_core::peer::PEER_HEADER, SECRET).send().await.unwrap().status(), 503);
    // A repo b holds: a still FORWARDS it (forwarding is safe), b serves.
    let res = c.get(format!("http://{}/{theirs}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 200, "unhealthy node still forwards what it does not hold");
    assert_eq!(b.store.pool.warm_count(), 1);
    assert_eq!(a.store.pool.warm_count(), 1, "a opened nothing new");
    // And a cold repo is not claimed: an unhealthy node must not take a lease it cannot serve.
    e.store.create_repo("alice", "cold").await.unwrap();
    let res = c.get(format!("http://{}/alice/cold/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(a.app.owner("alice/cold").await.unwrap(), None, "nothing claimed");
}

/// A claim on an unowned repo is granted to whoever asked, and ONLY that node opens the database.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_on_an_unowned_repo_is_granted_and_only_the_claimant_warms() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    e.store.create_repo("alice", "web").await.unwrap();
    // B is a follower: its claim travels to A, which decides and writes the map.
    let res = client().await
        .get(format!("http://{}/alice/web/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.app.owner("alice/web").await.unwrap().unwrap().node, "kloudlite-1");
    assert_eq!(b.store.pool.warm_count(), 1, "the claimant opened it");
    assert_eq!(a.store.pool.warm_count(), 0, "the leader did not — it only granted");
}

/// An invented repo name costs the elected writer nothing: `route` gates the claim on the prefix
/// existing, so a spray of distinct bad names writes no map entries at all. The create route is
/// the exemption, and it still claims — that is what keeps the first write to a new repo leased.
#[tokio::test(flavor = "multi_thread")]
async fn an_invented_repo_name_is_404_and_claims_nothing() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    for i in 0..5 {
        let res = client().await
            .get(format!("http://{}/alice/nope{i}/info/refs?service=git-upload-pack", b.public))
            .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
            .send().await.unwrap();
        assert_eq!(res.status(), 404, "an invented name is not found, not 503");
        assert_eq!(a.app.owner(&format!("alice/nope{i}")).await.unwrap(), None, "nothing claimed");
    }
    assert_eq!(b.store.pool.warm_count(), 0, "and nothing was opened");
    // A real repo still routes and claims exactly as before.
    e.store.create_repo("alice", "web").await.unwrap();
    let res = client().await
        .get(format!("http://{}/alice/web/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.app.owner("alice/web").await.unwrap().unwrap().node, "kloudlite-1");
}

/// The prefix probe and the map read are not one atomic look: a creator on another node can claim
/// the key and flush between them. Answering `Missing` then would send the request to a handler
/// HERE, which would open the database unleased and fence the owner. Routing must re-read and
/// forward instead — this is that ordering, with the entry already in place and the prefix empty.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_that_lands_while_the_prefix_is_still_empty_is_forwarded_not_served() {
    let e = common::env().await;
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    // A is mid-create: it holds the lease, but nothing of `alice/fresh` has been flushed yet.
    a.app.claim("alice/fresh").await.unwrap();
    assert!(!b.store.pool.exists("alice", "fresh").await.unwrap(), "the prefix must still be empty");
    match b.app.route("alice/fresh").await {
        kloudlite_storage::ownership::Route::Peer(p) => assert_eq!(p.name, LEADER),
        other => panic!("must forward to the claimant, got {other:?}"),
    }
    assert_eq!(b.store.pool.warm_count(), 0, "and B opened nothing");
}

/// The gate replaced one leader WRITE per invented name per LEASE_TTL with a leader READ — which
/// would be one per request without this cache. A repeated invented name must cost exactly one
/// ask per window, however often it is routed.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_invented_name_asks_the_leader_once_per_window() {
    let e = common::env().await;
    let f = fleet(2);
    let _a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let asks = || b.app.owner_asks.load(std::sync::atomic::Ordering::Relaxed);
    let before = asks();
    for _ in 0..5 {
        assert_eq!(b.app.route("alice/nope").await, kloudlite_storage::ownership::Route::Missing);
    }
    assert_eq!(asks() - before, 1, "one leader read for the whole window");
    // Past the window it asks again — the cache forgets, so a name that becomes real is seen.
    b.app.advance_clock(kloudlite_app::MISSING_ASK_EVERY + std::time::Duration::from_millis(1));
    assert_eq!(b.app.route("alice/nope").await, kloudlite_storage::ownership::Route::Missing);
    assert_eq!(asks() - before, 2, "and once more in the next window");
    assert_eq!(b.store.pool.warm_count(), 0);
}

/// `may_create` is the whole exempt set: the create route and registry writes claim an
/// empty-prefix name, everything else does not.
#[test]
fn only_the_create_routes_may_claim_a_name_that_does_not_exist() {
    use axum::http::Method;
    use kloudlite_server::router_test::may_create;
    assert!(may_create(&Method::POST, "/api/alice/web/create"));
    assert!(may_create(&Method::POST, "/v2/alice/web/blobs/uploads/"));
    assert!(may_create(&Method::PUT, "/v2/alice/web/manifests/v1"));
    assert!(!may_create(&Method::GET, "/v2/alice/web/manifests/v1"));
    assert!(may_create(&Method::PATCH, "/v2/alice/web/blobs/uploads/deadbeef"));
    assert!(!may_create(&Method::HEAD, "/v2/alice/web/blobs/sha256:abc"));
    // DELETE can only remove what is already there, so it never needs to claim a name that does
    // not exist — and left exempt it would be the same anonymous amplifier as an unGATED GET.
    assert!(!may_create(&Method::DELETE, "/v2/alice/web/manifests/v1"));
    assert!(!may_create(&Method::DELETE, "/v2/alice/web/blobs/sha256:abc"));
    assert!(!may_create(&Method::POST, "/alice/web/git-receive-pack"));
    assert!(!may_create(&Method::GET, "/api/alice/web/refs"));
    assert!(!may_create(&Method::DELETE, "/api/alice/web/volumedelete"));
}

/// A follower that is asked to decide ownership answers 421: it is not the leader, and the
/// caller's idea of who is has gone stale. It must not proxy the message on, and must not answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_follower_refuses_to_decide_ownership() {
    let e = common::env().await;
    let f = fleet(2);
    let _a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let c = client().await;
    for what in ["claim", "renew", "release"] {
        let res = c
            .post(format!("http://{}/own/{what}", b.peer))
            .header(kloudlite_core::peer::PEER_HEADER, SECRET)
            .body("alice/web\nkloudlite-1")
            .send().await.unwrap();
        assert_eq!(res.status(), 421, "/own/{what} on a follower");
    }
    // And the leader does answer, so 421 is about leadership, not about the route existing.
    let res = c
        .post(format!("http://{}/own/claim", _a.peer))
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
        .body("alice/web\nkloudlite-1")
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.text().await.unwrap().starts_with("granted\nkloudlite-1\n"));
}

/// The leader is unreachable: a cold repo gets a 503 and NOBODY opens it.
///
/// This is the "do not fail over to ordinal one" rule, end to end. Every previous design let a
/// surviving node conclude for itself that it should take over, and every one of them produced two
/// owners. An unclaimable repo is unavailable — bounded by how long pod zero takes to come back.
#[tokio::test(flavor = "multi_thread")]
async fn a_cold_repo_is_503_when_the_leader_cannot_be_reached() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    e.store.create_repo("alice", "web").await.unwrap();
    // A starts (it must, to create the ownership database) but B's view of the fleet points the
    // leader's name at a refused port — the partition case, and the restart case.
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let mut b_view = f.clone();
    b_view.iter_mut().find(|(n, _)| n == LEADER).unwrap().1 = "127.0.0.1:1".into();
    let b_addr = f[1].1.clone();
    let mut b_fleet = b_view.clone();
    b_fleet[1].1 = b_addr;
    let b = node(e.store.os.clone(), "kloudlite-1", &b_fleet).await;
    let res = client().await
        .get(format!("http://{}/alice/web/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 503, "no leader, no claim, no service");
    assert_eq!(b.store.pool.warm_count(), 0, "B must NOT take over");
    assert_eq!(a.store.pool.warm_count(), 0);
    assert_eq!(a.app.owner("alice/web").await.unwrap(), None, "nothing was granted");
}

/// A release deletes the entry, so the repo is claimable at once — the releaser closed its
/// database before releasing, so there is nothing left for the successor to fence.
#[tokio::test(flavor = "multi_thread")]
async fn a_release_makes_a_repo_claimable_at_once() {
    let e = common::env().await;
    let f = fleet(3);
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    let repo = "alice/web";
    assert!(matches!(a.app.claim(repo).await.unwrap(), kloudlite_storage::ownership::Grant::Granted(_)));
    a.app.release(repo).await.unwrap();
    // Read through the leader's writer handle: a follower's copy is up to a poll interval behind,
    // and what is under test is the delete, not the propagation.
    assert_eq!(leader.app.owner(repo).await.unwrap(), None, "the entry is deleted, not shortened");
    match b.app.claim(repo).await.unwrap() {
        kloudlite_storage::ownership::Grant::Granted(e) => assert_eq!(e.node, "kloudlite-2"),
        g => panic!("a released repo must be claimable at once: {g:?}"),
    }
}

/// A stale release must not delete somebody else's entry: the node that lost the repo says
/// "release" late, and the map must still name the new owner.
#[tokio::test(flavor = "multi_thread")]
async fn a_release_from_a_node_that_no_longer_holds_it_is_ignored() {
    let e = common::env().await;
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let repo = "alice/web";
    assert!(matches!(b.app.claim(repo).await.unwrap(), kloudlite_storage::ownership::Grant::Granted(_)));
    a.app.release(repo).await.unwrap(); // the leader never held it
    assert_eq!(
        a.app.owner(repo).await.unwrap().map(|e| e.node),
        Some("kloudlite-1".to_string()),
        "a stale release deleted the real owner's entry"
    );
}

async fn stream_listener(store: Arc<kloudlite_storage::store::Store>) -> String {
    let app = common::app(store).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { kloudlite_vcs::proxy::serve_peer_streams(app, l).await });
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
    let f = fleet(4);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let c = node(e.store.os.clone(), "kloudlite-3", &f).await;
    a.app.claim(&repo).await.unwrap();
    // Talk to C's STREAM port directly as if we were B, hops=1. C is not the owner and can reach A,
    // so C forwards to A and must relay A's status.
    let mut sock = tokio::net::TcpStream::connect(kloudlite_core::peer::stream_addr(&c.peer)).await.unwrap();
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
    let f = fleet(3);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let token = e.store.create_token(o).await.unwrap();

    let kd = tempfile::tempdir().unwrap();
    let key = kd.path().join("id_ed25519");
    assert!(std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()])
        .status().unwrap().success());
    let pubkey = std::fs::read_to_string(kd.path().join("id_ed25519.pub")).unwrap();
    let fp = common::ssh_fingerprint(&pubkey).unwrap();
    e.store.add_ssh_key(o, &fp).await.unwrap();

    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;

    // The fixture FIRST: `renew_once` only renews repos this node has OPEN, so the window
    // between the claim and A's first forwarded request is not covered by the beat. Doing the
    // slow local work before the claim keeps that window at roughly one HTTP round trip.
    //
    // b also speaks SSH; b does not own the repo, so every session it accepts is forwarded to a.
    let host_key = gen_host_key(kd.path());
    let ssh_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ssh_port = ssh_l.local_addr().unwrap().port();
    let b_app = b.app.clone();
    tokio::spawn(async move { kloudlite_vcs::ssh::serve(b_app, ssh_l, host_key).await.unwrap() });

    // One commit, pushed over a's public HTTP port, so the repo has content. This also opens a's
    // copy before the claim below — fine: `claim` on a repo this node already has open is a no-op
    // re-assert, and the beat covers it from then on.
    let w = tempfile::tempdir().unwrap();
    let http_url = format!("http://x:{token}@{}/{repo}.git", a.public);
    common::git(w.path(), &["clone", "-q", &http_url, "seed"]);
    let seed = w.path().join("seed");
    std::fs::write(seed.join("f.txt"), "one\n").unwrap();
    common::git(&seed, &["add", "."]);
    common::git(&seed, &["commit", "-qm", "one"]);
    common::git(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    a.app.claim(&repo).await.unwrap(); // A holds it; b forwards every session

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

/// A percent-encoded repo name must not bypass routing. The middleware sees the raw path and
/// cannot parse `we%62`; the handler would decode it to `web` and open it locally — on a node
/// that may not own it. Refuse at the middleware; neither node's pool may go warm.
#[tokio::test(flavor = "multi_thread")]
async fn a_percent_encoded_repo_path_is_refused_not_routed_around() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(2);
    // a repo b holds, so hitting a with an encoded name would — if routing were bypassed —
    // open it locally on a, the non-owner
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    b.app.claim(&repo).await.unwrap();
    // encode the last byte of the repo name
    let last = n.chars().last().unwrap();
    let encoded = format!("{}%{:02x}", &n[..n.len() - last.len_utf8()], last as u32);
    let res = client().await
        .get(format!("http://{}/{o}/{encoded}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 400, "encoded name: refused at the middleware");
    assert_eq!(a.store.pool.warm_count(), 0, "a (non-owner) must NOT have opened it");
    assert_eq!(b.store.pool.warm_count(), 0, "and it was not forwarded either");
}

/// The whole release ordering, end to end through the pool: a node that evicts a repo keeps the
/// lease AND the handle through the drain, then closes, and only then releases. A second node may
/// not have the repo until the database is actually shut.
#[tokio::test(flavor = "multi_thread")]
async fn an_evicted_repo_is_claimable_by_another_node_only_after_the_drain() {
    let e = common::env().await;
    let f = fleet(3);
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    let repo = "alice/web";
    // The repo must exist: routing does not claim a repo that does not, it lets the handler 404.
    e.store.create_repo("alice", "web").await.unwrap();

    assert_eq!(b.app.route_for(repo, true).await, kloudlite_storage::ownership::Route::Local);
    b.store.pool.get("alice", "web").await.unwrap();
    assert_eq!(b.store.pool.warm_count(), 1);

    // Nothing is using it and the TTL is now zero: the next sweep evicts it.
    b.store.pool.set_idle_ttl(std::time::Duration::ZERO);
    b.store.pool.sweep().await;

    assert_eq!(b.store.pool.warm_count(), 1, "the database must stay open through the drain");
    match a.app.claim(repo).await.unwrap() {
        kloudlite_storage::ownership::Grant::HeldBy(h) => assert_eq!(h.node, "kloudlite-2"),
        g => panic!("claimable while the loser still holds the database open: {g:?}"),
    }

    b.store.pool.await_retires().await;
    assert_eq!(b.store.pool.warm_count(), 0, "the database must be closed after the drain");
    match a.app.claim(repo).await.unwrap() {
        kloudlite_storage::ownership::Grant::Granted(g) => assert_eq!(g.node, a.app.self_name),
        g => panic!("still not claimable after the drain: {g:?}"),
    }
}

/// During the drain the evicting node is still the owner, and both nodes must still route there:
/// that is what the drain is for — a follower whose copy of the map is behind arrives and is
/// served, not fenced.
#[tokio::test(flavor = "multi_thread")]
async fn an_evicting_node_still_owns_the_repo_during_the_drain() {
    let e = common::env().await;
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let repo = "alice/web";
    assert_eq!(b.app.route_for(repo, true).await, kloudlite_storage::ownership::Route::Local);
    b.store.pool.get("alice", "web").await.unwrap();

    b.store.pool.set_idle_ttl(std::time::Duration::ZERO);
    b.store.pool.sweep().await;

    assert_eq!(b.store.pool.warm_count(), 1, "closed before the drain was over");
    assert_eq!(b.app.route_for(repo, true).await, kloudlite_storage::ownership::Route::Local, "still serving");
    match a.app.route_for(repo, true).await {
        kloudlite_storage::ownership::Route::Peer(p) => assert_eq!(p.name, "kloudlite-1"),
        r => panic!("the other node must still forward to the evicting node: {r:?}"),
    }
}

/// A lease can be taken away — a node that was partitioned long enough for its entry to lapse
/// comes back to find the repo elsewhere. It must close the database the moment its renewal is
/// declined, not wait to be fenced.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_loses_its_lease_closes_the_database() {
    let e = common::env().await;
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let repo = "alice/web";
    assert_eq!(b.app.route_for(repo, true).await, kloudlite_storage::ownership::Route::Local);
    b.store.pool.get("alice", "web").await.unwrap();
    assert_eq!(b.store.pool.warm_count(), 1);

    // The leader hands it to someone else, as it would after b's lease lapsed.
    a.app
        .ownership
        .put(repo, &kloudlite_storage::ownership::Entry {
            node: LEADER.into(),
            expires_ms: kloudlite_storage::ownership::now_ms() + 60_000,
        })
        .await
        .unwrap();

    b.app.renew_once().await.unwrap();
    assert_eq!(b.store.pool.warm_count(), 0, "a lost lease must close the database at once");
}

/// The rolling-restart case. Pod zero updates last, so while it is down every follower's renewals
/// fail and every entry ages out — but a node that is already holding a repo open, whose (expired)
/// entry names itself, may keep serving it. A grant only ever comes from the leader, so an
/// unreachable leader means nobody else can have been granted it. A COLD repo still 503s and
/// nobody opens it: that pair is the whole point.
#[tokio::test(flavor = "multi_thread")]
async fn a_warm_repo_still_serves_when_the_leader_is_unreachable() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    e.store.create_repo("alice", "web").await.unwrap();
    e.store.create_repo("alice", "cold").await.unwrap();
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let c = client().await;
    let get = |n: &Node, repo: &str| {
        let url = format!("http://{}/{repo}/info/refs?service=git-upload-pack", n.public);
        let (c, token) = (c.clone(), token.clone());
        async move {
            c.get(url)
                .basic_auth("x", Some(&token))
                .header("git-protocol", "version=2")
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    // B claims and warms alice/web while the leader is up.
    assert_eq!(get(&b, "alice/web").await, 200);
    assert_eq!(b.store.pool.warm_count(), 1);

    // The leader goes away, and its lease ages out: exactly what a roll produces.
    assert_eq!(a.app.owner("alice/web").await.unwrap().unwrap().node, "kloudlite-1");
    // Age the entry out the way a roll does — the leader is the last pod updated, so every
    // follower's renewals fail for the whole ten seconds it is down. Written directly rather than
    // slept through, then given a follower poll (200ms) to reach B.
    a.app
        .ownership
        .put(
            "alice/web",
            &kloudlite_storage::ownership::Entry {
                node: "kloudlite-1".into(),
                expires_ms: kloudlite_storage::ownership::now_ms() - 1,
            },
        )
        .await
        .unwrap();
    // B reads the map through a follower poll (200ms); wait for the expired entry to reach it.
    for _ in 0..50 {
        let seen = b.app.owner("alice/web").await.unwrap();
        if seen.is_some_and(|e| e.expires_ms < kloudlite_storage::ownership::now_ms()) { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(b.app.owner("alice/web").await.unwrap().unwrap().expires_ms < kloudlite_storage::ownership::now_ms(), "B never saw the lapsed entry");
    blackholed().lock().unwrap().insert(f[0].1.clone());

    assert_eq!(get(&b, "alice/web").await, 200, "warm and ours: keep serving");
    assert_eq!(get(&b, "alice/cold").await, 503, "cold: nobody may claim it");
    assert_eq!(b.store.pool.warm_count(), 1, "the cold repo must not be opened");
    assert_eq!(a.store.pool.warm_count(), 0);
}

/// The other half of drain-close-release: a request that lands DURING the drain keeps the database
/// open, so `close_all` skips it — and the release must be skipped with it. Releasing a handle that
/// stayed warm would leave this node holding an open database the map says nobody owns, which is
/// exactly the window in which a successor claims it and fences a live writer.
#[tokio::test(flavor = "multi_thread")]
async fn a_repo_that_goes_warm_again_during_the_drain_is_not_released() {
    let e = common::env().await;
    let f = fleet(2);
    let _a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let repo = "alice/web";
    e.store.create_repo("alice", "web").await.unwrap();
    assert_eq!(b.app.route_for(repo, true).await, kloudlite_storage::ownership::Route::Local);
    b.store.pool.get("alice", "web").await.unwrap();

    // Retire it, then take a reference back before the drain is over.
    b.store.pool.set_idle_ttl(std::time::Duration::ZERO);
    b.store.pool.sweep().await;
    let held = b.store.pool.get("alice", "web").await.unwrap();
    b.store.pool.await_retires().await;

    assert_eq!(b.store.pool.warm_count(), 1, "an in-use database must not be closed");
    assert_eq!(
        b.app.owner(repo).await.unwrap().map(|x| x.node),
        Some("kloudlite-1".to_string()),
        "the lease was released under a database that is still open"
    );
    drop(held);
}

/// A node that dies without releasing — kill -9, OOM, a partition — leaves its entry behind. The
/// lease timestamp is the only thing that reclaims it: it lapses, and the leader's prune deletes
/// it. This is why `expires_ms` survives release becoming a plain delete.
#[tokio::test(flavor = "multi_thread")]
async fn an_entry_left_by_a_dead_node_is_reclaimed_by_prune() {
    let e = common::env().await;
    let f = fleet(3);
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let repo = "alice/web";
    // As if kloudlite-2 claimed it and was killed: the entry is here, and it has lapsed.
    let dead = kloudlite_storage::ownership::Entry {
        node: "kloudlite-2".to_string(),
        expires_ms: kloudlite_storage::ownership::now_ms() - 1,
    };
    leader.app.ownership.put(repo, &dead).await.unwrap();

    leader.app.prune_once().await.unwrap();
    assert_eq!(leader.app.owner(repo).await.unwrap(), None, "a lapsed entry must be pruned");
    match a.app.claim(repo).await.unwrap() {
        kloudlite_storage::ownership::Grant::Granted(g) => assert_eq!(g.node, a.app.self_name),
        g => panic!("the repo must be claimable after the dead node's entry is pruned: {g:?}"),
    }
}

/// A node on its way out must not take a lease. SIGTERM releases every lease and closes the pool;
/// a request arriving in the drain window sees the released entry as absent and would claim the
/// repo straight back — and the leader cannot tell that the asker is seconds from exiting. The
/// grant would name a node whose `pool.get` fails, and every other node would forward there for a
/// full LEASE_TTL.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_whose_pool_is_closed_does_not_claim() {
    let e = common::env().await;
    let f = fleet(3);
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let repo = "alice/web";
    e.store.create_repo("alice", "web").await.unwrap();

    // Warm, then shut down the way SIGTERM does: releases the lease and closes the pool.
    assert_eq!(a.app.route_for(repo, true).await, kloudlite_storage::ownership::Route::Local);
    a.store.pool.get("alice", "web").await.unwrap();
    a.store.pool.close().await;

    assert!(a.store.pool.is_closed());
    assert_eq!(
        a.app.route_for(repo, true).await,
        kloudlite_storage::ownership::Route::Unavailable,
        "a closed pool must refuse, not claim the repo back on its way out"
    );
    assert_eq!(
        leader.app.owner(repo).await.unwrap(),
        None,
        "and the map must not name a node that is exiting"
    );
}

/// A forward into an owner that has already gone must recover, not 502.
///
/// This is the shape of every remaining failure of a rolling restart: the owner releases its lease
/// at SIGTERM and stops answering, while another node's copy of the map is still a poll behind, so
/// it forwards into a node that is no longer there. Recovery re-reads the map — which the leader
/// has since updated — and goes where it now points.
#[tokio::test(flavor = "multi_thread")]
async fn a_forward_to_a_departed_owner_recovers() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    e.store.create_repo("alice", "web").await.unwrap();
    e.store.create_repo("alice", "other").await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;

    // A owns both, so B's copy of the map points at A.
    a.app.claim("alice/other").await.unwrap();
    a.app.claim(&repo).await.unwrap();
    for _ in 0..50 {
        if b.app.owner(&repo).await.unwrap().is_some() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(b.app.owner(&repo).await.unwrap().unwrap().node, "kloudlite-1");

    // A leaves the way SIGTERM makes it leave: lease released, then unreachable.
    a.app.release(&repo).await.unwrap();
    blackholed().lock().unwrap().insert(f[1].1.clone());

    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send().await.unwrap();
    blackholed().lock().unwrap().remove(&f[1].1);
    assert_eq!(res.status(), 200, "a forward into a departed owner must recover, not 502");
    assert_eq!(b.store.pool.warm_count(), 1, "B took it over");
}

/// A browse request must be routed by the repo the BROWSE HANDLER will open, and by nothing else.
///
/// `/api/alice/info/refs` is the browse route of `alice/info` — that is what axum's matchit
/// dispatches. The middleware must agree. It is observable only on a node that owns neither repo:
/// `api/alice` is claimed by a DIFFERENT node here, so a middleware that read the path as the git
/// route of `api/alice` (as it once did) forwards to the leader instead of to A, and the wrong
/// node's pool goes warm. Warm counts are the evidence — a single-node fixture claims everything
/// locally and can never show this.
#[tokio::test(flavor = "multi_thread")]
async fn a_browse_request_is_routed_by_the_repo_the_handler_opens() {
    let e = common::env().await;
    e.store.create_repo("alice", "info").await.unwrap();
    let f = fleet(3);
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    // The two candidate readings of the one path, held by two different nodes.
    a.app.claim("alice/info").await.unwrap();
    leader.app.claim("api/alice").await.unwrap();

    // B owns neither, so it must forward — and where it forwards is the whole question.
    let res = client()
        .await
        .get(format!("http://{}/api/alice/info/refs", b.peer))
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
        .header(kloudlite_core::peer::OWNER_HEADER, "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.store.pool.warm_count(), 1, "A owns alice/info and served it");
    assert_eq!(
        leader.store.pool.warm_count(),
        0,
        "routed as api/alice: forwarded to the wrong repo's owner"
    );
    assert_eq!(b.store.pool.warm_count(), 0, "B owns neither, it forwards");
}

/// A three-segment `/api/` path matches no browse route, so matchit would hand it to the GIT
/// handler as owner=`api` name=`alice`. It must be refused in the middleware instead: falling
/// through serves a repo's upload-pack on a node that never checked whether it owns it.
#[tokio::test(flavor = "multi_thread")]
async fn an_api_path_that_is_not_a_browse_route_is_refused_not_dispatched() {
    let e = common::env().await;
    let f = fleet(2);
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    // A legacy repo owned by the now-reserved `api`, held by the leader.
    e.store.pool.get("api", "alice").await.unwrap();
    leader.app.claim("api/alice").await.unwrap();
    // POST for the git verbs: the real method, so a fall-through would run the handler rather than
    // stopping at a 405.
    for (post, path) in [
        (true, "/api/alice/git-upload-pack"),
        (true, "/api/alice/git-receive-pack"),
        (false, "/api/alice/info/x"),
        (false, "/api/alice"),
    ] {
        let url = format!("http://{}{path}", b.peer);
        let c = client().await;
        let r = if post { c.post(&url).body("0000") } else { c.get(&url) };
        let res = r
            .header(kloudlite_core::peer::PEER_HEADER, SECRET)
            .header(kloudlite_core::peer::OWNER_HEADER, "alice")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404, "{path}");
    }
    assert_eq!(b.store.pool.warm_count(), 0, "B opened a repo it does not own");
    assert_eq!(leader.store.pool.warm_count(), 0, "not forwarded either");
}

/// `admin set-visibility` used to write `meta/public` from its own process while the owning node
/// answered from its own handle, so a repo could be private in the database and still be authorized
/// as public by the node serving it. The flip now goes through `/api/{o}/{n}/visibility` on the
/// peer listener, which the `route` middleware delivers to the owner.
///
/// Catches: the flip being served locally by a node that does not own the repo. C must not open the
/// database; A must, because A is where the write has to land.
#[tokio::test(flavor = "multi_thread")]
async fn a_visibility_flip_is_routed_to_the_owner() {
    let e = common::env().await;
    let f = fleet(3);
    e.store.create_repo("alice", "web").await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let c = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim("alice/web").await.unwrap();
    // Warm A's handle BEFORE the flip: the defect is a stale view in an ALREADY-OPEN database, so
    // a test that flips first and reads after would pass even with a second writer.
    let res = client()
        .await
        .get(format!("http://{}/api/alice/web/refs", a.peer))
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
        .header(kloudlite_core::peer::OWNER_HEADER, "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "owner browse warms the serving node");
    assert_eq!(a.store.pool.warm_count(), 1);
    let res = client()
        .await
        .post(format!(
            "http://{}/api/alice/web/visibility?visibility=public",
            c.peer
        ))
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(a.store.pool.warm_count(), 1, "the owner performed the write");
    assert_eq!(c.store.pool.warm_count(), 0, "C must not have opened it");
    // The whole point of routing it there: the node that SERVES the repo now sees it as public.
    // A test that only re-read the database would pass even with the second-writer defect.
    let res = client()
        .await
        .get(format!("http://{}/api/alice/web/refs", a.peer))
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "anonymous browse on the serving node");
}

/// Catches: the flip being reachable from the client-facing port. Both halves matter — the status,
/// and that nothing was written: without the `/api/` guard in `route_inner`, a non-owner's PUBLIC
/// listener would forward this to the owner's peer port with the shared secret.
#[tokio::test(flavor = "multi_thread")]
async fn a_visibility_flip_is_refused_on_the_public_listener() {
    let e = common::env().await;
    let f = fleet(3);
    e.store.create_repo("alice", "web").await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let c = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim("alice/web").await.unwrap();
    for host in [&a.public, &c.public] {
        let res = client()
            .await
            .post(format!("http://{host}/api/alice/web/visibility?visibility=public"))
            .header(kloudlite_core::peer::PEER_HEADER, SECRET)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404, "public listener must refuse the flip");
    }
    assert!(
        !a.store.is_public("alice", "web").await.unwrap(),
        "still private: nothing may reach the owner from the public port"
    );
}

/// A node that dies HARD releases nothing, so the map keeps naming it for the whole LEASE_TTL and
/// every request for its repos 502s until the lease lapses. Recovery re-reads the map, finds the
/// same dead node, and — corroborated by a second connect failure ~350ms later — asks the leader to
/// move the repo. Warm counts are the evidence: a status code alone would not show that the repo
/// actually moved rather than being served by the wrong node.
///
/// Catches: the force-claim not being wired into the recovery path at all (502), and a leader that
/// still honours a live lease when the asker says it could not reach the holder.
#[tokio::test(flavor = "multi_thread")]
async fn a_hard_crashed_owner_is_taken_over_without_waiting_for_the_ttl() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    e.store.create_repo("alice", "web").await.unwrap();
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;

    a.app.claim(&repo).await.unwrap();
    for _ in 0..50 {
        if b.app.owner(&repo).await.unwrap().is_some() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(b.app.owner(&repo).await.unwrap().unwrap().node, "kloudlite-1");

    // Past the anti-flap window, so this is an old entry rather than one just written.
    // Age the entry past the anti-flap window on the LEADER's clock — the only clock
    // `decide_force_claim` reads. This ages the entry as A's claim wrote it; A's next renew
    // (3s) goes through the leader's own skewed clock and re-stamps it young again, so the
    // force-claim below must happen before then. It does: there is no await that long between.
    leader.app.advance_clock(kloudlite_storage::ownership::FORCE_MIN_AGE + std::time::Duration::from_millis(1));
    // A dies without a SIGTERM: no release, the entry stays live and named to A.
    blackholed().lock().unwrap().insert(f[1].1.clone());

    let started = std::time::Instant::now();
    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send().await.unwrap();
    let took = started.elapsed();
    blackholed().lock().unwrap().remove(&f[1].1);

    assert_eq!(res.status(), 200, "a hard-crashed owner must be taken over, not 502'd");
    assert!(took < kloudlite_storage::ownership::LEASE_TTL, "took {took:?}: that is waiting out the TTL");
    assert_eq!(b.store.pool.warm_count(), 1, "B took the repo over");
    assert_eq!(a.store.pool.warm_count(), 0, "A never served it");
    assert_eq!(
        leader.app.owner(&repo).await.unwrap().unwrap().node,
        "kloudlite-2",
        "the map must name the new owner"
    );
}

/// Two nodes recovering from the same dead owner race. The loser must be told who won and forward
/// there — never take the repo off the winner, which is how the repo ping-pongs and nobody serves.
///
/// Catches: `decide_force_claim` granting unconditionally (B would take it off C, and B's pool
/// would go warm), and a leader that answers HeldBy but lets the asker claim anyway.
#[tokio::test(flavor = "multi_thread")]
async fn a_force_claim_that_loses_the_race_honours_the_winner() {
    let e = common::env().await;
    let f = fleet(4);
    let repo = "alice/web".to_string();
    e.store.create_repo("alice", "web").await.unwrap();
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    let c = node(e.store.os.clone(), "kloudlite-3", &f).await;

    a.app.claim(&repo).await.unwrap();
    // Age the entry past the anti-flap window on the LEADER's clock — the only clock
    // `decide_force_claim` reads. This ages the entry as A's claim wrote it; A's next renew
    // (3s) goes through the leader's own skewed clock and re-stamps it young again, so the
    // force-claim below must happen before then. It does: there is no await that long between.
    leader.app.advance_clock(kloudlite_storage::ownership::FORCE_MIN_AGE + std::time::Duration::from_millis(1));
    blackholed().lock().unwrap().insert(f[1].1.clone());

    // C gets there first.
    match c.app.force_claim(&repo).await.unwrap() {
        kloudlite_storage::ownership::Grant::Granted(en) => assert_eq!(en.node, "kloudlite-3"),
        g => panic!("C should have won: {g:?}"),
    }
    // B arrives moments later, having failed against the SAME dead owner.
    match b.app.force_claim(&repo).await.unwrap() {
        kloudlite_storage::ownership::Grant::HeldBy(en) => assert_eq!(
            en.node, "kloudlite-3",
            "the loser must be told the winner so it forwards there"
        ),
        g => panic!("a force-claim losing the race must not be granted: {g:?}"),
    }
    blackholed().lock().unwrap().remove(&f[1].1);
    assert_eq!(b.store.pool.warm_count(), 0, "the loser must not open the repo");
    assert_eq!(
        leader.app.owner(&repo).await.unwrap().unwrap().node,
        "kloudlite-3",
        "the winner keeps it"
    );
}

/// One dropped TCP connect must never move a repo. A failed forward asks the leader, and if the
/// leader says the holder still owns the lease the request tries that holder ONCE MORE before
/// forcing anything: a blip answers on the retry, a crashed node does not. The retry is immediate,
/// so the evidence costs a round trip rather than a timer.
///
/// Catches: force-claiming on the first connect error, with no corroboration.
#[tokio::test(flavor = "multi_thread")]
async fn one_connect_failure_does_not_move_a_repo() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    e.store.create_repo("alice", "web").await.unwrap();
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;

    a.app.claim(&repo).await.unwrap();
    for _ in 0..50 {
        if b.app.owner(&repo).await.unwrap().is_some() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Old enough that the anti-flap guard would NOT shield it: if the repo survives here it is
    // because one connect failure was not treated as grounds to move it, not because it was young.
    // Age the entry past the anti-flap window on the LEADER's clock — the only clock
    // `decide_force_claim` reads. This ages the entry as A's claim wrote it; A's next renew
    // (3s) goes through the leader's own skewed clock and re-stamps it young again, so the
    // force-claim below must happen before then. It does: there is no await that long between.
    leader.app.advance_clock(kloudlite_storage::ownership::FORCE_MIN_AGE + std::time::Duration::from_millis(1));

    // A blip, not a death: A refuses exactly one connection and then answers normally.
    flaky().lock().unwrap().insert(f[1].1.clone(), 1);

    let url = format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.public);
    let res = client().await.get(url).basic_auth("x", Some(&token))
        .header("git-protocol", "version=2").send().await.unwrap();
    flaky().lock().unwrap().remove(&f[1].1);

    assert_eq!(res.status(), 200, "the retry should have reached A and been served");
    assert_eq!(
        leader.app.owner(&repo).await.unwrap().unwrap().node,
        "kloudlite-1",
        "one connect failure moved the repo: a blip must not fence a healthy owner"
    );
    assert_eq!(b.app.store.pool.warm_count(), 0, "B must not have opened the repo");
}

/// A node that claims a repo and then cannot open it must give the lease back at once. Otherwise
/// the map names an owner that cannot serve, and every request 502s until the TTL lapses — worse
/// after a forced claim, which fenced a peer to get here. Before force-claims existed only a
/// healthy node could claim, so this gap was unreachable; now it is not.
///
/// Catches: `open()` returning 500 and keeping the lease. The failure is a read-only cache dir,
/// which fails `create_dir_all` inside `open_repo` on a node that is healthy in every other way.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_open_releases_the_lease_it_was_just_granted() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(2);
    let repo = "alice/web".to_string();
    e.store.create_repo("alice", "web").await.unwrap();
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;

    // Jam the shelf: A can claim, but cannot lay the repo down on disk.
    let cache = a._tmp.path().join("cache");
    let mut perms = std::fs::metadata(&cache).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&cache, perms.clone()).unwrap();

    let res = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 500, "the open genuinely failed");

    // `open_repo` warmed the database (`repo_exists` opens it) before `create_dir_all` failed.
    // It must be closed BEFORE the lease goes back: `release` with the handle still warm lets
    // the next claimant open the database while A still holds a live writer — the two-writer
    // window the ownership invariant forbids.
    assert_eq!(a.store.pool.warm_count(), 0, "A released the lease with the database still open");

    // The lease must not be sitting on A. Either nobody holds it, or it has already lapsed.
    let held = leader.app.owner(&repo).await.unwrap()
        .filter(|en| !kloudlite_storage::ownership::is_expired(en, kloudlite_storage::ownership::now_ms()));
    assert!(
        held.is_none(),
        "A kept a lease on a repo it cannot open: {held:?}"
    );

    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&cache, perms).unwrap();
}

/// A forward that fails asks the leader; a second failed forward for the same repo within a second
/// must NOT ask again. During a blip that touches many forwards at once, every one of them asking
/// is a burst on pod zero at the moment it is least able to take one; the first ask has already
/// moved the map, and a request inside the window gets a plain 502 to retry.
///
/// Catches: the throttle helper being absent or mis-keyed. It exercises the helper directly, so it
/// does NOT prove where the helper is wired — that it sits on the failed-forward path only, and
/// never on the cold-repo claim in `route()`, is held by the single call site in `router/route.rs`, and by
/// `a_hard_crashed_owner_is_taken_over_without_waiting_for_the_ttl` still passing on its first ask.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_failed_forward_within_a_second_does_not_ask_the_leader_again() {
    let e = common::env().await;
    let repo = "alice/web".to_string();
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet(2);
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;

    assert!(a.app.may_ask_to_recover(&repo), "first ask goes through");
    assert!(!a.app.may_ask_to_recover(&repo), "second ask inside the window is refused");
    assert!(a.app.may_ask_to_recover("bob/other"), "the window is per repo, not global");
    a.app.advance_clock(kloudlite_app::RECOVERY_ASK_EVERY + std::time::Duration::from_millis(1));
    assert!(a.app.may_ask_to_recover(&repo), "and it reopens after the window");
}

/// A failed PUSH forward must not consume the once-per-second recovery window: a push cannot be
/// replayed, so it can never recover, and if it burned the token a GET arriving right behind it
/// would get a plain 502 instead of taking the repo over. Runs through the real route path — the
/// first cut of this guard swapped two tuple elements and changed nothing, because a tuple pattern
/// evaluates every element; only a test on the wire could have caught that.
///
/// Catches: the throttle being consulted before replay-ability is known.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_push_forward_does_not_burn_the_recovery_window() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web".to_string();
    e.store.create_repo("alice", "web").await.unwrap();
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;

    a.app.claim(&repo).await.unwrap();
    for _ in 0..50 {
        if b.app.owner(&repo).await.unwrap().is_some() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Age the entry past the anti-flap window on the LEADER's clock — the only clock
    // `decide_force_claim` reads. This ages the entry as A's claim wrote it; A's next renew
    // (3s) goes through the leader's own skewed clock and re-stamps it young again, so the
    // force-claim below must happen before then. It does: there is no await that long between.
    leader.app.advance_clock(kloudlite_storage::ownership::FORCE_MIN_AGE + std::time::Duration::from_millis(1));
    blackholed().lock().unwrap().insert(f[1].1.clone());

    // A push into the dead owner: not replayable, so it must fail without touching the window.
    let push = client().await
        .post(format!("http://{}/{repo}/git-receive-pack", b.public))
        .basic_auth("x", Some(&token))
        .header("content-type", "application/x-git-receive-pack-request")
        .body("0000")
        .send().await.unwrap();
    assert_ne!(push.status(), 200, "a push cannot be recovered; it fails");

    // A GET right behind it must still be able to ask the leader and take the repo over.
    let get = client().await
        .get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send().await.unwrap();
    blackholed().lock().unwrap().remove(&f[1].1);

    assert_eq!(get.status(), 200, "the push must not have consumed the recovery window");
    assert_eq!(b.store.pool.warm_count(), 1, "B took the repo over");
}

/// Two different nodes race the leader for the same, currently-unowned repo. Before the
/// `leader_lock` fix, `grant_claim` was read-decide-write with no serialization: both calls could
/// read `None` for the repo and both write a grant, handing one repo to two nodes — which is
/// exactly the "detected newer DB client" fencing incident. `tokio::join!` polls both futures
/// concurrently on this test's single-threaded runtime, interleaving at every `.await` inside
/// `grant_claim` (including the SlateDB round trips in `get`/`put`), which is enough to expose the
/// race without needing a multi-threaded flavor or an injected yield.
#[tokio::test]
async fn concurrent_claims_never_grant_one_repo_twice() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let f = fleet(3);
    let leader = node(e.store.os.clone(), LEADER, &f).await;

    let (a, b) = tokio::join!(
        leader.app.grant_claim("alice/web", "kloudlite-1", false),
        leader.app.grant_claim("alice/web", "kloudlite-2", false),
    );
    let results = [a.unwrap(), b.unwrap()];
    let granted: Vec<_> = results
        .iter()
        .filter(|g| matches!(g, kloudlite_storage::ownership::Grant::Granted(_)))
        .collect();
    assert_eq!(granted.len(), 1, "expected exactly one Granted, got {results:?}");

    let holder = leader.app.owner("alice/web").await.unwrap().expect("map must name a holder");
    assert!(
        holder.node == "kloudlite-1" || holder.node == "kloudlite-2",
        "unexpected holder {holder:?}"
    );
}

#[test]
fn v2_paths_derive_the_image_key() {
    // repo_of is private, so assert through the public helper the middleware uses.
    assert_eq!(
        kloudlite_registry::image_route("/v2/acme/nginx/blobs/sha256:ab").map(|(o, n)| kloudlite_registry::routing_key(o, n)),
        Some("img/acme/nginx".to_string())
    );
}

/// N-1: a name the map does not know and whose object-store prefix is still EMPTY — a repo,
/// image or volume whose first write has not landed yet — is claimed before any node opens it.
/// Serving it `Local` unclaimed let every node open the same fresh database inside that window.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_prefix_is_claimed_never_served_unclaimed() {
    use kloudlite_storage::ownership::Route;
    let e = common::env().await;
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    // B routes first: the answer may be Local, but only because B now HOLDS the lease.
    assert_eq!(b.app.route_for("alice/unflushed", true).await, Route::Local);
    assert_eq!(a.app.owner("alice/unflushed").await.unwrap().unwrap().node, "kloudlite-1");
    // A sees the same empty prefix and must defer to B, not open it too.
    match a.app.route_for("alice/unflushed", true).await {
        Route::Peer(p) => assert_eq!(p.name, "kloudlite-1"),
        other => panic!("A must forward to the claimant, got {other:?}"),
    }
    assert_eq!(a.store.pool.warm_count(), 0);
    assert_eq!(b.store.pool.warm_count(), 0, "routing claims; it never opens");
}

/// Q-19: creating a repo on a non-leader node takes the lease BEFORE `create_repo` opens the
/// database, so a request arriving anywhere else forwards to the creator instead of fencing it.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_holds_the_lease_before_it_opens_the_database() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let res = client()
        .await
        .post(format!("http://{}/api/alice/fresh/create?visibility=private", b.peer))
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201);
    assert_eq!(a.app.owner("alice/fresh").await.unwrap().unwrap().node, "kloudlite-1");
    assert_eq!(b.store.pool.warm_count(), 1, "the creator opened it under its own lease");
    // The very next request lands on the other node: it must forward, and A must stay cold.
    let res = client()
        .await
        .get(format!("http://{}/alice/fresh/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.store.pool.warm_count(), 0, "A forwarded; it never opened the new repo");
    assert_eq!(b.store.pool.warm_count(), 1);
}

/// Three nodes, one store: exactly one takes the lease, and every node reads the same holder.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_elects_exactly_one_leader() {
    let e = common::env().await;
    let f = fleet(3);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let c = node(e.store.os.clone(), "kloudlite-2", &f).await;
    let leaders: Vec<String> =
        [&a, &b, &c].iter().filter(|n| n.app.is_leader()).map(|n| n.app.self_name.clone()).collect();
    assert_eq!(leaders, vec![LEADER.to_string()], "started first, took the lease first");
    for n in [&a, &b, &c] {
        assert_eq!(n.app.leader().as_deref(), Some(LEADER));
        assert!(n.app.leader_live());
    }
    assert!(a.app.ownership.is_writer().await);
    assert!(!b.app.ownership.is_writer().await && !c.app.ownership.is_writer().await);
    // Another beat on a follower changes nothing: the lease is live and not its own.
    b.app.election_tick().await.unwrap();
    assert!(!b.app.is_leader());
    // Nor does a renewal on the holder change the epoch.
    a.app.election_tick().await.unwrap();
    assert_eq!(a.app.leader_epoch(), 1);
}

/// The leader dies: its lease stops renewing and lapses, a peer takes it with the next epoch, and
/// a claim that first asks the dead leader by its stale name still succeeds inside the claim
/// budget. The dead leader's own late write is refused: SlateDB fenced its writer the moment the
/// successor opened the map, the fence demoted it, and its `/own/*` answers 421 from then on.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_leader_is_replaced_and_its_late_write_is_refused() {
    use kloudlite_storage::ownership::lease::LEADER_TTL;
    use kloudlite_storage::ownership::Grant;
    let e = common::env().await;
    let f = fleet(3);
    let zero = node(e.store.os.clone(), LEADER, &f).await;
    let one = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let two = node(e.store.os.clone(), "kloudlite-2", &f).await;
    e.store.create_repo("alice", "web").await.unwrap();
    assert!(zero.app.is_leader() && !one.app.is_leader() && !two.app.is_leader());

    // Zero "dies": it never ticks again, so its lease lapses. Seen from ONE's clock it already has.
    one.app.advance_clock(LEADER_TTL + std::time::Duration::from_millis(1));
    one.app.election_tick().await.unwrap();
    assert!(one.app.is_leader(), "an expired lease is taken by the next node to look");
    assert_eq!(one.app.leader_epoch(), 2);

    // TWO still names zero, and zero still believes it leads — nothing has told it otherwise. Its
    // grant hits the fence, it demotes, and it answers 421; TWO re-reads the lease and lands on ONE.
    assert_eq!(two.app.leader().as_deref(), Some(LEADER));
    match two.app.claim("alice/web").await.unwrap() {
        Grant::Granted(en) => assert_eq!(en.node, "kloudlite-2"),
        g => panic!("expected a grant, got {g:?}"),
    }
    assert_eq!(two.app.leader().as_deref(), Some("kloudlite-1"), "the stale name was replaced by the lease");
    assert_eq!(one.app.owner("alice/web").await.unwrap().unwrap().node, "kloudlite-2");
    assert!(!zero.app.is_leader(), "the fenced grant demoted it");
    assert!(!zero.app.ownership.is_writer().await);

    // And a write from the old leader after that is refused in-process, before any storage.
    let late = zero.app.grant_claim("alice/late", "kloudlite-2", false).await;
    assert!(late.as_ref().is_err_and(|e| e.to_string().contains("not the leader")), "{late:?}");
}

/// `/healthz` follows the lease: ready while a live leader exists, un-ready while nobody holds a
/// live lease, ready again once somebody does — and "somebody" may be this node.
#[tokio::test(flavor = "multi_thread")]
async fn healthz_is_unready_only_while_no_leader_lives() {
    use kloudlite_storage::ownership::lease::LEADER_TTL;
    let e = common::env().await;
    let f = fleet(2);
    let _zero = node(e.store.os.clone(), LEADER, &f).await;
    let one = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let c = client().await;
    let healthz = |n: &Node| {
        let url = format!("http://{}/healthz", n.peer);
        let c = c.clone();
        async move { c.get(url).header(kloudlite_core::peer::PEER_HEADER, SECRET).send().await.unwrap().status() }
    };
    assert_eq!(healthz(&one).await, 200, "a live leader exists");

    // Zero stops renewing (it never ticks in this harness); on ONE's clock the lease has lapsed.
    one.app.advance_clock(LEADER_TTL + std::time::Duration::from_millis(1));
    assert_eq!(healthz(&one).await, 503, "no live leader: a node that cannot claim must not take traffic");

    one.app.election_tick().await.unwrap();
    assert!(one.app.is_leader());
    assert_eq!(healthz(&one).await, 200);
}

/// The handover a preStop hook performs: the repos a pod owns are named to a LIVE PEER in the map
/// before the pod closes them, so a follower's next read routes to a node that is up rather than
/// into a socket that has gone. That gap — released, unowned, and reclaimed only on somebody's
/// next claim — was 1-2 failed requests per pod on every roll (`bins/server/src/main.rs`).
///
/// Asserts the outcome — the entry names a live peer, this node's handle is gone, and a client
/// request for the repo is still served through a different node immediately after. The ORDER
/// (close before the peer is named) is pinned by `a_drain_closes_its_handle_before_naming_a_peer`.
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_hands_its_repos_to_a_live_peer() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(3);
    let repo = "alice/web";
    e.store.create_repo("alice", "web").await.unwrap();
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(repo).await.unwrap();
    a.store.pool.get("alice", "web").await.unwrap(); // warm, as serving one request would leave it
    assert_eq!(a.store.pool.warm_count(), 1);

    let c = client().await;
    let drain = |n: &Node| {
        let url = format!("http://{}/peer/v1/drain", n.peer);
        let c = c.clone();
        async move { c.post(url).header(kloudlite_core::peer::PEER_HEADER, SECRET).send().await.unwrap() }
    };
    assert_eq!(drain(&a).await.status(), 200);

    let owner = leader.app.owner(repo).await.unwrap().expect("the repo is still owned by somebody");
    assert_ne!(owner.node, "kloudlite-1", "a drained pod must not still own the repo");
    assert_eq!(a.store.pool.warm_count(), 0, "the database is closed once the entry has moved");
    assert!(a.app.is_draining());
    // Not-ready, so the Service drops it — while the peer listener keeps answering.
    let health = c.get(format!("http://{}/healthz", a.peer))
        .header(kloudlite_core::peer::PEER_HEADER, SECRET).send().await.unwrap();
    assert_eq!(health.status(), 503, "a draining pod must report not-ready");
    // Idempotent: the hook may fire twice, and a second handover over the top of the first is
    // exactly what must not happen.
    assert_eq!(drain(&a).await.status(), 200);

    // And the whole point: the repo still serves, at once, through another node.
    let res = c.get(format!("http://{}/{repo}/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token))
        .header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 200, "the handover left the repo servable with no gap");
}

/// A draining node that holds the leader lease resigns it FIRST. Every reassignment is a write to
/// the ownership map, and the map's writer is this process — so handing repos over before giving
/// the lease up would write them through a writer that is about to close, and the last of them
/// would be lost. Resign, wait for a successor, then reassign, then close.
#[tokio::test(flavor = "multi_thread")]
async fn a_draining_leader_resigns_before_it_hands_over() {
    let e = common::env().await;
    let f = fleet(2);
    let repo = "alice/web";
    e.store.create_repo("alice", "web").await.unwrap();
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    assert!(a.app.is_leader());
    a.app.claim(repo).await.unwrap();
    a.store.pool.get("alice", "web").await.unwrap();

    let res = client().await
        .post(format!("http://{}/peer/v1/drain", a.peer))
        .header(kloudlite_core::peer::PEER_HEADER, SECRET)
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(!a.app.is_leader(), "the lease is given back before anything is handed over");
    assert!(b.app.is_leader(), "and a peer has taken it: the map has a writer for the handover");
    assert_eq!(
        b.app.owner(repo).await.unwrap().map(|e| e.node),
        Some("kloudlite-1".to_string()),
        "the entry was written through the NEW writer, naming the only live peer",
    );
    assert_eq!(a.store.pool.warm_count(), 0);
}

/// The handle must be CLOSED before the map names the peer. Naming it first lets the peer open the
/// database while this pod still holds it, which fences this pod — and a fenced request here is
/// answered 503 (`fenced_elsewhere`), which git does not retry: the very sample the handover
/// exists to remove. Watched from the leader, whose map is the authority: at the moment the entry
/// stops naming the drained node, that node must already be holding nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_closes_its_handle_before_naming_a_peer() {
    let e = common::env().await;
    let f = fleet(3);
    let repo = "alice/web";
    e.store.create_repo("alice", "web").await.unwrap();
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let _b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(repo).await.unwrap();
    a.store.pool.get("alice", "web").await.unwrap();

    let (app, store) = (a.app.clone(), a.store.clone());
    let drained = tokio::spawn(async move { app.drain().await });
    // Poll the leader's own map. `evict_after_drain` sleeps `DRAIN` (500 ms) before closing, so
    // this has a wide window to catch a handover that named the peer too early.
    let mut saw_handover = false;
    for _ in 0..600 {
        match leader.app.owner(repo).await.unwrap() {
            Some(o) if o.node != "kloudlite-1" => {
                assert_eq!(store.pool.warm_count(), 0, "the peer was named while we still held the database open");
                saw_handover = true;
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
        }
    }
    assert_eq!(drained.await.unwrap(), (1, 0), "one repo moved, none kept");
    assert!(saw_handover, "the entry never named a peer");
}

/// A handover the leader will not grant must not be counted as one. The repo stays THIS pod's on
/// the record — it is still answering, and SIGTERM releases it as it always did — because a repo
/// reported as moved while the map names a pod that has closed it is a dead end for every node for
/// a full LEASE_TTL. Here the leader is unreachable, which is what a roll actually looks like.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_handover_keeps_the_repo_instead_of_reporting_it_moved() {
    let e = common::env().await;
    let f = fleet(3);
    let repo = "alice/web";
    e.store.create_repo("alice", "web").await.unwrap();
    let leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let _b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    a.app.claim(repo).await.unwrap();
    a.store.pool.get("alice", "web").await.unwrap();

    // A's every ask — release, then the grant to the peer — lands on a refused port.
    blackholed().lock().unwrap().insert(f[0].1.clone());
    let (moved, kept) = a.app.drain().await;
    blackholed().lock().unwrap().remove(&f[0].1);

    assert_eq!((moved, kept), (0, 1), "an unreachable leader granted nothing: kept, not moved");
    assert_eq!(
        leader.app.owner(repo).await.unwrap().map(|x| x.node),
        Some("kloudlite-1".to_string()),
        "the entry must still name the node that is still answering",
    );
    assert_eq!(a.store.pool.warm_count(), 0, "the handle is closed either way");
}

/// The registry half of `a_node_fenced_by_a_stray_process_reopens_when_it_is_still_the_owner`: an
/// image's database is fenced by a stray opener, routing still says this node owns the image, so
/// the manifest GET must reopen and serve 200 — not the 500 UNKNOWN it used to answer (the
/// "first registry request to a moved image can 500 once" gap in CLAUDE.md's Deploying section).
#[tokio::test(flavor = "multi_thread")]
async fn a_fenced_image_reopens_when_this_node_is_still_the_owner() {
    let e = common::env().await;
    let token = e.store.create_token("acme").await.unwrap();
    common::seed_blobs(&e, "acme", &[b"cfg", b"layer"]).await;
    let f = fleet(1);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"digest": kloudlite_registry::Digest::of(b"cfg").to_string(), "size": 3},
        "layers": [{"digest": kloudlite_registry::Digest::of(b"layer").to_string(), "size": 5}],
    })
    .to_string();
    let put = client()
        .await
        .put(format!("http://{}/v2/acme/nginx/manifests/latest", a.public))
        .basic_auth("acme", Some(&token))
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 201);

    // A stray opener (an admin command run against a live pod) takes the writer epoch.
    let (o, n) = kloudlite_registry::pool_coords("acme", "nginx");
    let adb = a.store.pool.get(o, &n).await.unwrap();
    let stray = slatedb::Db::builder(kloudlite_storage::pool::path(o, &n), e.store.os.clone())
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
    // The tag resolution reads the fenced database: still the owner, so reopen and serve.
    let res = client()
        .await
        .get(format!("http://{}/v2/acme/nginx/manifests/latest", a.public))
        .basic_auth("acme", Some(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "still the owner: reopen and serve");
    assert_eq!(res.bytes().await.unwrap().to_vec(), manifest.into_bytes());
}

/// The other half: the map now names a peer, so the fence was CORRECT and this node must not
/// reopen (that is what fences the legitimate owner). The client gets the OCI envelope with a
/// 503 and a `Retry-After` — which docker and crane retry — never a 500 UNKNOWN.
#[tokio::test(flavor = "multi_thread")]
async fn a_fenced_image_owned_by_a_peer_is_503_not_500() {
    let e = common::env().await;
    let f = fleet(3);
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "kloudlite-1", &f).await;
    let b = node(e.store.os.clone(), "kloudlite-2", &f).await;
    let (o, n) = kloudlite_registry::pool_coords("acme", "nginx");
    a.app.claim(&kloudlite_registry::routing_key("acme", "nginx")).await.unwrap();
    // b holds a stale handle; a takes the writer epoch, so b is fenced and the map names a.
    let bdb = b.store.pool.get(o, &n).await.unwrap();
    let _ = a.store.pool.get(o, &n).await.unwrap();
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
    let r: Result<(), axum::response::Response> =
        kloudlite_registry::fenced_retry(&b.app, "acme", "nginx", false, || async {
            b.store.pool.get(o, &n).await.map(|_| ())
        })
        .await;
    let resp = r.expect_err("a fence a peer owns must refuse, never reopen");
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "1");
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["errors"][0]["code"], "UNAVAILABLE");
    // An upload verb gets the same 503 under the code its clients already restart from: docker
    // and crane follow a 307 on a blob GET but not on POST/PATCH/PUT.
    let up = kloudlite_registry::fenced_elsewhere(true);
    assert_eq!(up.status(), 503);
    let body = axum::body::to_bytes(up.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["errors"][0]["code"], "BLOB_UPLOAD_UNKNOWN");
    assert_eq!(b.store.pool.warm_count(), 0, "b must not have reopened");
}
