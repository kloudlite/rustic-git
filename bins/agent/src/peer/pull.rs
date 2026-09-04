//! The other half of the transport: `pull_beat` decides which snapshots this node is missing and
//! GETs them from a peer that has them (`snapshot`, the send side, stays in `mod.rs` beside the
//! router). Ancestor-first, one volume's transfers serialized against a retry of the same volume,
//! keep-biased everywhere a CR read informs a delete.

use super::placement::{live_nodes, node_dead_secs, node_is_dead, pool_nodes, standby_count};
use super::sweeps::{reap_dead_replicas, retire_pass, sweep_dead_nodes};
use crate::controller::{replace_status, Ctx};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, PostParams};
use kube::ResourceExt;
use kloudlite_git_workspaces::crd;
use kloudlite_git_workspaces::engine::Engine;
use kloudlite_git_workspaces::replicate;
use futures::TryStreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_util::io::StreamReader;

fn subvolume_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    names
}

/// The most bytes a single receive of a volume's snapshot may write. Derived from the volume's own
/// `spec.quotaGb`, because that IS the answer to "how big can this volume's data be" — a separate
/// env would be a second, drifting copy of it. Times a slack factor for btrfs metadata,
/// reflink-broken copies and a snapshot cut just before a large delete; floored at 1 GiB so a
/// quota-less volume (`quotaGb: 0`) still receives rather than failing at zero.
///
/// ponytail: one ceiling per receive, not per volume total — N concurrent receives of one volume
/// can still exceed it N times. The pool-level guard is the quota `volume_work` already sets;
/// this is the bound on a single stream from a peer we do not otherwise trust to be finite.
pub fn receive_ceiling(quota_gb: u64, settings: &crate::controller::Settings) -> u64 {
    let slack = settings.load().peer_receive_slack;
    (quota_gb * slack * 1024 * 1024 * 1024).max(1024 * 1024 * 1024)
}

async fn delete_subvolume(btrfs_bin: &str, path: &std::path::Path) {
    let parts: Vec<&str> = btrfs_bin.split_whitespace().collect();
    let Some((prog, prefix)) = parts.split_first() else { return };
    let _ = tokio::process::Command::new(prog).args(prefix).arg("subvolume").arg("delete").arg(path).status().await;
}

/// `replica_secs`, `stored ?? env ?? default`.
pub fn replica_interval(settings: &crate::controller::Settings) -> std::time::Duration {
    std::time::Duration::from_secs(settings.load().replica_secs)
}

/// `{pod ip}:8444` for the `kloudlite-git-agent` pod on `node` — the peer listener's own address,
/// found through the ClusterRole's existing pods get/list grant rather than a DNS name, since a
/// DaemonSet pod has no stable per-node service.
pub(crate) async fn agent_pod_addr(client: &kube::Client, node: &str) -> Result<String, String> {
    let api: kube::Api<Pod> = kube::Api::namespaced(client.clone(), "kube-system");
    let lp = ListParams::default().labels("app=kloudlite-git-agent").fields(&format!("spec.nodeName={node}"));
    let pods = api.list(&lp).await.map_err(|e| e.to_string())?;
    let ip = pods
        .items
        .into_iter()
        // The label and the node are a selector, not an identity: a pod created in `kube-system`
        // by anyone can wear both. The ServiceAccount is the thing only our DaemonSet has, and a
        // pull redirected to an impostor is a root `btrfs receive` of whatever it answers with.
        .filter(|p| p.spec.as_ref().and_then(|s| s.service_account_name.as_deref()) == Some("kloudlite-git-agent"))
        .find_map(|p| p.status.and_then(|s| s.pod_ip))
        .ok_or_else(|| format!("no ready kloudlite-git-agent pod on {node}"))?;
    Ok(format!("{ip}:8444"))
}

/// `WS_PEER_SEND_TIMEOUT_SECS`, default 3600. A send is legitimately tens of GiB; this exists to
/// unwedge a connection that has actually stalled, not to police link speed. The receive side has
/// no timeout knob of its own — the sender's is the only bound on a transfer.
fn send_timeout(settings: &crate::controller::Settings) -> Duration {
    Duration::from_secs(settings.load().peer_send_timeout_secs)
}

/// The client every peer dial in this file shares. `connect_timeout` alone, not a blanket
/// `.timeout()`: the GET calls above set their own short bound per request, and the POST below
/// sets its own generous one — a client-wide default would have to be the smaller of the two and
/// wrongly cap the send.
pub fn peer_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().map_err(|e| e.to_string())
}

/// One pass of the puller — spawned beside `replicate_beat` in `controller/run.rs`. Inert without a
/// peer secret, same fail-closed rule every dial in this file follows: no secret, no
/// authenticated GET to another node's root-run `btrfs send`.
/// Returns true when some snapshot could not be fetched this pass, so the caller retries soon
/// instead of waiting out the full tick.
pub async fn pull_beat(ctx: &Arc<Ctx>) -> bool {
    if ctx.peer_secret.is_empty() {
        return false;
    }
    pull_beat_with(ctx, "btrfs", &ctx.peer_secret).await
}

/// Split out so tests can point the receive half at a fake `btrfs` — same shape as
/// `SendTo::btrfs_bin` on the send side — and pass the secret directly rather than through
/// `WS_PEER_SECRET`, which every test in this binary would otherwise share.
pub(crate) async fn pull_beat_with(ctx: &Arc<Ctx>, btrfs_bin: &str, secret: &str) -> bool {
    // Listed ONCE and threaded through everything below: a partial view of who is alive must reap,
    // unclaim and place nothing, and every one of those decisions needs to agree on the same list.
    let nodes = match Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(kind = "Node", error = %e, "listing.failed");
            return false;
        }
    };

    // One clock and one floor for the whole pass: reap, unclaim and live_nodes must agree on
    // exactly the same "dead" answer, not three readings a few nanoseconds apart.
    let now = k8s_openapi::jiff::Timestamp::now();
    let floor = node_dead_secs(&ctx.settings);

    // A node the cluster reads as dead must not sweep: its agent kept running through a kubelet
    // outage, so it went on reaping replicas, unclaiming volumes and retiring copies on a view of
    // the cluster nobody else shares — and every other live node was already doing that work
    // correctly. `node_is_dead`, never `unplaceable`: a DECOMMISSIONING node is alive and must keep
    // sweeping, or its own drain never finishes. The 180 s floor is the only guard here: a node
    // wrongly NotReady past it stops reconciling until its Node object recovers, which is the
    // deliberate trade — a wrong sweep deletes data, a paused one only waits.
    if node_is_dead(nodes.iter().find(|k| k.name_any() == ctx.node), floor, now) {
        tracing::warn!(node = %ctx.node, reason = "node-not-ready", "sweep.skipped");
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
            tracing::warn!(kind = "Node", reason = "pool", error = %e, "listing.failed");
            return false;
        }
    };

    let http = match peer_http_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "peer.client.failed");
            return false;
        }
    };

    let live = live_nodes(&candidates, &nodes, floor, now);
    let mut missed = false;
    for id in interesting_volumes(ctx, &beat, &live).await {
        missed |= pull_volume(ctx, &beat, btrfs_bin, &http, secret, &id, &live).await;
    }
    retire_pass(ctx, &beat, &live).await;
    missed
}

/// Every volume this node must hold a replica of: named by replication's rendezvous
/// (`replicate::targets`, standbys only — the owner already has everything by construction), OR
/// the volume behind a Workspace/Environment whose pod runs here right now, OR a volume this node
/// itself owns (`spec.nodeName == me`) — the owner's row is a source for every standby, and a
/// STOPPED volume (no pod, nothing in `Workspace/Environment.status.nodeName`) still needs one, or
/// the first standby to look finds an empty source list forever. A Volume-list hiccup now idles
/// the whole beat (keep-biased — see `beat`'s bail-out above) instead of falling back to only the
/// worktree-hosted volumes it used to still pull.
pub(crate) async fn interesting_volumes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, live: &[String]) -> Vec<String> {
    // One hop off the reactor for every probe this pass needs, rather than a `stat` per volume on
    // the reactor thread: the answer is a set, and the pool does not change under this beat.
    let ids: Vec<String> = beat.volumes.iter().map(|v| v.name_any()).collect();
    let engine = ctx.engine.clone();
    let held: HashSet<String> = tokio::task::spawn_blocking(move || {
        ids.into_iter().filter(|id| engine.pool.voldir(id).exists()).collect::<HashSet<String>>()
    })
    .await
    .unwrap_or_default();

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
        let hold_a_copy = held.contains(&id);
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

/// Local snapshots whose CR is gone entirely — retention's disk-side convergence. Pure, so
/// `pull_volume`'s "which locals to drop" decision is testable without real btrfs (`drop_snapshot`
/// itself is the engine's own concern, covered by `engine_snapshot.rs`'s loopback tests).
///
/// `any_pull_failed` reclaims NOTHING. The owner deletes `sync-A`'s CR the instant `sync-B` is
/// Ready, so a replica that could not reach the owner this pass would drop its local `sync-A` and
/// gain nothing — going from one sync point to none, in exactly the partition-then-owner-death
/// case sync points exist for. Deferring the reclaim costs a subvolume until the next clean pass.
/// The names this returns are CANDIDATES, not verdicts: the caller re-GETs each one before
/// deleting anything, because this list is computed from a Snapshot listing taken before
/// `local_snapshots` and a push cut in that window is on disk and absent from it.
///
/// ponytail: all-or-nothing rather than transients-only, because a retired name has no CR left to
/// read `spec.transient` off — telling a swept snapshot from a swept sync point here would mean
/// trusting the name prefix. Split it if held-back snapshots ever cost real space.
pub(crate) fn retired(have: &HashSet<String>, existing: &HashSet<String>, any_pull_failed: bool) -> Vec<String> {
    if any_pull_failed {
        return Vec::new();
    }
    have.iter().filter(|n| !existing.contains(*n)).cloned().collect()
}

/// Pulls every `Snapshot` this node is missing for `volume`, then rewrites this node's own
/// `VolumeReplica`. Keep-biased throughout: a `Snapshot`-list error skips the volume with nothing
/// touched, same as `replica_reconcile`'s lookup-error branch.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pull_volume(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, btrfs_bin: &str, http: &reqwest::Client, secret: &str, volume: &str, live: &[String]) -> bool {
    let snap_api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    // One list, all phases: the Ready subset drives the pull below, and the FULL name set is what
    // tells a deleted CR from a Working one, below — a `Snapshot` has no finalizer (see
    // `snapshot::reconcile_snapshot`'s module doc), so this diff against `local_snapshots` is the only
    // place any node ever notices a snapshot's CR is gone.
    let all: Vec<crd::Snapshot> = match snap_api.list(&ListParams::default().fields(&format!("spec.volume={volume}"))).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(kind = "Snapshot", %volume, error = %e, "listing.failed");
            return false;
        }
    };
    let existing: HashSet<String> = all.iter().map(|s| s.name_any()).collect();
    let ready: Vec<crd::Snapshot> =
        all.into_iter().filter(|s| s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready)).collect();

    let mut have: HashSet<String> = match ctx.engine.local_snapshots(volume) {
        Ok(names) => names.into_iter().collect(),
        Err(e) => {
            tracing::warn!(kind = "Snapshot", %volume, reason = "local", error = %e, "listing.failed");
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
    // snapshot yet — falling back to any other replica of the volume (including a Syncing one)
    // rather than giving up outright. Never my own row: pulling from myself is meaningless, and
    // an owner or a re-selected standby always sees its own (possibly stale) row in this list.
    let not_me = |r: &&&crd::VolumeReplica| r.spec.node != ctx.node;
    let synced = |r: &&&crd::VolumeReplica| r.status.as_ref().is_some_and(|s| s.phase == "Synced");
    let mut sources: Vec<&str> = replicas.iter().filter(not_me).filter(synced).map(|r| r.spec.node.as_str()).collect();
    sources.extend(replicas.iter().filter(not_me).filter(|r| !synced(r)).map(|r| r.spec.node.as_str()));
    // The OWNER, last, and only while it is live. Every snapshot exists on the owner by
    // construction, but a replica row for it may not exist yet (a fresh volume) or may have been
    // reaped — which left the first standby with an empty source list and a snapshot it could never
    // fetch. Last so a Synced peer is still preferred, and skipped when the owner is not in `live`
    // so a genuinely dead owner does not cost a failed dial per snapshot per pass.
    if let Some(owner) = beat.volumes.iter().find(|v| v.name_any() == volume).map(|v| v.spec.node_name.as_str()) {
        if !owner.is_empty() && owner != ctx.node && live.iter().any(|n| n == owner) && !sources.contains(&owner) {
            sources.push(owner);
        }
    }

    // Resolved ONCE per pass, before the snapshot loop: `agent_pod_addr` is a namespaced pod LIST
    // with two selectors, and a node catching up on N snapshots was making N of them per source to
    // learn the same IP. A source whose pod cannot be found now is skipped for the whole pass —
    // which is what the per-snapshot `continue` amounted to anyway, one list at a time.
    let mut addrs: Vec<(&str, String)> = Vec::new();
    for &source in &sources {
        match agent_pod_addr(&ctx.client, source).await {
            Ok(a) => addrs.push((source, a)),
            Err(e) => tracing::warn!(%volume, node = source, error = %e, "peer.addr.failed"),
        }
    }

    // The volume's own quota is the ceiling's source. A volume missing from the beat's listing is
    // one this node holds a copy of without a CR; the floor applies.
    let quota_gb = beat.volumes.iter().find(|v| v.name_any() == volume).map(|v| v.spec.quota_gb).unwrap_or(0);
    let max_bytes = receive_ceiling(quota_gb, &ctx.settings);

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
            // have pulled a different, shorter chain, or dropped an old snapshot already). A `-p`
            // the source doesn't recognize fails ITS `btrfs send`, which surfaces here as a
            // truncated body after the 200 header: the same "wrong -p, retry full" case
            // `send_to_target` already handles on the push side. One retry against the SAME
            // source with no parent at all before moving on, so a single bad guess costs one
            // extra full pull instead of losing this snapshot (and every descendant) forever.
            // Read fresh for each new send this pass starts — an in-flight one keeps the
            // deadline it started with, nothing here cancels or extends one already streaming.
            let timeout = send_timeout(&ctx.settings);
            let mut result = pull_one(&ctx.engine, btrfs_bin, http, addr, secret, volume, &name, my_parent.as_deref(), max_bytes, timeout).await;
            if result.is_err() && my_parent.is_some() {
                tracing::warn!(%volume, snapshot = %name, node = source, reason = "incremental-failed", "pull.retried");
                result = pull_one(&ctx.engine, btrfs_bin, http, addr, secret, volume, &name, None, max_bytes, timeout).await;
            }
            match result {
                Ok(()) => {
                    have.insert(name.clone());
                    pulled = true;
                    break;
                }
                Err(e) => tracing::warn!(%volume, snapshot = %name, node = source, reason = "receive", error = %e, "pull.failed"),
            }
        }
        if !pulled {
            any_pull_failed = true;
            tracing::warn!(%volume, snapshot = %name, reason = "no-source", "pull.failed");
        }
    }

    // Drop any local snapshot whose CR is gone entirely (not merely `Working` — `existing` holds
    // every phase). `drop_snapshot` is Ok-on-absent, so every node that ever held a copy converges
    // on the same disk state without a second round trip to confirm it.
    // Gated on `any_pull_failed` — see `retired`.
    for name in retired(&have, &existing, any_pull_failed) {
        // `existing` was listed before `local_snapshots`, and the Snapshot reconciler cuts in this
        // same process: a push whose CR was created inside that window is on disk and absent from
        // the listing, and deleting it loses a Ready push nothing can recover. One fresh GET per
        // candidate, exactly as `sweep_orphan_snap_bytes` does; a present record OR a failed GET
        // keeps the bytes. Candidates are rare, so this is a GET per reclaim, not per pass.
        if !matches!(snap_api.get_opt(&name).await, Ok(None)) {
            continue;
        }
        // btrfs delete takes a blocking flock and shells out — never on the reactor thread.
        let (engine, vol, cname) = (ctx.engine.clone(), volume.to_string(), name.clone());
        match tokio::task::spawn_blocking(move || engine.drop_snapshot(&vol, &cname)).await {
            Ok(Ok(())) => {
                have.remove(&name);
            }
            Ok(Err(e)) => tracing::warn!(%volume, snapshot = %name, reason = "retired", error = %e, "snapshot.drop.failed"),
            Err(e) => tracing::warn!(%volume, snapshot = %name, reason = "panicked", error = %e, "snapshot.drop.failed"),
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
        tracing::warn!(%volume, error = %e, "replica.status.write.failed");
    }
    any_pull_failed
}

/// One `GET /peer/v1/snapshot/{volume}/{name}` streamed straight into `btrfs receive
/// snap_dir/{volume}/`. A failed receive deletes the partial, same before/after diff the push
/// side's `replicate` handler uses, mirrored here on the pulling node.
// pub rather than private: bins/agent/tests/peer.rs is a separate integration-test crate and
// needs to drive the receive half directly — the send half is already reachable there through the
// router, but nothing else serves the other end of a `btrfs receive` to exercise from outside.
#[allow(clippy::too_many_arguments)]
pub async fn pull_one(
    engine: &Engine,
    btrfs_bin: &str,
    http: &reqwest::Client,
    addr: &str,
    secret: &str,
    volume: &str,
    name: &str,
    parent: Option<&str>,
    max_bytes: u64,
    timeout: Duration,
) -> Result<(), String> {
    let mut url = format!("http://{addr}/peer/v1/snapshot/{volume}/{name}?max={max_bytes}");
    if let Some(p) = parent {
        url = format!("{url}&parent={p}");
    }
    // ponytail: `timeout` bounds the WHOLE streamed pull, not just the connect — a first
    // replica larger than ~1h of transfer at whatever the link does is timed out and retried from
    // the next source rather than finishing. `peer_send_timeout_secs` is the escape hatch;
    // splitting "connect" from "whole body" is the upgrade if a legitimately huge first pull ever
    // needs longer than an operator wants to raise it for everyone.
    let resp = http.get(&url).header("x-peer-secret", secret).timeout(timeout).send().await.map_err(|e| e.to_string())?;
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
    // `take(max_bytes + 1)`: the extra byte is how a stream that WOULD exceed the ceiling is told
    // apart from one that exactly fills it. A peer answering with an unbounded body otherwise
    // fills the pool, and a full pool takes down every workspace on this node, not one volume.
    let mut reader =
        tokio::io::AsyncReadExt::take(StreamReader::new(resp.bytes_stream().map_err(std::io::Error::other)), max_bytes + 1);
    let started = std::time::Instant::now();
    let copy_result = tokio::io::copy(&mut reader, &mut stdin).await;
    let _ = stdin.shutdown().await;
    drop(stdin);
    let ok = match copy_result {
        Ok(n) if n > max_bytes => {
            tracing::warn!(%volume, snapshot = %name, bytes = max_bytes, reason = "ceiling", "pull.failed");
            let _ = child.wait().await;
            false
        }
        Ok(n) => {
            // Counted whatever `btrfs receive` then makes of them: these are wire bytes, and a
            // transfer that arrived and failed to apply still cost the link exactly this much.
            metrics::counter!("snapshot_transfer_bytes_total", "direction" => "pull").increment(n);
            matches!(child.wait().await, Ok(s) if s.success())
        }
        Err(_) => {
            let _ = child.wait().await;
            false
        }
    };

    metrics::histogram!("snapshot_transfer_duration_seconds", "direction" => "pull")
        .record(started.elapsed().as_secs_f64());
    if !ok {
        let after = subvolume_names(&dir);
        for n in after.iter().filter(|n| !before.contains(n)) {
            delete_subvolume(btrfs_bin, &dir.join(n)).await;
        }
        return Err("btrfs receive failed".to_string());
    }
    Ok(())
}

/// Create-or-update THIS node's own `VolumeReplica` — the sole writer, per the module doc.
/// `branches` is `worktree -> the newest Ready transient this node holds`, which is what every
/// placement decision reads; `phase` is `Synced` iff nothing was missing at the end of this pass.
pub(crate) async fn write_replica_status(
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
            // H2: owner is unknown here (only the volume id is), so only `kloudlite-git.io/volume`
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

