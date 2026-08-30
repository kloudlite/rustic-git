//! The receive side of replication: one node's `btrfs send` lands here as an HTTP POST body,
//! piped straight into `btrfs receive`. Sender-initiated (this node never asks for a stream) so
//! the listener only ever does what an authenticated peer told it to.
//!
//! `WS_REPLICA_COUNT` defaults to 1 (replication off), and an unset `WS_PEER_SECRET` disables
//! this listener entirely (`lib.rs` never spawns `serve`) — the fail-closed half of that: no
//! secret configured means no root-run `btrfs receive` reachable from the network, ever.

use crate::controller::{replace_status, volume_is_ready, Ctx};
use crate::janitor::SWEEP_MIN_AGE;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::ListParams;
use kube::ResourceExt;
use rustic_git_storage::store::valid_segment;
use rustic_git_workspaces::crd;
use rustic_git_workspaces::replicate;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
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

/// The node a CRD is placed on — Workspace checked first, then Environment, same pair
/// `widen_parent` walks. `Ok(None)` is "neither exists" (a replica whose parent is gone); `Err`
/// is a real API problem, which must never be confused with the object actually being gone —
/// the caller keep-biases on it.
async fn owner_node(client: &kube::Client, id: &str) -> Result<Option<String>, kube::Error> {
    let ws_api: kube::Api<crd::Workspace> = kube::Api::all(client.clone());
    if let Some(w) = ws_api.get_opt(id).await? {
        return Ok(Some(w.status.unwrap_or_default().node_name));
    }
    let env_api: kube::Api<crd::Environment> = kube::Api::all(client.clone());
    if let Some(e) = env_api.get_opt(id).await? {
        return Ok(Some(e.status.unwrap_or_default().node_name));
    }
    Ok(None)
}

/// The mirror of `widen_parent`: removes `node` from `compatibleNodes` instead of adding it.
/// ponytail: near-identical to `widen_parent` but for `with_me`/`without_me` — a generic version
/// parametrized over `Workspace`/`Environment` would need a shared status trait neither type has
/// today; worth it if a third writer of `compatibleNodes` shows up, not before.
async fn narrow_parent(client: &kube::Client, node: &str, id: &str) -> Result<(), kube::Error> {
    let ws_api: kube::Api<crd::Workspace> = kube::Api::all(client.clone());
    if let Some(mut w) = ws_api.get_opt(id).await? {
        for attempt in 0..2 {
            let status = w.status.clone().unwrap_or_default();
            let mut next = status.clone();
            next.compatible_nodes = crate::claim::without_me(&status.compatible_nodes, node);
            if next == status {
                return Ok(()); // Already absent: a retried deselection is a no-op.
            }
            match replace_status(&ws_api, &w, "Workspace", serde_json::to_value(&next).map_err(kube::Error::SerdeError)?).await {
                Ok(()) => return Ok(()),
                Err(kube::Error::Api(s)) if s.code == 409 && attempt == 0 => w = ws_api.get(id).await?,
                Err(e) => return Err(e),
            }
        }
        return Ok(());
    }

    let env_api: kube::Api<crd::Environment> = kube::Api::all(client.clone());
    if let Some(mut e) = env_api.get_opt(id).await? {
        for attempt in 0..2 {
            let status = e.status.clone().unwrap_or_default();
            let mut next = status.clone();
            next.compatible_nodes = crate::claim::without_me(&status.compatible_nodes, node);
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
    Ok(())
}

/// `{pool}/vol/{id}.replicated-gen-*` sidecars for an id whose CRD is gone — dead weight once the
/// replica they gate is itself deleted, swept in the same pass rather than left to rot forever.
fn sweep_gate_files(pool_root: &str, id: &str) {
    let dir = FsPath::new(pool_root).join("vol");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let prefix = format!("{id}.replicated-gen-");
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `entry`'s mtime is inside `min_age` of now — unreadable metadata is treated as "just written",
/// same keep-biased default every other sweep in this tree uses.
fn younger_than(entry: &std::fs::DirEntry, min_age: std::time::Duration) -> bool {
    entry.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().map(|e| e < min_age).unwrap_or(true)).unwrap_or(true)
}

/// `btrfs subvolume delete` every snapshot under `dir`, then the directory itself — best-effort,
/// same shape the old `janitor_sweep_repl` used before this reconcile replaced it.
async fn delete_repl_dir(dir: &FsPath, id: &str) {
    for sub in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        delete_subvolume("btrfs", &sub.path()).await;
    }
    if let Err(e) = std::fs::remove_dir_all(dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%id, dir = %dir.display(), error = %e, "replicate: reconcile: remove repl dir");
        }
    }
}

/// Reconciles `{pool}/repl/*` against the CRDs — the mechanism that replaces the old
/// vol/-keyed janitor sweep, which deleted every pure standby's replica (it has no `vol/{id}`
/// of its own, only `repl/{id}`) the moment it went idle past the age floor. Runs on every node
/// with a peer secret configured — see the gating comment at the call site.
///
/// Per `repl/{id}` directory: `owner_node` says what's true.
/// - Lookup error: keep everything, logged — a transient API hiccup must never cost real data.
/// - CRD gone (`Ok(None)`): orphaned, delete past the age floor (a dir mid-first-receive is not
///   swept), and drop its gate sidecars.
/// - CRD present, this node IS the owner: this is the primary's own send staging — left alone.
/// - CRD present, not owner, and this node is no longer in `replicate::targets` for it:
///   deselected. Delete the replica AND remove this node from `compatibleNodes` — every node
///   computes the same rendezvous independently, so the standby discovering its own deselection
///   needs no message from anyone.
/// - CRD present and this node is still a target: keep.
async fn replica_reconcile(ctx: &Arc<Ctx>, candidates: &[String]) {
    let repl_root = FsPath::new(&ctx.pool).join("repl");
    let Ok(entries) = std::fs::read_dir(&repl_root) else { return };
    let count = replica_count();
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };

        match owner_node(&ctx.client, &id).await {
            Err(e) => tracing::warn!(%id, error = %e, "replicate: reconcile lookup; keeping everything"),
            Ok(None) => {
                if younger_than(&entry, SWEEP_MIN_AGE) {
                    continue;
                }
                delete_repl_dir(&p, &id).await;
                sweep_gate_files(&ctx.pool, &id);
            }
            Ok(Some(owner)) if owner == ctx.node => {} // this node's own staging: keep
            Ok(Some(owner)) => {
                let targets = replicate::targets(&id, &owner, candidates, count);
                if targets.iter().any(|t| t == &ctx.node) {
                    continue; // still selected
                }
                delete_repl_dir(&p, &id).await;
                if let Err(e) = narrow_parent(&ctx.client, &ctx.node, &id).await {
                    tracing::warn!(%id, error = %e, "replicate: removing self from compatibleNodes");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The send side: this node's beat, mirroring the home-push beat in `controller.rs`, decides which
// of its own volumes are due for which standby node and streams a `btrfs send` to that node's
// `replicate` handler above.
// ---------------------------------------------------------------------------------------------

/// `WS_REPLICA_COUNT`, default 1 (replication off — no send). Read on every beat, never cached:
/// the reconcile half (`replica_reconcile`) uses it too, and a standby must react to the count
/// being turned back down as promptly as it reacted to being turned up.
fn replica_count() -> usize {
    std::env::var("WS_REPLICA_COUNT").ok().and_then(|v| v.parse().ok()).unwrap_or(1)
}

/// `WS_REPLICA_SECS`, default 300 — same shape as `controller::home_push_interval`.
pub fn replica_interval() -> std::time::Duration {
    std::time::Duration::from_secs(std::env::var("WS_REPLICA_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300))
}

/// Whether `id` needs a send to a target it has not already caught up: strictly past the last
/// generation confirmed landed there, never equal — an unchanged volume costs one `subvolume
/// show` per beat and nothing else. `None` (no gate file yet) always sends: a target that has
/// never received anything needs everything.
fn replica_due(current_gen: u64, replicated_gen: Option<u64>) -> bool {
    replicated_gen.is_none_or(|g| current_gen > g)
}

/// `{pool}/vol/{id}.replicated-gen-{target}` — a sidecar beside `.pushed-gen`, one per target
/// since a volume can replicate to several standbys at different confirmed generations.
fn gate_path(pool: &str, id: &str, target: &str) -> PathBuf {
    FsPath::new(pool).join("vol").join(format!("{id}.replicated-gen-{target}"))
}

fn read_gate(path: &FsPath) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn write_gate(path: &FsPath, gen: u64) -> std::io::Result<()> {
    std::fs::write(path, gen.to_string())
}

/// `cloneOf`'s source, when the volume has one — the only source a running node's disk can share
/// extents against.
fn clone_of(v: &crd::Volume) -> Option<String> {
    match &v.spec.source {
        Some(crd::VolumeSource::CloneOf { volume }) => Some(volume.clone()),
        _ => None,
    }
}

/// The newest `g{gen}` snapshot name present on both listings — the delta base a send should
/// resume from, so an unbroken chain costs a small delta instead of the whole volume again. Names
/// are parsed as `g{u64}`, not string-sorted: `g9` must not out-rank `g10`.
fn newest_shared(mine: &[String], theirs: &[String]) -> Option<String> {
    mine.iter().filter(|n| theirs.contains(n)).filter_map(|n| gen_num(n).map(|g| (g, n.clone()))).max_by_key(|(g, _)| *g).map(|(_, n)| n)
}

fn gen_num(name: &str) -> Option<u64> {
    name.strip_prefix('g')?.parse().ok()
}

/// Resolves what `blob::spawn_send` should be given for one target — pinned here per the brief:
/// "the argument set IS the sharing model". `-p` resumes THIS volume's own chain (`parent_name`,
/// looked up under `repl_dir`); `-c` lets the receiver share a clone's ANCESTOR volume's extents
/// (`ancestor`, `(that volume's repl dir, its shared snapshot name)`) instead of the sender
/// re-shipping data the target already holds under a different id.
fn send_args(repl_dir: &FsPath, parent_name: Option<&str>, ancestor: Option<(&FsPath, &str)>) -> (Option<PathBuf>, Vec<PathBuf>) {
    let parent = parent_name.map(|n| repl_dir.join(n));
    let clones = ancestor.map(|(dir, n)| dir.join(n)).into_iter().collect();
    (parent, clones)
}

/// The pool-eligible nodes, `rustic-git.io/pool=true`, name-sorted so `replicate::targets`'
/// rendezvous scoring is deterministic across every node running this beat.
async fn pool_nodes(client: &kube::Client) -> Result<Vec<String>, String> {
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

/// Short: a listing is one small JSON body, and a peer that stalls answering it must not park the
/// sequential beat behind it — see `send_timeout` for why the send itself gets a much longer one.
const GET_TIMEOUT: Duration = Duration::from_secs(10);

async fn remote_snapshots(http: &reqwest::Client, addr: &str, secret: &str, owner: &str, id: &str) -> Vec<String> {
    let url = format!("http://{addr}/peer/v1/snapshots/{owner}/{id}");
    match http.get(&url).header("x-peer-secret", secret).timeout(GET_TIMEOUT).send().await {
        Ok(resp) if resp.status().is_success() => resp.json().await.unwrap_or_default(),
        // A target that has never heard of this id (404/empty) or is briefly unreachable both
        // read as "nothing shared yet" — the send below just falls back to a fuller one.
        _ => Vec::new(),
    }
}

/// `WS_PEER_SEND_TIMEOUT_SECS`, default 3600 — the same generous shape as the receiver's
/// `WS_PEER_RECV_TIMEOUT_SECS`. A send is legitimately tens of GiB; this exists to unwedge a
/// connection that has actually stalled, not to police link speed.
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

/// RO-snapshots `id`'s live subvolume into `repl/{id}/g{gen}` if that generation is not already
/// staged there — one snapshot serves every target due this beat, so the second and later targets
/// for the same volume find it already present.
fn ensure_repl_snapshot(pool: &rustic_git_workspaces::engine::Pool, id: &str, gen: u64) -> Result<(), String> {
    let dst = pool.repl(id).join(format!("g{gen}"));
    if dst.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(pool.repl(id)).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("btrfs")
        .args(["subvolume", "snapshot", "-r", pool.live(id).to_str().unwrap(), dst.to_str().unwrap()])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(())
}

/// The fixed per-request coordinates a send needs, bundled so the request functions below stay
/// under clippy's argument-count lint — none of these vary between the incremental attempt and
/// its full-send retry.
struct SendTo<'a> {
    http: &'a reqwest::Client,
    addr: &'a str,
    secret: &'a str,
    owner: &'a str,
    id: &'a str,
    /// The `btrfs` binary to invoke — always `"btrfs"` in production (the one construction site
    /// in `replicate_beat`), a fake script in tests. Same shape as `PeerState::btrfs_bin` on the
    /// receive side; never an env var, since this selects which binary a root process executes.
    btrfs_bin: &'a str,
}

/// Streams one `btrfs send` for `id` at `gen` to `target`, retrying once as a full send (no `-p`,
/// no `-c`) on any non-2xx response — a `-c` refusal is indistinguishable from any other failure
/// at this layer, and a full send always succeeds if the incremental one could not.
async fn send_to_target(
    pool: &rustic_git_workspaces::engine::Pool,
    to: &SendTo<'_>,
    gen: u64,
    ancestor: Option<&str>,
) -> Result<(), String> {
    let remote = remote_snapshots(to.http, to.addr, to.secret, to.owner, to.id).await;
    let local = subvolume_names(&pool.repl(to.id));
    let parent = newest_shared(&local, &remote);

    let ancestor_pick = if let Some(anc) = ancestor {
        let remote_anc = remote_snapshots(to.http, to.addr, to.secret, to.owner, anc).await;
        let local_anc = subvolume_names(&pool.repl(anc));
        newest_shared(&local_anc, &remote_anc).map(|n| (pool.repl(anc), n))
    } else {
        None
    };
    let (parent_path, clones) =
        send_args(&pool.repl(to.id), parent.as_deref(), ancestor_pick.as_ref().map(|(d, n)| (d.as_path(), n.as_str())));

    let dst = pool.repl(to.id).join(format!("g{gen}"));
    // A non-2xx and a transport-level `Err` get the SAME treatment: a wrong `-p` (the receiver's
    // parent snapshot hand-deleted, say) can surface as a broken connection just as easily as a
    // clean status code, and either one must fall through to a full send rather than bricking this
    // volume's replication forever. Only the full attempt's own failure propagates.
    match post_send(to, &dst, parent_path, &clones).await {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(e) => tracing::warn!(id = %to.id, target = %to.addr, error = %e, "replicate: incremental send transport error, retrying full"),
    }
    // Full fallback: never `-p`/`-c` from a possibly-wrong guess. Its own warn, unconditional —
    // systematic `-c`/`-p` refusal (a stale or hand-deleted parent snapshot) is otherwise silent
    // as long as the full retry keeps succeeding, and that's exactly the signal an operator needs
    // to notice the incremental path stopped working.
    tracing::warn!(id = %to.id, target = %to.addr, "replicate: incremental send failed, falling back to a full send");
    if post_send(to, &dst, None, &[]).await? {
        return Ok(());
    }
    Err(format!("replicate {} -> {}: incremental and full send both failed", to.id, to.addr))
}

/// Runs one `btrfs send | POST`, checking BOTH halves before calling it a success: the HTTP
/// status (the receiver's own verdict) and the send child's own exit status (btrfs can fail
/// mid-stream in a way the receiver only sees as a truncated body, which `btrfs receive` may or
/// may not itself reject). stderr is drained concurrently with the POST, never after — reading it
/// only once the request finishes would let `btrfs send` block forever writing to a full pipe
/// nobody is emptying while the POST is still in flight.
async fn post_send(to: &SendTo<'_>, snapshot: &FsPath, parent: Option<PathBuf>, clones: &[PathBuf]) -> Result<bool, String> {
    let mut child = spawn_send_tokio(to.btrfs_bin, snapshot, parent.as_deref(), clones).map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("btrfs send: no stdout")?;
    let mut stderr = child.stderr.take().ok_or("btrfs send: no stderr")?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await;
        buf
    });
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(stdout));
    let url = format!("http://{}/peer/v1/replicate/{}/{}", to.addr, to.owner, to.id);
    let resp = to.http.post(&url).header("x-peer-secret", to.secret).timeout(send_timeout()).body(body).send().await;
    // A failed POST (timeout, transport error, non-2xx) means reqwest has already dropped the
    // body stream — nothing is draining `stdout` any more, so `btrfs send` blocks writing to a
    // full, unread pipe and a plain `wait()` would hang the beat right here, one layer below the
    // timeout that was supposed to unwedge it. `kill()` (SIGKILL, awaited so the reap actually
    // completes) makes the follow-up `wait()` return promptly on that path; a successful POST
    // means the child already finished writing and exited on its own, so `wait()` alone is fine.
    let post_failed = !matches!(&resp, Ok(r) if r.status().is_success());
    if post_failed {
        let _ = child.kill().await;
    }
    let exit = child.wait().await;
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    let exit_ok = matches!(&exit, Ok(s) if s.success());
    if !exit_ok {
        tracing::warn!(
            id = %to.id, target = %to.addr, status = ?exit, stderr = %tail_str(&stderr_bytes, 300),
            "replicate: btrfs send exited non-zero"
        );
    }
    match resp {
        Ok(r) => Ok(exit_ok && r.status().is_success()),
        Err(e) => Err(e.to_string()),
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

/// Deletes local `repl/{id}` snapshots older than the oldest generation ANY of `targets` still
/// needs — never touches a generation a target might still be behind. A target with no gate file
/// yet (nothing confirmed there ever) means "needs everything", so retention does nothing at all
/// until every target has landed at least once: keep-biased, same as every other sweep in this
/// tree.
fn retention_cleanup(pool: &rustic_git_workspaces::engine::Pool, pool_root: &str, id: &str, targets: &[String]) {
    let mut min_needed: Option<u64> = None;
    for t in targets {
        match read_gate(&gate_path(pool_root, id, t)) {
            Some(g) => min_needed = Some(min_needed.map_or(g, |m: u64| m.min(g))),
            None => return,
        }
    }
    let Some(min_needed) = min_needed else { return };
    let dir = pool.repl(id);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if gen_num(&name).is_some_and(|g| g < min_needed) {
            std::process::Command::new("btrfs").args(["subvolume", "delete"]).arg(dir.join(&name)).status().ok();
        }
    }
}

/// One pass of the sender beat — the spawned loop in `controller.rs` calls this on its own tick,
/// on its own blocking-safe async task. Every per-(volume, target) failure is a `tracing::warn!`
/// and a `continue`: the beat never aborts on one volume's bad day.
pub async fn replicate_beat(ctx: &Arc<Ctx>) {
    // Fail-closed, same rule the listener applies to itself: no secret, no dial to a peer this
    // node could not have authenticated to anyway.
    let secret = std::env::var("WS_PEER_SECRET").unwrap_or_default();
    if secret.is_empty() {
        return;
    }
    let candidates = match pool_nodes(&ctx.client).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "replicate: listing pool nodes");
            return;
        }
    };

    // The reconcile half runs on EVERY node with a secret configured, independent of the send
    // count below: a standby's own cleanup (an orphaned replica, a deselection) must keep working
    // even after the cluster-wide count is turned back down to 1 — see `replica_reconcile`.
    replica_reconcile(ctx, &candidates).await;

    let count = replica_count();
    if count <= 1 {
        return;
    }
    if let Err(e) = ctx.engine.sync_pool() {
        tracing::warn!(error = %e, "replicate: btrfs sync; skipping the send half of the beat");
        return;
    }

    let volumes: Vec<Arc<crd::Volume>> = ctx
        .volumes
        .state()
        .into_iter()
        .filter(|v| v.spec.node_name == ctx.node && v.metadata.deletion_timestamp.is_none())
        .filter(|v| !crd::is_home_volume(v))
        // A volume mid-teardown (its parent Stopped, pod already gone) is not worth a send this
        // beat — `volume_is_ready` is the same "materialized and not in flux" signal the home-push
        // beat trusts for an identical reason.
        .filter(|v| volume_is_ready(v))
        .collect();
    let by_id: HashMap<String, Arc<crd::Volume>> = volumes.iter().map(|v| (v.name_any(), v.clone())).collect();
    let pairs: Vec<(String, Option<String>)> = volumes.iter().map(|v| (v.name_any(), clone_of(v))).collect();

    let http = match peer_http_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "replicate: building the http client");
            return;
        }
    };
    for id in replicate::order_groups(&pairs) {
        let Some(v) = by_id.get(&id) else { continue };
        let targets = replicate::targets(&id, &ctx.node, &candidates, count);
        if targets.is_empty() {
            continue;
        }
        let (gen, due) = match due_targets(
            &targets,
            |t| read_gate(&gate_path(&ctx.pool, &id, t)),
            || ctx.engine.generation(&id).map_err(|e| e.to_string()),
            |g| ensure_repl_snapshot(&ctx.engine.pool, &id, g),
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%id, error = %e, "replicate: generation/snapshot");
                continue;
            }
        };

        for target in &due {
            let gate = gate_path(&ctx.pool, &id, target);
            let addr = match agent_pod_addr(&ctx.client, target).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(%id, %target, error = %e, "replicate: no peer address");
                    continue;
                }
            };
            let to = SendTo { http: &http, addr: &addr, secret: &secret, owner: &v.spec.owner, id: &id, btrfs_bin: "btrfs" };
            match send_to_target(&ctx.engine.pool, &to, gen, clone_of(v).as_deref()).await {
                Ok(()) => {
                    if let Err(e) = write_gate(&gate, gen) {
                        tracing::warn!(%id, %target, error = %e, "replicate: writing the gate file");
                    }
                }
                Err(e) => tracing::warn!(%id, %target, error = %e, "replicate: send"),
            }
        }
        if !due.is_empty() {
            retention_cleanup(&ctx.engine.pool, &ctx.pool, &id, &targets);
        }
    }
}

/// The per-volume decision `replicate_beat` makes: `generation` is read exactly once, before
/// `snapshot` ever runs — reading it a second time, or after the snapshot, could stamp a gate
/// file with a number the snapshot's own bytes don't actually contain (a write landing in the
/// gap). `snapshot` runs at most once, lazily, only if some target turns out due — an unmoved
/// volume must cost nothing beyond the one generation read (see `an_unmoved_generation_sends_nothing`).
fn due_targets(
    targets: &[String],
    replicated: impl Fn(&str) -> Option<u64>,
    generation: impl FnOnce() -> Result<u64, String>,
    mut snapshot: impl FnMut(u64) -> Result<(), String>,
) -> Result<(u64, Vec<String>), String> {
    let gen = generation()?;
    let due: Vec<String> = targets.iter().filter(|t| replica_due(gen, replicated(t))).cloned().collect();
    if !due.is_empty() {
        snapshot(gen)?;
    }
    Ok((gen, due))
}

#[cfg(test)]
mod sender_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The send argument set IS the sharing model: -p resumes this volume's own chain, -c lets a
    /// clone reference its ancestor's extents on the receiver. Wrong arguments silently ship full
    /// copies forever, so the construction is pinned here.
    #[test]
    fn send_args_use_p_for_own_parent_and_c_for_ancestor() {
        let repl_dir = FsPath::new("/pool/repl/ws-1");
        let ancestor_dir = FsPath::new("/pool/repl/ws-0");
        let (parent, clones) = send_args(repl_dir, Some("g3"), Some((ancestor_dir, "g2")));
        assert_eq!(parent, Some(repl_dir.join("g3")), "own chain resumes with -p");
        assert_eq!(clones, vec![ancestor_dir.join("g2")], "the ancestor's shared snapshot is a -c, never a -p");

        let (parent, clones) = send_args(repl_dir, None, None);
        assert_eq!(parent, None, "no shared generation yet — a full send, not a guessed parent");
        assert!(clones.is_empty());
    }

    /// An unchanged volume must cost nothing: the beat is every 300s forever, on every volume.
    #[test]
    fn an_unmoved_generation_sends_nothing() {
        assert!(!replica_due(5, Some(5)), "generation == gate: already caught up");
        assert!(replica_due(6, Some(5)), "generation moved past the gate");
        assert!(replica_due(5, None), "no gate yet: never replicated, always due");
    }

    #[test]
    fn newest_shared_picks_by_generation_not_lexicographic_order() {
        let mine = vec!["g2".to_string(), "g10".to_string()];
        let theirs = vec!["g2".to_string(), "g10".to_string()];
        assert_eq!(newest_shared(&mine, &theirs), Some("g10".to_string()), "g10 outranks g2 numerically");
    }

    /// Pins the ordering: `generation` must run before `snapshot`, and exactly once, regardless
    /// of how many targets are due. Moving the read after the snapshot call (or duplicating it)
    /// would let the gate file record a generation the snapshot doesn't actually contain.
    #[test]
    fn due_targets_reads_generation_once_before_snapshotting() {
        let calls = std::cell::RefCell::new(Vec::new());
        let targets = vec!["b".to_string(), "c".to_string()];
        let (gen, due) = due_targets(
            &targets,
            |t| if t == "b" { Some(7) } else { None }, // b already caught up, c never replicated
            || {
                calls.borrow_mut().push("read");
                Ok(7)
            },
            |g| {
                calls.borrow_mut().push("snapshot");
                assert_eq!(g, 7, "the snapshot must be taken for exactly the generation just read");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(gen, 7);
        assert_eq!(due, vec!["c".to_string()]);
        assert_eq!(*calls.borrow(), vec!["read", "snapshot"], "generation must be read before, and only once before, the snapshot");
    }

    /// The other half of "an unmoved generation sends nothing": nobody due means no snapshot at
    /// all, not just no send.
    #[test]
    fn due_targets_skips_the_snapshot_when_nothing_is_due() {
        let calls = std::cell::RefCell::new(Vec::new());
        let targets = vec!["b".to_string()];
        let (_, due) = due_targets(
            &targets,
            |_| Some(7),
            || {
                calls.borrow_mut().push("read");
                Ok(7)
            },
            |_| {
                calls.borrow_mut().push("snapshot");
                Ok(())
            },
        )
        .unwrap();
        assert!(due.is_empty());
        assert_eq!(*calls.borrow(), vec!["read"], "an unmoved volume must not snapshot at all");
    }

    /// The hang this whole file exists to avoid, one layer down: once the POST has failed,
    /// nothing is left draining the send child's stdout, so a plain `wait()` blocks forever on a
    /// child stuck writing to a full pipe. `post_send` must `kill()` it first. The fake `btrfs` is
    /// a `sleep 300` that never produces enough output to fill a pipe on its own — it stands in
    /// for the real hang, which needs multi-GiB output no test should actually produce.
    #[tokio::test]
    async fn post_send_kills_a_send_whose_post_failed_instead_of_hanging_the_reap() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("btrfs");
        std::fs::write(&bin, "#!/bin/sh\nsleep 300\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = bin.to_string_lossy().into_owned();

        let http = peer_http_client().unwrap();
        // Port 1 refuses the connection immediately on every platform this runs on — the POST
        // fails fast without a mock server, which is the point: `post_send` must react to that
        // failure by killing the child, not by waiting out its exit.
        let to = SendTo { http: &http, addr: "127.0.0.1:1", secret: "s", owner: "alice", id: "v1", btrfs_bin: &bin };
        let snap = tmp.path().join("snap");

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), post_send(&to, &snap, None, &[])).await;
        assert!(result.is_ok(), "post_send must return promptly once the POST fails, not hang reaping an undrained child");
        assert!(result.unwrap().is_err(), "the connection refusal is itself the reported failure");
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
    use rustic_git_workspaces::kube_test::{mock_client, not_found, Recorder, Route};
    use rustic_git_workspaces::registry_client::RegistryClient;

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
        let engine = Engine::new(
            EnginePool::new(pool),
            Arc::new(object_store::memory::InMemory::new()),
            RegistryClient::new("http://127.0.0.1:1", "unused"),
        );
        std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
        (
            Arc::new(Ctx::new(
                client,
                Arc::new(engine),
                node.into(),
                pool.to_string_lossy().into(),
                "r1".into(),
                vec![],
                Arc::new(NoopNix),
                pool.join("profiles"),
            )),
            rec,
        )
    }

    const WS_GET: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1";
    const WS_STATUS: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1/status";
    const ENV_GET: &str = "/apis/rustic-git.io/v1alpha1/environments/ws-1";

    fn workspace_json(node_name: &str, compatible: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1",
            "kind": "Workspace",
            "metadata": {"name": "ws-1", "uid": "uid-1", "generation": 1},
            "spec": {"owner": "alice", "name": "ws-1", "region": "r1", "image": "img", "desiredState": "running", "packages": []},
            "status": {"phase": "ready", "nodeName": node_name, "compatibleNodes": compatible},
        })
    }

    fn repl_dir(pool: &std::path::Path, id: &str) -> std::path::PathBuf {
        let dir = pool.join("repl").join(id);
        std::fs::create_dir_all(dir.join("g1")).unwrap();
        dir
    }

    /// THE regression this whole fix round exists for: a pure standby (`repl/{id}`, no
    /// `vol/{id}` — the old vol/-keyed janitor sweep would have deleted this) whose CRD still
    /// selects this node must be kept, with no age floor at all — a live replica is live
    /// regardless of how long it has sat unchanged.
    #[tokio::test]
    async fn reconcile_keeps_a_standby_replica_the_crd_still_selects() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = repl_dir(tmp.path(), "ws-1");
        let routes = vec![Route { method: "GET", path: WS_GET.into(), status: 200, body: workspace_json("node-a", &["node-a"]) }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        std::env::set_var("WS_REPLICA_COUNT", "2");

        // Only one non-owner candidate, so it is deterministically the sole target — no hash guessing.
        replica_reconcile(&ctx, &["node-a".to_string(), "node-b".to_string()]).await;

        assert!(dir.exists(), "a replica the CRD still selects this node for must survive");
        assert!(rec.sent("PUT", WS_STATUS).is_empty(), "still selected: nothing to remove from compatibleNodes");
    }

    /// Deselection: the CRD exists, this node is not the owner, and this node is not among the
    /// current `targets` (simulated by leaving it out of `candidates` entirely — deterministic,
    /// no hash to reproduce). Both the disk and the `compatibleNodes` entry must go.
    #[tokio::test]
    async fn reconcile_removes_a_deselected_replica_and_itself_from_compatible_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = repl_dir(tmp.path(), "ws-1");
        let routes = vec![
            Route { method: "GET", path: WS_GET.into(), status: 200, body: workspace_json("node-a", &["node-a", "node-b"]) },
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: workspace_json("node-a", &["node-a"]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        std::env::set_var("WS_REPLICA_COUNT", "2");

        // node-b (this node) is not in the candidate list at all, so it can never be a target.
        replica_reconcile(&ctx, &["node-a".to_string(), "node-c".to_string()]).await;

        assert!(!dir.exists(), "a deselected replica's disk must be reclaimed");
        let sent = rec.sent("PUT", WS_STATUS);
        assert_eq!(sent.len(), 1);
        let compatible = sent[0]["status"]["compatibleNodes"].as_array().unwrap();
        assert!(!compatible.iter().any(|n| n == "node-b"), "a deselected node removes itself from compatibleNodes");
    }

    /// Orphaned: neither Workspace nor Environment answers — a volume deleted after it replicated
    /// out. Deleted only past the age floor, so a dir mid-first-receive is not swept out from
    /// under itself.
    #[tokio::test]
    async fn reconcile_deletes_an_orphaned_replica_once_the_crd_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = repl_dir(tmp.path(), "ws-1");
        // Backdate past SWEEP_MIN_AGE (1h) so the age floor does not itself keep it.
        let old = std::time::SystemTime::now() - (SWEEP_MIN_AGE + std::time::Duration::from_secs(60));
        std::fs::File::open(&dir).unwrap().set_modified(old).unwrap();
        let gate = tmp.path().join("vol").join("ws-1.replicated-gen-node-c");
        std::fs::create_dir_all(gate.parent().unwrap()).unwrap();
        std::fs::write(&gate, "3").unwrap();

        let routes = vec![not_found(WS_GET), not_found(ENV_GET)];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-b", routes);
        std::env::set_var("WS_REPLICA_COUNT", "2");

        replica_reconcile(&ctx, &["node-a".to_string(), "node-b".to_string()]).await;

        assert!(!dir.exists(), "an orphaned replica past the age floor must be reclaimed");
        assert!(!gate.exists(), "its gate sidecars go with it");
    }

    /// Keep-biased: a lookup ERROR (not a 404 — a real API problem) must never be read as "the CRD
    /// is gone". Deleting real replica data over a transient hiccup is the failure this whole
    /// design exists to avoid.
    #[tokio::test]
    async fn reconcile_keeps_everything_when_the_lookup_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = repl_dir(tmp.path(), "ws-1");
        let old = std::time::SystemTime::now() - (SWEEP_MIN_AGE + std::time::Duration::from_secs(60));
        std::fs::File::open(&dir).unwrap().set_modified(old).unwrap();

        let routes = vec![Route { method: "GET", path: WS_GET.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) }];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-b", routes);
        std::env::set_var("WS_REPLICA_COUNT", "2");

        replica_reconcile(&ctx, &["node-a".to_string(), "node-b".to_string()]).await;

        assert!(dir.exists(), "a lookup error must keep everything, not delete it");
    }
}
