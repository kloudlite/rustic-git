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
        Self { http: reqwest::Client::new(), base: base.trim_end_matches('/').to_string(), secret }
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

    if let Err(e) = gate.api.start(&slug).await {
        outcome("refused");
        tracing::warn!(%slug, error = %e, "gate.start.failed");
        return;
    }

    let waited = gate.central.load().builder_start_secs;
    if !wait_ready(&gate, &slug, Duration::from_secs(waited)).await {
        // Deliberately no `stop`: the builder may simply be slow, and stopping it here would
        // tear down the very pod the next connection is waiting for.
        outcome("timeout");
        tracing::warn!(%slug, secs = waited, "gate.start.timeout");
        return;
    }

    // Counted from BEFORE the dial: a builder that is up must not be stopped out from under a
    // connection that is still being established.
    let open = splice::Open::new(gate.clone(), slug.clone());
    let addr = gate.buildkit_addr(&slug);
    let upstream = match tokio::net::TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            outcome("refused");
            tracing::warn!(%slug, %addr, error = %e, "gate.dial.failed");
            return;
        }
    };
    outcome("ok");
    splice::splice(sock, upstream).await;
    drop(open);
}

async fn wait_ready(gate: &Gate, slug: &str, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if gate.api.ready(slug).await.unwrap_or(false) {
            return true;
        }
        if tokio::time::Instant::now() + POLL_GAP > deadline {
            return false;
        }
        tokio::time::sleep(POLL_GAP).await;
    }
}

fn outcome(o: &'static str) {
    metrics::counter!("builder_gate_starts_total", "outcome" => o).increment(1);
}
