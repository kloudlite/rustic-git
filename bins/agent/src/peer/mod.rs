//! Replication's transport, both halves, plus the listener that ties them together. It is
//! PULL-based: `pull::pull_beat` decides which snapshots this node is missing and GETs them from a
//! peer that has them, and `snapshot` (below) serves the other side of that — `btrfs send`'s stdout
//! streamed as the response body. A node therefore only ever receives what it asked for, and no
//! peer can push bytes at it.
//!
//! An unset `WS_PEER_SECRET` disables both halves (`lib.rs` never spawns `serve`, and every dial
//! in this file returns early) — fail-closed: no secret configured means no root-run `btrfs send`
//! reachable from the network, ever.
//!
//! The four halves live in their own modules: `placement` (who may hold a volume, and who is
//! alive enough to be asked), `wake` (the wake protocol), `pull` (this node's own puller), and
//! `sweeps` (every sweep — dead-node, decommission, orphan, retire). This file keeps only the
//! router, the send side (`snapshot`), and the auth/framing both sides share.

mod placement;
mod pull;
mod sweeps;
mod wake;
#[cfg(test)]
mod tests;

pub use pull::{peer_http_client, pull_beat, pull_one, receive_ceiling, replica_interval};
pub use wake::wake_peers;
// Some of these are not dialled through `crate::peer::…` by any caller today (tests reach the
// submodule directly, `use super::<mod>::*`), but the path is the contract this split promised to
// hold: every name a caller outside this module could already reach stays reachable the same way.
#[allow(unused_imports)]
pub(crate) use placement::{
    decommissioning, live_nodes, newest_transient, node_dead_secs, node_is_dead, placeable_nodes, pool_nodes, preferred_node,
    unplaceable, up_to_date, up_to_date_nodes,
};
#[allow(unused_imports)]
pub(crate) use sweeps::{mark_parent, sweep_volumes, unplace_parent, volume_decision, VolumeVerdict};
#[allow(unused_imports)]
pub(crate) use wake::{after_pass, Next, MIN_WAKE_GAP, RETRY_SOON};
pub(crate) use crd::newest_transient_of;

use crate::controller::Ctx;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use kube::api::Api;
use kloudlite_git_storage::store::valid_segment;
use kloudlite_git_workspaces::crd;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

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
    /// The same live handle as `Ctx::settings` — the serve deadline and the receive ceiling are
    /// read off it per request, never cached at listener-bind time.
    pub settings: crate::controller::Settings,
}

impl PeerState {
    /// The one constructor — `sends` starts empty and is never meaningfully set any other
    /// way, so nothing outside this module (tests included) builds a `PeerState` by struct
    /// literal.
    #[allow(clippy::too_many_arguments)]
    pub fn new(client: kube::Client, pool: String, node: String, secret: String, btrfs_bin: String, settings: crate::controller::Settings) -> PeerState {
        PeerState {
            client,
            pool,
            node,
            secret,
            btrfs_bin,
            sends: StdMutex::new(HashMap::new()),
            pull_wake: Arc::new(tokio::sync::Notify::new()),
            settings,
        }
    }

    pub fn from_ctx(ctx: &Ctx, secret: String) -> PeerState {
        // The listener's notify must be the PULLER's, not a fresh one: a wake that fired a
        // private `Notify` would be a 204 nobody is waiting on.
        PeerState {
            pull_wake: ctx.pull_wake.clone(),
            ..PeerState::new(ctx.client.clone(), ctx.pool.clone(), ctx.node.clone(), secret, "btrfs".into(), ctx.settings.clone())
        }
    }

    fn send_lock(&self, id: &str) -> Arc<AsyncMutex<()>> {
        self.sends.lock().unwrap_or_else(|p| p.into_inner()).entry(id.to_string()).or_default().clone()
    }
}

pub fn router(state: PeerState) -> Router {
    Router::new()
        .route("/peer/v1/snapshot/{volume}/{name}", get(snapshot))
        // A poke, not a transfer: the body is empty and the answer is 204. Same secret as the
        // snapshot route and the same NetworkPolicy, because it drives the same root-run machinery.
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
///
/// `==` on the digests, deliberately, and NOT a constant-time-compare crate: the values compared
/// are SHA-256 digests of both sides, so an early-exit `memcmp` over them leaks where two DIGESTS
/// differ, which says nothing about the secret. The length-independence above is the property that
/// matters and it is already held. Changing this buys nothing.
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

#[derive(serde::Deserialize)]
struct SnapshotQuery {
    parent: Option<String>,
    max: Option<u64>,
}

/// The pull side's send: streams `btrfs send [-p parent] snap_dir/{name}`'s stdout as the response
/// body. Auth and `valid_segment` before anything the
/// path could steer — the body here is a root-run `btrfs send`.
async fn snapshot(
    State(state): State<Arc<PeerState>>,
    headers: HeaderMap,
    Path((volume, name)): Path<(String, String)>,
    Query(q): Query<SnapshotQuery>,
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

    // The puller declares what it will accept; a source that cannot fit a full send under it says
    // so BEFORE streaming. A truncated body after a 200 costs both sides the whole transfer, and
    // the puller cannot tell it from a crashed `btrfs send`. One Volume GET, on a path that is
    // about to spawn a root `btrfs send` and stream tens of GiB — not a cost worth avoiding.
    if let Some(max) = q.max {
        let quota =
            Api::<crd::Volume>::all(state.client.clone()).get_opt(&volume).await.ok().flatten().map(|v| v.spec.quota_gb).unwrap_or(0);
        if max < receive_ceiling(quota, &state.settings) {
            return (StatusCode::PAYLOAD_TOO_LARGE, Body::empty()).into_response();
        }
    }

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
    let (volume_id, snapshot_name) = (volume.clone(), name.clone());
    let stderr_task = child.stderr.take().map(|mut se| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut se, &mut buf).await;
            buf
        })
    });
    let killer = KillOnDrop { stdout, child: Some(child), stderr_task, volume: volume_id, name: snapshot_name, _guard: guard };
    // Dropping the stream on timeout drops `KillOnDrop`, which kills and reaps the child AND
    // releases the send lock — the whole point. The puller sees a truncated body after its 200,
    // which is the case it already handles (`pull_one`'s failed-receive path deletes the partial
    // and tries the next source).
    let stream = tokio_util::io::ReaderStream::new(killer);
    let deadline = tokio::time::Instant::now() + serve_timeout(&state.settings);
    let body = Body::from_stream(futures::stream::unfold(Box::pin(stream), move |mut s| async move {
        match tokio::time::timeout_at(deadline, futures::StreamExt::next(&mut s)).await {
            Ok(Some(chunk)) => Some((chunk, s)),
            Ok(None) => None,
            // The deadline is on the WHOLE body, not per chunk: a puller trickling one byte a
            // minute is the same wedge as one reading nothing.
            Err(_) => Some((Err(std::io::Error::other("peer: serve timeout")), s)),
        }
    }));
    (StatusCode::OK, body).into_response()
}

/// Wraps a streamed `btrfs send`'s stdout so a response body dropped mid-stream (a disconnected
/// or timed-out puller) kills and reaps the child instead of leaking a root process writing to a
/// pipe nobody reads any more — the same failure `post_send`'s `kill()` exists to avoid on the
/// sending side, mirrored here on the receiving-of-the-request-but-sending-the-body side.
struct KillOnDrop {
    stdout: tokio::process::ChildStdout,
    child: Option<tokio::process::Child>,
    /// Drains stderr concurrently with the streamed body — see the comment at the `snapshot`
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
                tracing::warn!(%volume, %name, status = ?exit, stderr = %tail_str(&stderr, 300), "snapshot: btrfs send exited non-zero");
            }
        });
    }
}

/// `WS_PEER_SERVE_TIMEOUT_SECS`, default 900. Deliberately SHORTER than the client's
/// `send_timeout` (3600) and a separate knob: the client's bound protects the puller, this one
/// protects the SOURCE. The per-volume send lock is held for the life of this body, so a puller
/// that opens the connection and stops reading otherwise blocks every other node's pull of that
/// volume for the client's full hour — one wedged connection stopping a volume's replication
/// fleet-wide. A legitimate send that needs longer than 15 minutes of TOTAL wall clock raises
/// this; the puller retries from the next source either way.
fn serve_timeout(settings: &crate::controller::Settings) -> Duration {
    Duration::from_secs(settings.load().peer_serve_timeout_secs)
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

