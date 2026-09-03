//! Shared test module for the peer split (see `peer/mod.rs`'s doc): pull, sweeps, wake and
//! placement tests all live here rather than four smaller files, because most of them exercise
//! more than one half at once (a pull test calls `sweep_dead_nodes`, a sweep test calls
//! `retire_pass` which calls back into pull-adjacent helpers) — splitting by module would mean
//! duplicating fixtures or reaching across `#[cfg(test)]` boundaries for no reader benefit.

use super::placement::*;
use super::pull::*;
use super::sweeps::*;
use super::wake::*;
use super::*;
use crate::testsupport::test_ctx;
use k8s_openapi::api::core::v1::Node;
use rustic_git_workspaces::crd;
use rustic_git_workspaces::kube_test::{get, mock_client, not_found, Recorder, Route};
use rustic_git_workspaces::replicate;
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

/// M2: the code default IS the cluster's floor. Two numbers — a 600 s default with a 180 s
/// deploy override — is a node declared dead at one interval in production and another in
/// every test, and the comments disagreed about which.
#[test]
fn the_dead_node_floor_defaults_to_the_number_the_cluster_runs() {
    std::env::remove_var("WS_NODE_DEAD_SECS");
    assert_eq!(node_dead_secs(), 180);
}

/// M7: a pull target must be the agent's own ServiceAccount, not merely a pod wearing its
/// label in `kube-system`. Creating a pod there is cluster-admin-adjacent already, so this is
/// depth, not a hole — but the check is one line and the alternative is a redirected pull.
#[tokio::test]
async fn a_pod_wearing_the_label_but_not_the_service_account_is_not_a_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let impostor = serde_json::json!({
        "apiVersion": "v1", "kind": "PodList",
        "items": [{
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "not-us", "namespace": "kube-system", "labels": {"app": "rustic-git-agent"}},
            "spec": {"nodeName": "node-b", "serviceAccountName": "default"},
            "status": {"podIp": "10.0.0.9"},
        }],
    });
    let routes = vec![get("/api/v1/namespaces/kube-system/pods", impostor)];
    let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);

    assert!(agent_pod_addr(&ctx.client, "node-b").await.is_err(), "an impostor pod is not a peer address");
}

/// I4: the ceiling is the volume's own quota times slack, never unbounded, and never below a
/// floor a snapshot's metadata needs even on a tiny or quota-less volume.
#[test]
fn the_receive_ceiling_follows_the_volumes_quota() {
    assert_eq!(receive_ceiling(10), 10 * 3 * 1024 * 1024 * 1024);
    assert_eq!(receive_ceiling(0), 1024 * 1024 * 1024, "a quota-less volume still gets the floor");
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
        "spec": {"volume": volume, "owner": "alice", "worktree": "ws-1", "parent": parent},
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

/// C2: the crash window. The volume's pin is already empty (the release CAS landed) and its
/// parents still name the dead node (the un-place did not). No watch anywhere matches that
/// state, so the sweep is the only thing that can free it — it must not skip the volume for
/// having no owner.
#[tokio::test]
async fn an_empty_pinned_volume_with_placed_parents_is_still_unplaced() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-1", "uid": "ws-uid", "generation": 1, "resourceVersion": "9"},
        "spec": {"owner": "alice", "name": "ws-1", "region": "r1", "image": "img", "desiredState": "stopped", "packages": []},
        "status": {"phase": "ready", "nodeName": "node-dead", "volumeRef": "vol-1"},
    });
    let routes = vec![
        get(format!("{WORKSPACES}/ws-1"), ws.clone()),
        Route { method: "PUT", path: format!("{WORKSPACES}/ws-1/status"), status: 200, body: ws },
    ];
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

    // The volume as the crash left it: empty pin, and a parent still placed on the dead node.
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "vol-1", "uid": "v1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "", "region": "r1",
                 "quotaGb": 5, "replicas": 2},
    });
    let mut parent = parent_at("Workspace", "ws-1", "vol-1", crd::Phase::Stopped, true);
    parent.node_name = "node-dead".into();
    parent.pod_ref = None;
    let beat = crate::listing::Beat {
        volumes: vec![serde_json::from_value(vol).unwrap()],
        replicas: vec![],
        parents: vec![parent.clone()],
        all_parents: vec![parent],
    };
    let dead: HashSet<String> = ["node-dead".to_string()].into_iter().collect();

    sweep_volumes(&ctx, &beat, &dead, "NodeDead", true).await;

    let sent = rec.sent("PUT", &format!("{WORKSPACES}/ws-1/status"));
    assert_eq!(sent.len(), 1, "the stranded parent must be un-placed exactly once: {:?}", rec.calls());
    assert_eq!(sent[0]["status"]["nodeName"], "", "un-place clears the parent's pin: {}", sent[0]);
    assert!(
        rec.calls().iter().all(|c| !c.starts_with("PATCH ")),
        "the pin is already clear; nothing re-patches the volume spec: {:?}",
        rec.calls()
    );
}

/// The guard's other side: an empty-pinned volume whose parents are ALSO unplaced is nobody's
/// business, and the sweep must not walk it every beat forever.
#[tokio::test]
async fn an_empty_pinned_volume_with_no_placed_parents_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![]);
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "vol-1", "uid": "v1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "", "region": "r1", "quotaGb": 5, "replicas": 2},
    });
    let mut parent = parent_at("Workspace", "ws-1", "vol-1", crd::Phase::Stopped, true);
    parent.node_name = String::new();
    parent.pod_ref = None;
    let beat = crate::listing::Beat {
        volumes: vec![serde_json::from_value(vol).unwrap()],
        replicas: vec![],
        parents: vec![parent.clone()],
        all_parents: vec![parent],
    };
    let dead: HashSet<String> = ["node-dead".to_string()].into_iter().collect();

    sweep_volumes(&ctx, &beat, &dead, "NodeDead", true).await;

    assert!(rec.calls().is_empty(), "an already-converged volume costs no writes: {:?}", rec.calls());
}

/// The same empty pin with a parent RUNNING on a LIVE node is the spread path's crash window,
/// not this one: `resolve_volume`'s mismatch branch heals it on the node it names, and a
/// running working copy never moves. The sweep must keep its hands off it.
#[tokio::test]
async fn an_empty_pinned_volume_with_a_running_parent_on_a_live_node_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![]);
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "vol-1", "uid": "v1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "", "region": "r1", "quotaGb": 5, "replicas": 2},
    });
    let parent = parent_at("Workspace", "ws-1", "vol-1", crd::Phase::Running, false);
    let beat = crate::listing::Beat {
        volumes: vec![serde_json::from_value(vol).unwrap()],
        replicas: vec![],
        parents: vec![parent.clone()],
        all_parents: vec![parent],
    };
    let dead: HashSet<String> = ["node-dead".to_string()].into_iter().collect();

    sweep_volumes(&ctx, &beat, &dead, "NodeDead", true).await;

    assert!(rec.calls().is_empty(), "a live node's running parent is untouched: {:?}", rec.calls());
}

fn replica_of(volume: &str, node: &str, phase: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": format!("{volume}.{node}"), "uid": format!("uid-{node}")},
        "spec": {"volume": volume, "node": node},
        "status": {"phase": phase, "branches": {}},
    })
}

/// A `Snapshot`-list error must keep every local snapshot untouched and write no replica
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

/// C1: a snapshot whose CR appeared AFTER this pass's listing (a push racing the pull) is on
/// disk and absent from `existing` — the fresh GET is what stops the pull beat deleting a
/// Ready push's bytes.
#[tokio::test]
async fn a_push_that_landed_during_the_pass_is_never_retired() {
    let tmp = tempfile::tempdir().unwrap();
    let snap_dir = tmp.path().join("vol").join("vol-1").join("snap");
    std::fs::create_dir_all(snap_dir.join("push-late")).unwrap();
    let routes = vec![
        // The pass's own listing: empty, taken before the push's CR existed.
        get(SNAPSHOTS, list_of("Snapshot", vec![])),
        // The fresh per-candidate GET: the CR is there now.
        get(format!("{SNAPSHOTS}/push-late"), ready_snapshot("push-late", "vol-1", "")),
        not_found(format!("{VOLREPLICAS}/{}", crd::replica_name("vol-1", "node-b"))),
    ];
    let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
    let http = peer_http_client().unwrap();

    pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1", &[]).await;

    assert!(
        rec.calls().contains(&format!("GET {SNAPSHOTS}/push-late")),
        "the retire loop must GET each candidate fresh before deleting it: {:?}",
        rec.calls()
    );
    assert!(snap_dir.join("push-late").exists(), "a snapshot whose CR exists must keep its bytes");
}

/// The other half: a name whose CR really is gone is still visited and handed to
/// `drop_snapshot`, so the guard did not turn the retire into a no-op. The bytes themselves are
/// btrfs's business — `drop_snapshot` is deliberately Ok-on-a-plain-directory, so off a real
/// filesystem the reachable assertion is the fresh GET, and `engine_snapshot.rs`'s loopback
/// tests cover the delete.
#[tokio::test]
async fn a_snapshot_with_no_cr_at_all_is_still_retired() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol").join("vol-1").join("snap").join("gone")).unwrap();
    let routes = vec![
        get(SNAPSHOTS, list_of("Snapshot", vec![])),
        not_found(format!("{SNAPSHOTS}/gone")),
        not_found(format!("{VOLREPLICAS}/{}", crd::replica_name("vol-1", "node-b"))),
    ];
    let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
    let http = peer_http_client().unwrap();

    pull_volume(&ctx, &beat_of(vec![], vec![], vec![]), "btrfs", &http, "s3cret", "vol-1", &[]).await;

    assert!(rec.calls().contains(&format!("GET {SNAPSHOTS}/gone")), "{:?}", rec.calls());
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

/// Nothing missing (every Ready `Snapshot` is already a local snapshot): `pull_volume` makes no
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

    assert!(rec.calls().iter().all(|c| !c.contains("/peer/v1/snapshot/")), "nothing missing: no GET should ever be issued");
    let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
    assert_eq!(sent.len(), 1, "exactly one replica status write");
    assert_eq!(sent[0]["status"]["phase"], "Synced");
}

/// A `Snapshot` CR that has been deleted (absent from the volume's list entirely, every
/// phase) is exactly what `retired` picks out — the "least new machinery" this task's
/// deletion handling uses: no finalizer on the new `Snapshot` kind, so this diff against
/// `local_snapshots` is the only place any node ever notices the CR is gone. `drop_snapshot`
/// itself is real btrfs and is `pull_volume`'s only caller of it — covered end to end by
/// `engine_snapshot.rs`'s loopback tests, not repeated here.
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
/// is not necessarily one the SOURCE holds too) must not lose the snapshot forever: after the
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

    // The peer server: a real `snapshot` endpoint, so `pull_one` exercises the actual HTTP
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
        "spec": {"serviceAccountName": "rustic-git-agent"},
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

    assert!(tmp.path().join("vol/vol-1/snap/vol-1-child").exists(), "the full-pull retry must land the snapshot");
    let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"]["phase"], "Synced", "the fallback succeeded: nothing is missing any more");
    // Before the guard: see `PeerServer`.
    peer_server.stop().await;
}

/// F4 (drill, 2026-09-03): with no replica row for the owner (a fresh volume, or a row the
/// reaper took) the first standby had an EMPTY source list and could never fetch a thing. The
/// owner is a source of last resort — and only while it is live, so a genuinely dead owner
/// costs no failed dial per snapshot per pass.
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
    assert!(missed, "the snapshot did not land, so the pass asks for a retry");

    let dead = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(dead.path(), "node-b", routes());
    pull_volume(&ctx, &beat, "btrfs", &http, "s3cret", "v1", &[]).await;
    assert_eq!(pod_lists(&rec), 0, "a dead owner is not dialled at all: {:?}", rec.calls());
}

/// Catching up on three snapshots from one source resolves that source's pod address ONCE, not
/// once per snapshot: a full namespaced pod list with two selectors is not a per-snapshot cost.
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
    assert_eq!(pod_lists, 1, "one address lookup per source per pass, not per snapshot: {:?}", rec.calls());
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

    let ids = interesting_volumes(&ctx, &beat_of(vec![volume], vec![], vec![]), &live).await;

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

    assert_eq!(interesting_volumes(&ctx, &beat_of(vec![volume], vec![], vec![]), &live).await, vec!["v2".to_string()]);
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
    assert!(interesting_volumes(&ctx, &beat, &live).await.is_empty(), "no local copy: nothing to do");

    std::fs::create_dir_all(ctx.engine.pool.voldir("v0")).unwrap();
    assert_eq!(interesting_volumes(&ctx, &beat, &live).await, vec!["v0".to_string()]);
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
        "status": {"phase": "ready", "nodeName": node, "volumeRef": format!("vol-{name}")},
    })
}

fn ws_placed_stopped(name: &str, node: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9"},
        "spec": {"owner": "alice", "name": name, "region": "r1", "image": "img", "desiredState": "stopped", "packages": []},
        "status": {"phase": "ready", "nodeName": node, "volumeRef": format!("vol-{name}")},
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
        "status": {"phase": "creating", "nodeName": node, "volumeRef": format!("vol-{name}")},
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
        "spec": {"volume": volume, "owner": "alice", "worktree": "ws-1", "parent": parent, "transient": true},
        "status": {"phase": "ready"},
    })
}

/// A transient is addressed, pulled, and counted toward `Synced` exactly like a snapshot: same
/// `GET /peer/v1/snapshot/{volume}/{name}` shape (its name just happens to start with `sync-`),
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
    // fetched over `/peer/v1/snapshot/{volume}/{name}` — the exact same endpoint a real snapshot
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
        "spec": {"serviceAccountName": "rustic-git-agent"},
        "status": {"podIP": "127.0.0.1"},
    });
    let routes = vec![
        // One already-local snapshot alongside the missing transient: proves the transient is
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

    assert!(tmp.path().join("vol/vol-1/snap/sync-ws-1-x").exists(), "the transient must land on disk like any other snapshot");
    let paths = seen_paths.lock().unwrap().clone();
    assert!(
        paths.iter().any(|p| p.contains("/peer/v1/snapshot/vol-1/sync-ws-1-")),
        "the transient is fetched over the same snapshot endpoint as a real snapshot: {paths:?}"
    );
    let created = rec.sent("POST", VOLREPLICAS);
    assert_eq!(created.len(), 1, "the replica row is created fresh (Syncing, per the mocked response) before the final status write");
    let sent = rec.sent("PUT", &format!("{VOLREPLICAS}/vol-1.node-b/status"));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"]["phase"], "Synced", "and lands Synced once the transient is pulled");
    // Before the guard: see `PeerServer`.
    peer_server.stop().await;
}

/// A transient's `Snapshot` CR being gone is exactly the same "retired" case a deleted snapshot
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

/// I1: the retire's btrfs work must not run on the reactor. On a single-threaded runtime a
/// `spawn_blocking`'d walk lets a concurrent `yield_now` ticker rack up many ticks while the
/// blocking pool thread does the real work; an inline walk starves the ticker completely,
/// because nothing yields until the walk (and the `remove_dir_all` after it) is done. A
/// watcher records the ticker's count at the instant the directory is observed gone, which
/// stays in the single digits for the inline form (measured by temporarily reverting the
/// orphan call site to a direct `janitor::cleanup_local` call: 1 tick — the ticker and the
/// watcher each get exactly one poll once the already-finished walk finally lets the task
/// yield) and reaches the thousands for the `spawn_blocking` form (8000 plain directories
/// measured ~140ms to walk plus ~400ms to `remove_dir_all` on this machine — real wall-clock
/// work, not a sleep).
#[test]
fn the_retire_pass_does_not_walk_the_pool_on_the_reactor() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..8000 {
        std::fs::create_dir_all(tmp.path().join("vol").join("orphan").join("snap").join(format!("c{i}"))).unwrap();
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", vec![get(SNAPSHOTS, list_of("Snapshot", vec![]))]);

        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let t = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                t.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        });

        let orphan = tmp.path().join("vol").join("orphan");
        let ticks_at_gone = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorded = ticks_at_gone.clone();
        let watched = ticks.clone();
        let watcher = tokio::spawn(async move {
            loop {
                if !orphan.exists() {
                    recorded.store(watched.load(std::sync::atomic::Ordering::SeqCst), std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                tokio::task::yield_now().await;
            }
        });

        retire_pass(&ctx, &beat_of(vec![], vec![], vec![]), &[]).await;
        watcher.await.unwrap();
        ticker.abort();

        let ticks_before_gone = ticks_at_gone.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            ticks_before_gone >= 50,
            "the orphan walk must run off the reactor: only {ticks_before_gone} ticker ticks happened \
             before the directory was reclaimed (inline blocking gives 0)"
        );
        assert!(!tmp.path().join("vol").join("orphan").exists(), "the orphan voldir is still reclaimed");
    });
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
        "spec": {"volume": volume, "owner": "alice", "worktree": volume, "parent": "", "transient": false},
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

// -----------------------------------------------------------------------------------------
// Task 5: `collect_unreferenced_volumes`, the crash-between-steps safety net for design rule
// 5 — a Volume with no owner entry, no `beat.parents`, and no snapshot older than one beat.
// -----------------------------------------------------------------------------------------

/// Old enough to collect: `WS_REPLICA_SECS` defaults to 300 and nothing here sets it.
const LONG_AGO: &str = "2000-01-01T00:00:00Z";

fn vol_unowned(name: &str, node: &str, created_at: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9", "creationTimestamp": created_at},
        "spec": {"owner": "alice", "team": "", "nodeName": node, "region": "r1", "quotaGb": 5, "replicas": 2},
        "status": {"phase": "ready"},
    })
}

fn vol_still_owned(name: &str, node: &str, created_at: &str) -> serde_json::Value {
    let mut v = vol_unowned(name, node, created_at);
    v["metadata"]["ownerReferences"] = serde_json::json!([
        {"apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace", "name": "ws-1", "uid": "ws-uid", "controller": true}
    ]);
    v
}

#[tokio::test]
async fn retire_pass_collects_a_volume_no_working_copy_and_no_snapshot_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![get(SNAPSHOTS, snap_list(vec![]))]);
    std::fs::create_dir_all(tmp.path().join("vol/v1")).unwrap();

    retire_pass(&ctx, &beat_of(vec![vol_unowned("v1", "node-a", LONG_AGO)], vec![], vec![]), &["node-a".to_string()]).await;

    assert_eq!(rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect::<Vec<_>>(), vec![format!("DELETE {VOLUMES}/v1")]);
}

#[tokio::test]
async fn retire_pass_keeps_an_unreferenced_volume_that_has_a_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![get(SNAPSHOTS, snap_list(vec![snap_of("v1.aaaa", "v1")]))]);

    retire_pass(&ctx, &beat_of(vec![vol_unowned("v1", "node-a", LONG_AGO)], vec![], vec![]), &["node-a".to_string()]).await;

    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
}

/// Even with no ownerReference yet — the finalizer that would have removed it may simply not
/// have run this beat — a live parent still using the volume must never be collected under it.
#[tokio::test]
async fn retire_pass_keeps_an_unowned_volume_a_parent_still_names() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![get(SNAPSHOTS, snap_list(vec![]))]);

    retire_pass(&ctx, &beat_of(vec![vol_unowned("v1", "node-a", LONG_AGO)], vec![], vec![("Workspace", "ws-1", "v1")]), &["node-a".to_string()]).await;

    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
}

#[tokio::test]
async fn retire_pass_keeps_an_unreferenced_volume_younger_than_a_beat() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![get(SNAPSHOTS, snap_list(vec![]))]);
    let just_now = k8s_openapi::jiff::Timestamp::now().to_string();

    retire_pass(&ctx, &beat_of(vec![vol_unowned("v1", "node-a", &just_now)], vec![], vec![]), &["node-a".to_string()]).await;

    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
}

#[tokio::test]
async fn retire_pass_keeps_a_still_owned_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![get(SNAPSHOTS, snap_list(vec![]))]);

    retire_pass(&ctx, &beat_of(vec![vol_still_owned("v1", "node-a", LONG_AGO)], vec![], vec![]), &["node-a".to_string()]).await;

    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
}

/// Only the pinned node collects — a non-owner never races the one deleter the doc string
/// promises.
#[tokio::test]
async fn retire_pass_never_collects_a_volume_pinned_to_another_node() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![get(SNAPSHOTS, snap_list(vec![]))]);

    retire_pass(&ctx, &beat_of(vec![vol_unowned("v1", "node-b", LONG_AGO)], vec![], vec![]), &["node-a".to_string(), "node-b".to_string()]).await;

    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
}

/// The Snapshot listing is the one shared by every sweep on this pass: a failed list must
/// starve this one too, not just the record- and byte-side sweeps beside it.
#[tokio::test]
async fn retire_pass_collects_no_volume_when_the_snapshot_list_fails() {
    let tmp = tempfile::tempdir().unwrap();
    // No `SNAPSHOTS` route at all: the mock answers 404, which is a list failure.
    let (ctx, rec) = test_ctx(tmp.path(), "node-a", Vec::new());

    retire_pass(&ctx, &beat_of(vec![vol_unowned("v1", "node-a", LONG_AGO)], vec![], vec![]), &["node-a".to_string()]).await;

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

    let dropped = sweep_orphan_snap_bytes(&ctx, &beat, &[serde_json::from_value(snap_of("v1-aaaa", "v1")).unwrap()]).await;

    assert_eq!(dropped, vec![("v1".to_string(), "v1-bbbb".to_string())]);
    assert!(ctx.engine.pool.snap("v1", "v1-aaaa").exists(), "the recorded snapshot's bytes stay put");
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

    assert!(sweep_orphan_snap_bytes(&ctx, &beat, &[]).await.is_empty());
}

/// The beat's listing is stale by the time the bytes are swept — a push that created its CR
/// and cut its subvolume in that window must survive on the strength of the fresh GET.
#[tokio::test]
async fn the_byte_sweep_keeps_a_snap_whose_record_appeared_after_the_listing() {
    let tmp = tempfile::tempdir().unwrap();
    snap_pool(tmp.path(), "v1", &["v1-fresh"]);
    let (ctx, _rec) = test_ctx(tmp.path(), "node-a", vec![get(format!("{SNAPSHOTS}/v1-fresh"), snap_of("v1-fresh", "v1"))]);
    let beat = beat_of(vec![vol_owned("v1", "node-a")], vec![], vec![]);

    assert!(sweep_orphan_snap_bytes(&ctx, &beat, &[]).await.is_empty(), "present on the fresh GET: kept");
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
                 "transient": true},
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
        ready_snapshot("vol-1-snapshot", "vol-1", ""),
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
/// a Synced replica holds every Ready snapshot, which is all there is to hold.
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
    // `local_snapshots` is a plain listing of `snap/{volume}` — a directory per held subvolume.
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
    // No local snapshots: nothing was pulled, so nothing is held.
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
    assert_eq!(after_pass(&wake, false, &mut misses, MIN_WAKE_GAP), Next::Wait, "no wake, no extra pass");
    for _ in 0..5 {
        wake.notify_one();
    }
    assert_eq!(after_pass(&wake, false, &mut misses, MIN_WAKE_GAP), Next::RunAgain, "a wake during the pass runs it again");
    assert_eq!(after_pass(&wake, false, &mut misses, MIN_WAKE_GAP), Next::Wait, "five wakes are one permit, not five passes");
}

/// F4 (drill, 2026-09-03): a pass that could not fetch a snapshot waited out the full tick. It
/// now comes back in 30 s. A pending wake no longer shortens that (I2: a missed pass's own
/// backoff is always longer than `MIN_WAKE_GAP` and is never worth cutting short) — a stop
/// waiting on a replica still isn't delayed, because `spawn_pull` races the retry sleep against
/// the wake itself, outside `after_pass`.
#[test]
fn a_pass_that_missed_a_snapshot_retries_soon_even_with_a_wake_pending() {
    let wake = tokio::sync::Notify::new();
    let mut misses = 0;
    assert_eq!(after_pass(&wake, true, &mut misses, MIN_WAKE_GAP), Next::RetrySoon(RETRY_SOON));
    wake.notify_one();
    assert_eq!(after_pass(&wake, true, &mut misses, MIN_WAKE_GAP), Next::RetrySoon(retry_delay(2)), "the backoff still governs");
}

/// Round 2: an unfetchable snapshot used to pin the whole node at a 30 s pass forever. The delay
/// doubles per consecutive miss, caps at the ordinary tick, and a single clean pass resets it.
#[test]
fn consecutive_misses_back_off_to_the_ordinary_tick_and_one_clean_pass_resets() {
    let cap = replica_interval();
    let wake = tokio::sync::Notify::new();
    let mut misses = 0;
    let delays: Vec<Duration> = (0..6)
        .map(|_| match after_pass(&wake, true, &mut misses, MIN_WAKE_GAP) {
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

    assert_eq!(after_pass(&wake, false, &mut misses, MIN_WAKE_GAP), Next::Wait, "a clean pass goes back to the tick");
    assert_eq!(misses, 0, "and forgets the streak");
    assert_eq!(after_pass(&wake, true, &mut misses, MIN_WAKE_GAP), Next::RetrySoon(RETRY_SOON), "so the next miss starts over at 30 s");
}

/// I2: a wake still wins, but never sooner than `MIN_WAKE_GAP` after the last pass STARTED —
/// a peer looping POSTs on `/peer/v1/wake` must not pin this node in a back-to-back beat.
#[test]
fn a_wake_arriving_inside_the_floor_waits_out_the_remainder() {
    let wake = tokio::sync::Notify::new();
    wake.notify_one();
    let mut misses = 0;
    let next = after_pass(&wake, false, &mut misses, Duration::from_secs(1));
    assert_eq!(next, Next::RetrySoon(MIN_WAKE_GAP - Duration::from_secs(1)));
}

#[test]
fn a_wake_after_the_floor_runs_again_at_once() {
    let wake = tokio::sync::Notify::new();
    wake.notify_one();
    let mut misses = 0;
    assert_eq!(after_pass(&wake, false, &mut misses, MIN_WAKE_GAP), Next::RunAgain);
}

/// The floor never delays a RETRY that is already longer than it: a missed pass's own backoff
/// still governs, and a wake inside the floor does not shorten it.
#[test]
fn the_floor_never_shortens_a_missed_passes_backoff() {
    let wake = tokio::sync::Notify::new();
    wake.notify_one();
    let mut misses = 3;
    let next = after_pass(&wake, true, &mut misses, Duration::from_secs(0));
    assert_eq!(next, Next::RetrySoon(retry_delay(4)));
}

fn agent_pod(node: &str, ip: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": format!("agent-{node}"), "namespace": "kube-system"},
        "spec": {"nodeName": node, "serviceAccountName": "rustic-git-agent"},
        "status": {"podIP": ip},
    })
}
