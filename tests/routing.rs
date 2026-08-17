//! Which node ends up serving a repo, and who may claim an identity.
mod common;

use rustic_git::ownership::OwnershipStore;
use rustic_git::App;
use std::sync::Arc;

const SECRET: &str = "test-peer-secret";
/// The leader is a name, not a decision: ordinal zero, always.
const LEADER: &str = "rustic-git-0";

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

/// Bring up a node named `name` (`rustic-git-N`). `fleet` is every node's (name, peer addr); it is
/// how a node resolves a name from the map to somewhere to forward. **The leader must be started
/// first** — it creates the ownership database that every follower opens read-only.
async fn node(
    os: Arc<dyn slatedb::object_store::ObjectStore>,
    name: &str,
    fleet: &[(String, String)],
) -> Node {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rustic_git::store::Store::open(os.clone(), tmp.path().join("cache"), false)
            .await
            .unwrap(),
    );
    let ownership = OwnershipStore::open(os, name == LEADER).await.unwrap();
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
            addr
        }),
        SECRET.into(),
        fleet.len() as u32,
    ));
    // Eviction gives the lease back before it closes the database, exactly as `serve()` wires it.
    store.pool.set_release_hook(
        Arc::downgrade(&app) as std::sync::Weak<dyn rustic_git::pool::ReleaseHook>
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
fn blackholed() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static B: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    B.get_or_init(Default::default)
}

/// A fleet of `n` nodes, `rustic-git-0` (the leader) first.
fn fleet(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| format!("rustic-git-{i}"))
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
        .header(rustic_git::proxy::OWNER_HEADER, "alice")
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
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
    let f = fleet(1);
    let a = node(e.store.os.clone(), LEADER, &f).await;
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let c = node(e.store.os.clone(), "rustic-git-2", &f).await;
    a.app.claim(&repo).await.unwrap();
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
    a.app.claim(&repo).await.unwrap(); // A holds it; every request to B is forwarded
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
    let f = fleet(1);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let a = node(e.store.os.clone(), LEADER, &f).await;
    a.app.claim(&repo).await.unwrap();
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
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
    assert_eq!(c.get(format!("http://{}/healthz", a.peer)).header(rustic_git::proxy::PEER_HEADER, SECRET).send().await.unwrap().status(), 200);

    // Flip a unhealthy directly (the probe loop is not under test).
    a.store.healthy.store(false, std::sync::atomic::Ordering::Relaxed);

    // The repo it holds: 503, not served. Pool count unchanged (no new open).
    let res = c.get(format!("http://{}/{mine}/info/refs?service=git-upload-pack", a.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2").send().await.unwrap();
    assert_eq!(res.status(), 503, "unhealthy: must not serve even what it holds");
    assert_eq!(c.get(format!("http://{}/healthz", a.peer)).header(rustic_git::proxy::PEER_HEADER, SECRET).send().await.unwrap().status(), 503);
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
    let b = node(e.store.os.clone(), "rustic-git-1", &f).await;
    e.store.create_repo("alice", "web").await.unwrap();
    // B is a follower: its claim travels to A, which decides and writes the map.
    let res = client().await
        .get(format!("http://{}/alice/web/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.app.owner("alice/web").await.unwrap().unwrap().node, "rustic-git-1");
    assert_eq!(b.store.pool.warm_count(), 1, "the claimant opened it");
    assert_eq!(a.store.pool.warm_count(), 0, "the leader did not — it only granted");
}

/// A follower that is asked to decide ownership answers 421: it is not the leader, and the
/// caller's idea of who is has gone stale. It must not proxy the message on, and must not answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_follower_refuses_to_decide_ownership() {
    let e = common::env().await;
    let f = fleet(2);
    let _a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let c = client().await;
    for what in ["claim", "renew", "release"] {
        let res = c
            .post(format!("http://{}/own/{what}", b.peer))
            .header(rustic_git::proxy::PEER_HEADER, SECRET)
            .body("alice/web\nrustic-git-1")
            .send().await.unwrap();
        assert_eq!(res.status(), 421, "/own/{what} on a follower");
    }
    // And the leader does answer, so 421 is about leadership, not about the route existing.
    let res = c
        .post(format!("http://{}/own/claim", _a.peer))
        .header(rustic_git::proxy::PEER_HEADER, SECRET)
        .body("alice/web\nrustic-git-1")
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.text().await.unwrap().starts_with("granted\nrustic-git-1\n"));
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
    let b = node(e.store.os.clone(), "rustic-git-1", &b_fleet).await;
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
    let repo = "alice/web";
    assert!(matches!(a.app.claim(repo).await.unwrap(), rustic_git::ownership::Grant::Granted(_)));
    a.app.release(repo).await.unwrap();
    // Read through the leader's writer handle: a follower's copy is up to a poll interval behind,
    // and what is under test is the delete, not the propagation.
    assert_eq!(leader.app.owner(repo).await.unwrap(), None, "the entry is deleted, not shortened");
    match b.app.claim(repo).await.unwrap() {
        rustic_git::ownership::Grant::Granted(e) => assert_eq!(e.node, "rustic-git-2"),
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
    let b = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let repo = "alice/web";
    assert!(matches!(b.app.claim(repo).await.unwrap(), rustic_git::ownership::Grant::Granted(_)));
    a.app.release(repo).await.unwrap(); // the leader never held it
    assert_eq!(
        a.app.owner(repo).await.unwrap().map(|e| e.node),
        Some("rustic-git-1".to_string()),
        "a stale release deleted the real owner's entry"
    );
}

async fn stream_listener(store: Arc<rustic_git::store::Store>) -> String {
    let app = common::app(store).await;
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
    let f = fleet(4);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let c = node(e.store.os.clone(), "rustic-git-3", &f).await;
    a.app.claim(&repo).await.unwrap();
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
    e.store
        .add_ssh_key(o, &std::fs::read_to_string(kd.path().join("id_ed25519.pub")).unwrap())
        .await
        .unwrap();

    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
    a.app.claim(&repo).await.unwrap(); // A holds it; b forwards every session

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
    let b = node(e.store.os.clone(), "rustic-git-1", &f).await;
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
    let repo = "alice/web";
    // The repo must exist: routing does not claim a repo that does not, it lets the handler 404.
    e.store.create_repo("alice", "web").await.unwrap();

    assert_eq!(b.app.route(repo).await, rustic_git::ownership::Route::Local);
    b.store.pool.get("alice", "web").await.unwrap();
    assert_eq!(b.store.pool.warm_count(), 1);

    // Nothing is using it and the TTL is now zero: the next sweep evicts it.
    b.store.pool.set_idle_ttl(std::time::Duration::ZERO);
    b.store.pool.sweep().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await; // the release lands

    assert_eq!(b.store.pool.warm_count(), 1, "the database must stay open through the drain");
    match a.app.claim(repo).await.unwrap() {
        rustic_git::ownership::Grant::HeldBy(h) => assert_eq!(h.node, "rustic-git-2"),
        g => panic!("claimable while the loser still holds the database open: {g:?}"),
    }

    tokio::time::sleep(rustic_git::ownership::DRAIN + std::time::Duration::from_millis(400)).await;
    assert_eq!(b.store.pool.warm_count(), 0, "the database must be closed after the drain");
    match a.app.claim(repo).await.unwrap() {
        rustic_git::ownership::Grant::Granted(g) => assert_eq!(g.node, a.app.self_name),
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
    let b = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let repo = "alice/web";
    assert_eq!(b.app.route(repo).await, rustic_git::ownership::Route::Local);
    b.store.pool.get("alice", "web").await.unwrap();

    b.store.pool.set_idle_ttl(std::time::Duration::ZERO);
    b.store.pool.sweep().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(b.store.pool.warm_count(), 1, "closed before the drain was over");
    assert_eq!(b.app.route(repo).await, rustic_git::ownership::Route::Local, "still serving");
    match a.app.route(repo).await {
        rustic_git::ownership::Route::Peer(p) => assert_eq!(p.name, "rustic-git-1"),
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
    let b = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let repo = "alice/web";
    assert_eq!(b.app.route(repo).await, rustic_git::ownership::Route::Local);
    b.store.pool.get("alice", "web").await.unwrap();
    assert_eq!(b.store.pool.warm_count(), 1);

    // The leader hands it to someone else, as it would after b's lease lapsed.
    a.app
        .ownership
        .put(repo, &rustic_git::ownership::Entry {
            node: LEADER.into(),
            expires_ms: rustic_git::ownership::now_ms() + 60_000,
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
    let b = node(e.store.os.clone(), "rustic-git-1", &f).await;
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
    assert_eq!(a.app.owner("alice/web").await.unwrap().unwrap().node, "rustic-git-1");
    // Age the entry out the way a roll does — the leader is the last pod updated, so every
    // follower's renewals fail for the whole ten seconds it is down. Written directly rather than
    // slept through, then given a follower poll (200ms) to reach B.
    a.app
        .ownership
        .put(
            "alice/web",
            &rustic_git::ownership::Entry {
                node: "rustic-git-1".into(),
                expires_ms: rustic_git::ownership::now_ms() - 1,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
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
    let b = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let repo = "alice/web";
    e.store.create_repo("alice", "web").await.unwrap();
    assert_eq!(b.app.route(repo).await, rustic_git::ownership::Route::Local);
    b.store.pool.get("alice", "web").await.unwrap();

    // Retire it, then take a reference back before the drain is over.
    b.store.pool.set_idle_ttl(std::time::Duration::ZERO);
    b.store.pool.sweep().await;
    let held = b.store.pool.get("alice", "web").await.unwrap();
    tokio::time::sleep(rustic_git::ownership::DRAIN + std::time::Duration::from_millis(400)).await;

    assert_eq!(b.store.pool.warm_count(), 1, "an in-use database must not be closed");
    assert_eq!(
        b.app.owner(repo).await.unwrap().map(|x| x.node),
        Some("rustic-git-1".to_string()),
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
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let repo = "alice/web";
    // As if rustic-git-2 claimed it and was killed: the entry is here, and it has lapsed.
    let dead = rustic_git::ownership::Entry {
        node: "rustic-git-2".to_string(),
        expires_ms: rustic_git::ownership::now_ms() - 1,
    };
    leader.app.ownership.put(repo, &dead).await.unwrap();

    leader.app.prune_once().await.unwrap();
    assert_eq!(leader.app.owner(repo).await.unwrap(), None, "a lapsed entry must be pruned");
    match a.app.claim(repo).await.unwrap() {
        rustic_git::ownership::Grant::Granted(g) => assert_eq!(g.node, a.app.self_name),
        g => panic!("the repo must be claimable after the dead node's entry is pruned: {g:?}"),
    }
}
