//! The gate end to end: a real api (an `axum` server recording every call), a real fake buildkit
//! (a `TcpListener` that answers), and a resolver standing in for the pod reflector — everything
//! except the two things a test cannot have, the Kubernetes watch and CoreDNS.
//!
//! The clock is paused wherever a test waits out a timeout or an idle period: `builder_idle_secs`
//! has a floor of 60, and waiting one out for real is a minute of CI per assertion.

use axum::extract::{Path, State};
use axum::routing::{get, post};
use kloudlite_builder_gate::{idle, serve, who, ApiClient, Gate};
use kloudlite_core::settings::{CentralSettings, LiveSettings};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &str = "builder-secret";
const IDLE: u64 = 120;
const START: u64 = 60;

// ── the api ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct MockApi {
    calls: Mutex<Vec<String>>,
    /// The GET count at which `ready` flips to true. `u64::MAX` is "never ready".
    ready_after: AtomicU64,
    gets: AtomicU64,
    running: Mutex<Vec<String>>,
}

impl MockApi {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn count(&self, prefix: &str) -> usize {
        self.calls().iter().filter(|c| c.starts_with(prefix)).count()
    }
}

/// The secret is checked here and not merely recorded: an unauthenticated gate would work in a
/// test that only looked at paths and 401 against the real api.
async fn auth(headers: &axum::http::HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {SECRET}"))
}

async fn mock_api(api: Arc<MockApi>) -> String {
    async fn start(
        State(s): State<Arc<MockApi>>,
        headers: axum::http::HeaderMap,
        Path(slug): Path<String>,
    ) -> axum::http::StatusCode {
        if !auth(&headers).await {
            return axum::http::StatusCode::UNAUTHORIZED;
        }
        s.calls.lock().unwrap().push(format!("start {slug}"));
        axum::http::StatusCode::ACCEPTED
    }
    async fn stop(
        State(s): State<Arc<MockApi>>,
        headers: axum::http::HeaderMap,
        Path(slug): Path<String>,
    ) -> axum::http::StatusCode {
        if !auth(&headers).await {
            return axum::http::StatusCode::UNAUTHORIZED;
        }
        s.calls.lock().unwrap().push(format!("stop {slug}"));
        axum::http::StatusCode::ACCEPTED
    }
    async fn one(
        State(s): State<Arc<MockApi>>,
        headers: axum::http::HeaderMap,
        Path(slug): Path<String>,
    ) -> Result<axum::Json<serde_json::Value>, axum::http::StatusCode> {
        if !auth(&headers).await {
            return Err(axum::http::StatusCode::UNAUTHORIZED);
        }
        s.calls.lock().unwrap().push(format!("get {slug}"));
        let n = s.gets.fetch_add(1, Ordering::SeqCst);
        let ready = n >= s.ready_after.load(Ordering::SeqCst);
        Ok(axum::Json(serde_json::json!({
            "id": format!("bld-{slug}"),
            "state": if ready { "running" } else { "creating" },
            "ready": ready,
            "conditions": [],
        })))
    }
    async fn list(
        State(s): State<Arc<MockApi>>,
        headers: axum::http::HeaderMap,
    ) -> Result<axum::Json<serde_json::Value>, axum::http::StatusCode> {
        if !auth(&headers).await {
            return Err(axum::http::StatusCode::UNAUTHORIZED);
        }
        s.calls.lock().unwrap().push("list".into());
        Ok(axum::Json(serde_json::json!({ "running": s.running.lock().unwrap().clone() })))
    }

    let app = axum::Router::new()
        .route("/v1/internal/builders", get(list))
        .route("/v1/internal/builders/{slug}", get(one))
        .route("/v1/internal/builders/{slug}/start", post(start))
        .route("/v1/internal/builders/{slug}/stop", post(stop))
        .with_state(api);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

// ── the pods ─────────────────────────────────────────────────────────────

struct TestPods(HashMap<IpAddr, (String, String)>);

impl who::Resolver for TestPods {
    fn resolve(&self, ip: IpAddr) -> Option<(String, String)> {
        self.0.get(&ip).cloned()
    }
}

// ── buildkit ─────────────────────────────────────────────────────────────

/// Answers `buildkit:{whatever it was sent}` — a reply DERIVED from the request, so a passing
/// assertion cannot be explained by anything but bytes having crossed in both directions.
async fn fake_buildkit() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        return;
                    }
                    let mut reply = b"buildkit:".to_vec();
                    reply.extend_from_slice(&buf[..n]);
                    if s.write_all(&reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr.to_string()
}

// ── the gate under test ──────────────────────────────────────────────────

fn settings() -> LiveSettings<CentralSettings> {
    LiveSettings::new(CentralSettings {
        builder_idle_secs: IDLE,
        builder_start_secs: START,
        ..CentralSettings::built_in_defaults()
    })
}

/// The gate on a local port, with every connection attributed to `peer` — the reflector's answer
/// is what the `TestPods` map decides, not the loopback address the test actually dials from.
async fn gate_on(gate: Arc<Gate>, peer: IpAddr) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((sock, _)) = l.accept().await {
            tokio::spawn(serve(gate.clone(), sock, peer));
        }
    });
    addr
}

fn build_gate(base: String, pods: TestPods, buildkit: Option<String>) -> Arc<Gate> {
    Arc::new(Gate {
        api: ApiClient::new(base, SECRET.into()),
        who: Arc::new(pods),
        idle: idle::Idle::default(),
        central: settings(),
        buildkit,
    })
}

fn pods(rows: &[(&str, &str, &str)]) -> TestPods {
    TestPods(rows.iter().map(|(ip, o, t)| (ip.parse().unwrap(), ((*o).to_string(), (*t).to_string()))).collect())
}

/// One round trip through the gate: what came back for what was sent.
async fn round_trip(addr: SocketAddr, msg: &[u8]) -> Vec<u8> {
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(msg).await.unwrap();
    let mut buf = vec![0u8; 256];
    let n = c.read(&mut buf).await.unwrap();
    buf.truncate(n);
    buf
}

/// The recorder is process-wide and installed once; without it `render()` is empty and every
/// metric assertion would pass vacuously.
/// Wait for something the GATE did. A paused clock makes `sleep` return before any of the gate's
/// real HTTP has happened, so a test that only slept would assert against a task still in its
/// first request. Each tiny sleep parks the runtime, which is what lets the IO driver run.
async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..5000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("never happened: {what}");
}

fn metric(labels: &str) -> f64 {
    kloudlite_core::metrics::init();
    let want = format!("builder_gate_starts_total{{{labels}}} ");
    kloudlite_core::metrics::render()
        .lines()
        .find_map(|l| l.strip_prefix(&want).and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0.0)
}

// ── the cases ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_workspace_pod_starts_its_owners_builder_and_the_bytes_cross() {
    let api = Arc::new(MockApi::default());
    // Not ready on the first two polls: the wait is a loop, and a mock that was ready immediately
    // would pass with no loop at all.
    api.ready_after.store(2, Ordering::SeqCst);
    let base = mock_api(api.clone()).await;
    let bk = fake_buildkit().await;
    let gate = build_gate(base, pods(&[("10.0.0.5", "alice", "")]), Some(bk));
    let addr = gate_on(gate, "10.0.0.5".parse().unwrap()).await;

    assert_eq!(round_trip(addr, b"hello").await, b"buildkit:hello".as_slice());
    let calls = api.calls();
    assert_eq!(calls[0], "start alice", "start comes before any poll: {calls:?}");
    assert!(calls.iter().filter(|c| *c == "get alice").count() >= 3, "polled until ready: {calls:?}");
}

#[tokio::test]
async fn a_team_pod_reaches_the_teams_builder() {
    let api = Arc::new(MockApi::default());
    let base = mock_api(api.clone()).await;
    let bk = fake_buildkit().await;
    let gate = build_gate(base, pods(&[("10.0.0.6", "alice", "acme")]), Some(bk));
    let addr = gate_on(gate, "10.0.0.6".parse().unwrap()).await;

    assert_eq!(round_trip(addr, b"hi").await, b"buildkit:hi".as_slice());
    assert_eq!(api.calls()[0], "start acme", "a team workspace builds on the TEAM's builder");
}

#[tokio::test]
async fn an_unknown_peer_is_closed_and_costs_the_api_nothing() {
    let api = Arc::new(MockApi::default());
    let base = mock_api(api.clone()).await;
    let gate = build_gate(base, pods(&[("10.0.0.5", "alice", "")]), Some("127.0.0.1:1".into()));
    let addr = gate_on(gate, "10.9.9.9".parse().unwrap()).await;

    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(c.read(&mut buf).await.unwrap(), 0, "closed without a byte");
    assert!(api.calls().is_empty(), "no api call for an IP we cannot name: {:?}", api.calls());
}

#[tokio::test(start_paused = true)]
async fn a_builder_that_never_becomes_ready_times_out_without_a_stop() {
    let api = Arc::new(MockApi::default());
    api.ready_after.store(u64::MAX, Ordering::SeqCst);
    let base = mock_api(api.clone()).await;
    let gate = build_gate(base, pods(&[("10.0.0.5", "alice", "")]), Some("127.0.0.1:1".into()));
    let addr = gate_on(gate, "10.0.0.5".parse().unwrap()).await;

    let before = metric(r#"outcome="timeout""#);
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(c.read(&mut buf).await.unwrap(), 0, "closed once the budget ran out");
    assert!(metric(r#"outcome="timeout""#) > before, "the timeout is counted");
    // A slow builder is not a broken one: stopping it here would tear down the pod the next
    // connection is waiting for.
    assert_eq!(api.count("stop"), 0, "no stop on a timeout: {:?}", api.calls());
}

#[tokio::test(start_paused = true)]
async fn the_builder_stops_once_after_the_last_connection_closes() {
    let api = Arc::new(MockApi::default());
    let base = mock_api(api.clone()).await;
    let bk = fake_buildkit().await;
    let gate = build_gate(base, pods(&[("10.0.0.5", "alice", "")]), Some(bk));
    tokio::spawn(idle::beat(gate.clone()));
    let addr = gate_on(gate, "10.0.0.5".parse().unwrap()).await;

    let mut a = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut b = tokio::net::TcpStream::connect(addr).await.unwrap();
    a.write_all(b"x").await.unwrap();
    b.write_all(b"y").await.unwrap();
    let mut buf = [0u8; 10];
    a.read_exact(&mut buf).await.unwrap();
    b.read_exact(&mut buf).await.unwrap();

    // The FIRST close is not the clock: while one connection remains, nothing is idle.
    drop(a);
    tokio::time::sleep(Duration::from_secs(IDLE * 2)).await;
    assert_eq!(api.count("stop"), 0, "one connection still open: {:?}", api.calls());

    drop(b);
    // Halfway through the countdown a new connection arrives, which must CANCEL the stop rather
    // than merely postpone the timer by one tick.
    tokio::time::sleep(Duration::from_secs(IDLE / 2)).await;
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(b"z").await.unwrap();
    c.read_exact(&mut buf).await.unwrap();
    tokio::time::sleep(Duration::from_secs(IDLE * 2)).await;
    assert_eq!(api.count("stop"), 0, "the arrival cancelled the stop: {:?}", api.calls());

    drop(c);
    tokio::time::sleep(Duration::from_secs(IDLE - 30)).await;
    assert_eq!(api.count("stop"), 0, "not yet: builder_idle_secs has not elapsed");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(api.count("stop"), 1, "stopped exactly once: {:?}", api.calls());

    // And exactly once: the beat keeps ticking over an idle builder forever.
    tokio::time::sleep(Duration::from_secs(IDLE * 3)).await;
    assert_eq!(api.count("stop"), 1, "one stop per idle period: {:?}", api.calls());
}

#[tokio::test(start_paused = true)]
async fn a_builder_left_running_across_a_restart_is_stopped() {
    let api = Arc::new(MockApi::default());
    *api.running.lock().unwrap() = vec!["alice".into()];
    let base = mock_api(api.clone()).await;
    let gate = build_gate(base, pods(&[]), Some("127.0.0.1:1".into()));

    // What `main` does at boot, and the reason it does it: this process has seen no connection
    // for `alice`, so without the seed nothing would ever stop her builder.
    for slug in gate.api.running().await.unwrap() {
        gate.idle.seed(&slug);
    }
    tokio::spawn(idle::beat(gate.clone()));

    tokio::time::sleep(Duration::from_secs(IDLE - 30)).await;
    assert_eq!(api.count("stop"), 0, "seeded idle-from-now, not idle-forever");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(api.count("stop"), 1, "stopped one idle period after boot: {:?}", api.calls());
}

// ── round 1: the three defects the review found ──────────────────────────

#[tokio::test(start_paused = true)]
async fn a_connection_holds_a_builder_that_was_already_counting_down() {
    let api = Arc::new(MockApi::default());
    // Slow to become ready, so the whole start window sits inside the idle countdown — which is
    // exactly the window the old ordering left the builder stoppable in.
    api.ready_after.store(u64::MAX, Ordering::SeqCst);
    *api.running.lock().unwrap() = vec!["alice".into()];
    let base = mock_api(api.clone()).await;
    let gate = build_gate(base, pods(&[("10.0.0.5", "alice", "")]), Some("127.0.0.1:1".into()));
    // The longest permitted start budget, so the whole test sits INSIDE one start: the property
    // under test is what the beat may do while a connection is waiting for its builder.
    gate.central.store(CentralSettings {
        builder_idle_secs: IDLE,
        builder_start_secs: 600,
        ..CentralSettings::built_in_defaults()
    });
    for slug in gate.api.running().await.unwrap() {
        gate.idle.seed(&slug);
    }
    tokio::spawn(idle::beat(gate.clone()));
    let addr = gate_on(gate, "10.0.0.5".parse().unwrap()).await;

    // A build arrives halfway through the countdown; `start` is POSTed right after the count, so
    // seeing it is how the test knows the connection is held.
    tokio::time::sleep(Duration::from_secs(IDLE / 2)).await;
    let _c = tokio::net::TcpStream::connect(addr).await.unwrap();
    let a = api.clone();
    until("the gate took the connection", move || a.count("start") > 0).await;
    // Well past the deadline the beat would otherwise have fired on, with the builder still not
    // ready — the window the old ordering left the builder stoppable in.
    tokio::time::sleep(Duration::from_secs(IDLE * 2)).await;
    assert_eq!(
        api.count("stop"),
        0,
        "the connection is counted before `start`, so the builder it is waiting for is held: {:?}",
        api.calls()
    );
}

#[tokio::test(start_paused = true)]
async fn a_client_that_hangs_up_mid_start_stops_the_polling() {
    let api = Arc::new(MockApi::default());
    api.ready_after.store(u64::MAX, Ordering::SeqCst);
    let base = mock_api(api.clone()).await;
    let gate = build_gate(base, pods(&[("10.0.0.5", "alice", "")]), Some("127.0.0.1:1".into()));
    let addr = gate_on(gate, "10.0.0.5".parse().unwrap()).await;

    let before = metric(r#"outcome="timeout""#);
    let gone_before = metric(r#"outcome="client_gone""#);
    let c = tokio::net::TcpStream::connect(addr).await.unwrap();
    // A poll or two in, the client leaves.
    let a = api.clone();
    until("polling began", move || a.count("get") > 0).await;
    drop(c);
    until("the hangup was noticed", || metric(r#"outcome="client_gone""#) > gone_before).await;

    // The full budget is START/2 polls; a handful is "it noticed and stopped" rather than
    // "it polled on to the end for a socket nobody was reading".
    let polls = api.count("get");
    assert!(polls < (START / 4) as usize, "polling stopped with the client, saw {polls} polls");
    assert_eq!(metric(r#"outcome="timeout""#), before, "a client leaving is not a builder timeout");
    assert_eq!(api.count("stop"), 0, "still no stop: {:?}", api.calls());
}

#[tokio::test]
async fn healthz_is_503_until_the_pods_have_been_listed_once() {
    let pods = who::Pods::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = kloudlite_builder_gate::health(pods.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let url = format!("http://{addr}/healthz");
    let http = reqwest::Client::new();

    assert_eq!(http.get(&url).send().await.unwrap().status(), 503, "not ready before the first LIST");
    // What the reflector does at `InitDone`.
    pods.mark_listed();
    assert_eq!(http.get(&url).send().await.unwrap().status(), 200);
}
