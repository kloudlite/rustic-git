//! The receive side of replication: one node's `btrfs send` lands here as an HTTP POST body,
//! piped straight into `btrfs receive`. Sender-initiated (this node never asks for a stream) so
//! the listener only ever does what an authenticated peer told it to.
//!
//! `WS_REPLICA_COUNT` defaults to 1 (replication off), and an unset `WS_PEER_SECRET` disables
//! this listener entirely (`lib.rs` never spawns `serve`) — the fail-closed half of that: no
//! secret configured means no root-run `btrfs receive` reachable from the network, ever.

use crate::controller::{replace_status, Ctx};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use futures::TryStreamExt;
use rustic_git_storage::store::valid_segment;
use rustic_git_workspaces::crd;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;
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
    /// Serializes the whole receive (before/receive/diff/cleanup/widen) per volume id. A retried
    /// sender can overlap with the receive it is retrying; without this, the loser's before/after
    /// diff is computed against a directory the winner is concurrently writing into, and the
    /// loser's cleanup can `btrfs subvolume delete` the winner's just-landed snapshot — a node
    /// left in `compatibleNodes` whose subvolume is gone. Chosen over a temp-dir-then-rename
    /// because the sender's ancestor-first ordering already wants receives for one id sequential.
    receives: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl PeerState {
    /// The one constructor — `receives` starts empty and is never meaningfully set any other
    /// way, so nothing outside this module (tests included) builds a `PeerState` by struct
    /// literal.
    pub fn new(client: kube::Client, pool: String, node: String, secret: String, btrfs_bin: String) -> PeerState {
        PeerState { client, pool, node, secret, btrfs_bin, receives: StdMutex::new(HashMap::new()) }
    }

    pub fn from_ctx(ctx: &Ctx, secret: String) -> PeerState {
        PeerState::new(ctx.client.clone(), ctx.pool.clone(), ctx.node.clone(), secret, "btrfs".into())
    }

    fn receive_lock(&self, id: &str) -> Arc<AsyncMutex<()>> {
        self.receives.lock().unwrap_or_else(|p| p.into_inner()).entry(id.to_string()).or_default().clone()
    }
}

pub fn router(state: PeerState) -> Router {
    Router::new()
        .route("/peer/v1/snapshots/{owner}/{id}", get(snapshots))
        .route("/peer/v1/replicate/{owner}/{id}", post(replicate))
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

async fn snapshots(State(state): State<Arc<PeerState>>, headers: HeaderMap, Path((_owner, id)): Path<(String, String)>) -> impl IntoResponse {
    if !secret_ok(&headers, &state.secret) {
        return (StatusCode::UNAUTHORIZED, axum::Json(Vec::<String>::new())).into_response();
    }
    if !valid_segment(&id) {
        return (StatusCode::BAD_REQUEST, axum::Json(Vec::<String>::new())).into_response();
    }
    let names = subvolume_names(&std::path::Path::new(&state.pool).join("repl").join(&id));
    axum::Json(names).into_response()
}

fn subvolume_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    names
}

async fn replicate(
    State(state): State<Arc<PeerState>>,
    headers: HeaderMap,
    Path((owner, id)): Path<(String, String)>,
    body: Body,
) -> impl IntoResponse {
    // Auth first, before anything the body or the path segments could steer: the body is a
    // root-run `btrfs receive`, and this header is the only gate in front of it.
    if !secret_ok(&headers, &state.secret) {
        return (StatusCode::UNAUTHORIZED, String::new()).into_response();
    }
    // {owner} is not otherwise used: it is carried for URL surface and log legibility, not as an
    // authorization boundary. The secret is the boundary — a holder of it can already name any
    // owner, so checking ownership here would add a kube GET per receive without moving the line
    // that actually matters.
    if !valid_segment(&owner) || !valid_segment(&id) {
        return (StatusCode::BAD_REQUEST, String::new()).into_response();
    }

    // One receive at a time per id: see the `receives` field's doc comment on `PeerState` for
    // why a second, concurrent request for the same id must not run its before/after diff and
    // cleanup against a directory the first request is still writing.
    let lock = state.receive_lock(&id);
    let _guard = lock.lock().await;

    let dir = std::path::Path::new(&state.pool).join("repl").join(&id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("create {}: {e}", dir.display())).into_response();
    }
    let before = subvolume_names(&dir);

    let bin_parts: Vec<&str> = state.btrfs_bin.split_whitespace().collect();
    let Some((prog, prefix)) = bin_parts.split_first() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "empty btrfs_bin".to_string()).into_response();
    };
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(prefix).arg("receive").arg(&dir).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            cleanup_partials(&state.btrfs_bin, &dir, &before).await;
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn btrfs receive: {e}")).into_response();
        }
    };
    let mut stdin = child.stdin.take().expect("stdin was piped");
    // No explicit body size limit: a send stream is legitimately tens of GiB, and the limit that
    // actually matters is pool space — `btrfs receive` hitting ENOSPC IS the enforcement.
    let mut reader = StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));
    // A time bound, not a size bound: an authenticated connection that stalls mid-stream would
    // otherwise pin a root `btrfs receive` and its directory forever. `WS_PEER_RECV_TIMEOUT_SECS`
    // (default 3600) is generous on purpose — tens of GiB over a slow link is a legitimate
    // receive, and this exists to unpin a wedged process, not to police link speed. A timeout
    // takes the same path as any other failed receive: kill, diff, cleanup, 500.
    let recv_timeout = std::time::Duration::from_secs(
        std::env::var("WS_PEER_RECV_TIMEOUT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3600),
    );
    let copy_result = tokio::time::timeout(recv_timeout, tokio::io::copy(&mut reader, &mut stdin)).await;
    let _ = stdin.shutdown().await;
    drop(stdin);
    let ok = match copy_result {
        Ok(Ok(_)) => matches!(child.wait().await, Ok(s) if s.success()),
        Ok(Err(_)) => {
            let _ = child.wait().await;
            false
        }
        Err(_) => {
            // Timed out: the copy task is dropped here, but the child still holds the pipe end
            // and may still be running — kill it before diffing, or its own eventual exit could
            // race the cleanup below.
            let _ = child.kill().await;
            let _ = child.wait().await;
            false
        }
    };
    let after = subvolume_names(&dir);
    let new_names: Vec<&String> = after.iter().filter(|n| !before.contains(n)).collect();

    if !ok {
        // Keep-biased in the failure direction that matters here: a partial subvolume left behind
        // would let `compatibleNodes` advertise a node that cannot actually start the workload.
        for name in &new_names {
            delete_subvolume(&state.btrfs_bin, &dir.join(name)).await;
        }
        return (StatusCode::INTERNAL_SERVER_ERROR, "receive failed".to_string()).into_response();
    }

    let Some(received) = new_names.first().map(|s| s.to_string()) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "receive reported success but produced no new subvolume".to_string()).into_response();
    };

    if let Err(e) = widen_parent(&state, &owner, &id).await {
        // The bytes are safely on disk; a failed status write only means this node is not yet
        // advertised as a compatible placement target. Log and still answer 200 — the caller
        // asked for a receive, not a status write, and the next reconcile beat can widen it too
        // if this node ever runs one for that object.
        tracing::warn!(%owner, %id, error = %e, "receive landed but widening compatibleNodes failed");
    }

    (StatusCode::OK, received).into_response()
}

async fn cleanup_partials(btrfs_bin: &str, dir: &std::path::Path, before: &[String]) {
    for name in subvolume_names(dir) {
        if !before.contains(&name) {
            delete_subvolume(btrfs_bin, &dir.join(name)).await;
        }
    }
}

async fn delete_subvolume(btrfs_bin: &str, path: &std::path::Path) {
    let parts: Vec<&str> = btrfs_bin.split_whitespace().collect();
    let Some((prog, prefix)) = parts.split_first() else { return };
    let _ = tokio::process::Command::new(prog).args(prefix).arg("subvolume").arg("delete").arg(path).status().await;
}

/// `compatibleNodes` says "every node known to hold this object's data" — writable only by the
/// node that just proved it, by finishing a clean receive. Tries `Workspace` first, then
/// `Environment`: a replica for a parent that exists on neither (a deleted volume whose replica
/// outlived it) is not an error — the janitor sweeps `repl/` entries no lineage names, same as
/// `recv/`.
///
/// `status.unwrap_or_default()` below fabricates an empty status for a parent that has none yet
/// (still `Pending`, never reconciled). A `replace_status` PUT against that default may itself be
/// rejected by the API server — acceptable here: the caller only warns and answers 200 either
/// way (see the call site), and the reconcile loop's own next pass writes a real status and
/// carries this node forward the next time a receive lands.
async fn widen_parent(state: &PeerState, _owner: &str, id: &str) -> Result<(), kube::Error> {
    let ws_api: kube::Api<crd::Workspace> = kube::Api::all(state.client.clone());
    if let Some(mut w) = ws_api.get_opt(id).await? {
        for attempt in 0..2 {
            let status = w.status.clone().unwrap_or_default();
            let mut next = status.clone();
            next.compatible_nodes = crate::claim::with_me(&status.compatible_nodes, &state.node);
            if next == status {
                return Ok(()); // Already known: re-running a clean receive is a no-op, not a rewrite.
            }
            match replace_status(&ws_api, &w, "Workspace", serde_json::to_value(&next).map_err(kube::Error::SerdeError)?).await {
                Ok(()) => return Ok(()),
                Err(kube::Error::Api(s)) if s.code == 409 && attempt == 0 => w = ws_api.get(id).await?,
                Err(e) => return Err(e),
            }
        }
        return Ok(());
    }

    let env_api: kube::Api<crd::Environment> = kube::Api::all(state.client.clone());
    if let Some(mut e) = env_api.get_opt(id).await? {
        for attempt in 0..2 {
            let status = e.status.clone().unwrap_or_default();
            let mut next = status.clone();
            next.compatible_nodes = crate::claim::with_me(&status.compatible_nodes, &state.node);
            if next == status {
                return Ok(());
            }
            match replace_status(&env_api, &e, "Environment", serde_json::to_value(&next).map_err(kube::Error::SerdeError)?).await {
                Ok(()) => return Ok(()),
                Err(kube::Error::Api(s)) if s.code == 409 && attempt == 0 => e = env_api.get(id).await?,
                Err(e2) => return Err(e2),
            }
        }
    }
    // Neither API had it: a replica with no parent left at all. 200 either way — see doc comment.
    Ok(())
}
