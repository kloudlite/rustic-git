//! Replication's transport, both halves. It is PULL-based: `pull_beat` decides which commits this
//! node is missing and GETs them from a peer that has them, and the listener serves the other
//! side of that — `btrfs send`'s stdout streamed as the response body. A node therefore only ever
//! receives what it asked for, and no peer can push bytes at it.
//!
//! An unset `WS_PEER_SECRET` disables both halves (`lib.rs` never spawns `serve`, and every dial
//! in this file returns early) — fail-closed: no secret configured means no root-run `btrfs send`
//! reachable from the network, ever.

use crate::controller::{replace_status, Ctx};
use crate::janitor;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, PostParams};
use kube::ResourceExt;
use rustic_git_storage::store::valid_segment;
use rustic_git_workspaces::crd;
use rustic_git_workspaces::engine::Engine;
use rustic_git_workspaces::replicate;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::io::StreamReader;

/// Everything the router needs, minus the parts of `Ctx` it does not use (janitor state, nix,
/// in-flight map) — a peer request never touches any of those.
pub struct PeerState {
    pub client: kube::Client,
    pub pool: String,
    pub node: String,
    pub secret: String,
    /// The `btrfs` binary to invoke, for both `receive` and `subvolume delete`. Always `"btrfs"`
    /// in production; tests point this at a fake script so the router is testable without root
    /// or a real filesystem — see `bins/agent/tests/peer.rs`.
    pub btrfs_bin: String,
    /// Serializes this node's outbound `btrfs send`s per volume id: a puller that retried can
    /// overlap its retry with the send it is retrying, and two concurrent sends of one volume buy
    /// nothing but disk contention. The puller's own ancestor-first ordering already wants one
    /// volume's transfers sequential.
    sends: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Shared with `Ctx::pull_wake`, so `/peer/v1/wake` reaches the puller's own beat.
    pub pull_wake: Arc<tokio::sync::Notify>,
}

impl PeerState {
    /// The one constructor — `sends` starts empty and is never meaningfully set any other
    /// way, so nothing outside this module (tests included) builds a `PeerState` by struct
    /// literal.
    pub fn new(client: kube::Client, pool: String, node: String, secret: String, btrfs_bin: String) -> PeerState {
        PeerState {
            client,
            pool,
            node,
            secret,
            btrfs_bin,
            sends: StdMutex::new(HashMap::new()),
            pull_wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn from_ctx(ctx: &Ctx, secret: String) -> PeerState {
        // The listener's notify must be the PULLER's, not a fresh one: a wake that fired a
        // private `Notify` would be a 204 nobody is waiting on.
        PeerState { pull_wake: ctx.pull_wake.clone(), ..PeerState::new(ctx.client.clone(), ctx.pool.clone(), ctx.node.clone(), secret, "btrfs".into()) }
    }

    fn send_lock(&self, id: &str) -> Arc<AsyncMutex<()>> {
        self.sends.lock().unwrap_or_else(|p| p.into_inner()).entry(id.to_string()).or_default().clone()
    }
}

pub fn router(state: PeerState) -> Router {
    Router::new()
        .route("/peer/v1/commit/{volume}/{name}", get(commit))
        // A poke, not a transfer: the body is empty and the answer is 204. Same secret as the
        // commit route and the same NetworkPolicy, because it drives the same root-run machinery.
        .route("/peer/v1/wake", axum::routing::post(wake))
        .with_state(Arc::new(state))
}

/// `WS_PEER_ADDR`, default `0.0.0.0:8444`. Spawned from `lib.rs` only when `WS_PEER_SECRET` is
/// set — see the module comment.
pub async fn serve(ctx: &Ctx, secret: String) -> Result<(), String> {
    let addr = std::env::var("WS_PEER_ADDR").unwrap_or_else(|_| "0.0.0.0:8444".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| format!("{addr}: {e}"))?;
    tracing::info!(%addr, "peer listener up");
    axum::serve(listener, router(PeerState::from_ctx(ctx, secret))).await.map_err(|e| e.to_string())
}

/// Equal in time proportional to the digest length, not to where the strings first differ — a
/// timing side-channel on a bearer secret is a real attack, and this compares SHA-256 digests of
/// both sides rather than pulling in a dedicated constant-time-compare crate for one call site.
fn secret_ok(headers: &HeaderMap, want: &str) -> bool {
    // An empty `want` must never authenticate: `unwrap_or("")` below means a request with no
    // header at all would otherwise compare equal to a misconfigured empty secret. Unreachable in
    // production today — `lib.rs` only spawns this listener when `WS_PEER_SECRET` is non-empty —
    // but the guard belongs here, at the trust boundary, not at the one caller that happens to
    // enforce it today.
    if want.is_empty() {
        return false;
    }
    let got = headers.get("x-peer-secret").and_then(|v| v.to_str().ok()).unwrap_or("");
    let (a, b) = (Sha256::digest(got.as_bytes()), Sha256::digest(want.as_bytes()));
    a == b
}

fn subvolume_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    names
}

#[derive(serde::Deserialize)]
struct CommitQuery {
    parent: Option<String>,
}

/// The pull side's send: streams `btrfs send [-p parent] snap_dir/{name}`'s stdout as the response
/// body. Auth and `valid_segment` before anything the
/// path could steer — the body here is a root-run `btrfs send`.
async fn commit(
    State(state): State<Arc<PeerState>>,
    headers: HeaderMap,
    Path((volume, name)): Path<(String, String)>,
    Query(q): Query<CommitQuery>,
) -> impl IntoResponse {
    if !secret_ok(&headers, &state.secret) {
        return (StatusCode::UNAUTHORIZED, Body::empty()).into_response();
    }
    if !valid_segment(&volume) || !valid_segment(&name) || !q.parent.as_deref().map(valid_segment).unwrap_or(true) {
        return (StatusCode::BAD_REQUEST, Body::empty()).into_response();
    }

    let dir = std::path::Path::new(&state.pool).join("vol").join(&volume).join("snap");
    let snap = dir.join(&name);
    if !snap.exists() {
        return (StatusCode::NOT_FOUND, Body::empty()).into_response();
    }
    let parent_path = q.parent.as_ref().map(|p| dir.join(p));

    // Held for the life of the stream (moved into `KillOnDrop` below): a retried pull for the
    // same volume must not race a send still in flight for it.
    let guard = state.send_lock(&volume).lock_owned().await;

    let mut child = match spawn_send_tokio(&state.btrfs_bin, &snap, parent_path.as_deref(), &[]) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn btrfs send: {e}")).into_response(),
    };
    let Some(stdout) = child.stdout.take() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "btrfs send: no stdout".to_string()).into_response();
    };
    // Drained concurrently, same reason `post_send` drains the sender's stderr while the POST is
    // in flight: nothing else empties this pipe, and an unread 64K of stderr would otherwise
    // block a chatty `btrfs send` forever, invisibly, on both ends of this stream.
    let (volume_id, commit_name) = (volume.clone(), name.clone());
    let stderr_task = child.stderr.take().map(|mut se| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut se, &mut buf).await;
            buf
        })
    });
    let killer = KillOnDrop { stdout, child: Some(child), stderr_task, volume: volume_id, name: commit_name, _guard: guard };
    (StatusCode::OK, Body::from_stream(tokio_util::io::ReaderStream::new(killer))).into_response()
}

/// Wraps a streamed `btrfs send`'s stdout so a response body dropped mid-stream (a disconnected
/// or timed-out puller) kills and reaps the child instead of leaking a root process writing to a
/// pipe nobody reads any more — the same failure `post_send`'s `kill()` exists to avoid on the
/// sending side, mirrored here on the receiving-of-the-request-but-sending-the-body side.
struct KillOnDrop {
    stdout: tokio::process::ChildStdout,
    child: Option<tokio::process::Child>,
    /// Drains stderr concurrently with the streamed body — see the comment at the `commit`
    /// handler's spawn site.
    stderr_task: Option<tokio::task::JoinHandle<Vec<u8>>>,
    volume: String,
    name: String,
    _guard: OwnedMutexGuard<()>,
}

impl tokio::io::AsyncRead for KillOnDrop {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stdout).poll_read(cx, buf)
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // Fire-and-forget: Drop cannot await. A child that already exited cleanly (the normal,
        // successful-send case) makes `kill`/`wait` here a cheap no-op; a child still writing
        // when the body was dropped early gets SIGKILL and reaped rather than orphaned.
        let Some(mut child) = self.child.take() else { return };
        let stderr_task = self.stderr_task.take();
        let (volume, name) = (self.volume.clone(), self.name.clone());
        tokio::spawn(async move {
            let _ = child.kill().await;
            let exit = child.wait().await;
            if !matches!(&exit, Ok(s) if s.success()) {
                let stderr = match stderr_task {
                    Some(t) => t.await.unwrap_or_default(),
                    None => Vec::new(),
                };
                tracing::warn!(%volume, %name, status = ?exit, stderr = %tail_str(&stderr, 300), "commit: btrfs send exited non-zero");
            }
        });
    }
}

async fn delete_subvolume(btrfs_bin: &str, path: &std::path::Path) {
    let parts: Vec<&str> = btrfs_bin.split_whitespace().collect();
    let Some((prog, prefix)) = parts.split_first() else { return };
    let _ = tokio::process::Command::new(prog).args(prefix).arg("subvolume").arg("delete").arg(path).status().await;
}

/// `WS_REPLICA_SECS`, default 300.
pub fn replica_interval() -> std::time::Duration {
    std::time::Duration::from_secs(std::env::var("WS_REPLICA_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300))
}

/// The pool-eligible nodes, `rustic-git.io/pool=true`, name-sorted so `replicate::targets`'
/// rendezvous scoring is deterministic across every node running this beat.
pub(crate) async fn pool_nodes(client: &kube::Client) -> Result<Vec<String>, String> {
    let api: kube::Api<Node> = kube::Api::all(client.clone());
    let lp = ListParams::default().labels("rustic-git.io/pool=true");
    let list = api.list(&lp).await.map_err(|e| e.to_string())?;
    let mut names: Vec<String> = list.items.into_iter().map(|n| n.name_any()).collect();
    names.sort();
    Ok(names)
}

/// `{pod ip}:8444` for the `rustic-git-agent` pod on `node` — the peer listener's own address,
/// found through the ClusterRole's existing pods get/list grant rather than a DNS name, since a
/// DaemonSet pod has no stable per-node service.
async fn agent_pod_addr(client: &kube::Client, node: &str) -> Result<String, String> {
    let api: kube::Api<Pod> = kube::Api::namespaced(client.clone(), "kube-system");
    let lp = ListParams::default().labels("app=rustic-git-agent").fields(&format!("spec.nodeName={node}"));
    let pods = api.list(&lp).await.map_err(|e| e.to_string())?;
    let ip = pods
        .items
        .into_iter()
        .find_map(|p| p.status.and_then(|s| s.pod_ip))
        .ok_or_else(|| format!("no ready rustic-git-agent pod on {node}"))?;
    Ok(format!("{ip}:8444"))
}

/// `WS_PEER_SEND_TIMEOUT_SECS`, default 3600. A send is legitimately tens of GiB; this exists to
/// unwedge a connection that has actually stalled, not to police link speed. The receive side has
/// no timeout knob of its own — the sender's is the only bound on a transfer.
fn send_timeout() -> Duration {
    Duration::from_secs(std::env::var("WS_PEER_SEND_TIMEOUT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3600))
}

/// The client every peer dial in this file shares. `connect_timeout` alone, not a blanket
/// `.timeout()`: the GET calls above set their own short bound per request, and the POST below
/// sets its own generous one — a client-wide default would have to be the smaller of the two and
/// wrongly cap the send.
fn peer_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().map_err(|e| e.to_string())
}

/// "Something you replicate just changed; pull now." The whole handler is one `notify_one`: the
/// puller decides what to fetch, exactly as it does on its own ticker. Nothing here is trusted
/// beyond the secret — a wake can only make a pass happen sooner, never change what it pulls.
async fn wake(State(state): State<Arc<PeerState>>, headers: HeaderMap) -> impl IntoResponse {
    if !secret_ok(&headers, &state.secret) {
        return StatusCode::UNAUTHORIZED;
    }
    state.pull_wake.notify_one();
    StatusCode::NO_CONTENT
}

/// POST `/peer/v1/wake` to every placeable node but me, ALL AT ONCE. Serially, one dead peer cost
/// the caller its full timeout before the next was even dialled, so a stop behind N unreachable
/// nodes stalled N x 5 s; concurrently the whole fan-out is bounded by the slowest single node.
/// Every failure is a warn and never an error: the wake is an optimisation on top of the ticker,
/// and a stop that failed because a peer was unreachable would be strictly worse than a stop that
/// replicates a beat later.
///
/// The secret is a parameter, not an env read: the callers already hold one (`Ctx::peer_secret`,
/// read once at boot) and a function that reads process env is a function whose tests must write
/// process env. Tests therefore pass their own without touching the process.
pub async fn wake_peers(ctx: &Arc<Ctx>, live: &[String], secret: &str) {
    if secret.is_empty() {
        return; // fail-closed, same rule as every other dial in this file
    }
    let Ok(http) = peer_http_client() else { return };
    let dials = live.iter().filter(|n| *n != &ctx.node).map(|node| {
        let (http, secret) = (&http, &secret);
        async move {
            let addr = match agent_pod_addr(&ctx.client, node).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(%node, error = %e, "wake: no peer address; the ticker will get it");
                    return;
                }
            };
            let url = format!("http://{addr}/peer/v1/wake");
            match http.post(&url).header("x-peer-secret", *secret).timeout(Duration::from_secs(5)).send().await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => tracing::warn!(%node, status = %r.status(), "wake: peer refused"),
                Err(e) => tracing::warn!(%node, error = %e, "wake: peer unreachable; the ticker will get it"),
            }
        }
    });
    futures::future::join_all(dials).await;
}

/// What `spawn_pull` does when a pass ends: the coalescing rule, lifted out of the loop so it can
/// be tested without a clock. `notify_one` leaves at most ONE permit however many wakes arrived, so
/// taking it here without waiting turns a burst into exactly one extra pass.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    RunAgain,
    /// Something could not be fetched: come back after this long rather than at the next tick, and
    /// still take a wake the moment one arrives.
    RetrySoon(Duration),
    Wait,
}

/// The FIRST retry delay after a missed pass. Short enough that a source coming back is picked up
/// while the person is still watching, long enough not to hammer a peer that is simply down.
pub(crate) const RETRY_SOON: Duration = Duration::from_secs(30);

/// `RETRY_SOON` doubled per CONSECUTIVE missed pass, capped at the ordinary tick. Without the cap
/// a permanently unfetchable commit — a Snapshot whose only source is gone for good — pinned every
/// node placed on that volume at a 30 s beat forever, node-wide: the flag is per-PASS, so one
/// stuck volume paid the whole node's listing cost every 30 s until someone deleted the CR.
/// Capping at `replica_interval` makes the worst case exactly today's steady state.
fn retry_delay(misses: u32) -> Duration {
    RETRY_SOON.saturating_mul(1u32 << misses.saturating_sub(1).min(16)).min(replica_interval())
}

/// `misses` counts CONSECUTIVE passes that missed something, and is reset by any clean pass — a
/// volume that starts fetching again returns the node to its ordinary beat immediately.
pub(crate) fn after_pass(wake: &tokio::sync::Notify, missed: bool, misses: &mut u32) -> Next {
    use futures::FutureExt;
    *misses = if missed { misses.saturating_add(1) } else { 0 };
    if wake.notified().now_or_never().is_some() {
        Next::RunAgain
    } else if missed {
        Next::RetrySoon(retry_delay(*misses))
    } else {
        Next::Wait
    }
}

/// Last `n` bytes of a possibly-binary buffer, lossily decoded — enough to see the actual btrfs
/// error without risking a multi-megabyte log line on a chatty failure.
fn tail_str(buf: &[u8], n: usize) -> String {
    let start = buf.len().saturating_sub(n);
    String::from_utf8_lossy(&buf[start..]).trim().to_string()
}

/// Same `send`/`-p`/`-c` shape as `blob::spawn_send`, but `tokio::process::Command` with a piped
/// stderr — the sender streams the child's stdout straight into the POST body, which needs an
/// async stdout handle `blob::spawn_send`'s `std::process::Child` cannot give without a
/// runtime-specific fd conversion.
fn spawn_send_tokio(btrfs_bin: &str, path: &FsPath, parent: Option<&FsPath>, clones: &[PathBuf]) -> std::io::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new(btrfs_bin);
    cmd.args(["send", "-q"]);
    if let Some(p) = parent {
        cmd.arg("-p").arg(p);
    }
    for c in clones {
        cmd.arg("-c").arg(c);
    }
    cmd.arg(path).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd.spawn()
}

/// `WS_NODE_DEAD_SECS`, default 600 — how long a node must be observed NotReady before its
/// `VolumeReplica` rows are reaped. Long enough that a rolling restart or a brief kubelet hiccup
/// never costs a replica row; the row is cheap to recreate, a wrongly-reaped one is not.
pub(crate) fn node_dead_secs() -> i64 {
    std::env::var("WS_NODE_DEAD_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600)
}

/// One pass of the puller — spawned beside `replicate_beat` in `controller/run.rs`. Inert without a
/// peer secret, same fail-closed rule every dial in this file follows: no secret, no
/// authenticated GET to another node's root-run `btrfs send`.
/// Returns true when some commit could not be fetched this pass, so the caller retries soon
/// instead of waiting out the full tick.
pub async fn pull_beat(ctx: &Arc<Ctx>) -> bool {
    if ctx.peer_secret.is_empty() {
        return false;
    }
    pull_beat_with(ctx, "btrfs", &ctx.peer_secret).await
}

/// The nodes a stopped parent could start on, from this node's own view — the pool minus the
/// unplaceable. Keep-biased: a listing error is an empty list, which wakes nobody and places
/// nothing, rather than a guess about who is alive.
pub(crate) async fn placeable_nodes(ctx: &Arc<Ctx>) -> Vec<String> {
    let (Ok(pool), Ok(nodes)) = (
        pool_nodes(&ctx.client).await,
        Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await,
    ) else {
        return Vec::new();
    };
    live_nodes(&pool, &nodes.items, node_dead_secs(), k8s_openapi::jiff::Timestamp::now())
}

/// Rendezvous over the FULL pool keeps electing a corpse: the reaper deletes its row every beat
/// and no live node ever becomes a target, so a volume sits one copy short until the node comes
/// back. Placement therefore sees only nodes that pass `unplaceable` — dead, or decommissioning —
/// and a node with no Node object at all is dead, not unknown.
fn live_nodes(pool: &[String], nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp) -> Vec<String> {
    pool.iter().filter(|n| !unplaceable(nodes.iter().find(|k| k.name_any() == n.as_str()), floor, now)).cloned().collect()
}

/// `targets()` counts the owner as one of `total` and hands back `total - 1` standbys. A dead or
/// decommissioning owner holds nothing anyone can reach, so it is not a copy: ask for one standby more.
fn standby_count(owner_alive: bool, replicas: u32) -> usize {
    replicas as usize + usize::from(!owner_alive)
}

/// Split out so tests can point the receive half at a fake `btrfs` — same shape as
/// `SendTo::btrfs_bin` on the send side — and pass the secret directly rather than through
/// `WS_PEER_SECRET`, which every test in this binary would otherwise share.
async fn pull_beat_with(ctx: &Arc<Ctx>, btrfs_bin: &str, secret: &str) -> bool {
    // Listed ONCE and threaded through everything below: a partial view of who is alive must reap,
    // unclaim and place nothing, and every one of those decisions needs to agree on the same list.
    let nodes = match Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(error = %e, "pull: listing nodes; reaping, unclaiming and placing nothing");
            return false;
        }
    };

    // One clock and one floor for the whole pass: reap, unclaim and live_nodes must agree on
    // exactly the same "dead" answer, not three readings a few nanoseconds apart.
    let now = k8s_openapi::jiff::Timestamp::now();
    let floor = node_dead_secs();

    // A node the cluster reads as dead must not sweep: its agent kept running through a kubelet
    // outage, so it went on reaping replicas, unclaiming volumes and retiring copies on a view of
    // the cluster nobody else shares — and every other live node was already doing that work
    // correctly. `node_is_dead`, never `unplaceable`: a DECOMMISSIONING node is alive and must keep
    // sweeping, or its own drain never finishes. The 180 s floor is the only guard here: a node
    // wrongly NotReady past it stops reconciling until its Node object recovers, which is the
    // deliberate trade — a wrong sweep deletes data, a paused one only waits.
    if node_is_dead(nodes.iter().find(|k| k.name_any() == ctx.node), floor, now) {
        tracing::warn!(node = %ctx.node, "pull: my own Node reads NotReady past the floor; sweeping nothing this pass");
        return false;
    }

    // One LISTING for the whole pass, for the same reason the node list is threaded: reap,
    // unclaim, place and retire each decide what to delete, and two of them acting on different
    // views of the cluster is how a copy nobody else holds gets dropped. The sweeps below run
    // once this has succeeded, beside each other so the two never drift onto different dead-node
    // rules; a partial listing bails the whole beat rather than let any of them act on it.
    let Some(beat) = crate::listing::beat(ctx).await else { return false };

    reap_dead_replicas(ctx, &beat, &nodes, floor, now).await;
    // DEAD nodes only, never merely decommissioning ones: a decommissioning node is alive, its
    // running work keeps running, and it releases its volumes at its own pace from its own beat.
    sweep_dead_nodes(ctx, &beat, &nodes, floor, now).await;

    let candidates = match pool_nodes(&ctx.client).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "pull: listing pool nodes");
            return false;
        }
    };

    let http = match peer_http_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "pull: building the http client");
            return false;
        }
    };

    let live = live_nodes(&candidates, &nodes, floor, now);
    let mut missed = false;
    for id in interesting_volumes(ctx, &beat, &live) {
        missed |= pull_volume(ctx, &beat, btrfs_bin, &http, secret, &id, &live).await;
    }
    retire_pass(ctx, &beat, &live).await;
    missed
}

/// Every volume this node must hold a commit-model replica of: named by replication's rendezvous
/// (`replicate::targets`, standbys only — the owner already has everything by construction), OR
/// the volume behind a Workspace/Environment whose pod runs here right now, OR a volume this node
/// itself owns (`spec.nodeName == me`) — the owner's row is a source for every standby, and a
/// STOPPED volume (no pod, nothing in `Workspace/Environment.status.nodeName`) still needs one, or
/// the first standby to look finds an empty source list forever. A Volume-list hiccup now idles
/// the whole beat (keep-biased — see `beat`'s bail-out above) instead of falling back to only the
/// worktree-hosted volumes it used to still pull.
fn interesting_volumes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, live: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in &beat.volumes {
        if v.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let id = v.name_any();
        let i_am_owner = v.spec.node_name == ctx.node;
        let owner_alive = live.iter().any(|n| n == &v.spec.node_name);
        let targets = replicate::targets(&id, &v.spec.node_name, live, standby_count(owner_alive, v.spec.replicas));
        // Holding a copy on disk is interesting on its own: with `replicas: 1` a returning node's
        // replica row was reaped while it was dead and rendezvous elected someone else who has no
        // source at all, so nothing would ever re-register the one copy that exists.
        let hold_a_copy = ctx.engine.pool.voldir(&id).exists();
        if (i_am_owner || hold_a_copy || targets.iter().any(|t| t == &ctx.node)) && !out.contains(&id) {
            out.push(id);
        }
    }
    // The parent half: a worktree running here needs its volume pulled whether or not rendezvous
    // named this node. Same list `retire_pass` and the sync beat read.
    for p in &beat.parents {
        if !out.contains(&p.volume) {
            out.push(p.volume.clone());
        }
    }
    out
}

/// The chain-walk `pull_volume` needs before every GET: `cur`'s nearest ancestor (inclusive) this
/// node already holds locally, or `None` for "nothing shared yet — a full send". Walks
/// `SnapshotSpec::parent`, never creation time — same rule the CR's own doc comment states.
fn nearest_held_ancestor(mut cur: Option<String>, by_name: &HashMap<String, (String, String)>, have: &HashSet<String>) -> Option<String> {
    while let Some(name) = cur {
        if have.contains(&name) {
            return Some(name);
        }
        cur = by_name.get(&name).map(|(parent, _)| parent.clone()).filter(|p| !p.is_empty());
    }
    None
}

/// Local commits whose CR is gone entirely — retention's disk-side convergence. Pure, so
/// `pull_volume`'s "which locals to drop" decision is testable without real btrfs (`drop_commit`
/// itself is the engine's own concern, covered by `engine_commit.rs`'s loopback tests).
///
/// `any_pull_failed` reclaims NOTHING. The owner deletes `sync-A`'s CR the instant `sync-B` is
/// Ready, so a replica that could not reach the owner this pass would drop its local `sync-A` and
/// gain nothing — going from one sync point to none, in exactly the partition-then-owner-death
/// case sync points exist for. Deferring the reclaim costs a subvolume until the next clean pass.
/// ponytail: all-or-nothing rather than transients-only, because a retired name has no CR left to
/// read `spec.transient` off — telling a swept commit from a swept sync point here would mean
/// trusting the name prefix. Split it if held-back commits ever cost real space.
fn retired(have: &HashSet<String>, existing: &HashSet<String>, any_pull_failed: bool) -> Vec<String> {
    if any_pull_failed {
        return Vec::new();
    }
    have.iter().filter(|n| !existing.contains(*n)).cloned().collect()
}

/// Pulls every `Snapshot` this node is missing for `volume`, then rewrites this node's own
/// `VolumeReplica`. Keep-biased throughout: a `Snapshot`-list error skips the volume with nothing
/// touched, same as `replica_reconcile`'s lookup-error branch.
#[allow(clippy::too_many_arguments)]
async fn pull_volume(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, btrfs_bin: &str, http: &reqwest::Client, secret: &str, volume: &str, live: &[String]) -> bool {
    let snap_api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    // One list, all phases: the Ready subset drives the pull below, and the FULL name set is what
    // tells a deleted CR from a Working one, below — a `Snapshot` has no finalizer (see
    // `snapshot::reconcile_commit`'s module doc), so this diff against `local_commits` is the only
    // place any node ever notices a commit's CR is gone.
    let all: Vec<crd::Snapshot> = match snap_api.list(&ListParams::default().fields(&format!("spec.volume={volume}"))).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(%volume, error = %e, "pull: listing snapshots; keeping everything");
            return false;
        }
    };
    let existing: HashSet<String> = all.iter().map(|s| s.name_any()).collect();
    let ready: Vec<crd::Snapshot> =
        all.into_iter().filter(|s| s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready)).collect();

    let mut have: HashSet<String> = match ctx.engine.local_commits(volume) {
        Ok(names) => names.into_iter().collect(),
        Err(e) => {
            tracing::warn!(%volume, error = %e, "pull: local_commits");
            return false;
        }
    };

    // name -> (parent, owner), for the ancestor walk and for the pairs `order_groups` wants.
    let by_name: HashMap<String, (String, String)> =
        ready.iter().map(|s| (s.name_any(), (s.spec.parent.clone(), s.spec.owner.clone()))).collect();
    let pairs: Vec<(String, Option<String>)> = ready
        .iter()
        .filter(|s| !have.contains(&s.name_any()))
        .map(|s| (s.name_any(), if s.spec.parent.is_empty() { None } else { Some(s.spec.parent.clone()) }))
        .collect();
    let order = replicate::order_groups(&pairs);

    let replicas: Vec<&crd::VolumeReplica> = beat.replicas.iter().filter(|r| r.spec.volume == volume).collect();
    // Synced sources first — a Syncing replica may itself be mid-pull and not actually have the
    // commit yet — falling back to any other replica of the volume (including a Syncing one)
    // rather than giving up outright. Never my own row: pulling from myself is meaningless, and
    // an owner or a re-selected standby always sees its own (possibly stale) row in this list.
    let not_me = |r: &&&crd::VolumeReplica| r.spec.node != ctx.node;
    let synced = |r: &&&crd::VolumeReplica| r.status.as_ref().is_some_and(|s| s.phase == "Synced");
    let mut sources: Vec<&str> = replicas.iter().filter(not_me).filter(synced).map(|r| r.spec.node.as_str()).collect();
    sources.extend(replicas.iter().filter(not_me).filter(|r| !synced(r)).map(|r| r.spec.node.as_str()));
    // The OWNER, last, and only while it is live. Every commit exists on the owner by
    // construction, but a replica row for it may not exist yet (a fresh volume) or may have been
    // reaped — which left the first standby with an empty source list and a commit it could never
    // fetch. Last so a Synced peer is still preferred, and skipped when the owner is not in `live`
    // so a genuinely dead owner does not cost a failed dial per commit per pass.
    if let Some(owner) = beat.volumes.iter().find(|v| v.name_any() == volume).map(|v| v.spec.node_name.as_str()) {
        if !owner.is_empty() && owner != ctx.node && live.iter().any(|n| n == owner) && !sources.contains(&owner) {
            sources.push(owner);
        }
    }

    // Resolved ONCE per pass, before the commit loop: `agent_pod_addr` is a namespaced pod LIST
    // with two selectors, and a node catching up on N commits was making N of them per source to
    // learn the same IP. A source whose pod cannot be found now is skipped for the whole pass —
    // which is what the per-commit `continue` amounted to anyway, one list at a time.
    let mut addrs: Vec<(&str, String)> = Vec::new();
    for &source in &sources {
        match agent_pod_addr(&ctx.client, source).await {
            Ok(a) => addrs.push((source, a)),
            Err(e) => tracing::warn!(%volume, source, error = %e, "pull: no peer address; skipping this source"),
        }
    }

    // Any pull that could not be satisfied this pass. It gates the retire pass below, because
    // the two together would otherwise LOSE a sync point: the owner deletes `sync-A`'s CR the
    // instant `sync-B` is Ready, so a replica that cannot reach the owner right now would drop
    // its local `sync-A` and gain nothing — from one sync point to none, in exactly the
    // partition-then-owner-death case sync points exist for.
    let mut any_pull_failed = false;
    for name in order {
        if have.contains(&name) {
            continue;
        }
        let parent = by_name.get(&name).map(|(p, _)| p.clone()).filter(|p| !p.is_empty());
        let my_parent = nearest_held_ancestor(parent, &by_name, &have);

        let mut pulled = false;
        for (source, addr) in &addrs {
            let source = *source;
            // `my_parent` is MY nearest held ancestor — the source may never have had it (it can
            // have pulled a different, shorter chain, or dropped an old commit already). A `-p`
            // the source doesn't recognize fails ITS `btrfs send`, which surfaces here as a
            // truncated body after the 200 header: the same "wrong -p, retry full" case
            // `send_to_target` already handles on the push side. One retry against the SAME
            // source with no parent at all before moving on, so a single bad guess costs one
            // extra full pull instead of losing this commit (and every descendant) forever.
            let mut result = pull_one(&ctx.engine, btrfs_bin, http, addr, secret, volume, &name, my_parent.as_deref()).await;
            if result.is_err() && my_parent.is_some() {
                tracing::warn!(%volume, %name, source, "pull: incremental receive failed, falling back to a full pull from the same source");
                result = pull_one(&ctx.engine, btrfs_bin, http, addr, secret, volume, &name, None).await;
            }
            match result {
                Ok(()) => {
                    have.insert(name.clone());
                    pulled = true;
                    break;
                }
                Err(e) => tracing::warn!(%volume, %name, source, error = %e, "pull: receive failed; trying next source"),
            }
        }
        if !pulled {
            any_pull_failed = true;
            tracing::warn!(%volume, %name, "pull: no source could supply this commit this pass");
        }
    }

    // Drop any local commit whose CR is gone entirely (not merely `Working` — `existing` holds
    // every phase). `drop_commit` is Ok-on-absent, so every node that ever held a copy converges
    // on the same disk state without a second round trip to confirm it.
    // Gated on `any_pull_failed` — see `retired`.
    for name in retired(&have, &existing, any_pull_failed) {
        if let Err(e) = ctx.engine.drop_commit(volume, &name) {
            tracing::warn!(%volume, snapshot = %name, error = %e, "pull: dropping a retired commit failed; left for the next pass");
        } else {
            have.remove(&name);
        }
    }

    let missing_at_end = ready.iter().any(|s| !have.contains(&s.name_any()));
    // What this node HOLDS, per worktree — not what it listed. A transient whose subvolume never
    // landed here would otherwise advertise data this node cannot serve, and placement would then
    // start a worktree on a node with no bytes for it. `have` is the disk, after the pull loop and
    // after the retire sweep above, so this is the honest answer for this pass.
    // `ready` is already the Ready subset, so the same (generation, name) key as
    // `newest_transient_of` is all that is left to apply — one pass, max per worktree.
    let mut best: std::collections::BTreeMap<String, (u64, String)> = Default::default();
    for s in ready.iter().filter(|s| s.spec.transient && have.contains(&s.name_any())) {
        let key = (crd::transient_generation_of(s), s.name_any());
        let slot = best.entry(s.spec.worktree.clone()).or_insert_with(|| key.clone());
        if key > *slot {
            *slot = key;
        }
    }
    let branches: std::collections::BTreeMap<String, String> = best.into_iter().map(|(w, (_, n))| (w, n)).collect();
    if let Err(e) = write_replica_status(ctx, volume, !missing_at_end, branches).await {
        tracing::warn!(%volume, error = %e, "pull: writing VolumeReplica status");
    }
    any_pull_failed
}

/// One `GET /peer/v1/commit/{volume}/{name}` streamed straight into `btrfs receive
/// snap_dir/{volume}/`. A failed receive deletes the partial, same before/after diff the push
/// side's `replicate` handler uses, mirrored here on the pulling node.
#[allow(clippy::too_many_arguments)]
async fn pull_one(
    engine: &Engine,
    btrfs_bin: &str,
    http: &reqwest::Client,
    addr: &str,
    secret: &str,
    volume: &str,
    name: &str,
    parent: Option<&str>,
) -> Result<(), String> {
    let mut url = format!("http://{addr}/peer/v1/commit/{volume}/{name}");
    if let Some(p) = parent {
        url = format!("{url}?parent={p}");
    }
    // ponytail: `send_timeout()` bounds the WHOLE streamed pull, not just the connect — a first
    // replica larger than ~1h of transfer at whatever the link does is timed out and retried from
    // the next source rather than finishing. `WS_PEER_SEND_TIMEOUT_SECS` is the escape hatch;
    // splitting "connect" from "whole body" is the upgrade if a legitimately huge first pull ever
    // needs longer than an operator wants to raise the env for everyone.
    let resp = http.get(&url).header("x-peer-secret", secret).timeout(send_timeout()).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: status {}", resp.status()));
    }

    let dir = engine.pool.snap_dir(volume);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let before = subvolume_names(&dir);

    let bin_parts: Vec<&str> = btrfs_bin.split_whitespace().collect();
    let Some((prog, prefix)) = bin_parts.split_first() else { return Err("empty btrfs_bin".to_string()) };
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(prefix).arg("receive").arg(&dir).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let mut reader = StreamReader::new(resp.bytes_stream().map_err(std::io::Error::other));
    let copy_result = tokio::io::copy(&mut reader, &mut stdin).await;
    let _ = stdin.shutdown().await;
    drop(stdin);
    let ok = match copy_result {
        Ok(_) => matches!(child.wait().await, Ok(s) if s.success()),
        Err(_) => {
            let _ = child.wait().await;
            false
        }
    };

    if !ok {
        let after = subvolume_names(&dir);
        for n in after.iter().filter(|n| !before.contains(n)) {
            delete_subvolume(btrfs_bin, &dir.join(n)).await;
        }
        return Err("btrfs receive failed".to_string());
    }
    Ok(())
}

/// The ordering key and the "newest transient" rule both live in `crd` now: `/v1` picks a clone
/// cut's parent with the same function this node's placement reads, and two copies of that key is
/// how two tiers disagree about which cut is newest.
pub(crate) use crd::newest_transient_of;

/// THE placement bar, and the only one: a replica is up to date for a worktree when it HOLDS that
/// worktree's newest Ready transient, by name — never by comparing clocks, which a skew between
/// nodes could make an old copy look current. A worktree with no transient at all (never ran, or a
/// fresh restore) has nothing to name, so plain `Synced` is the right bar: a Synced replica holds
/// every Ready commit.
pub(crate) fn up_to_date(replica: &crd::VolumeReplica, worktree: &str, newest_transient: Option<&str>) -> bool {
    let Some(st) = replica.status.as_ref() else { return false };
    match newest_transient {
        None => st.phase == "Synced",
        Some(want) => st.branches.get(worktree).is_some_and(|held| held == want),
    }
}

/// Which of these replica rows are up to date for `worktree` — the candidate set a start or a
/// clone chooses among, the owner being added by the caller (it holds the bytes by construction).
pub(crate) fn up_to_date_nodes(worktree: &str, newest: Option<&str>, rows: &[crd::VolumeReplica]) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().filter(|r| up_to_date(r, worktree, newest)).map(|r| r.spec.node.clone()).collect();
    out.sort();
    out
}

/// Rendezvous over the candidate set, keyed by the volume id — `replicate::targets`' own hash, so
/// the spread is deterministic and even by count and a retry lands on the same answer. Every node
/// computes the same result with no coordinator.
///
/// ponytail: by COUNT, not by load. Weighting by free CPU or pool space is the named upgrade and
/// needs an input every node computes identically — a per-node metric every agent can read the
/// same way, not one node's opinion.
pub(crate) fn preferred_node(volume: &str, candidates: &[String]) -> Option<String> {
    // `targets(volume, me = "", candidates, total = 2)` is "the top-scoring candidate", which is
    // the same ordering the replication spread already uses.
    replicate::targets(volume, "", candidates, 2).into_iter().next()
}

/// The newest Ready transient of `worktree` ANYWHERE IN THE CLUSTER — one field-selected list, for
/// a caller with no beat listing of its own. It deliberately ignores what this node holds: it is
/// the bar `up_to_date` compares a replica's `branches` against, so intersecting it with local
/// state would let a node behind on its pulls declare itself current. `snapshot::latest_transient`
/// is the local-hold variant, for a caller asking what it can actually check out right now.
pub(crate) async fn newest_transient(ctx: &Arc<Ctx>, volume: &str, worktree: &str) -> Result<Option<String>, kube::Error> {
    let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let list = api.list(&ListParams::default().fields(&format!("spec.volume={volume}"))).await?;
    Ok(newest_transient_of(&list.items, worktree))
}

/// Create-or-update THIS node's own `VolumeReplica` — the sole writer, per the module doc.
/// `branches` is `worktree -> the newest Ready transient this node holds`, which is what every
/// placement decision reads; `phase` is `Synced` iff nothing was missing at the end of this pass.
async fn write_replica_status(
    ctx: &Arc<Ctx>,
    volume: &str,
    synced: bool,
    branches: std::collections::BTreeMap<String, String>,
) -> Result<(), kube::Error> {
    let name = crd::replica_name(volume, &ctx.node);
    let api: Api<crd::VolumeReplica> = Api::all(ctx.client.clone());
    let mut obj = match api.get_opt(&name).await? {
        Some(o) => o,
        None => {
            let spec = crd::VolumeReplicaSpec { volume: volume.to_string(), node: ctx.node.clone() };
            let mut r = crd::VolumeReplica::new(&name, spec);
            // H2: owner is unknown here (only the volume id is), so only `rustic-git.io/volume`
            // is stamped — the e2e (`tests/ws_e2e.sh`) selects on exactly that.
            r.metadata.labels = Some(std::collections::BTreeMap::from([(crd::VOLUME_LABEL.to_string(), volume.to_string())]));
            api.create(&PostParams::default(), &r).await?
        }
    };
    let status = crd::VolumeReplicaStatus { phase: if synced { "Synced" } else { "Syncing" }.to_string(), branches };
    for attempt in 0..2 {
        match replace_status(&api, &obj, "VolumeReplica", serde_json::to_value(&status).map_err(kube::Error::SerdeError)?).await {
            Ok(()) => return Ok(()),
            Err(kube::Error::Api(s)) if s.code == 409 && attempt == 0 => obj = api.get(&name).await?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Deletes any `VolumeReplica` whose `spec.node` has been observed NotReady for longer than
/// `WS_NODE_DEAD_SECS` — positive evidence only. A nodes-list error reaps nothing (the whole
/// listing, not per-row, since a partial list would make an actually-live node look absent). A
/// node absent from a POSITIVELY-listed set counts as dead; a node present with no readable
/// `Ready` condition history does not — the API server just hasn't reported one yet.
/// The one positive-evidence rule both dead-node sweeps below apply, factored out once so the
/// replica reaper and the claim-unclaim sweep can never drift apart: absent from a nodes list we
/// DID get is dead; present with `Ready=false` past `floor` seconds is dead; present with no
/// readable `Ready` condition at all is NOT dead — the API server just hasn't converged one yet.
pub(crate) fn node_is_dead(node: Option<&Node>, floor: i64, now: k8s_openapi::jiff::Timestamp) -> bool {
    match node {
        None => true,
        Some(n) => n
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.type_ == "Ready"))
            .is_some_and(|c| c.status != "True" && c.last_transition_time.as_ref().is_some_and(|t| now.as_second() - t.0.as_second() > floor)),
    }
}

/// Whether an operator has asked for this node to be retired. Exact value only — see the constant.
pub(crate) fn decommissioning(node: Option<&Node>) -> bool {
    node.and_then(|n| n.metadata.labels.as_ref()).and_then(|l| l.get(crd::DECOMMISSION_LABEL)).is_some_and(|v| v == "true")
}

/// "Not a place to run", the ONE predicate every placement decision uses. Dead (NotReady past the
/// floor, or absent from a listing we did get) and decommissioning are the same answer here: both
/// mean nothing new may land, and keeping them as two tests is how the rendezvous and the sweep
/// eventually disagree about whether a node still owns anything.
///
/// It is deliberately NOT `node_is_dead`, which stays the reaper's rule: a decommissioning node is
/// alive, keeps serving pulls, and its replica rows must not be reaped out from under a peer that
/// is mid-transfer from it.
pub(crate) fn unplaceable(node: Option<&Node>, floor: i64, now: k8s_openapi::jiff::Timestamp) -> bool {
    node_is_dead(node, floor, now) || decommissioning(node)
}

// ponytail: `now` is THIS node's own clock against another node's `lastTransitionTime`
// (apiserver-stamped, but ultimately from whichever node reported the condition) — a fast
// local clock reaps a row slightly early, a slow one slightly late. `WS_NODE_DEAD_SECS`'s
// default (600s) swallows ordinary NTP drift; upgrade to an apiserver-relative delta (read the
// list's own server timestamp instead of a local `now`) if skew ever gets close to the floor.
async fn reap_dead_replicas(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp) {
    let replica_api: Api<crd::VolumeReplica> = Api::all(ctx.client.clone());
    for r in &beat.replicas {
        if node_is_dead(nodes.iter().find(|n| n.name_any() == r.spec.node), floor, now) {
            let rname = r.name_any();
            if let Err(e) = replica_api.delete(&rname, &Default::default()).await {
                tracing::warn!(replica = %rname, error = %e, "pull: reaper: deleting a dead node's replica row");
            }
        }
    }
}

/// What a sweep decides about one volume: `Mark` writes the condition and keeps the pin,
/// `Release` clears the pin and un-places every parent.
#[derive(Debug)]
pub(crate) enum VolumeVerdict {
    Mark { why: String },
    Release { why: String, reason: &'static str },
}

/// THE per-volume decision, for both sweeps. Ownership is per volume, so moving is decided per
/// volume — never per parent, which is exactly the bug this replaces: un-placing a stopped
/// workspace while a running clone of it kept the same volume pinned left the stopped one
/// claimable on a node that owns nothing.
///
/// The three arms, in the spec's order:
///   1. any parent Running        → nothing moves, pin kept, every parent marked;
///   2. some parent not replicated → nothing moves yet, pin kept — every parent must be
///      covered, or starting elsewhere loses that one's last edits;
///   3. otherwise                 → pin cleared, parents un-placed, an up-to-date node takes it.
///
/// `reason` is the condition reason the caller wants (`NodeDead` for the dead-node sweep,
/// `Decommissioned` for a drain) — the arms are identical, only the word differs.
pub(crate) fn volume_decision(
    volume: &str,
    owner: &str,
    parents: &[&crate::listing::Parent],
    reason: &'static str,
) -> VolumeVerdict {
    if let Some(running) = parents.iter().find(|p| p.is_live_worktree()) {
        return VolumeVerdict::Mark {
            why: format!(
                "owner {owner} is unavailable; a Running worktree ({}) still names volume {volume}, so it stays pinned",
                running.name
            ),
        };
    }
    // `replicated` is the OWNER's own `Replicated` condition off the listing, never recomputed
    // here: two nodes computing "is it replicated" independently is two truths that can disagree.
    let waiting: Vec<&str> = parents.iter().filter(|p| !p.replicated).map(|p| p.name.as_str()).collect();
    if !waiting.is_empty() {
        // Every one of them, not the first: an operator reading this needs to know which parents
        // are holding the volume, and the set shrinks one name at a time as replicas catch up.
        return VolumeVerdict::Mark {
            why: format!("owner {owner} is unavailable; waiting for a replica of: {}", waiting.join(", ")),
        };
    }
    VolumeVerdict::Release {
        why: format!("owner {owner} is unavailable; released, waiting for an up-to-date node to take it"),
        reason,
    }
}

/// Applies `volume_decision` to every volume whose owner is in `owners`. One place, called by the
/// dead-node sweep and by the decommission beat with different sets and different reasons — the
/// arms must never drift, and two copies of them is how they would.
///
/// `mark_running` is what separates the two callers' Mark arms. The dead sweep marks (true): the
/// node is gone, so `Unavailable`/`Degraded` is the literal truth and the only place the API can
/// say why nothing will start there. A drain does NOT (false): the node is alive and the workspace
/// is happily running, so writing `Degraded` would libel a healthy worktree — and `/v1`'s
/// `interrupted()` keys on exactly that condition, which would start 409ing clones of it. The
/// drain's `Decommissioning=True/NodeLeaving` on the parent already carries the whole message.
pub(crate) async fn sweep_volumes(
    ctx: &Arc<Ctx>,
    beat: &crate::listing::Beat,
    owners: &HashSet<String>,
    reason: &'static str,
    mark_running: bool,
) {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    for vol in beat.volumes.iter().cloned() {
        let owner = vol.spec.node_name.clone();
        if owner.is_empty() || !owners.contains(&owner) {
            continue;
        }
        let name = vol.name_any();
        // `all_parents`, not `parents`: this volume is owned by another node, so this node's own
        // scoped list would show none of its parents and every arm would read as "nothing on it".
        let parents: Vec<&crate::listing::Parent> = beat.all_parents.iter().filter(|p| p.volume == name).collect();
        // The reason comes back OUT of the verdict, so the word written is the one the decision
        // made rather than a second copy of the caller's argument.
        let (why, reason, release) = match volume_decision(&name, &owner, &parents, reason) {
            VolumeVerdict::Mark { .. } if !mark_running => continue,
            VolumeVerdict::Mark { why } => (why, reason, false),
            VolumeVerdict::Release { why, reason } => (why, reason, true),
        };
        let mut cur = vol;
        if release {
            // The pin FIRST, before anything is un-placed: a failed CAS with parents already
            // cleared would leave them claimable on a node that does not own the volume — the
            // exact bug this whole function exists to make impossible.
            //
            // `test` proves the owner hadn't already moved (a survivor's takeover landing between
            // our list and this patch), THEN `replace` clears it; a failed test (409/422) means we
            // lost that race, so nothing at all is written this beat.
            let ops = json_patch::Patch(vec![
                json_patch::PatchOperation::Test(json_patch::TestOperation {
                    path: "/spec/nodeName".parse().expect("static pointer parses"),
                    value: serde_json::json!(owner),
                }),
                json_patch::PatchOperation::Replace(json_patch::ReplaceOperation {
                    path: "/spec/nodeName".parse().expect("static pointer parses"),
                    value: serde_json::json!(""),
                }),
            ]);
            match api.patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Json::<crd::Volume>(ops)).await {
                // The patched object, not our stale copy: the PUT below carries a
                // `resourceVersion`, and the patch just bumped it.
                Ok(v) => cur = v,
                Err(kube::Error::Api(s)) if s.code == 409 || s.code == 422 => continue,
                Err(e) => {
                    tracing::warn!(volume = %name, error = %e, "sweep: releasing an unavailable owner's volume");
                    continue;
                }
            }
        }
        let prev = cur.status.clone().unwrap_or_default();
        let idle = prev.phase == crd::Phase::Unavailable
            && !release
            && prev.conditions.iter().any(|c| c.type_ == "Available" && c.reason == reason && c.message == why);
        if !idle {
            // The same re-read-on-409 loop `mark_parent_of` and `write_replica_status` use, and for
            // the same reason: this is a PUT carrying `resourceVersion`, and a lost race used to
            // just warn — leaving the volume `Available=True` for a dead owner until something else
            // happened to touch it.
            //
            // THREE attempts is enough only because the owner is no longer writing back: the
            // parent and volume reconcilers bail on `my_node` (see `controller::my_node`), so a
            // partitioned agent no longer rewrites this status every 15 s. Against that, no bound
            // would have been enough; against one-shot writers, three is plenty.
            for attempt in 0..3 {
                let mut st = cur.status.clone().unwrap_or_default();
                st.phase = crd::Phase::Unavailable;
                let gen = cur.metadata.generation.unwrap_or(0);
                // No `Released` reason: `Unavailable` with an empty pin IS released, and a third
                // word would restate the pin the object already carries.
                st.conditions = vec![crd::condition("Available", false, reason, &why, gen)];
                match replace_status(&api, &cur, "Volume", serde_json::to_value(st).expect("VolumeStatus serializes")).await {
                    Ok(()) => break,
                    Err(kube::Error::Api(s)) if s.code == 409 && attempt < 2 => match api.get(&name).await {
                        Ok(fresh) => cur = fresh,
                        Err(e) => {
                            tracing::warn!(volume = %name, error = %e, "sweep: re-read after conflict");
                            break;
                        }
                    },
                    Err(e) => {
                        tracing::warn!(volume = %name, error = %e, "sweep: marking an unavailable owner's volume");
                        break;
                    }
                }
            }
        }
        // Every parent on the volume carries the condition, whatever the verdict — that is how the
        // API answers "why will this not start". Last, because on a release the pin is already
        // clear: an un-placed parent is only safe once nothing owns the volume.
        //
        // `Degraded=True` only where something actually failed — the dead-node sweep, whose owner
        // really is gone. A drain (`mark_running: false`) only ever reaches here on a RELEASE, of a
        // volume whose every parent is stopped and replicated: nothing about that is degraded, and
        // writing the word would paint a healthy workspace red in the API and the web for a routine
        // retirement. `Placed=False` is the condition the claim itself owns, so the next node's
        // claim overwrites it — exactly as a spread's `Moving` does.
        let cond = if mark_running { ("Degraded", true) } else { ("Placed", false) };
        for p in &parents {
            mark_parent(ctx, p, cond, reason, &why, release).await;
        }
    }
}

/// Clear one parent's claim so the node the volume was just handed to can take it. The same write
/// the sweep's release arm makes, for the same reason — a parent left pointing at the old owner
/// would never be looked at by the new one.
/// `Placed=False`, not `Degraded`: a spread is a routine start-time decision, and writing the word
/// the dead-node sweep writes would make every healthy move look like a failure in the API and the
/// web. `Placed` is the condition the claim itself sets, so the next node's claim overwrites it.
pub(crate) async fn unplace_parent(ctx: &Arc<Ctx>, p: &crate::listing::Parent) {
    mark_parent(ctx, p, ("Placed", false), "Moving", "released so an up-to-date node can start it", true).await;
}

/// One parent's status write for the sweep: the condition always, `nodeName: ""` only on a
/// release. The same guarded primitive the claim uses (`replace_status`, a PUT carrying
/// `resourceVersion`, one re-read on a 409) — clearing a claim races the same way winning one does.
/// `cond` is the condition TYPE and its truth: the sweep says `Degraded=True`, a spread says
/// `Placed=False`. Both the idle check and `replaced` key by type, so one type never disturbs the
/// other's condition.
pub(crate) async fn mark_parent(ctx: &Arc<Ctx>, p: &crate::listing::Parent, cond: (&'static str, bool), reason: &str, why: &str, release: bool) {
    match p.kind {
        "Workspace" => mark_parent_of::<crd::Workspace>(ctx, &p.name, "Workspace", cond, reason, why, release).await,
        _ => mark_parent_of::<crd::Environment>(ctx, &p.name, "Environment", cond, reason, why, release).await,
    }
}

/// The generic half. Status is edited as JSON because `Workspace` and `Environment` share no
/// status type — the same reason `listing::Parent` exists at all.
#[allow(clippy::too_many_arguments)]
async fn mark_parent_of<K>(ctx: &Arc<Ctx>, name: &str, kind: &'static str, (cond_type, cond_status): (&'static str, bool), reason: &str, why: &str, release: bool)
where
    K: kube::Resource<DynamicType = ()> + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let api: Api<K> = Api::all(ctx.client.clone());
    let mut cur = match api.get_opt(name).await {
        Ok(Some(o)) => o,
        Ok(None) => return, // deleted between the listing and now: nothing to mark
        Err(e) => {
            tracing::warn!(%kind, %name, error = %e, "sweep: reading a parent to mark it");
            return;
        }
    };
    for attempt in 0..2 {
        let mut status = serde_json::to_value(&cur).unwrap_or_default()["status"].take();
        if status.is_null() {
            status = serde_json::json!({});
        }
        let gen = cur.meta().generation.unwrap_or(0);
        let prev: Vec<crd::Condition> = serde_json::from_value(status["conditions"].clone()).unwrap_or_default();
        let cond = crd::condition_since(prev.iter().find(|c| c.type_ == cond_type), cond_type, cond_status, reason, why, gen);
        // Idle when nothing changed: this runs on every beat of every node, and rewriting an
        // identical status per volume forever is churn the API server pays for.
        if !release && prev.iter().any(|c| c.type_ == cond_type && c.reason == cond.reason && c.message == cond.message) {
            return;
        }
        // Replaced by type, not `kept_conditions`: `Replicated` is what the next beat's second arm
        // reads, and dropping it here would make the volume look unreplicated forever.
        status["conditions"] =
            serde_json::to_value(crate::controller::replaced(&prev, cond)).expect("conditions serialize");
        if release {
            status["nodeName"] = serde_json::json!("");
        }
        match replace_status(&api, &cur, kind, status).await {
            Ok(()) => return,
            Err(kube::Error::Api(s)) if s.code == 409 && attempt == 0 => match api.get(name).await {
                Ok(fresh) => cur = fresh,
                Err(e) => {
                    tracing::warn!(%kind, %name, error = %e, "sweep: re-read after conflict");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!(%kind, %name, error = %e, "sweep: marking an unavailable node's parent");
                return;
            }
        }
    }
}

/// The dead half: the set of owners that are dead, handed to `sweep_volumes`. The parents come
/// from the beat's own listing, which is why the per-kind list-and-decide plumbing (`unclaim_kind`,
/// its `releasable` closures, and the `running_volumes` set threaded between it and the release
/// pass) is gone — the listing already knows every parent on every volume.
///
/// `node_is_dead`, deliberately NOT `unplaceable`: a decommissioning node is alive and its running
/// work keeps running. Task 11's decommission beat calls `sweep_volumes` itself, with its own set.
async fn sweep_dead_nodes(
    ctx: &Arc<Ctx>,
    beat: &crate::listing::Beat,
    nodes: &[Node],
    floor: i64,
    now: k8s_openapi::jiff::Timestamp,
) {
    let dead: HashSet<String> = beat
        .volumes
        .iter()
        .map(|v| v.spec.node_name.clone())
        .filter(|n| !n.is_empty() && node_is_dead(nodes.iter().find(|k| k.name_any() == *n), floor, now))
        .collect();
    sweep_volumes(ctx, beat, &dead, "NodeDead", true).await;
}

/// A copy whose rendezvous slot moved (a node joined, or a dead one came back) is not just
/// wasted disk: its stale Synced row still wins claims and satisfies stop's flush gate with
/// data that is no longer being pulled. It goes only once every CURRENT target is Synced, so a
/// spread never passes through a moment with fewer live copies than before. An unowned volume
/// is a dead node's mid-takeover: keep everything until someone owns it again. An EMPTY target
/// list is not "every target is synced" — it's this node itself missing from `live` (its own
/// Node object flapped NotReady while the agent kept running); `all()` is vacuously true on an
/// empty iterator, which would otherwise retire every copy on this node in one beat.
fn should_retire(me: &str, owner: &str, targets: &[String], hosted: bool, synced: &HashSet<String>) -> bool {
    !owner.is_empty()
        && owner != me
        && !hosted
        && !targets.is_empty()
        && !targets.iter().any(|t| t == me)
        && targets.iter().all(|t| synced.contains(t))
}

/// Directories under `{pool}/vol` that no listed Volume names. Files beside them (`{id}.owner`,
/// `{id}.lock`) are not volumes and are cleaned with their directory by `cleanup_local`.
fn orphan_voldirs(vol_root: &std::path::Path, known: &HashSet<String>) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(vol_root) else { return Vec::new() };
    let mut out: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !known.contains(n))
        .collect();
    out.sort();
    out
}

/// `Snapshot` CRs whose `spec.volume` names no Volume at all. Every snapshot minted today carries
/// an ownerReference — to its parent (`api.rs`, `sync.rs`) or, since `migrate_and_seed_baseline`
/// gained one, to its Volume — so Kubernetes GC is the real answer; this sweep is for the records
/// already out there (13 on the cluster), and the backstop for any future path that forgets one.
///
/// Keep-biased twice over. `known` comes from the beat's Volume list, which is the only reason this
/// pass runs at all — a failed one bails before here — but that list and this one are separate
/// round trips, so a Volume created between them looks absent while its brand-new baseline does
/// not. One fresh GET per candidate, right before the delete, closes that window, exactly as the
/// stale-worktree drop below does; a failed GET keeps the snapshot. An unlistable snapshot set
/// deletes nothing — `retire_pass` makes that ONE listing and does not call this at all when it
/// fails.
///
/// Every node runs this and no node owns it: the race is three DELETEs for one object, of which two
/// answer 404, which this already tolerates. Electing one node (rendezvous over `live`) would buy
/// nothing an idempotent delete does not already give.
async fn sweep_orphan_snapshots(ctx: &Arc<Ctx>, known: &HashSet<String>, snapshots: &[crd::Snapshot]) {
    let api = Api::<crd::Snapshot>::all(ctx.client.clone());
    for s in snapshots.iter().filter(|s| !known.contains(&s.spec.volume)) {
        if !matches!(Api::<crd::Volume>::all(ctx.client.clone()).get_opt(&s.spec.volume).await, Ok(None)) {
            continue;
        }
        let name = s.name_any();
        match api.delete(&name, &Default::default()).await {
            Ok(_) => tracing::info!(volume = %s.spec.volume, snapshot = %name, "pull: retire: no Volume CR; dropping the orphaned snapshot"),
            Err(e) if matches!(&e, kube::Error::Api(st) if st.code == 404) => {}
            Err(e) => tracing::warn!(snapshot = %name, error = %e, "pull: retire: deleting an orphaned snapshot"),
        }
    }
}

/// The names under `snap/` that no `Snapshot` record claims. Pure so the keep rules are testable
/// without btrfs: a record in ANY phase keeps its directory — a `Working` cut is a receive in
/// flight, and deleting under it loses the bytes it is still writing.
fn orphan_snaps(local: &[String], records: &HashSet<String>) -> Vec<String> {
    local.iter().filter(|n| !records.contains(*n)).cloned().collect()
}

/// The BYTE half of "an explicit delete is the only way a snapshot dies": a `snap/<name>`
/// subvolume whose record is gone has nothing left that could ever check it out, and the pull
/// beat's own retire (`retired`) only visits volumes this node is still pulling — a pinned
/// snapshot's volume outlives its workspace and is not one of them.
///
/// Keep-biased throughout: only volumes whose bytes are actually here (a voldir), never one
/// mid-delete, and a per-volume listing error skips that volume rather than guessing it empty.
/// Returns what it DECIDED to drop — the decision, not the btrfs outcome, is what a test on a
/// machine without btrfs can read, and a failed delete is retried by the next beat anyway.
///
/// ponytail: one full snap listing per held volume per beat; index records by name if a volume
/// ever grows past thousands of commits.
fn sweep_orphan_snap_bytes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, snapshots: &[crd::Snapshot]) -> Vec<(String, String)> {
    let mut dropped = Vec::new();
    for v in &beat.volumes {
        let id = v.name_any();
        // A volume being deleted takes its whole voldir with it (`cleanup_local`); racing that
        // with per-commit deletes buys nothing.
        if v.metadata.deletion_timestamp.is_some() || !ctx.engine.pool.voldir(&id).exists() {
            continue;
        }
        let local = match ctx.engine.local_commits(&id) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(volume = %id, error = %e, "pull: retire: listing local commits; skipping this volume");
                continue;
            }
        };
        let records: HashSet<String> = snapshots.iter().filter(|s| s.spec.volume == id).map(|s| s.name_any()).collect();
        for name in orphan_snaps(&local, &records) {
            tracing::info!(volume = %id, snapshot = %name, "pull: retire: no Snapshot CR; dropping the orphaned commit bytes");
            if let Err(e) = ctx.engine.drop_commit(&id, &name) {
                tracing::warn!(volume = %id, snapshot = %name, error = %e, "pull: retire: dropping orphaned commit bytes; left for the next pass");
            }
            dropped.push((id.clone(), name));
        }
    }
    dropped
}

/// Drops this node's copy of any volume whose rendezvous slot over `live` no longer names it —
/// see `should_retire`. Runs at the end of `pull_beat_with`, after the pull loop, so a new
/// target's pull lands before anyone retires the copy it just replaced.
async fn retire_pass(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, live: &[String]) {
    let vols = &beat.volumes;
    let rows = &beat.replicas;
    let hosted = beat.hosted_volumes();
    // A local voldir with no Volume CR at all is an orphan: nothing lists it, so no pull, no
    // retire and no worktree drop ever visits it again. The Volume is always created before any
    // node makes its directory (the parent's reconciler creates the CR, the pull beat only pulls
    // listed volumes), so "no CR" is never "not yet", only "gone". A CR mid-deletion still counts
    // as present — garbage collection finishes on its own and the next beat sees it absent.
    let known: HashSet<String> = vols.iter().map(|v| v.name_any()).collect();
    for id in orphan_voldirs(&ctx.engine.pool.root.join("vol"), &known) {
        tracing::info!(volume = %id, "pull: retire: no Volume CR; dropping the orphaned local copy");
        janitor::cleanup_local(&ctx.engine, &id);
    }
    // The row half of the same orphan. `retire_pass` only ever visits LISTED volumes, so a
    // `VolumeReplica` whose Volume is gone was never revisited by anything: it outlived the
    // workspace, and its stale `Synced` still satisfies a stop's flush gate and wins claims.
    // ponytail: a sweep, not an ownerReference on the row — the sweep has to exist anyway for the
    // rows already out there, and `write_replica_status` has no Volume UID to hand without a GET
    // per row it creates. Stamp the ownerReference at creation if row garbage ever outgrows this.
    for r in beat.replicas.iter().filter(|r| r.spec.node == ctx.node && !known.contains(&r.spec.volume)) {
        let rname = r.name_any();
        tracing::info!(volume = %r.spec.volume, row = %rname, "pull: retire: no Volume CR; dropping my orphaned replica row");
        if let Err(e) = Api::<crd::VolumeReplica>::all(ctx.client.clone()).delete(&rname, &Default::default()).await {
            if !matches!(&e, kube::Error::Api(s) if s.code == 404) {
                tracing::warn!(row = %rname, error = %e, "pull: retire: deleting my orphaned replica row");
            }
        }
    }
    // ONE Snapshot listing for both record-side and byte-side sweeps: each is cluster-wide and
    // neither may act on a partial view, so a failure skips both rather than deleting on absence.
    match Api::<crd::Snapshot>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => {
            sweep_orphan_snapshots(ctx, &known, &l.items).await;
            sweep_orphan_snap_bytes(ctx, beat, &l.items);
        }
        Err(e) => tracing::warn!(error = %e, "pull: retire: listing snapshots; sweeping none"),
    }
    for v in vols {
        let id = v.name_any();
        if v.metadata.deletion_timestamp.is_some() || !ctx.engine.pool.voldir(&id).exists() {
            continue;
        }
        let owner_alive = live.iter().any(|n| n == &v.spec.node_name);
        let targets = replicate::targets(&id, &v.spec.node_name, live, standby_count(owner_alive, v.spec.replicas));
        let synced: HashSet<String> = rows
            .iter()
            .filter(|r| r.spec.volume == id && r.status.as_ref().is_some_and(|s| s.phase == "Synced"))
            .map(|r| r.spec.node.clone())
            .collect();
        if !should_retire(&ctx.node, &v.spec.node_name, &targets, hosted.contains(&id), &synced) {
            // Still a target/replica, just not the owner: a `live/{ws}` worktree under it
            // belongs only to the owner and is what a takeover away from this node left behind
            // — UNLESS this node is `hosted` (serving a pod from it right now): the owner record
            // can lag a pod that's actually running here, and deleting a live worktree out from
            // under a running pod is the one thing this pass must never do.
            if !hosted.contains(&id) {
                // `v.spec.node_name` is from `beat.volumes`, listed before the pull loop ran; a
                // takeover landing in that window makes it stale, and against a stale owner this
                // would delete the worktree this node just created for itself. One fresh GET,
                // right before the delete, catches that race; a failed GET keeps everything.
                // Keep-bias: a failed GET, like `mine`, skips the drop rather than risking one
                // against a node name that may already be stale.
                if let Ok(Some(fresh)) = Api::<crd::Volume>::all(ctx.client.clone()).get_opt(&id).await {
                    if fresh.spec.node_name != ctx.node {
                        let dropped = janitor::drop_stale_worktrees(&ctx.engine, &id, &v.spec.node_name, &ctx.node);
                        if dropped > 0 {
                            tracing::info!(volume = %id, dropped, "pull: dropped stale live worktree(s) left by a takeover");
                        }
                    }
                }
            }
            continue;
        }
        let rname = crd::replica_name(&id, &ctx.node);
        if let Err(e) = Api::<crd::VolumeReplica>::all(ctx.client.clone()).delete(&rname, &Default::default()).await {
            if !matches!(&e, kube::Error::Api(s) if s.code == 404) {
                tracing::warn!(volume = %id, error = %e, "pull: retire: deleting my replica row; keeping the copy");
                continue; // row first, copy second: a copy without a row is harmless, a row without a copy is a lie
            }
        }
        janitor::cleanup_local(&ctx.engine, &id);
        tracing::info!(volume = %id, "pull: retire: slot moved elsewhere, copy dropped");
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
    use rustic_git_workspaces::kube_test::{get, mock_client, not_found, Recorder, Route};
    use std::os::unix::fs::PermissionsExt;

    struct NoopNix;
    #[async_trait::async_trait]
    impl crate::nix::Nix for NoopNix {
        async fn build(&self, _expr: &str, _timeout: std::time::Duration) -> Result<std::path::PathBuf, String> {
            Ok(std::path::PathBuf::from("/tmp"))
        }
        async fn ping(&self) -> Result<(), String> {
            Ok(())
        }
        async fn collect_garbage(&self) -> Result<u64, String> {
            Ok(0)
        }
    }

    fn test_ctx(pool: &std::path::Path, node: &str, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
        let (client, rec) = mock_client(routes);
        let engine = Engine::new(EnginePool::new(pool));
        std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
        (
            Arc::new(Ctx::new(
                client,
                Arc::new(engine),
                node.into(),
                pool.to_string_lossy().into(),
                "r1".into(),
                vec![],
                Some("test:/".into()),
                Arc::new(NoopNix),
                pool.join("profiles"),
            )),
            rec,
        )
    }

    // -----------------------------------------------------------------------------------------
    // The pull side: `pull_beat`, `pull_volume`, `reap_dead_replicas`.
    // -----------------------------------------------------------------------------------------

    const SNAPSHOTS: &str = "/apis/rustic-git.io/v1alpha1/snapshots";
    const VOLREPLICAS: &str = "/apis/rustic-git.io/v1alpha1/volumereplicas";
    const NODES: &str = "/api/v1/nodes";
    const VOLUMES: &str = "/apis/rustic-git.io/v1alpha1/volumes";
    const WORKSPACES: &str = "/apis/rustic-git.io/v1alpha1/workspaces";
    const ENVIRONMENTS: &str = "/apis/rustic-git.io/v1alpha1/environments";

    fn ready_snapshot(name: &str, volume: &str, parent: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1",
            "kind": "Snapshot",
            "metadata": {"name": name, "uid": "snap-uid"},
            "spec": {"volume": volume, "owner": "alice", "worktree": "ws-1", "parent": parent, "pinned": false},
            "status": {"phase": "ready"},
        })
    }

    fn node_json(name: &str, ready: &str, transitioned_at: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": name},
            "status": {"conditions": [{"type": "Ready", "status": ready, "lastTransitionTime": transitioned_at}]},
        })
    }

    fn node_ready_obj(name: &str) -> Node {
        serde_json::from_value(node_json(name, "True", "2000-01-01T00:00:00Z")).unwrap()
    }

    fn node_dead_obj(name: &str, transitioned_at: &str) -> Node {
        serde_json::from_value(node_json(name, "False", transitioned_at)).unwrap()
    }

    fn list_of(kind: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"apiVersion": "v1", "kind": format!("{kind}List"), "items": items})
    }

    /// The per-beat listing, built inline: these tests exercise what each consumer DECIDES from a
    /// beat, not how the beat is listed — `listing.rs` owns that half.
    fn beat_of(
        volumes: Vec<serde_json::Value>,
        replicas: Vec<serde_json::Value>,
        parents: Vec<(&'static str, &str, &str)>,
    ) -> crate::listing::Beat {
        let parents: Vec<crate::listing::Parent> = parents
            .into_iter()
            .map(|(kind, name, volume)| parent_at(kind, name, volume, crd::Phase::Ready, false))
            .collect();
        crate::listing::Beat {
            volumes: volumes.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect(),
            replicas: replicas.into_iter().map(|r| serde_json::from_value(r).unwrap()).collect(),
            parents: parents.clone(),
            all_parents: parents,
        }
    }

    /// One `listing::Parent` as the sweep reads it: the phase and the `Replicated` answer are the
    /// only two facts the three arms turn on.
    fn parent_at(kind: &'static str, name: &str, volume: &str, phase: crd::Phase, replicated: bool) -> crate::listing::Parent {
        crate::listing::Parent {
            kind,
            name: name.into(),
            volume: volume.into(),
            owner: "alice".into(),
            node_name: "node-b".into(),
            head: None,
            phase,
            pod_ref: (kind == "Workspace").then(|| format!("ws-alice/{name}")),
            owner_ref: Default::default(),
            replicated,
            state: crd::SnapshotState::Workspace {
                image: "alpine:3.20".into(),
                packages: vec![],
                resources: Default::default(),
                quota_gb: 5,
                attached_environment: None,
            },
        }
    }

    fn replica_of(volume: &str, node: &str, phase: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": format!("{volume}.{node}"), "uid": format!("uid-{node}")},
            "spec": {"volume": volume, "node": node},
            "status": {"phase": phase, "branches": {}},
        })
    }

    /// A `Snapshot`-list error must keep every local commit untouched and write no replica
    /// status — the same keep-biased rule `replica_reconcile`'s lookup-error branch follows.
    #[tokio::test]
    async fn pull_volume_keeps_everything_on_a_snapshot_list_error() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route { method: "GET", path: SNAPSHOTS.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let http = peer_http_client().unwrap();

        pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1", &[]).await;

        assert!(rec.calls().iter().all(|c| !c.contains("volumereplicas")), "a snapshot-list error must never reach the replica write");
    }

    /// H2: creating this node's `VolumeReplica` for the first time stamps `rustic-git.io/volume`
    /// — the e2e (`tests/ws_e2e.sh`) selects replicas by exactly that label, and nothing else in
    /// this codebase writes a `VolumeReplica`.
    #[tokio::test]
    async fn write_replica_status_stamps_the_volume_label_on_create() {
        let tmp = tempfile::tempdir().unwrap();
        let name = crd::replica_name("vol-1", "node-b");
        let routes = vec![
            not_found(format!("{VOLREPLICAS}/{name}")),
            Route {
                method: "POST",
                path: VOLREPLICAS.into(),
                status: 201,
                body: serde_json::json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                    "metadata": {"name": name, "uid": "vr-uid"},
                    "spec": {"volume": "vol-1", "node": "node-b"},
                }),
            },
            Route { method: "PUT", path: format!("{VOLREPLICAS}/{name}/status"), status: 200, body: serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica", "metadata": {"name": name}, "spec": {"volume": "vol-1", "node": "node-b"}, "status": {"phase": "Synced", "branches": {}}}) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);

        write_replica_status(&ctx, "vol-1", true, Default::default()).await.unwrap();

        let created = rec.sent("POST", VOLREPLICAS);
        assert_eq!(created.len(), 1, "{:?}", rec.calls());
        assert_eq!(created[0]["metadata"]["labels"]["rustic-git.io/volume"], "vol-1");
    }

    /// Nothing missing (every Ready `Snapshot` is already a local commit): `pull_volume` makes no
    /// network pull at all and writes its own `VolumeReplica` as `Synced` — v1's branches: this
    /// task writes `branches: {}` and phase only (see the brief's allowed shortcut), Task 4 fills
    /// in the per-branch heads.
    #[tokio::test]
    async fn a_clean_pull_with_nothing_missing_writes_synced() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-aaaaaaaa")).unwrap();

        let created = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": "vol-1.node-b", "uid": "vr-uid"},
            "spec": {"volume": "vol-1", "node": "node-b"},
            "status": {"phase": "Syncing", "branches": {}},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![ready_snapshot("vol-1-aaaaaaaa", "vol-1", "")]) },
            not_found(format!("{VOLREPLICAS}/vol-1.node-b")),
            Route { method: "POST", path: VOLREPLICAS.into(), status: 201, body: created.clone() },
            Route { method: "PUT", path: format!("{VOLREPLICAS}/vol-1.node-b/status"), status: 200, body: created },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let http = peer_http_client().unwrap();

        pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1", &[]).await;

        assert!(rec.calls().iter().all(|c| !c.contains("/peer/v1/commit/")), "nothing missing: no GET should ever be issued");
        let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
        assert_eq!(sent.len(), 1, "exactly one replica status write");
        assert_eq!(sent[0]["status"]["phase"], "Synced");
    }

    /// A `Snapshot` CR that has been deleted (absent from the volume's list entirely, every
    /// phase) is exactly what `retired` picks out — the "least new machinery" this task's
    /// deletion handling uses: no finalizer on the new `Snapshot` kind, so this diff against
    /// `local_commits` is the only place any node ever notices the CR is gone. `drop_commit`
    /// itself is real btrfs and is `pull_volume`'s only caller of it — covered end to end by
    /// `engine_commit.rs`'s loopback tests, not repeated here.
    #[test]
    fn retired_picks_out_locals_whose_cr_is_gone() {
        let have: HashSet<String> = ["a".into(), "b".into(), "c".into()].into_iter().collect();
        let existing: HashSet<String> = ["a".into(), "c".into()].into_iter().collect();
        assert_eq!(retired(&have, &existing, false), vec!["b".to_string()]);
        assert!(retired(&have, &have, false).is_empty(), "nothing missing: nothing retired");
    }

    /// C2: a pass that could not pull something reclaims NOTHING. The owner deletes `sync-A`'s CR
    /// the moment `sync-B` is Ready, so a replica that cannot reach the owner this pass (a
    /// partition, a peer 500, a `send_timeout`) would drop its only local sync point and gain no
    /// replacement — one sync point to zero, in the exact case the feature exists for.
    #[test]
    fn a_failed_pull_reclaims_nothing_this_pass() {
        let have: HashSet<String> = ["sync-A".into()].into_iter().collect();
        // `sync-A`'s CR is gone (retention deleted it when `sync-B` turned Ready); `sync-B` is the
        // one this pass failed to fetch, so it is not in `have`.
        let existing: HashSet<String> = ["sync-B".into()].into_iter().collect();
        assert!(retired(&have, &existing, true).is_empty(), "a failed pull must not drop the last sync point");
        assert_eq!(retired(&have, &existing, false), vec!["sync-A".to_string()], "a clean pass still reclaims it");
    }

    // These two tests each spin up a real peer server on the fixed `:8444` production port
    // (`agent_pod_addr` hard-codes it, so there's no way around binding it for real) — serialized
    // so they never race each other for the port when the harness runs them concurrently.
    // An async mutex on purpose: the guard is held across the test's awaits (that is the point —
    // the fixed port stays taken for the whole body), and a std guard across an await is a lint.
    fn peer_port_lock() -> Arc<tokio::sync::Mutex<()>> {
        static LOCK: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }

    /// The port lock and the listener as one thing, because holding the lock is NOT enough on its
    /// own: a finished test's server task lives until its runtime is dropped, which happens after
    /// the guard is released, and `bind` sets `SO_REUSEADDR` — so the next test bound `:8444`
    /// successfully and the kernel kept handing connections to the stale listener. `stop()` closes
    /// this one before the guard goes.
    struct PeerServer {
        stop: tokio::sync::oneshot::Sender<()>,
        task: tokio::task::JoinHandle<()>,
        _guard: OwnedMutexGuard<()>,
    }

    impl PeerServer {
        async fn stop(self) {
            let _ = self.stop.send(());
            let _ = self.task.await;
        }
    }

    /// Serve `app` on the fixed production port, exclusively.
    async fn serve_on_the_peer_port(app: Router) -> PeerServer {
        let guard = peer_port_lock().lock_owned().await;
        let listener = std::net::TcpListener::bind("127.0.0.1:8444").unwrap();
        listener.set_nonblocking(true).unwrap();
        let tokio_listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        // Select rather than `with_graceful_shutdown`: a pooled keep-alive connection the test
        // still holds would keep a graceful shutdown waiting forever.
        let task = tokio::spawn(async move {
            tokio::select! {
                r = axum::serve(tokio_listener, app) => { let _ = r; }
                _ = rx => {}
            }
        });
        PeerServer { stop, task, _guard: guard }
    }

    /// An incremental receive whose `-p` the source never had (this node's nearest held ancestor
    /// is not necessarily one the SOURCE holds too) must not lose the commit forever: after the
    /// first attempt fails, `pull_one` is retried against the SAME source with no parent at all
    /// before moving on. The fake `btrfs receive` fails call 1 (truncated body, standing in for
    /// the source's own `-p` failure surfacing as an incomplete stream) and succeeds call 2.
    #[tokio::test]
    async fn an_incremental_pull_that_fails_falls_back_to_a_full_pull_from_the_same_source() {
        let tmp = tempfile::tempdir().unwrap();
        // I already hold "vol-1-parent" locally — so `my_parent` is `Some`, and the first GET
        // carries `?parent=vol-1-parent`.
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-parent")).unwrap();

        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let seq = bin_dir.join("seq");
        let bin = bin_dir.join("btrfs");
        std::fs::write(
            &bin,
            format!(
                r#"#!/bin/sh
if [ "$1" = "receive" ]; then
    n=$(( $(cat "{seq}" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "{seq}"
    cat >/dev/null
    if [ "$n" = "1" ]; then
        exit 1
    fi
    mkdir -p "$2/vol-1-child"
    exit 0
fi
"#,
                seq = seq.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = bin.to_string_lossy().into_owned();

        // The peer server: a real `commit` endpoint, so `pull_one` exercises the actual HTTP
        // round trip rather than a canned kube-mock response. Its own fake `btrfs send` just
        // needs to produce SOME bytes — the receive side is what decides success or failure here.
        let send_bin = bin_dir.join("btrfs-send");
        std::fs::write(&send_bin, "#!/bin/sh\nprintf 'bytes'\nexit 0\n").unwrap();
        std::fs::set_permissions(&send_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let send_bin = send_bin.to_string_lossy().into_owned();
        let source_pool = tmp.path().join("source-pool");
        std::fs::create_dir_all(source_pool.join("vol/vol-1/snap/vol-1-child")).unwrap();
        let (client, _rec) = mock_client(vec![]);
        let peer_state = PeerState::new(client, source_pool.to_string_lossy().into(), "node-a".into(), "s3cret".into(), send_bin);
        // `agent_pod_addr` hard-codes `:8444` (the peer listener's fixed port in production), so
        // the fake source server must actually listen there for this end-to-end test to reach it.
        let peer_server = serve_on_the_peer_port(router(peer_state)).await;

        let pod = serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "agent-a"},
            "status": {"podIP": "127.0.0.1"},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![ready_snapshot("vol-1-child", "vol-1", "vol-1-parent")]) },
            Route { method: "GET", path: "/api/v1/namespaces/kube-system/pods".into(), status: 200, body: list_of("Pod", vec![pod]) },
            not_found(format!("{VOLREPLICAS}/vol-1.node-b")),
            Route { method: "POST", path: VOLREPLICAS.into(), status: 201, body: serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                "metadata": {"name": "vol-1.node-b", "uid": "vr-b"},
                "spec": {"volume": "vol-1", "node": "node-b"},
                "status": {"phase": "Syncing", "branches": {}},
            }) },
            Route { method: "PUT", path: format!("{VOLREPLICAS}/vol-1.node-b/status"), status: 200, body: serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                "metadata": {"name": "vol-1.node-b", "uid": "vr-b"},
                "spec": {"volume": "vol-1", "node": "node-b"},
                "status": {"phase": "Synced", "branches": {}},
            }) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let http = peer_http_client().unwrap();

        pull_volume(&ctx, &beat_of(vec![], vec![replica_of("vol-1", "node-a", "Synced")], vec![]), &bin, &http, "s3cret", "vol-1", &[]).await;

        assert!(tmp.path().join("vol/vol-1/snap/vol-1-child").exists(), "the full-pull retry must land the commit");
        let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["status"]["phase"], "Synced", "the fallback succeeded: nothing is missing any more");
        // Before the guard: see `PeerServer`.
        peer_server.stop().await;
    }

    /// F4 (drill, 2026-09-03): with no replica row for the owner (a fresh volume, or a row the
    /// reaper took) the first standby had an EMPTY source list and could never fetch a thing. The
    /// owner is a source of last resort — and only while it is live, so a genuinely dead owner
    /// costs no failed dial per commit per pass.
    #[tokio::test]
    async fn the_owner_is_a_last_resort_source_only_while_it_is_live() {
        let routes = || {
            vec![
                Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![ready_snapshot("v1-aaaa", "v1", "")]) },
                Route { method: "GET", path: "/api/v1/namespaces/kube-system/pods".into(), status: 200, body: list_of("Pod", vec![]) },
                not_found(format!("{VOLREPLICAS}/v1.node-b")),
                Route { method: "POST", path: VOLREPLICAS.into(), status: 201, body: replica_of("v1", "node-b", "Syncing") },
                Route { method: "PUT", path: format!("{VOLREPLICAS}/v1.node-b/status"), status: 200, body: replica_of("v1", "node-b", "Syncing") },
            ]
        };
        let http = peer_http_client().unwrap();
        let beat = beat_of(vec![vol_owned("v1", "node-a")], vec![], vec![]);
        let pod_lists = |rec: &Recorder| rec.calls().iter().filter(|c| c.as_str() == "GET /api/v1/namespaces/kube-system/pods").count();

        let live = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(live.path(), "node-b", routes());
        let missed = pull_volume(&ctx, &beat, "btrfs", &http, "s3cret", "v1", &["node-a".to_string()]).await;
        assert_eq!(pod_lists(&rec), 1, "the live owner is tried: {:?}", rec.calls());
        assert!(missed, "the commit did not land, so the pass asks for a retry");

        let dead = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(dead.path(), "node-b", routes());
        pull_volume(&ctx, &beat, "btrfs", &http, "s3cret", "v1", &[]).await;
        assert_eq!(pod_lists(&rec), 0, "a dead owner is not dialled at all: {:?}", rec.calls());
    }

    /// Catching up on three commits from one source resolves that source's pod address ONCE, not
    /// once per commit: a full namespaced pod list with two selectors is not a per-commit cost.
    #[tokio::test]
    async fn pull_volume_resolves_a_source_address_once_per_pass() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v1/snap")).unwrap();
        let snaps = vec![
            ready_snapshot("v1-aaaa", "v1", ""),
            ready_snapshot("v1-bbbb", "v1", "v1-aaaa"),
            ready_snapshot("v1-cccc", "v1", "v1-bbbb"),
        ];
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", snaps) },
            Route { method: "GET", path: "/api/v1/namespaces/kube-system/pods".into(), status: 200, body: list_of("Pod", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let http = peer_http_client().unwrap();

        pull_volume(&ctx, &beat_of(vec![], vec![replica_of("v1", "node-b", "Synced")], vec![]), "btrfs", &http, "s3cret", "v1", &[]).await;

        let pod_lists = rec.calls().iter().filter(|c| c.as_str() == "GET /api/v1/namespaces/kube-system/pods").count();
        assert_eq!(pod_lists, 1, "one address lookup per source per pass, not per commit: {:?}", rec.calls());
    }

    /// The owner of a STOPPED volume (no pod, so no Workspace/Environment names it in
    /// `status.nodeName`) must still be counted as interested in its own volume: it's the only
    /// source the first standby has, and `targets()` itself excludes the owner from its own
    /// output.
    #[tokio::test]
    async fn a_volumes_owner_is_always_interesting_even_with_nothing_running() {
        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "vol-1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-b", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        });
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, _rec) = test_ctx(tmp.path(), "node-b", Vec::new());
        let live = vec!["node-b".to_string()];

        let ids = interesting_volumes(&ctx, &beat_of(vec![volume], vec![], vec![]), &live);

        assert_eq!(ids, vec!["vol-1".to_string()], "a volume this node owns is always interesting, running or not");
    }

    /// Rendezvous over the FULL pool keeps electing a corpse forever — `live_nodes` must drop a
    /// dead node (`node-b`) and a node with no `Node` object at all (`node-c`).
    #[test]
    fn dead_nodes_leave_the_candidate_list() {
        let now = k8s_openapi::jiff::Timestamp::now();
        let pool = vec!["node-a".to_string(), "node-b".to_string(), "node-c".to_string()];
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)]; // node-c: no Node object at all
        assert_eq!(live_nodes(&pool, &nodes, 600, now), vec!["node-a".to_string()]);
    }

    /// `targets()` hands back `total - 1` standbys, counting the owner as one of `total`. A dead
    /// owner holds nothing reachable, so it isn't a copy: one more standby is asked for.
    #[test]
    fn a_dead_owner_is_not_a_copy() {
        assert_eq!(standby_count(true, 2), 2, "targets() subtracts the owner itself");
        assert_eq!(standby_count(false, 2), 3, "one more standby replaces the dead owner");
        assert_eq!(standby_count(false, 1), 2);
    }

    fn node_decommissioning(name: &str) -> Node {
        let mut n = node_ready_obj(name);
        n.metadata.labels.get_or_insert_with(Default::default).insert(crd::DECOMMISSION_LABEL.into(), "true".into());
        n
    }

    /// Dead and decommissioning are the SAME thing to placement, and nothing downstream is allowed
    /// to tell them apart: one predicate, or the sweep and the rendezvous eventually disagree
    /// about whether a node is a place to run and a volume ends up owned by nobody.
    #[test]
    fn decommissioning_is_unplaceable_but_not_dead() {
        let now = k8s_openapi::jiff::Timestamp::now();
        let floor = 180;
        let leaving = node_decommissioning("node-b");
        assert!(unplaceable(Some(&leaving), floor, now), "a decommissioning node takes no new work");
        assert!(!node_is_dead(Some(&leaving), floor, now), "but it is alive: its rows are not reaped and it still serves pulls");
        assert!(unplaceable(Some(&node_dead_obj("node-c", "2000-01-01T00:00:00Z")), floor, now));
        assert!(unplaceable(None, floor, now), "absent from a positive listing is unplaceable");
        assert!(!unplaceable(Some(&node_ready_obj("node-a")), floor, now));
    }

    /// A label value other than exactly "true" is not a decommission: a half-typed `kubectl label`
    /// must not silently drain a node.
    #[test]
    fn only_the_exact_true_value_decommissions() {
        let mut n = node_ready_obj("node-b");
        n.metadata.labels.get_or_insert_with(Default::default).insert(crd::DECOMMISSION_LABEL.into(), "yes".into());
        assert!(!decommissioning(Some(&n)));
        assert!(!decommissioning(Some(&node_ready_obj("node-a"))));
        assert!(decommissioning(Some(&node_decommissioning("node-b"))));
    }

    /// Rendezvous must stop naming a decommissioning node, or its copies never re-home: the whole
    /// "copies settle on their own" half of a drain is this one line.
    #[test]
    fn a_decommissioning_node_leaves_the_candidate_list_and_is_not_a_copy() {
        let pool: Vec<String> = ["node-a", "node-b", "node-c"].iter().map(|s| s.to_string()).collect();
        let nodes = vec![node_ready_obj("node-a"), node_decommissioning("node-b"), node_ready_obj("node-c")];
        let live = live_nodes(&pool, &nodes, 180, k8s_openapi::jiff::Timestamp::now());
        assert_eq!(live, vec!["node-a".to_string(), "node-c".to_string()]);
        // A decommissioning OWNER is not a copy either, so the volume asks for one standby more
        // and rendezvous places the replacement while the original is still serving pulls.
        assert_eq!(standby_count(false, 2), 3);
        assert_eq!(standby_count(true, 2), 2);
    }

    /// `v2` is picked so that rendezvous over the FULL pool elects `node-b` (dead, or here simply
    /// absent from the live list) as the standby for owner `node-a`: `targets("v2", "node-a",
    /// [node-a, node-b, node-c], 2) == ["node-b"]`. Over the live-only candidate list a third node,
    /// `node-c`, is the only standby left to pick — proving placement heals onto it rather than
    /// sitting one copy short forever.
    #[tokio::test]
    async fn a_third_node_finds_a_dead_standbys_volume_interesting() {
        assert_eq!(replicate::targets("v2", "node-a", &["node-a".into(), "node-b".into(), "node-c".into()], 2), vec!["node-b".to_string()]);

        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v2"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        });
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, _rec) = test_ctx(tmp.path(), "node-c", Vec::new());
        let live = vec!["node-a".to_string(), "node-c".to_string()];

        assert_eq!(interesting_volumes(&ctx, &beat_of(vec![volume], vec![], vec![]), &live), vec!["v2".to_string()]);
    }

    /// `replicas: 1` return path: the reaper deleted this node's replica row while it was dead,
    /// so rendezvous over the live pool elects someone else and no source exists anywhere. Holding
    /// the copy on disk is what makes the volume interesting again, and re-registers the row.
    #[tokio::test]
    async fn a_node_holding_the_only_copy_finds_it_interesting() {
        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v0"},
            "spec": {"owner": "alice", "team": "", "nodeName": "", "region": "r1", "quotaGb": 5, "replicas": 1},
            "status": {"phase": "unavailable"},
        });
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, _rec) = test_ctx(tmp.path(), "node-c", Vec::new());
        let live = vec!["node-a".to_string(), "node-b".to_string(), "node-c".to_string()];
        let beat = beat_of(vec![volume], vec![], vec![]);
        assert!(interesting_volumes(&ctx, &beat, &live).is_empty(), "no local copy: nothing to do");

        std::fs::create_dir_all(ctx.engine.pool.voldir("v0")).unwrap();
        assert_eq!(interesting_volumes(&ctx, &beat, &live), vec!["v0".to_string()]);
    }

    /// A Workspace list error hides every parent, so the sweep would read every volume as
    /// "nothing on it" and release the lot. The listing is `None` and the whole beat stops before
    /// a single Volume is even listed.
    #[tokio::test]
    async fn a_parent_list_error_sweeps_no_volume() {
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_json("node-b", "False", "2000-01-01T00:00:00Z")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-1", "node-b")]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);

        pull_beat_with(&ctx, "btrfs", "s3cret").await;

        assert!(
            !rec.calls().iter().any(|c| c.starts_with("PUT") || c.starts_with("PATCH")),
            "a partial listing moves nothing: {:?}", rec.calls()
        );
    }

    /// The reaper: a node absent from a list we DID get, or Ready=false past the age floor, is
    /// reaped; a node Ready=false but young is kept, and so is a node present with NO readable
    /// `Ready` condition at all — positive evidence only, in both directions.
    #[tokio::test]
    async fn reaper_deletes_dead_keeps_young_keeps_absent_condition() {
        let old = "2000-01-01T00:00:00Z";
        let young = chrono::Utc::now().to_rfc3339();
        let no_ready_condition: Node = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": "node-e"},
            "status": {"conditions": []},
        }))
        .unwrap();
        let nodes: Vec<Node> = vec![
            serde_json::from_value(node_json("node-a", "True", old)).unwrap(),
            serde_json::from_value(node_json("node-b", "False", old)).unwrap(),
            serde_json::from_value(node_json("node-c", "False", &young)).unwrap(),
            no_ready_condition,
        ];

        let replica = |node: &str| {
            serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                "metadata": {"name": format!("vol-1.{node}"), "uid": format!("uid-{node}")},
                "spec": {"volume": "vol-1", "node": node},
                "status": {"phase": "Synced", "branches": {}},
            })
        };
        // node-d: absent from the node list entirely. node-e: present, but with no `Ready`
        // condition reported yet — the API server just hasn't converged it, not a fact about
        // liveness.
        let replica_rows = vec![replica("node-a"), replica("node-b"), replica("node-c"), replica("node-d"), replica("node-e")];

        let routes = vec![
            Route { method: "DELETE", path: format!("{VOLREPLICAS}/vol-1.node-b"), status: 200, body: serde_json::json!({}) },
            Route { method: "DELETE", path: format!("{VOLREPLICAS}/vol-1.node-d"), status: 200, body: serde_json::json!({}) },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        reap_dead_replicas(&ctx, &beat_of(vec![], replica_rows, vec![]), &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        let deletes: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes.len(), 2, "{deletes:?}");
        assert!(deletes.iter().any(|c| c.ends_with("vol-1.node-b")), "old NotReady node reaped");
        assert!(deletes.iter().any(|c| c.ends_with("vol-1.node-d")), "node absent from the list reaped");
        assert!(!deletes.iter().any(|c| c.ends_with("vol-1.node-a")), "Ready node kept");
        assert!(!deletes.iter().any(|c| c.ends_with("vol-1.node-c")), "young NotReady node kept");
        assert!(!deletes.iter().any(|c| c.ends_with("vol-1.node-e")), "no Ready condition at all: kept, not treated as dead");
    }

    /// A nodes-list error must reap, unclaim and place nothing — `pull_beat_with` lists Nodes
    /// once and bails before any of the three run, so a partial view of who is alive never reaches
    /// the reaper, the unclaim sweep, or placement.
    #[tokio::test]
    async fn pull_beat_reaps_unclaims_and_places_nothing_on_a_node_list_error() {
        let routes = vec![Route { method: "GET", path: NODES.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) }];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);

        pull_beat_with(&ctx, "btrfs", "s3cret").await;

        assert_eq!(rec.calls(), vec![format!("GET {NODES}")], "nothing beyond the failed nodes list should ever be called");
    }

    // -----------------------------------------------------------------------------------------
    // The per-volume sweep: `volume_decision` and `sweep_dead_nodes`, beside the reaper, same
    // dead-node rule.
    // -----------------------------------------------------------------------------------------

    fn ws_placed(name: &str, node: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "name": name, "region": "r1", "image": "img", "desiredState": "running", "packages": []},
            "status": {"phase": "ready", "nodeName": node, "compatibleNodes": [node], "volumeRef": format!("vol-{name}")},
        })
    }

    fn ws_placed_stopped(name: &str, node: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "name": name, "region": "r1", "image": "img", "desiredState": "stopped", "packages": []},
            "status": {"phase": "ready", "nodeName": node, "compatibleNodes": [node], "volumeRef": format!("vol-{name}")},
        })
    }

    fn vol_owned(name: &str, node: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "team": "", "nodeName": node, "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        })
    }

    fn vol_at_rv(name: &str, node: &str, rv: &str) -> serde_json::Value {
        let mut v = vol_owned(name, node);
        v["metadata"]["resourceVersion"] = serde_json::json!(rv);
        v
    }

    fn env_placed_stopped(name: &str, node: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Environment",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "acme", "name": name, "region": "r1", "services": [], "desiredState": "stopped"},
            "status": {"phase": "creating", "nodeName": node, "compatibleNodes": [node], "volumeRef": format!("vol-{name}")},
        })
    }

    /// Arm one: a Running parent pins the volume, full stop. Nothing on it moves — stopped
    /// siblings included, which is the bug this rule exists to make impossible: the parent is
    /// never looked at alone.
    #[test]
    fn a_running_parent_pins_the_whole_volume() {
        let running = parent_at("Workspace", "ws-run", "vol-1", crd::Phase::Ready, false);
        let stopped = parent_at("Workspace", "ws-stop", "vol-1", crd::Phase::Stopped, true);
        match volume_decision("vol-1", "node-b", &[&running, &stopped], "NodeDead") {
            VolumeVerdict::Mark { why } => assert!(why.contains("Running worktree"), "{why}"),
            other => panic!("a running sibling must keep the pin, got {other:?}"),
        }
    }

    /// Arm two: everything stopped, but one of them is not replicated anywhere — the volume waits
    /// for the node. Every parent must be covered, or a start elsewhere would lose that one's
    /// last edits.
    #[test]
    fn one_unreplicated_stopped_parent_holds_the_whole_volume() {
        let ok = parent_at("Workspace", "ws-a", "vol-1", crd::Phase::Stopped, true);
        let waiting = parent_at("Workspace", "ws-b", "vol-1", crd::Phase::Stopped, false);
        match volume_decision("vol-1", "node-b", &[&ok, &waiting], "NodeDead") {
            VolumeVerdict::Mark { why } => assert!(why.contains("ws-b"), "the message names the holder: {why}"),
            other => panic!("expected a mark, got {other:?}"),
        }
    }

    /// Arm three: everything stopped and every one replicated — the pin is cleared and every
    /// parent un-placed, so an up-to-date node claims them on the next start.
    #[test]
    fn a_fully_replicated_stopped_volume_is_released() {
        let a = parent_at("Workspace", "ws-a", "vol-1", crd::Phase::Stopped, true);
        let b = parent_at("Environment", "env-b", "vol-1", crd::Phase::Stopped, true);
        match volume_decision("vol-1", "node-b", &[&a, &b], "NodeDead") {
            VolumeVerdict::Release { reason, .. } => assert_eq!(reason, "NodeDead"),
            other => panic!("expected a release, got {other:?}"),
        }
        // A volume with no parents at all is releasable too: nothing on it can lose anything.
        assert!(matches!(volume_decision("vol-1", "node-b", &[], "NodeDead"), VolumeVerdict::Release { .. }));
    }

    /// The drill from the spec, exactly: one volume, one stopped workspace and one RUNNING clone
    /// of it. The old code un-placed the stopped one while the running sibling kept the pin —
    /// which left it claimable on a node that owns nothing. Nothing moves.
    #[tokio::test]
    async fn a_stopped_parent_beside_a_running_clone_on_one_volume_never_moves() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)];
        let routes = vec![
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-stop", ws_placed_stopped("ws-stop", "node-b")),
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-clone", ws_placed("ws-clone", "node-b")),
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-stop/status".into(), status: 200, body: ws_placed_stopped("ws-stop", "node-b") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-clone/status".into(), status: 200, body: ws_placed("ws-clone", "node-b") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status".into(), status: 200, body: vol_owned("vol-1", "node-b") },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        let mut beat = beat_of(vec![vol_owned("vol-1", "node-b")], vec![], vec![]);
        beat.all_parents = vec![
            parent_at("Workspace", "ws-stop", "vol-1", crd::Phase::Stopped, true),
            // Replicated, so ONLY the running-sibling arm can be what keeps this pin.
            parent_at("Workspace", "ws-clone", "vol-1", crd::Phase::Ready, true),
        ];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        assert!(
            !rec.calls().iter().any(|c| c.starts_with("PATCH /apis/rustic-git.io/v1alpha1/volumes/vol-1")),
            "the pin is never cleared while a sibling runs: {:?}",
            rec.calls()
        );
        let stop_writes = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-stop/status");
        assert!(
            stop_writes.iter().all(|w| w["status"]["nodeName"] == "node-b"),
            "the stopped sibling keeps its placement: {stop_writes:?}"
        );
        // Both parents carry NodeDead so the API can say why neither will start.
        for name in ["ws-stop", "ws-clone"] {
            let sent = rec.sent("PUT", &format!("/apis/rustic-git.io/v1alpha1/workspaces/{name}/status"));
            assert!(sent.iter().any(|w| w["status"]["conditions"].as_array().unwrap().iter().any(|c| c["reason"] == "NodeDead")), "{name}");
        }
    }

    /// F3(a) (drill, 2026-09-03): the volume status write raced the owner's own controller and a
    /// 409 only warned, leaving a dead owner's volume `Available=True`. Re-read and retry, the
    /// same shape `mark_parent_of` and `write_replica_status` already use.
    #[tokio::test]
    async fn the_sweep_retries_a_conflicted_volume_status_write() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-x"), node_dead_obj("node-b", old)];
        let conflict = serde_json::to_value(kube::core::Status::failure("conflict", "Conflict").with_code(409)).unwrap();
        let routes = vec![
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-1", ws_placed("ws-1", "node-b")),
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-1/status".into(), status: 200, body: ws_placed("ws-1", "node-b") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status".into(), status: 409, body: conflict },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status".into(), status: 200, body: vol_at_rv("vol-1", "node-b", "10") },
            get("/apis/rustic-git.io/v1alpha1/volumes/vol-1", vol_at_rv("vol-1", "node-b", "10")),
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        // Not replicated ⇒ Mark, so the pin is never touched and only the status write is under test.
        let mut beat = beat_of(vec![vol_owned("vol-1", "node-b")], vec![], vec![]);
        beat.all_parents = vec![parent_at("Workspace", "ws-1", "vol-1", crd::Phase::Ready, false)];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        let writes = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status");
        assert_eq!(writes.len(), 2, "the conflicted write is retried once: {:?}", rec.calls());
        assert_eq!(writes[1]["metadata"]["resourceVersion"], "10", "and against the re-read object");
        assert_eq!(writes[1]["status"]["conditions"][0]["reason"], "NodeDead");
    }

    /// F3(b) (drill, 2026-09-03): the agent kept sweeping through its own node's kubelet outage —
    /// reaping, unclaiming and retiring on a view nobody else shared. A node the cluster reads as
    /// dead does nothing; the absence of every other route makes that provable.
    #[tokio::test]
    async fn a_node_the_cluster_reads_as_dead_sweeps_nothing() {
        let old = "2000-01-01T00:00:00Z";
        let routes = vec![get(NODES, list_of("Node", vec![node_json("node-x", "False", old), node_json("node-a", "True", old)]))];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);

        pull_beat_with(&ctx, "/bin/true", "s3cret").await;

        assert_eq!(rec.calls(), vec![format!("GET {NODES}")], "the node list and nothing else");
    }

    /// A dead node's parents are un-placed — `status.nodeName` alone — on both kinds, once every
    /// one of them is stopped and replicated; a live node's volume is never even looked at, which
    /// the absence of its routes makes provable (the mock 404s any call it did not expect).
    #[tokio::test]
    async fn the_sweep_unplaces_a_dead_owners_parents_and_never_touches_a_live_one() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)];
        let routes = vec![
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-dead", ws_placed_stopped("ws-dead", "node-b")),
            get("/apis/rustic-git.io/v1alpha1/environments/env-dead", env_placed_stopped("env-dead", "node-b")),
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-dead/status".into(), status: 200, body: ws_placed_stopped("ws-dead", "") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/environments/env-dead/status".into(), status: 200, body: env_placed_stopped("env-dead", "") },
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-dead".into(), status: 200, body: vol_at_rv("vol-ws-dead", "", "10") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-dead/status".into(), status: 200, body: vol_owned("vol-ws-dead", "") },
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-env-dead".into(), status: 200, body: vol_at_rv("vol-env-dead", "", "10") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-env-dead/status".into(), status: 200, body: vol_owned("vol-env-dead", "") },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        let mut beat = beat_of(
            vec![vol_owned("vol-ws-dead", "node-b"), vol_owned("vol-env-dead", "node-b"), vol_owned("vol-live", "node-a")],
            vec![],
            vec![],
        );
        beat.all_parents = vec![
            parent_at("Workspace", "ws-dead", "vol-ws-dead", crd::Phase::Stopped, true),
            parent_at("Environment", "env-dead", "vol-env-dead", crd::Phase::Stopped, true),
        ];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        let ws_sent = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-dead/status");
        assert_eq!(ws_sent.len(), 1, "{:?}", rec.calls());
        assert_eq!(ws_sent[0]["status"]["nodeName"], "", "nodeName cleared");
        assert_eq!(ws_sent[0]["status"]["phase"], "ready", "nothing else in status is touched");
        let env_sent = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/environments/env-dead/status");
        assert_eq!(env_sent.len(), 1);
        assert_eq!(env_sent[0]["status"]["nodeName"], "");
        assert!(!rec.calls().iter().any(|c| c.contains("/volumes/vol-live")), "{:?}", rec.calls());
    }

    /// A Running worktree on a dead node keeps its node — the person decides, not the sweep —
    /// and its Volume is marked Unavailable but its pin stays.
    #[tokio::test]
    async fn a_running_worktree_on_a_dead_node_is_marked_not_moved() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)];
        let routes = vec![
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-run", ws_placed("ws-run", "node-b")),
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-run/status".into(), status: 200, body: ws_placed("ws-run", "node-b") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-run/status".into(), status: 200, body: vol_owned("vol-ws-run", "node-b") },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        let mut beat = beat_of(vec![vol_owned("vol-ws-run", "node-b")], vec![], vec![]);
        beat.all_parents = vec![parent_at("Workspace", "ws-run", "vol-ws-run", crd::Phase::Ready, false)];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        let ws_sent = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-run/status");
        assert_eq!(ws_sent.len(), 1, "{:?}", rec.calls());
        assert_eq!(ws_sent[0]["status"]["nodeName"], "node-b", "a running worktree keeps its node");
        assert_eq!(ws_sent[0]["status"]["conditions"][0]["reason"], "NodeDead");
        assert!(
            !rec.calls().iter().any(|c| c == "PATCH /apis/rustic-git.io/v1alpha1/volumes/vol-ws-run"),
            "pin untouched: {:?}", rec.calls()
        );
        let vol_sent = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-run/status");
        assert_eq!(vol_sent.len(), 1);
        assert_eq!(vol_sent[0]["status"]["phase"], "unavailable");
        assert_eq!(vol_sent[0]["status"]["conditions"][0]["reason"], "NodeDead");
    }

    /// A second pass over the same still-Running, still-dead state must write nothing at all —
    /// neither the parent's status (already carries the same `NodeDead` message) nor the volume's
    /// (already `Unavailable` with that message, and still pinned): a beat every few seconds must
    /// not churn either object forever while the person has not yet acted.
    #[tokio::test]
    async fn a_second_pass_over_the_same_running_dead_state_writes_nothing() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)];
        // Verbatim from `volume_decision`'s first arm: the idle guard is a message comparison, so
        // a drifted message is a rewrite every beat and this is what catches that.
        let why = "owner node-b is unavailable; a Running worktree (ws-run) still names volume vol-ws-run, so it stays pinned";
        let already_degraded = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-run", "uid": "uid-ws-run", "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "name": "ws-run", "region": "r1", "image": "img", "desiredState": "running", "packages": []},
            "status": {
                "phase": "ready", "nodeName": "node-b", "volumeRef": "vol-ws-run",
                "conditions": [{"type": "Degraded", "status": "True", "reason": "NodeDead", "message": why, "observedGeneration": 1, "lastTransitionTime": "2026-09-01T00:00:00Z"}],
            },
        });
        let already_unavailable = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "vol-ws-run", "uid": "uid-vol-ws-run", "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-b", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {
                "phase": "unavailable",
                "conditions": [{"type": "Available", "status": "False", "reason": "NodeDead", "message": why, "observedGeneration": 1, "lastTransitionTime": "2026-09-01T00:00:00Z"}],
            },
        });
        let routes = vec![get("/apis/rustic-git.io/v1alpha1/workspaces/ws-run", already_degraded)];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        let mut beat = beat_of(vec![already_unavailable], vec![], vec![]);
        beat.all_parents = vec![parent_at("Workspace", "ws-run", "vol-ws-run", crd::Phase::Ready, false)];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        assert!(
            !rec.calls().iter().any(|c| c.starts_with("PUT") || c.starts_with("PATCH")),
            "no write of any kind on an unchanged pass: {:?}", rec.calls()
        );
    }

    /// A Stopped, replicated worktree on a dead node is un-placed and its Volume released (pin
    /// cleared with a guarded `test`+`replace`, phase Unavailable, reason still `NodeDead` — an
    /// empty pin IS the released state) — a sibling Volume on a live node is left alone entirely.
    #[tokio::test]
    async fn a_stopped_worktree_on_a_dead_node_is_released_with_its_volume() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)];
        let routes = vec![
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-stop", ws_placed_stopped("ws-stop", "node-b")),
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-stop/status".into(), status: 200, body: ws_placed_stopped("ws-stop", "") },
            // The API server bumps resourceVersion on the patch; the status PUT must carry the
            // NEW one or it 409s and the volume never gets marked.
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop".into(), status: 200, body: vol_at_rv("vol-ws-stop", "", "10") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop/status".into(), status: 200, body: vol_owned("vol-ws-stop", "") },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        let mut beat = beat_of(vec![vol_owned("vol-ws-stop", "node-b"), vol_owned("vol-live", "node-a")], vec![], vec![]);
        beat.all_parents = vec![parent_at("Workspace", "ws-stop", "vol-ws-stop", crd::Phase::Stopped, true)];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        let ws_sent = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-stop/status");
        assert_eq!(ws_sent.len(), 1);
        assert_eq!(ws_sent[0]["status"]["nodeName"], "");
        let patched = rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop");
        assert_eq!(patched.len(), 1);
        // A guarded JSON patch, not a blind merge: `test` proves the owner hadn't already moved
        // (a survivor's takeover landing between our list and this patch), THEN `replace` clears
        // it — so a lost race is refused rather than clobbering a fresh owner back to "".
        let ops = patched[0].as_array().expect("a JSON Patch is an array of ops");
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert_eq!(ops[0]["op"], "test");
        assert_eq!(ops[0]["path"], "/spec/nodeName");
        assert_eq!(ops[0]["value"], "node-b");
        assert_eq!(ops[1]["op"], "replace");
        assert_eq!(ops[1]["path"], "/spec/nodeName");
        assert_eq!(ops[1]["value"], "");
        let vol_sent = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop/status");
        assert_eq!(vol_sent.len(), 1);
        assert_eq!(vol_sent[0]["metadata"]["resourceVersion"], "10", "the status PUT must carry the patch's resourceVersion, not the stale one");
        assert_eq!(vol_sent[0]["spec"]["nodeName"], "", "and the patched spec it read back");
        assert_eq!(vol_sent[0]["status"]["phase"], "unavailable");
        assert_eq!(vol_sent[0]["status"]["conditions"][0]["reason"], "NodeDead");
        assert!(!rec.calls().iter().any(|c| c.contains("/volumes/vol-live")), "{:?}", rec.calls());
    }

    /// A lost CAS writes NOTHING: a survivor's takeover landed between the listing and the patch,
    /// so the volume is owned again and un-placing its parents would leave them claimable on a
    /// node that owns nothing. The pin is therefore attempted FIRST, and its failure ends the beat
    /// for this volume.
    #[tokio::test]
    async fn a_lost_pin_cas_leaves_the_volume_and_its_parents_untouched() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)];
        let routes = vec![
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-stop", ws_placed_stopped("ws-stop", "node-b")),
            Route {
                method: "PATCH",
                path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop".into(),
                status: 422,
                body: serde_json::to_value(kube::core::Status::failure("the test operation failed", "Invalid").with_code(422)).unwrap(),
            },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        let mut beat = beat_of(vec![vol_owned("vol-ws-stop", "node-b")], vec![], vec![]);
        beat.all_parents = vec![parent_at("Workspace", "ws-stop", "vol-ws-stop", crd::Phase::Stopped, true)];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        assert!(
            !rec.calls().iter().any(|c| c.starts_with("PUT")),
            "a failed CAS writes no volume status and un-places no parent: {:?}", rec.calls()
        );
    }

    /// Two parents of different kinds on ONE volume, both stopped and both replicated: one pin
    /// cleared, one volume marked, and BOTH un-placed — the volume is the unit, so no parent on it
    /// is left behind pinned to a node that no longer owns it.
    #[tokio::test]
    async fn a_shared_volume_releases_every_parent_on_it_at_once() {
        let old = "2000-01-01T00:00:00Z";
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b", old)];
        let routes = vec![
            get("/apis/rustic-git.io/v1alpha1/workspaces/ws-a", ws_placed_stopped("ws-a", "node-b")),
            get("/apis/rustic-git.io/v1alpha1/environments/env-b", env_placed_stopped("env-b", "node-b")),
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-a/status".into(), status: 200, body: ws_placed_stopped("ws-a", "") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/environments/env-b/status".into(), status: 200, body: env_placed_stopped("env-b", "") },
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1".into(), status: 200, body: vol_at_rv("vol-1", "", "10") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status".into(), status: 200, body: vol_owned("vol-1", "") },
        ];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-x", routes);
        let mut beat = beat_of(vec![vol_owned("vol-1", "node-b")], vec![], vec![]);
        beat.all_parents = vec![
            parent_at("Workspace", "ws-a", "vol-1", crd::Phase::Stopped, true),
            parent_at("Environment", "env-b", "vol-1", crd::Phase::Stopped, true),
        ];

        sweep_dead_nodes(&ctx, &beat, &nodes, node_dead_secs(), k8s_openapi::jiff::Timestamp::now()).await;

        assert_eq!(rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/volumes/vol-1").len(), 1, "one pin patch for the volume, not one per parent");
        assert_eq!(rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status").len(), 1);
        let ws = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-a/status");
        let env = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/environments/env-b/status");
        assert_eq!(ws.len(), 1, "{:?}", rec.calls());
        assert_eq!(env.len(), 1, "{:?}", rec.calls());
        assert_eq!(ws[0]["status"]["nodeName"], "");
        assert_eq!(env[0]["status"]["nodeName"], "");
    }

    /// `resolve_volume`'s takeover half, `controller::take_volume`: a CAS win writes the same
    /// two-op shape the release side above reads back, and a lost race (the API server's `test`
    /// failing) is reported quietly rather than as an error.
    #[tokio::test]
    async fn take_volume_wins_with_a_test_op_on_an_empty_pin() {
        let routes = vec![Route {
            method: "PATCH",
            path: "/apis/rustic-git.io/v1alpha1/volumes/v1".into(),
            status: 200,
            body: vol_owned("v1", "node-a"),
        }];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        assert!(crate::controller::volume::take_volume(&ctx, "v1", "node-a").await.unwrap());

        let sent = rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/volumes/v1");
        assert_eq!(sent.len(), 1);
        let ops = sent[0].as_array().expect("a JSON Patch is an array of ops");
        assert_eq!(ops[0], serde_json::json!({"op": "test", "path": "/spec/nodeName", "value": ""}));
        assert_eq!(ops[1], serde_json::json!({"op": "replace", "path": "/spec/nodeName", "value": "node-a"}));
    }

    #[tokio::test]
    async fn take_volume_loses_quietly_when_the_test_op_fails() {
        let routes = vec![Route {
            method: "PATCH",
            path: "/apis/rustic-git.io/v1alpha1/volumes/v1".into(),
            status: 422,
            body: serde_json::to_value(kube::core::Status::failure("test failed", "Invalid").with_code(422))
                .expect("Status serializes"),
        }];
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        assert!(!crate::controller::volume::take_volume(&ctx, "v1", "node-a").await.unwrap());
        assert_eq!(rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/volumes/v1").len(), 1);
    }

    // A nodes-list error's effect on both sweeps together is covered by
    // `pull_beat_reaps_unclaims_and_places_nothing_on_a_node_list_error` above — `reap_dead_replicas`
    // and `unclaim_dead_nodes` no longer list Nodes themselves, so there is nothing left to error on
    // in isolation.

    // -----------------------------------------------------------------------------------------
    // Task 6: a transient (sync point) is just another `Snapshot` to the pull beat — no separate
    // code path exists for it, so these prove the existing plumbing already replicates one.
    // -----------------------------------------------------------------------------------------

    fn ready_transient(name: &str, volume: &str, parent: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1",
            "kind": "Snapshot",
            "metadata": {"name": name, "uid": "snap-uid-transient"},
            "spec": {"volume": volume, "owner": "alice", "worktree": "ws-1", "parent": parent, "pinned": false, "transient": true},
            "status": {"phase": "ready"},
        })
    }

    /// A transient is addressed, pulled, and counted toward `Synced` exactly like a commit: same
    /// `GET /peer/v1/commit/{volume}/{name}` shape (its name just happens to start with `sync-`),
    /// same replica-status write at the end of the pass. No code change should be needed for this
    /// to pass — that is the point of Task 6.
    #[tokio::test]
    async fn a_ready_transient_is_pulled_and_counts_toward_synced() {
        let tmp = tempfile::tempdir().unwrap();

        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin = bin_dir.join("btrfs");
        std::fs::write(
            &bin,
            r#"#!/bin/sh
if [ "$1" = "receive" ]; then
    cat >/dev/null
    mkdir -p "$2/sync-ws-1-x"
    exit 0
fi
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = bin.to_string_lossy().into_owned();

        let send_bin = bin_dir.join("btrfs-send");
        std::fs::write(&send_bin, "#!/bin/sh\nprintf 'bytes'\nexit 0\n").unwrap();
        std::fs::set_permissions(&send_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let send_bin = send_bin.to_string_lossy().into_owned();
        let source_pool = tmp.path().join("source-pool");
        std::fs::create_dir_all(source_pool.join("vol/vol-1/snap/sync-ws-1-x")).unwrap();
        let (client, _rec) = mock_client(vec![]);
        let peer_state = PeerState::new(client, source_pool.to_string_lossy().into(), "node-a".into(), "s3cret".into(), send_bin);
        // Captures every request path the real peer server sees, so we can prove the transient is
        // fetched over `/peer/v1/commit/{volume}/{name}` — the exact same endpoint a real commit
        // uses — rather than trusting the on-disk result alone.
        let seen_paths: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_paths2 = seen_paths.clone();
        let app = router(peer_state).layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let seen_paths = seen_paths2.clone();
            async move {
                seen_paths.lock().unwrap().push(req.uri().to_string());
                next.run(req).await
            }
        }));
        let peer_server = serve_on_the_peer_port(app).await;

        let pod = serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "agent-a"},
            "status": {"podIP": "127.0.0.1"},
        });
        let routes = vec![
            // One already-local commit alongside the missing transient: proves the transient is
            // just another item on the same list, not a special case that only fires when it's
            // the sole entry.
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![ready_snapshot("vol-1-aaaaaaaa", "vol-1", ""), ready_transient("sync-ws-1-x", "vol-1", "")]) },
            Route { method: "GET", path: "/api/v1/namespaces/kube-system/pods".into(), status: 200, body: list_of("Pod", vec![pod]) },
            not_found(format!("{VOLREPLICAS}/vol-1.node-b")),
            Route { method: "POST", path: VOLREPLICAS.into(), status: 201, body: serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                "metadata": {"name": "vol-1.node-b", "uid": "vr-b"},
                "spec": {"volume": "vol-1", "node": "node-b"},
                "status": {"phase": "Syncing", "branches": {}},
            }) },
            Route { method: "PUT", path: format!("{VOLREPLICAS}/vol-1.node-b/status"), status: 200, body: serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                "metadata": {"name": "vol-1.node-b", "uid": "vr-b"},
                "spec": {"volume": "vol-1", "node": "node-b"},
                "status": {"phase": "Synced", "branches": {}},
            }) },
        ];
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/snap/vol-1-aaaaaaaa")).unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let http = peer_http_client().unwrap();

        pull_volume(&ctx, &beat_of(vec![], vec![replica_of("vol-1", "node-a", "Synced")], vec![]), &bin, &http, "s3cret", "vol-1", &[]).await;

        assert!(tmp.path().join("vol/vol-1/snap/sync-ws-1-x").exists(), "the transient must land on disk like any other commit");
        let paths = seen_paths.lock().unwrap().clone();
        assert!(
            paths.iter().any(|p| p.contains("/peer/v1/commit/vol-1/sync-ws-1-")),
            "the transient is fetched over the same commit endpoint as a real commit: {paths:?}"
        );
        let created = rec.sent("POST", VOLREPLICAS);
        assert_eq!(created.len(), 1, "the replica row is created fresh (Syncing, per the mocked response) before the final status write");
        let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["status"]["phase"], "Synced", "and lands Synced once the transient is pulled");
        // Before the guard: see `PeerServer`.
        peer_server.stop().await;
    }

    /// A transient's `Snapshot` CR being gone is exactly the same "retired" case a deleted commit
    /// is — `pull_volume` diffs local names against the full CR list regardless of `transient`,
    /// so a local sync point whose CR disappeared is dropped the same way.
    #[tokio::test]
    async fn a_deleted_transient_is_dropped_from_every_replica() {
        let have: HashSet<String> = ["vol-1-aaaaaaaa".into(), "sync-ws-1-a".into()].into_iter().collect();
        // "sync-ws-1-a" is local but absent from the CR list entirely — its Snapshot was deleted.
        let existing: HashSet<String> = ["vol-1-aaaaaaaa".into()].into_iter().collect();
        assert_eq!(retired(&have, &existing, false), vec!["sync-ws-1-a".to_string()], "a clean pass reclaims it");
    }

    // -----------------------------------------------------------------------------------------
    // Task 0b: `should_retire`, `retire_pass` — dropping a copy whose rendezvous slot moved.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn should_retire_only_an_unwanted_copy_whose_replacements_are_synced() {
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let synced = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        assert!(!should_retire("b", "b", &t(&["c"]), false, &synced(&["c"])), "owner never retires");
        assert!(!should_retire("b", "a", &t(&["b"]), false, &synced(&["b"])), "still a target");
        assert!(!should_retire("b", "a", &t(&["c"]), true, &synced(&["c"])), "hosting a worktree here");
        assert!(!should_retire("b", "a", &t(&["c"]), false, &synced(&[])), "replacement not synced yet: keep");
        assert!(!should_retire("b", "", &t(&["c"]), false, &synced(&["c"])), "unowned (dead owner): keep until taken");
        assert!(!should_retire("b", "a", &t(&[]), false, &synced(&[])), "empty targets (me missing from live) must not vacuously retire");
        assert!(should_retire("b", "a", &t(&["c"]), false, &synced(&["c"])));
    }

    /// `v1` is picked so that `targets("v1", "node-a", [node-a, node-b, node-c], 2) == ["node-c"]`
    /// — node-b's slot moved to node-c, and node-c's row is Synced, so node-b's copy is retirable.
    #[tokio::test]
    async fn retire_pass_drops_a_copy_whose_slot_moved_once_the_replacement_is_synced() {
        assert_eq!(
            replicate::targets("v1", "node-a", &["node-a".into(), "node-b".into(), "node-c".into()], 2),
            vec!["node-c".to_string()]
        );

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v1")).unwrap();

        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        });
        let replica_c = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": "v1.node-c", "uid": "uid-c"},
            "spec": {"volume": "v1", "node": "node-c"},
            "status": {"phase": "Synced", "branches": {}},
        });
        let beat = beat_of(vec![volume], vec![replica_c], vec![]);
        let routes = vec![
            Route {
                method: "DELETE",
                path: format!("{VOLREPLICAS}/v1.node-b"),
                status: 200,
                body: serde_json::json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                    "metadata": {"name": "v1.node-b", "uid": "uid-b"},
                    "spec": {"volume": "v1", "node": "node-b"},
                    "status": {"phase": "Synced", "branches": {}},
                }),
            },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let live = vec!["node-a".to_string(), "node-b".to_string(), "node-c".to_string()];

        retire_pass(&ctx, &beat, &live).await;

        assert!(rec.calls().iter().any(|c| c == &format!("DELETE {VOLREPLICAS}/v1.node-b")), "{:?}", rec.calls());
        assert!(!ctx.engine.pool.voldir("v1").exists(), "the local copy must be gone");
    }

    /// Same setup, but node-c's row is still `Syncing` — node-b's copy must be kept, on disk and
    /// in its `VolumeReplica` row, until the replacement actually finishes.
    #[tokio::test]
    async fn retire_pass_keeps_a_copy_whose_replacement_is_not_synced_yet() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v1")).unwrap();

        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        });
        let replica_c = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": "v1.node-c", "uid": "uid-c"},
            "spec": {"volume": "v1", "node": "node-c"},
            "status": {"phase": "Syncing", "branches": {}},
        });
        let beat = beat_of(vec![volume], vec![replica_c], vec![]);
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", Vec::new());
        let live = vec!["node-a".to_string(), "node-b".to_string(), "node-c".to_string()];

        retire_pass(&ctx, &beat, &live).await;

        assert!(rec.calls().iter().all(|c| !c.starts_with("DELETE")), "{:?}", rec.calls());
        assert!(ctx.engine.pool.voldir("v1").exists(), "an unsynced replacement must not cost the copy");
    }

    /// This node isn't the owner and its replacement (node-c) is fully synced — `should_retire`
    /// would drop the whole copy but for one thing: a `Workspace` is running here right now
    /// against this volume (`hosted`). The owner record can lag a pod that's already up, so
    /// neither the whole copy NOR its live worktree may be touched while that's true.
    #[test]
    fn orphan_voldirs_names_only_directories_no_volume_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vol");
        std::fs::create_dir_all(root.join("v-live")).unwrap();
        std::fs::create_dir_all(root.join("v-gone")).unwrap();
        std::fs::write(root.join("v-gone.lock"), b"").unwrap();
        let known: HashSet<String> = ["v-live".to_string()].into_iter().collect();
        assert_eq!(orphan_voldirs(&root, &known), vec!["v-gone".to_string()]);
        assert!(orphan_voldirs(&tmp.path().join("missing"), &known).is_empty(), "no vol dir yet: nothing to name");
    }

    #[tokio::test]
    async fn retire_pass_drops_a_voldir_whose_volume_cr_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v-gone/snap")).unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v-live/snap")).unwrap();
        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v-live"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        });
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", Vec::new());
        retire_pass(&ctx, &beat_of(vec![volume], vec![], vec![]), &["node-a".to_string()]).await;
        assert!(!ctx.engine.pool.voldir("v-gone").exists(), "no CR: the copy goes");
        assert!(ctx.engine.pool.voldir("v-live").exists(), "listed: untouched");
    }

    /// F2 (drill, 2026-09-03): `VolumeReplica` rows outlived their deleted workspaces — nothing
    /// ever revisited them, because every other arm of this pass walks LISTED volumes. Mine go;
    /// another node's rows are its own business, and it runs this same sweep.
    #[tokio::test]
    async fn retire_pass_drops_my_replica_row_whose_volume_cr_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", Vec::new());
        let beat = beat_of(
            vec![vol_owned("v-live", "node-a")],
            vec![
                replica_of("v-gone", "node-a", "Synced"),
                replica_of("v-live", "node-a", "Synced"),
                replica_of("v-gone", "node-b", "Synced"),
            ],
            vec![],
        );

        retire_pass(&ctx, &beat, &["node-a".to_string()]).await;

        let deletes: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes, vec![format!("DELETE {VOLREPLICAS}/v-gone.node-a")], "only my orphan: {deletes:?}");
    }

    fn snap_of(name: &str, volume: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": name, "uid": format!("uid-{name}")},
            "spec": {"volume": volume, "owner": "alice", "worktree": volume, "parent": "", "pinned": false, "transient": false},
            "status": {"phase": "ready"},
        })
    }

    fn snap_list(items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {"resourceVersion": "1"}, "items": items})
    }

    /// The baseline `Snapshot` used to carry no ownerReference at all, so it outlived its volume
    /// forever. The sweep is what clears the ones already out there.
    #[tokio::test]
    async fn retire_pass_drops_a_snapshot_whose_volume_cr_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(
            tmp.path(),
            "node-a",
            vec![
                get(SNAPSHOTS, snap_list(vec![snap_of("v-gone.aaaa", "v-gone"), snap_of("v-live.bbbb", "v-live")])),
                // The confirming GET: really gone, not merely younger than the beat's volume list.
                not_found(format!("{VOLUMES}/v-gone")),
            ],
        );

        retire_pass(&ctx, &beat_of(vec![vol_owned("v-live", "node-a")], vec![], vec![]), &["node-a".to_string()]).await;

        let deletes: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deletes, vec![format!("DELETE {SNAPSHOTS}/v-gone.aaaa")], "only the orphan: {deletes:?}");
    }

    /// The two listings are separate round trips: a Volume created after the beat's list looks
    /// absent, and its brand-new baseline must survive on the strength of the fresh GET.
    #[tokio::test]
    async fn retire_pass_keeps_a_snapshot_whose_volume_appeared_after_the_beats_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = test_ctx(
            tmp.path(),
            "node-a",
            vec![get(SNAPSHOTS, snap_list(vec![snap_of("v-new.aaaa", "v-new")])), get(format!("{VOLUMES}/v-new"), vol_owned("v-new", "node-b"))],
        );

        retire_pass(&ctx, &beat_of(vec![], vec![], vec![]), &["node-a".to_string()]).await;

        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
    }

    /// Keep-biased: an unlistable snapshot set is "we do not know", never "there are none".
    #[tokio::test]
    async fn retire_pass_deletes_no_snapshot_on_a_list_error() {
        let tmp = tempfile::tempdir().unwrap();
        // No `SNAPSHOTS` route at all: the mock answers 404, which is a list failure.
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", Vec::new());

        retire_pass(&ctx, &beat_of(vec![], vec![], vec![]), &["node-a".to_string()]).await;

        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
    }

    /// Task 3: bytes follow records. A `snap/<name>` no Snapshot claims is the only thing that
    /// goes; a pinned one whose workspace is long gone stays because its record does.
    #[test]
    fn orphan_snaps_keeps_every_recorded_name_whatever_its_phase() {
        let local = vec!["v1-aaaa".to_string(), "v1-bbbb".to_string(), "v1-cccc".to_string()];
        // `records` is the record set, phase-blind on purpose: a `Working` cut is mid-receive.
        let records: HashSet<String> = ["v1-aaaa".to_string(), "v1-cccc".to_string()].into_iter().collect();
        assert_eq!(orphan_snaps(&local, &records), vec!["v1-bbbb".to_string()]);
        assert!(orphan_snaps(&[], &records).is_empty());
    }

    fn snap_pool(tmp: &std::path::Path, volume: &str, names: &[&str]) {
        for n in names {
            std::fs::create_dir_all(tmp.join("vol").join(volume).join("snap").join(n)).unwrap();
        }
    }

    #[tokio::test]
    async fn the_byte_sweep_drops_a_snap_whose_record_is_gone_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        snap_pool(tmp.path(), "v1", &["v1-aaaa", "v1-bbbb"]);
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", Vec::new());
        let beat = beat_of(vec![vol_owned("v1", "node-a")], vec![], vec![]);

        let dropped = sweep_orphan_snap_bytes(&ctx, &beat, &[serde_json::from_value(snap_of("v1-aaaa", "v1")).unwrap()]);

        assert_eq!(dropped, vec![("v1".to_string(), "v1-bbbb".to_string())]);
        // The BYTE sweep never touches a record: only an explicit delete kills a Snapshot CR.
        assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
    }

    /// A volume whose bytes are not here at all, and one with no `snap/` yet, are both nothing to
    /// sweep — never "every record is orphaned".
    #[tokio::test]
    async fn the_byte_sweep_skips_volumes_this_node_holds_no_bytes_for() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v2")).unwrap(); // voldir, no snap/ yet
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", Vec::new());
        let beat = beat_of(vec![vol_owned("v1", "node-a"), vol_owned("v2", "node-a")], vec![], vec![]);

        assert!(sweep_orphan_snap_bytes(&ctx, &beat, &[]).is_empty());
    }

    /// Keep-biased at the top: a failed Snapshot listing skips both sweeps, so the bytes stay.
    #[tokio::test]
    async fn the_byte_sweep_deletes_nothing_when_the_snapshot_list_fails() {
        let tmp = tempfile::tempdir().unwrap();
        snap_pool(tmp.path(), "v1", &["v1-aaaa"]);
        // No `SNAPSHOTS` route: the mock answers 404, which is a list failure.
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", Vec::new());

        retire_pass(&ctx, &beat_of(vec![vol_owned("v1", "node-a")], vec![], vec![]), &["node-a".to_string()]).await;

        assert!(ctx.engine.pool.snap("v1", "v1-aaaa").exists(), "unlistable records: nothing goes");
    }

    #[tokio::test]
    async fn retire_pass_keeps_a_hosted_worktree_even_when_its_replacement_is_synced() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v1/live/ws-1")).unwrap();

        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        });
        let replica_c = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": "v1.node-c", "uid": "uid-c"},
            "spec": {"volume": "v1", "node": "node-c"},
            "status": {"phase": "Synced", "branches": {}},
        });
        let beat = beat_of(vec![volume], vec![replica_c], vec![("Workspace", "ws-1", "v1")]);
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", Vec::new());
        let live = vec!["node-a".to_string(), "node-b".to_string(), "node-c".to_string()];

        retire_pass(&ctx, &beat, &live).await;

        assert!(rec.calls().iter().all(|c| !c.starts_with("DELETE")), "{:?}", rec.calls());
        assert!(ctx.engine.pool.voldir("v1").exists(), "hosting a worktree here must keep the whole copy");
        assert!(ctx.engine.pool.live("v1").join("ws-1").exists(), "and must not drop the live worktree either");
    }

    /// `beat.volumes` is listed before the pull loop runs; a takeover landing in that window makes
    /// `v.spec.node_name` stale. Here the list still says node-a, but a takeover has already moved
    /// the volume to node-b (me) by the time this pass gets around to it — the fresh GET right
    /// before the delete must catch that and keep the worktree this node just created for itself.
    #[tokio::test]
    async fn retire_pass_rechecks_ownership_before_dropping_a_worktree_a_fresh_takeover_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v1/live/ws-1")).unwrap();

        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        });
        let beat = beat_of(vec![volume], vec![], vec![]);
        let routes = vec![Route {
            method: "GET",
            path: format!("{VOLUMES}/v1"),
            status: 200,
            body: serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
                "metadata": {"name": "v1"},
                "spec": {"owner": "alice", "team": "", "nodeName": "node-b", "region": "r1", "quotaGb": 5, "replicas": 2},
                "status": {"phase": "ready"},
            }),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let live = vec!["node-a".to_string(), "node-b".to_string()];

        retire_pass(&ctx, &beat, &live).await;

        assert!(ctx.engine.pool.live("v1").join("ws-1").exists(), "a fresh takeover made this worktree mine; it must survive");
        assert!(rec.calls().iter().any(|c| c == &format!("GET {VOLUMES}/v1")), "{:?}", rec.calls());
    }

    /// The listing budget: one pull beat over one volume makes ONE Volume list, ONE VolumeReplica
    /// list for the beat, ONE Workspace list and ONE Environment list for this node's parents —
    /// plus the sweep's cluster-wide Workspace/Environment pair and the per-volume snapshot list.
    /// What it must never do again is re-list Volumes three times and Workspaces/Environments
    /// three times.
    #[tokio::test]
    async fn a_pull_beat_lists_each_kind_once_for_the_beat() {
        let tmp = tempfile::tempdir().unwrap();
        let volume = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 1},
            "status": {"phase": "ready"},
        });
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_json("node-a", "True", "2000-01-01T00:00:00Z")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![volume]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        pull_beat_with(&ctx, "btrfs", "s3cret").await;

        let count = |p: &str| rec.calls().iter().filter(|c| c.as_str() == format!("GET {p}")).count();
        assert_eq!(count(VOLUMES), 1, "{:?}", rec.calls());
        assert_eq!(count(VOLREPLICAS), 1, "{:?}", rec.calls());
        assert!(count(WORKSPACES) <= 2, "{:?}", rec.calls());
        assert!(count(ENVIRONMENTS) <= 2, "{:?}", rec.calls());
    }

    // ---------------------------------------------------------------------------------------
    // Task 1: `status.branches` is the newest Ready transient this node HOLDS, per worktree —
    // the one thing placement is allowed to read, because a name cannot be skewed by a clock.
    // ---------------------------------------------------------------------------------------

    fn transient_gen(name: &str, volume: &str, worktree: &str, generation: u64) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1",
            "kind": "Snapshot",
            "metadata": {"name": name, "uid": format!("uid-{name}"),
                         "annotations": {"rustic-git.io/synced-generation": generation.to_string()}},
            "spec": {"volume": volume, "owner": "alice", "worktree": worktree, "parent": "",
                     "pinned": false, "transient": true},
            "status": {"phase": "ready"},
        })
    }

    fn snaps_of(items: Vec<serde_json::Value>) -> Vec<crd::Snapshot> {
        items.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect()
    }

    /// Generation, not creation time, and not the name's suffix: the annotation is the btrfs
    /// generation the sync beat actually replicated, and it is the only ordering that survives
    /// clock skew between the owner and a puller.
    #[test]
    fn newest_transient_is_the_highest_generation_of_that_worktree() {
        let snaps = snaps_of(vec![
            transient_gen("sync-ws-1-aaaa", "vol-1", "ws-1", 10),
            transient_gen("sync-ws-1-bbbb", "vol-1", "ws-1", 42),
            transient_gen("sync-ws-2-cccc", "vol-1", "ws-2", 99),
            ready_snapshot("vol-1-commit", "vol-1", ""),
        ]);
        assert_eq!(newest_transient_of(&snaps, "ws-1").as_deref(), Some("sync-ws-1-bbbb"));
        assert_eq!(newest_transient_of(&snaps, "ws-2").as_deref(), Some("sync-ws-2-cccc"));
        assert_eq!(newest_transient_of(&snaps, "ws-none"), None, "a worktree with no transient has none");
    }

    /// The stop transient carries no generation annotation at all (the stop path cuts it before
    /// the post-cut re-stamp), so it reads as 0 — and must still LOSE to an annotated one rather
    /// than winning by being newest-created. Ties break by name so two nodes agree.
    #[test]
    fn an_unannotated_transient_reads_as_generation_zero() {
        let mut stop = transient_gen("stop-ws-1-7", "vol-1", "ws-1", 0);
        stop["metadata"]["annotations"] = serde_json::json!({});
        let snaps = snaps_of(vec![stop.clone(), transient_gen("sync-ws-1-aaaa", "vol-1", "ws-1", 5)]);
        assert_eq!(newest_transient_of(&snaps, "ws-1").as_deref(), Some("sync-ws-1-aaaa"));
        assert_eq!(newest_transient_of(&snaps_of(vec![stop]), "ws-1").as_deref(), Some("stop-ws-1-7"));
    }

    fn replica_with_branches(volume: &str, node: &str, phase: &str, branches: serde_json::Value) -> crd::VolumeReplica {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": format!("{volume}.{node}"), "uid": format!("uid-{node}")},
            "spec": {"volume": volume, "node": node},
            "status": {"phase": phase, "branches": branches},
        }))
        .unwrap()
    }

    /// The whole placement bar, in one function: the NAME must match. A `Synced` row whose
    /// branches still name the previous sync point is a replica that has not pulled the stop cut
    /// — exactly the retention case the spec calls out — and must not be allowed to start it.
    #[test]
    fn up_to_date_compares_names_never_phases_or_clocks() {
        let holding = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({"ws-1": "sync-ws-1-bbbb"}));
        let behind = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({"ws-1": "sync-ws-1-aaaa"}));
        assert!(up_to_date(&holding, "ws-1", Some("sync-ws-1-bbbb")));
        assert!(!up_to_date(&behind, "ws-1", Some("sync-ws-1-bbbb")));
        assert!(!up_to_date(&holding, "ws-2", Some("sync-ws-2-cccc")), "another worktree's branch is not this one's");
    }

    /// A running source's clone lands on the OWNER by arithmetic, not by policy: at the instant
    /// of the cut the owner is the only node up to date for that worktree. There is no same-node
    /// rule in the code, and this test asserts the reason, not just the result.
    #[test]
    fn a_running_sources_clone_lands_on_the_owner_because_nothing_else_is_up_to_date_yet() {
        let newest = Some("clone-ws-1-cafe");
        let peer = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({"ws-1": "sync-ws-1-old"}));
        assert!(!up_to_date(&peer, "ws-1", newest), "the peer has not pulled the fresh cut yet");
        // The owner needs no row at all: it holds the bytes by construction (Task 5's may_claim).
        assert_eq!(up_to_date_nodes("ws-1", newest, &[peer]), Vec::<String>::new());
    }

    /// Once the peer HAS pulled the cut, both nodes qualify and rendezvous decides — the same
    /// deterministic hash a start uses, so a retry lands on the same answer.
    #[test]
    fn once_a_peer_holds_the_cut_rendezvous_decides_between_them() {
        let newest = Some("clone-ws-1-cafe");
        let peer = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({"ws-1": "clone-ws-1-cafe"}));
        assert_eq!(up_to_date_nodes("ws-1", newest, &[peer]), vec!["node-b".to_string()]);
        let candidates = vec!["node-a".to_string(), "node-b".to_string()];
        assert_eq!(
            preferred_node("vol-1", &candidates),
            preferred_node("vol-1", &candidates),
            "deterministic: a retry lands on the same node"
        );
    }

    /// No transient at all (never ran, or a fresh restore): plain `Synced` is the right bar —
    /// a Synced replica holds every Ready commit, which is all there is to hold.
    #[test]
    fn with_no_transient_plain_synced_is_up_to_date() {
        let synced = replica_with_branches("vol-1", "node-b", "Synced", serde_json::json!({}));
        let syncing = replica_with_branches("vol-1", "node-b", "Syncing", serde_json::json!({}));
        assert!(up_to_date(&synced, "ws-1", None));
        assert!(!up_to_date(&syncing, "ws-1", None));
        assert!(!up_to_date(&syncing, "ws-1", Some("sync-ws-1-bbbb")), "mid-pull is never up to date");
    }

    /// The other half: of the transients this node DOES hold for a worktree, exactly one — the
    /// highest generation — is reported. An older held sync point is still on disk and still
    /// servable, but naming it would make this node look behind to `up_to_date`.
    #[tokio::test]
    async fn a_pull_pass_reports_only_the_newest_held_transient_per_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let created = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": "vol-1.node-b", "uid": "r-uid"},
            "spec": {"volume": "vol-1", "node": "node-b"},
            "status": {"phase": "Syncing", "branches": {}},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![
                transient_gen("sync-ws-1-old", "vol-1", "ws-1", 2),
                transient_gen("sync-ws-1-aaaa", "vol-1", "ws-1", 5),
                transient_gen("sync-ws-1-unheld", "vol-1", "ws-1", 9),
                transient_gen("sync-ws-2-cccc", "vol-1", "ws-2", 1),
            ])},
            Route { method: "GET", path: "/api/v1/namespaces/kube-system/pods".into(), status: 200, body: list_of("Pod", vec![]) },
            Route { method: "GET", path: format!("{VOLREPLICAS}/vol-1.node-b"), status: 200, body: created.clone() },
            Route { method: "PUT", path: format!("{VOLREPLICAS}/vol-1.node-b/status"), status: 200, body: created },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        // `local_commits` is a plain listing of `snap/{volume}` — a directory per held subvolume.
        for held in ["sync-ws-1-old", "sync-ws-1-aaaa", "sync-ws-2-cccc"] {
            std::fs::create_dir_all(ctx.engine.pool.snap_dir("vol-1").join(held)).unwrap();
        }

        let http = peer_http_client().unwrap();
        pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1", &[]).await;

        let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
        let branches = &sent[0]["status"]["branches"];
        assert_eq!(branches["ws-1"], "sync-ws-1-aaaa", "the newest HELD one, not the newest listed: {branches:?}");
        assert_eq!(branches["ws-2"], "sync-ws-2-cccc");
        assert_eq!(branches.as_object().unwrap().len(), 2, "one entry per worktree: {branches:?}");
    }

    /// The pull pass writes what it HOLDS, not what it listed: a transient whose subvolume never
    /// landed here must not appear in `branches`, or this node advertises data it cannot serve.
    #[tokio::test]
    async fn a_pull_pass_records_only_the_transients_it_actually_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let created = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": "vol-1.node-b", "uid": "r-uid"},
            "spec": {"volume": "vol-1", "node": "node-b"},
            "status": {"phase": "Syncing", "branches": {}},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![
                transient_gen("sync-ws-1-aaaa", "vol-1", "ws-1", 5),
            ])},
            Route { method: "GET", path: "/api/v1/namespaces/kube-system/pods".into(), status: 200, body: list_of("Pod", vec![]) },
            Route { method: "GET", path: format!("{VOLREPLICAS}/vol-1.node-b"), status: 200, body: created.clone() },
            Route { method: "PUT", path: format!("{VOLREPLICAS}/vol-1.node-b/status"), status: 200, body: created },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        // No local commits: nothing was pulled, so nothing is held.
        std::fs::create_dir_all(ctx.engine.pool.snap_dir("vol-1")).unwrap();

        let http = peer_http_client().unwrap();
        pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1", &[]).await;

        let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["status"]["phase"], "Syncing");
        assert!(
            sent[0]["status"]["branches"].as_object().is_none_or(|b| b.is_empty()),
            "a transient this node does not hold must never appear in branches: {:?}",
            sent[0]["status"]["branches"]
        );
    }

    // ---------------------------------------------------------------------------------------
    // Task 2: the wake. A stop or a clone pokes every placeable peer so the pull happens in
    // seconds instead of at the next `WS_REPLICA_SECS` beat.
    // ---------------------------------------------------------------------------------------

    /// One POST per live peer, never to myself, and an unreachable peer is a warn — the ticker
    /// still comes, so a wake that cannot be delivered must never fail the stop that sent it.
    /// Nothing is listening on `:8444` here, so this is also the unreachable case: it must return
    /// normally rather than propagate anything.
    #[tokio::test]
    async fn wake_peers_posts_once_per_live_peer_and_skips_me() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route {
            method: "GET",
            path: "/api/v1/namespaces/kube-system/pods".into(),
            status: 200,
            body: list_of("Pod", vec![agent_pod("node-b", "127.0.0.1")]),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        wake_peers(&ctx, &["node-a".to_string(), "node-b".to_string()], "s3cret").await;

        // node-a is me: no address is ever resolved for it, so exactly one pod lookup happens —
        // the one for node-b, which is the POST that was attempted and failed.
        let looked_up = rec.requests().into_iter().filter(|r| r.contains("/pods?")).count();
        assert_eq!(looked_up, 1, "one address lookup, for the peer only: {:?}", rec.requests());
    }

    /// The POST really lands: a live peer's listener fires ITS pull notify. Asserted against a real
    /// server because `agent_pod_addr` hard-codes `:8444` and the kube Recorder never sees a peer
    /// dial — so the notify on the far side is the only proof the request was made.
    #[tokio::test]
    async fn a_wake_reaches_a_live_peers_notify() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, _rec) = mock_client(vec![]);
        let peer_state = PeerState::new(client, tmp.path().to_string_lossy().into(), "node-b".into(), "s3cret".into(), "btrfs".into());
        let peer_notify = peer_state.pull_wake.clone();
        let peer_server = serve_on_the_peer_port(router(peer_state)).await;

        let routes = vec![Route {
            method: "GET",
            path: "/api/v1/namespaces/kube-system/pods".into(),
            status: 200,
            body: list_of("Pod", vec![agent_pod("node-b", "127.0.0.1")]),
        }];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);

        wake_peers(&ctx, &["node-a".to_string(), "node-b".to_string()], "s3cret").await;

        assert!(
            tokio::time::timeout(Duration::from_millis(500), peer_notify.notified()).await.is_ok(),
            "the peer's pull notify must have been fired by the POST"
        );
        // Before the guard: see `PeerServer`.
        peer_server.stop().await;
    }

    /// The coalescing rule itself: a burst of wakes during one pass is ONE more pass, and the pass
    /// after that waits. Driven through `after_pass`, so the count is asserted rather than timed.
    #[test]
    fn a_burst_of_wakes_during_a_pass_buys_exactly_one_more_pass() {
        let wake = tokio::sync::Notify::new();
        let mut misses = 0;
        assert_eq!(after_pass(&wake, false, &mut misses), Next::Wait, "no wake, no extra pass");
        for _ in 0..5 {
            wake.notify_one();
        }
        assert_eq!(after_pass(&wake, false, &mut misses), Next::RunAgain, "a wake during the pass runs it again");
        assert_eq!(after_pass(&wake, false, &mut misses), Next::Wait, "five wakes are one permit, not five passes");
    }

    /// F4 (drill, 2026-09-03): a pass that could not fetch a commit waited out the full tick. It
    /// now comes back in 30 s — but a pending wake still wins, because a stop waiting on a replica
    /// must never be delayed by a retry.
    #[test]
    fn a_pass_that_missed_a_commit_retries_soon_unless_a_wake_is_pending() {
        let wake = tokio::sync::Notify::new();
        let mut misses = 0;
        assert_eq!(after_pass(&wake, true, &mut misses), Next::RetrySoon(RETRY_SOON));
        wake.notify_one();
        assert_eq!(after_pass(&wake, true, &mut misses), Next::RunAgain, "a pending wake beats the retry");
    }

    /// Round 2: an unfetchable commit used to pin the whole node at a 30 s pass forever. The delay
    /// doubles per consecutive miss, caps at the ordinary tick, and a single clean pass resets it.
    #[test]
    fn consecutive_misses_back_off_to_the_ordinary_tick_and_one_clean_pass_resets() {
        let cap = replica_interval();
        let wake = tokio::sync::Notify::new();
        let mut misses = 0;
        let delays: Vec<Duration> = (0..6)
            .map(|_| match after_pass(&wake, true, &mut misses) {
                Next::RetrySoon(d) => d,
                other => panic!("expected a retry, got {other:?}"),
            })
            .collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(30),
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(240),
                // Capped: 480 s would be longer than the beat it is meant to accelerate.
                cap,
                cap,
            ]
        );

        assert_eq!(after_pass(&wake, false, &mut misses), Next::Wait, "a clean pass goes back to the tick");
        assert_eq!(misses, 0, "and forgets the streak");
        assert_eq!(after_pass(&wake, true, &mut misses), Next::RetrySoon(RETRY_SOON), "so the next miss starts over at 30 s");
    }

    fn agent_pod(node: &str, ip: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": format!("agent-{node}"), "namespace": "kube-system"},
            "spec": {"nodeName": node},
            "status": {"podIP": ip},
        })
    }
}
