//! `kloudlite-builder-gate`: what makes "start on demand" invisible to `docker build`.
//!
//! A workspace's buildx is configured once, at `tcp://builder-gate.kloudlite-system.svc:1234`
//! (`k8s::BUILDKIT_HOST`), and points there forever. Behind that address the owner's buildkitd
//! only exists while somebody is building: the gate starts it on the first connection, waits for
//! it, splices the bytes, and stops it once nobody has been connected for `builder_idle_secs`.
//! Neither the workspace nor buildx ever learns any of that happened — the alternative was a
//! per-owner endpoint the client had to be reconfigured for every time the builder moved.
//!
//! The caller is identified BY POD IP and by nothing else. A build client speaks buildkit's gRPC,
//! not a protocol we can put a token in, and the connection carries no identity of its own; the
//! pod IP is the one fact the network gives us that the tenant cannot forge (the CNI assigns it,
//! and Task 6's NetworkPolicies are what stop anything but a workspace pod reaching this port).

pub mod idle;
pub mod splice;
pub mod who;

use kloudlite_core::settings::{CentralSettings, LiveSettings};
use kloudlite_workspaces::crd;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// How often the gate re-reads a starting builder's readiness. Not a setting: it bounds nothing
/// an operator would tune, only how promptly a ready builder is noticed inside `builder_start_secs`.
const POLL_GAP: Duration = Duration::from_secs(2);

/// The api's internal builder routes. The gate holds a shared secret, not a person's identity —
/// there is no user behind a `docker build`'s TCP connection to authenticate as.
#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    secret: String,
}

impl ApiClient {
    pub fn new(base: String, secret: String) -> Self {
        // An explicit User-Agent, because the api is reached through Cloudflare and its bot check
        // answers 1010 to some defaults (python-urllib is refused; curl and this string pass). A
        // gate whose every api call is a 1010 hangs every build at `start`.
        let http = reqwest::Client::builder()
            .user_agent(concat!("kloudlite-builder-gate/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self { http, base: base.trim_end_matches('/').to_string(), secret }
    }

    async fn call(&self, method: reqwest::Method, path: &str) -> Result<reqwest::Response, String> {
        self.http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.secret)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())
    }

    pub async fn start(&self, slug: &str) -> Result<(), String> {
        self.call(reqwest::Method::POST, &format!("/v1/internal/builders/{slug}/start")).await.map(|_| ())
    }

    pub async fn stop(&self, slug: &str) -> Result<(), String> {
        self.call(reqwest::Method::POST, &format!("/v1/internal/builders/{slug}/stop")).await.map(|_| ())
    }

    /// `ready` and not `state`: an environment whose pod exists is not a buildkitd that answers,
    /// and the api computes exactly that distinction for us.
    pub async fn ready(&self, slug: &str) -> Result<bool, String> {
        let body: serde_json::Value = self
            .call(reqwest::Method::GET, &format!("/v1/internal/builders/{slug}"))
            .await?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        Ok(body.get("ready").and_then(serde_json::Value::as_bool).unwrap_or(false))
    }

    /// Every builder the api believes is meant to be running — the boot re-seed, below.
    pub async fn running(&self) -> Result<Vec<String>, String> {
        let body: serde_json::Value =
            self.call(reqwest::Method::GET, "/v1/internal/builders").await?.json().await.map_err(|e| e.to_string())?;
        Ok(body
            .get("running")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default())
    }
}

/// `/healthz`. 503 until the pod reflector has listed once: with `strategy: Recreate` the new pod
/// joins the Service the moment it is Ready, and serving before the index exists closes the first
/// build after every roll as an unknown peer. Liveness reads the same route — a gate that never
/// lists is a gate that will never work.
pub fn health(pods: who::Pods) -> axum::Router {
    axum::Router::new().route(
        "/healthz",
        axum::routing::get(move || {
            let pods = pods.clone();
            async move {
                match pods.listed() {
                    true => (axum::http::StatusCode::OK, "ok"),
                    false => (axum::http::StatusCode::SERVICE_UNAVAILABLE, "listing pods"),
                }
            }
        }),
    )
}

pub struct Gate {
    pub api: ApiClient,
    pub who: Arc<dyn who::Resolver>,
    pub idle: idle::Idle,
    pub central: LiveSettings<CentralSettings>,
    /// `None` derives the in-cluster address from the slug, which is the only shape production
    /// ever uses. The tests set it to a local listener, because a test that resolved
    /// `buildkit.env-bld-alice.svc` would be testing CoreDNS.
    pub buildkit: Option<String>,
}

impl Gate {
    /// Never formatted by hand: `builder_id` and `env_namespace` are the two functions that
    /// decide where a builder lives, and a third spelling here would be a silently unreachable
    /// address the day either one changes.
    pub fn buildkit_addr(&self, slug: &str) -> String {
        match &self.buildkit {
            Some(a) => a.clone(),
            None => format!("buildkit.{}.svc:1234", crd::env_namespace(&crd::builder_id(slug))),
        }
    }
}

/// One accepted connection, start to finish. Every early return counts an outcome, so a build
/// that never happened is visible as something other than an absent series.
pub async fn serve(gate: Arc<Gate>, sock: tokio::net::TcpStream, peer: IpAddr) {
    let Some((owner, team)) = gate.who.resolve(peer) else {
        // No api call at all: an IP we cannot name is either a pod that has just gone or
        // something that has no business here, and neither is worth starting a builder for.
        outcome("unknown_peer");
        tracing::warn!(%peer, "gate.peer.unknown");
        return;
    };
    let slug = who::slug_of(&owner, &team);

    // Counted BEFORE `start`, and this is the whole point of the ordering: the builder may
    // already be running and halfway through its idle countdown (or seeded by a restart), and the
    // beat would then POST `stop` for the very builder this connection is about to use. The count
    // is what holds it open; the `Drop` guard releases it on every early return below.
    let open = splice::Open::new(gate.clone(), slug.clone());

    if let Err(e) = gate.api.start(&slug).await {
        outcome("refused");
        tracing::warn!(%slug, error = %e, "gate.start.failed");
        return;
    }

    let waited = gate.central.load().builder_start_secs;
    let upstream = match wait_ready(&gate, &slug, Duration::from_secs(waited), &sock).await {
        Wait::Ready(up) => up,
        Wait::Refused(addr, e) => {
            outcome("refused");
            tracing::warn!(%slug, %addr, error = %e, secs = waited, "gate.dial.failed");
            return;
        }
        Wait::Timeout => {
            // Deliberately no `stop`: the builder may simply be slow, and stopping it here would
            // tear down the very pod the next connection is waiting for.
            outcome("timeout");
            tracing::warn!(%slug, secs = waited, "gate.start.timeout");
            return;
        }
        Wait::ClientGone => {
            // NOT a timeout: nothing failed, the client left. Counting it as one would report a
            // slow builder every time somebody hits ^C, and polling on to the full budget for a
            // socket nobody is reading is two minutes of api calls per abandoned build.
            outcome("client_gone");
            tracing::info!(%slug, "gate.client.gone");
            return;
        }
    };
    outcome("ok");
    splice::splice(sock, upstream).await;
    drop(open);
}

enum Wait {
    Ready(tokio::net::TcpStream),
    /// The budget ran out with the dial itself still failing — the builder said ready and the
    /// address did not answer, which is a different fault from one that never came up.
    Refused(String, std::io::Error),
    Timeout,
    ClientGone,
}

/// Poll the builder AND dial it until buildkit answers, the budget runs out, or the CLIENT hangs
/// up. The dial belongs inside this loop rather than after it: `ready` is a report about the
/// Service, and between the report and the connect there is a moment where the name does not
/// resolve yet. Failing the connection there closed the build for a builder that was seconds from
/// answering, so a dial failure inside the budget is "not ready yet" and nothing else.
///
/// `peek`, never `read`: those bytes are the client's own request (buildkit's HTTP/2 preface
/// arrives immediately, before the builder is anywhere near up) and consuming one would corrupt
/// the stream the splice is about to carry. `Ok(0)` from a peek is EOF and nothing else.
async fn wait_ready(gate: &Gate, slug: &str, budget: Duration, sock: &tokio::net::TcpStream) -> Wait {
    let deadline = tokio::time::Instant::now() + budget;
    // Once the client has sent something, its socket stays readable and watching it would spin
    // this loop; from then on the wait is a plain sleep.
    // ponytail: with bytes already buffered, a FIN is indistinguishable from data without
    // draining the stream the splice still needs — so a hangup AFTER the first bytes is only
    // found when the pump reaches it. Upgrade path if the polling cost ever shows: peek the
    // buffered bytes aside and hand them to `copy_bidirectional` as a prefix.
    let mut watch = true;
    // The last dial failure, if there was one: it decides whether running out of the budget is a
    // builder that never came up or one whose address never answered.
    let mut dialled: Option<(String, std::io::Error)> = None;
    macro_rules! out_of_budget {
        () => {
            match dialled.take() {
                Some((addr, e)) => return Wait::Refused(addr, e),
                None => return Wait::Timeout,
            }
        };
    }
    loop {
        if gate.api.ready(slug).await.unwrap_or(false) {
            let addr = gate.buildkit_addr(slug);
            match tokio::net::TcpStream::connect(&addr).await {
                Ok(up) => return Wait::Ready(up),
                Err(e) => {
                    tracing::debug!(%slug, %addr, error = %e, "gate.dial.retry");
                    dialled = Some((addr, e));
                }
            }
        }
        if !watch {
            if tokio::time::Instant::now() + POLL_GAP > deadline {
                out_of_budget!();
            }
            tokio::time::sleep(POLL_GAP).await;
            continue;
        }
        let mut byte = [0u8; 1];
        tokio::select! {
            // The hangup is looked at FIRST. A client that left at the same instant the budget
            // ran out is a client that left, not a builder that timed out — and under a paused
            // test clock the deadline arm is always ready, so without the bias it always won and
            // a hangup was counted as a timeout whenever the run was slow (seen in the full
            // workspace test run, never alone).
            biased;
            peeked = sock.peek(&mut byte) => match peeked {
                Ok(0) => return Wait::ClientGone,
                Ok(_) => {
                    watch = false;
                    tokio::time::sleep(POLL_GAP).await;
                }
                // A broken socket is a gone client; there is nothing to splice to either way.
                Err(_) => return Wait::ClientGone,
            },
            _ = tokio::time::sleep_until(deadline) => out_of_budget!(),
            _ = tokio::time::sleep(POLL_GAP) => {}
        }
    }
}

fn outcome(o: &'static str) {
    metrics::counter!("builder_gate_starts_total", "outcome" => o).increment(1);
}
