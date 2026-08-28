//! The node controller's three load-bearing behaviours, against a mocked API server
//! (`rustic_git_workspaces::kube_test`) — no cluster, no btrfs.
//!
//! These are deliberately about the *loop*, not about btrfs: what the reconcile starts, what it
//! refuses to start twice, and what it never deletes. The btrfs half is covered by the engine's own
//! loopback tests and by `tests/ws_e2e.sh` against real k3s.

use rustic_git_agent::controller::{Ctx, Done};
use rustic_git_workspaces::crd;
use rustic_git_workspaces::engine::{Engine, Pool};
use rustic_git_workspaces::kube_test::{mock_client, Recorder, Route};
use rustic_git_workspaces::registry_client::RegistryClient;
use rustic_git_workspaces::store::MemStore;
use std::sync::Arc;

const VOL_STATUS: &str = "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status";

/// A fake `Nix` that records what it was asked to build and answers as told. A successful build
/// leaves the out-link behind, because that is what the reconciler publishes.
struct FakeNix {
    builds: std::sync::Mutex<Vec<(String, std::path::PathBuf)>>,
    answer: std::sync::Mutex<Result<(), String>>,
}
impl Default for FakeNix {
    fn default() -> Self {
        FakeNix { builds: std::sync::Mutex::new(Vec::new()), answer: std::sync::Mutex::new(Ok(())) }
    }
}
impl rustic_git_agent::nix::Nix for FakeNix {
    fn build(&self, expr: &str, out: &std::path::Path, _: std::time::Duration) -> Result<(), String> {
        self.builds.lock().unwrap().push((expr.to_string(), out.to_path_buf()));
        let r = self.answer.lock().unwrap().clone();
        if r.is_ok() {
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            let _ = std::os::unix::fs::symlink("/tmp", out);
        }
        r
    }
    fn ping(&self) -> Result<(), String> { Ok(()) }
    fn collect_garbage(&self) -> Result<u64, String> { Ok(0) }
}

fn patch_ok(path: &str) -> Route {
    Route { method: "PATCH", path: path.into(), status: 200, body: volume_json(1) }
}

fn volume_json(generation: i64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1",
        "kind": "Volume",
        "metadata": {"name": "vol-1", "uid": "uid-1", "generation": generation},
        "spec": {"owner": "alice", "nodeName": "node-a", "region": "r1", "quotaGb": 10},
    })
}

fn volume(generation: i64) -> crd::Volume {
    serde_json::from_value(volume_json(generation)).unwrap()
}

fn ctx(pool: &std::path::Path, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
    // Port 1: nothing listens, so every registry read fails — the migration's "skip the history
    // backfill" path, which is what every non-history test wants.
    ctx_with_registry(pool, routes, "http://127.0.0.1:1")
}

fn ctx_with_registry(pool: &std::path::Path, routes: Vec<Route>, registry: &str) -> (Arc<Ctx>, Recorder) {
    ctx_full(pool, routes, registry, Arc::new(FakeNix::default()))
}

/// The one constructor: every test's profile root is a directory under its own pool tempdir, so no
/// test can reach the node's real `/nix` and none of them race each other over it.
fn ctx_full(pool: &std::path::Path, routes: Vec<Route>, registry: &str, nix: Arc<FakeNix>) -> (Arc<Ctx>, Recorder) {
    let (client, rec) = mock_client(routes);
    // Best effort: one test hands a plain file as its "pool" on purpose.
    let profiles = pool.join("profiles");
    let _ = std::fs::create_dir_all(&profiles);
    let engine = Engine::new(
        Pool::new(pool),
        Arc::new(object_store::memory::InMemory::new()),
        Arc::new(MemStore::new()),
        RegistryClient::new(registry, "unused"),
    );
    (
        Arc::new(Ctx::new(
            client,
            Arc::new(engine),
            "node-a".into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            vec!["session".into(), "env".into()],
            nix,
            profiles,
        )),
        rec,
    )
}

/// Block until every in-flight operation has finished — "observed on a LATER pass" is the
/// behaviour under test, and which pass that is depends on a thread, not on the reconcile.
async fn wait_idle(ctx: &Arc<Ctx>) {
    for _ in 0..200 {
        if ctx.running.lock().unwrap().values().all(|(_, h)| h.is_finished()) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("operation never finished");
}

/// The single-flight guard: a second reconcile of the same {uid, generation} while a push is
/// running must NOT start a second one. This replaces the 120s-lease-with-no-renewal that audit
/// H2 is about — the sweep requeuing a still-running job and it racing itself.
#[tokio::test]
async fn a_second_reconcile_of_a_running_generation_does_not_start_a_second_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let v = volume(1);

    // Stand in for an operation already in flight for this exact {uid, generation}.
    ctx.running.lock().unwrap().insert(
        "uid-1".to_string(),
        (1, tokio::task::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            Ok(Done::default())
        })),
    );

    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    // Starting the operation is what creates the volume directory — its absence is the assertion
    // that nothing was started, independent of whether btrfs exists on this machine.
    assert!(!tmp.path().join("vol/vol-1").exists(), "a second operation was started");
}

/// A finished operation is observed on a LATER pass and written to status, and the reconcile that
/// observes it requeues no further.
#[tokio::test]
async fn a_finished_operation_writes_observed_generation_and_stops_requeueing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let v = volume(7);
    ctx.running.lock().unwrap().insert(
        "uid-1".to_string(),
        (7, tokio::task::spawn_blocking(|| Ok(Done { phase: rustic_git_workspaces::crd::Phase::Ready, ..Done::default() }))),
    );

    wait_idle(&ctx).await;

    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    let sent = rec.sent("PATCH", VOL_STATUS);
    assert_eq!(sent.len(), 1, "exactly one status write");
    assert_eq!(sent[0]["status"]["observedGeneration"], 7);
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");

    // The guard against the classic hot loop: the same status, computed again, is not rewritten.
    let mut observed = v.clone();
    observed.status = serde_json::from_value(sent[0]["status"].clone()).unwrap();
    let action = rustic_git_agent::controller::apply_volume(&observed, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert_eq!(rec.sent("PATCH", VOL_STATUS).len(), 1, "an unchanged status must not be rewritten");
}

/// Keep-biased: an API error or an unreadable pool means requeue with backoff, never "reality
/// doesn't match, so remove it". Same discipline as crates/registry/src/gc.rs.
#[tokio::test]
async fn a_reconcile_that_cannot_read_the_pool_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    // A regular file where the pool root must be: every path under it fails with NotADirectory.
    let pool = tmp.path().join("pool");
    std::fs::write(&pool, b"not a directory").unwrap();
    let (ctx, rec) = ctx(&pool, vec![patch_ok(VOL_STATUS)]);
    let v = volume(1);

    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));

    // Let the doomed operation finish, then observe it.
    wait_idle(&ctx).await;
    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_ne!(
        action,
        kube::runtime::controller::Action::await_change(),
        "a failed operation must be retried, not abandoned"
    );

    let sent = rec.sent("PATCH", VOL_STATUS);
    let last = sent.last().expect("a failure must be reported in status");
    assert!(last["status"]["observedGeneration"].is_null(), "a failed generation is not observed");
    assert!(
        last["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Ready" && c["status"] == "False"),
        "the failure is reported as Ready=False: {last}"
    );
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "nothing may be deleted: {:?}", rec.calls());
}

/// Every phase string the controller writes must deserialize into the enum `/v1` projects it into.
///
/// `api::phase` falls back to a default on an unknown string instead of erroring, so a controller
/// that invents a word does not fail — it silently reports the default. That shipped: the workspace
/// reconcile wrote `running`, `WsState` spells that state `Ready`, and a healthy workspace showed
/// "Creating" in the UI indefinitely. Nothing failed and nothing logged.
#[test]
fn phase_names_the_doc_enum() {
    use rustic_git_workspaces::model::{EnvState, WsState};

    // Grepped from controller.rs. Volume phases are excluded deliberately: a Volume is never
    // projected into a doc, so its vocabulary is its own.
    use rustic_git_workspaces::crd::Phase;
    for p in [Phase::Ready, Phase::Stopped, Phase::Error, Phase::Creating].map(Phase::as_str) {
        assert!(
            serde_json::from_value::<WsState>(serde_json::json!(p)).is_ok(),
            "workspace phase {p:?} does not deserialize as WsState"
        );
    }
    for p in [Phase::Running, Phase::Stopped, Phase::Error].map(Phase::as_str) {
        assert!(
            serde_json::from_value::<EnvState>(serde_json::json!(p)).is_ok(),
            "environment phase {p:?} does not deserialize as EnvState"
        );
    }

    // The exact regressions: neither of these is a state of its enum, and both were written.
    assert!(serde_json::from_value::<WsState>(serde_json::json!("running")).is_err());
    assert!(serde_json::from_value::<EnvState>(serde_json::json!("stopping")).is_err());
}

/// Deleting a volume while a push is still reading it must WAIT, not reclaim underneath it.
///
/// `cleanup_local` removes the subvolume. Running that against a live `btrfs send` destroys the
/// source mid-stream, and the finalizer is precisely what makes waiting free: the object cannot go
/// away until cleanup returns, so a requeue costs one tick.
#[tokio::test]
async fn deleting_a_volume_waits_for_an_in_flight_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let v = volume(1);

    // A push still in flight for this volume.
    ctx.running.lock().unwrap().insert(
        "uid-1".to_string(),
        (1, tokio::task::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(700));
            Ok(Done::default())
        })),
    );

    let action = rustic_git_agent::controller::cleanup_volume(&v, &ctx).await.unwrap();
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)),
        "cleanup must requeue while an operation is running, not reclaim the subvolume"
    );
    // Still held: nothing was drained by a cleanup that decided to wait.
    assert!(!ctx.running.lock().unwrap().is_empty());

    // Once it finishes, the same call drains the handle and proceeds instead of requeueing
    // forever — while deleting, the finalizer routes every pass here, so nothing else could.
    wait_idle(&ctx).await;
    rustic_git_agent::controller::cleanup_volume(&v, &ctx).await.unwrap();
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained by cleanup");
}

// ── placement claims ─────────────────────────────────────────────────────

const WS_STATUS: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1/status";
const BINDINGS: &str = "/apis/rustic-git.io/v1alpha1/ownerbindings";

fn ws_json(status: serde_json::Value) -> serde_json::Value {
    let mut o = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1",
        "kind": "Workspace",
        // `resourceVersion` is not decoration here: the claim carries it, and a test that omits it
        // would pass against a forced apply — the exact primitive this design refuses.
        "metadata": {"name": "ws-1", "uid": "ws-uid-1", "generation": 1, "resourceVersion": "42",
                     "labels": {"rustic-git.io/owner": "alice", "rustic-git.io/kind": "workspace",
                                "rustic-git.io/team": ""}},
        "spec": {"owner": "alice", "team": "", "name": "web", "region": "r1",
                 "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"},
    });
    // An object that has never been reconciled has NO status at all, not an empty one: `phase` is
    // required by the schema, so `status: {}` is a shape the API server can never return.
    if status != serde_json::json!({}) {
        o["status"] = status;
    }
    o
}

fn workspace(status: serde_json::Value) -> crd::Workspace {
    serde_json::from_value(ws_json(status)).unwrap()
}

/// The owner's binding, already NamespaceReady — the gate every workspace pass has to get past.
fn ready_binding() -> Route {
    rustic_git_workspaces::kube_test::get(
        "/apis/rustic-git.io/v1alpha1/ownerbindings/r1-alice",
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                           "metadata": {"name": "r1-alice"},
                           "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"},
                           "status": {"conditions": [{"type": "NamespaceReady", "status": "True",
                                                      "reason": "Converged", "message": "ok",
                                                      "lastTransitionTime": "2026-08-27T00:00:00Z"}]}}),
    )
}

fn pv_route(name: &str) -> Route {
    Route { method: "PATCH", path: format!("/api/v1/persistentvolumes/{name}"), status: 200,
            body: serde_json::json!({"apiVersion": "v1", "kind": "PersistentVolume", "metadata": {"name": name}}) }
}

fn pvc_route(name: &str) -> Route {
    Route { method: "PATCH", path: format!("/api/v1/namespaces/ws-alice/persistentvolumeclaims/{name}"), status: 200,
            body: serde_json::json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": name}}) }
}

fn binding_route() -> Route {
    rustic_git_workspaces::kube_test::post(
        BINDINGS,
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                           "metadata": {"name": "r1-alice"},
                           "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"}}),
    )
}

/// The claim is ONE status write, and it is a status write — an API-authored spec is never touched
/// by a controller. Everything downstream (the Volume's node, the PV's affinity, therefore the
/// pod's node) is derived from this one field.
///
/// It is a PUT (`replace_status`), not a forced apply: this is the one write in the system that
/// must be able to lose, and it carries the object's `resourceVersion` so that losing is a 409.
#[tokio::test]
async fn an_unplaced_workspace_is_claimed_with_one_optimistic_status_write() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );

    rustic_git_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();

    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "exactly one status write");
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
    assert_eq!(sent[0]["status"]["compatibleNodes"], serde_json::json!(["node-a"]));
    // The schema declares `status.phase` required; a write without it is a 422 from a real server.
    assert_eq!(sent[0]["status"]["phase"], "pending", "every status write carries a phase: {}", sent[0]);
    assert_eq!(
        sent[0]["metadata"]["resourceVersion"], "42",
        "without the resourceVersion the write cannot conflict, and the claim cannot race: {}", sent[0]
    );
    assert!(
        sent[0]["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Placed"),
        "the claim records itself as a condition: {}", sent[0]
    );
    assert!(rec.calls().iter().any(|c| c == &format!("POST {BINDINGS}")), "the binding must exist after a claim");
    assert!(
        !rec.calls().iter().any(|c| c == "PATCH /apis/rustic-git.io/v1alpha1/workspaces/ws-1"),
        "a controller never patches an API-authored spec: {:?}", rec.calls()
    );
}

/// `compatibleNodes` is the memory: a node not listed must leave the object alone so a listed one
/// can take it. Nothing today writes more than one entry, and nothing may assume that.
#[tokio::test]
async fn a_node_outside_compatible_nodes_does_not_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "compatibleNodes": ["node-b"]}));

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "a node that does not hold the disk writes nothing: {:?}", rec.calls());
}

/// An already-placed object is not re-claimed; a stop keeps `status.nodeName` precisely so a later
/// start reconciles on the same node with no placement step.
#[tokio::test]
async fn an_already_placed_workspace_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let w = workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "{:?}", rec.calls());
}

/// A pre-migration object matches the unplaced watch (it has no `status.nodeName` at all) while
/// already being placed by the deprecated `spec.nodeName`. Claiming it would hand it to whichever
/// agent saw it first, which is exactly how an owner's data ends up split across two pools.
#[tokio::test]
async fn a_legacy_spec_placed_workspace_is_not_claimed() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let mut w = workspace(serde_json::json!({}));
    w.spec.node_name = Some("node-b".into());

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "the migration places these, not the claim: {:?}", rec.calls());
}

/// Losing the race must be a REAL conflict. `Patch::Apply(..).force()` never conflicts — it is the
/// wrong primitive for the one write in this system that must race — so the claim is an optimistic
/// write carrying the object's `resourceVersion`, and a 409 means another node won.
///
/// A 409 is not assumed to mean "placed": the claim RE-READS and runs the same decision again, so
/// a peer that only widened `compatibleNodes` does not scare this node off a claim it may still
/// make. Here the peer really did place it, so the re-read decides "leave it alone".
///
/// The loser must also not create the OwnerBinding: that would bind an owner to a node that did
/// not win, and every later workspace of theirs would follow it.
#[tokio::test]
async fn a_claim_that_loses_the_race_re_reads_and_binds_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let conflict = Route {
        method: "PUT",
        path: WS_STATUS.into(),
        status: 409,
        body: serde_json::json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": "Conflict", "code": 409,
            "message": "the object has been modified; please apply your changes to the latest version"
        }),
    };
    let won_by_peer = ws_json(serde_json::json!({"phase": "pending", "nodeName": "node-b", "compatibleNodes": ["node-b"]}));
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![conflict, rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces/ws-1", won_by_peer)],
    );

    let action = rustic_git_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "the winner's write is our wake-up");
    assert!(
        rec.calls().iter().any(|c| c == "GET /apis/rustic-git.io/v1alpha1/workspaces/ws-1"),
        "a 409 must re-read and re-decide, not assume: {:?}", rec.calls()
    );
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("POST")),
        "the loser must not bind the owner to a node it did not win: {:?}", rec.calls()
    );
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "one attempt, then re-read and yield — not a retry loop");
}

/// `compatibleNodes` is a SET. Appending on a re-run grows the array without bound, and a
/// level-triggered reconciler re-runs by design.
#[tokio::test]
async fn claiming_twice_does_not_grow_compatible_nodes() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    // Already lists this node, but has no `nodeName` — the shape a claim that wrote
    // `compatibleNodes` and then lost its status write leaves behind.
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "compatibleNodes": ["node-a"]}));

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent[0]["status"]["compatibleNodes"], serde_json::json!(["node-a"]), "union, not append");
}

/// A `cloneOf` needs the SOURCE's disk, so the new object's own (empty) `compatibleNodes` cannot
/// decide — the source's can. A node that does not hold the source must not claim, or the clone
/// stops being a local btrfs snapshot and becomes a network copy of data that is already here.
#[tokio::test]
async fn a_clone_is_claimed_only_where_its_source_lives() {
    let tmp = tempfile::tempdir().unwrap();
    let src = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-src"},
        "spec": {"owner": "alice", "team": "", "name": "src", "region": "r1",
                 "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"},
        "status": {"phase": "ready", "nodeName": "node-b", "compatibleNodes": ["node-b"]}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces/ws-src", src)],
    );
    let mut w = workspace(serde_json::json!({}));
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into() }),
    });

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(
        rec.sent("PUT", WS_STATUS).is_empty(),
        "node-a does not hold ws-src's disk and must not claim its clone: {:?}", rec.calls()
    );
}

fn env_json(status: serde_json::Value) -> serde_json::Value {
    let mut o = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1",
        "kind": "Environment",
        "metadata": {"name": "env-1", "uid": "env-uid-1", "generation": 1, "resourceVersion": "7"},
        "spec": {"owner": "acme", "name": "staging", "region": "r1", "services": [],
                 "storage": {"quotaGb": 20}, "desiredState": "running"},
    });
    if status != serde_json::json!({}) {
        o["status"] = status;
    }
    o
}

fn environment(status: serde_json::Value) -> crd::Environment {
    serde_json::from_value(env_json(status)).unwrap()
}

/// The environment claim is the workspace claim with a different opening phase — an environment has
/// containers to bring up before it is `running`, so it is `creating`, never `pending`.
#[tokio::test]
async fn an_unplaced_environment_is_claimed_as_creating() {
    const ENV_STATUS: &str = "/apis/rustic-git.io/v1alpha1/environments/env-1/status";
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: ENV_STATUS.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::post(
                BINDINGS,
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-acme"},
                                   "spec": {"owner": "acme", "region": "r1", "nodeName": "node-a"}}),
            ),
        ],
    );

    rustic_git_agent::claim::claim_environment(&environment(serde_json::json!({})), &ctx).await.unwrap();
    let sent = rec.sent("PUT", ENV_STATUS);
    assert_eq!(sent.len(), 1, "exactly one status write");
    assert_eq!(sent[0]["status"]["phase"], "creating");
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
    assert_eq!(sent[0]["metadata"]["resourceVersion"], "7", "the claim races or it is not a claim: {}", sent[0]);
    assert!(rec.calls().iter().any(|c| c == &format!("POST {BINDINGS}")), "the winner binds the owner");

}

/// A legacy environment is the migration's job, not the claim's — same rule as the workspace side,
/// read off the environment's own deprecated `spec.nodeName`.
#[tokio::test]
async fn a_legacy_spec_placed_environment_is_not_claimed() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let mut legacy = environment(serde_json::json!({}));
    legacy.spec.node_name = Some("node-b".into());

    rustic_git_agent::claim::claim_environment(&legacy, &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "the migration places legacy environments, not the claim: {:?}", rec.calls());
}

const BINDING_STATUS: &str = "/apis/rustic-git.io/v1alpha1/ownerbindings/r1-alice/status";

fn ws_in_team(team: &str, node: &str) -> serde_json::Value {
    let mut o = ws_json(serde_json::json!({"phase": "ready", "nodeName": node}));
    o["spec"]["team"] = serde_json::json!(team);
    o
}

/// Every object the binding ensures in one namespace, answered with itself.
fn ns_routes(ns: &str) -> Vec<Route> {
    let ok = |path: String, api: &str, kind: &str| Route {
        method: "PATCH",
        path,
        status: 200,
        body: serde_json::json!({"apiVersion": api, "kind": kind, "metadata": {"name": "x"}}),
    };
    let mut r = vec![
        ok(format!("/api/v1/namespaces/{ns}"), "v1", "Namespace"),
        ok(format!("/api/v1/namespaces/{ns}/limitranges/slot"), "v1", "LimitRange"),
        ok(
            format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings/api-secrets"),
            "rbac.authorization.k8s.io/v1",
            "RoleBinding",
        ),
    ];
    for p in ["default-deny", "allow-dns", "allow-same-namespace", "allow-internet-egress"] {
        r.push(ok(
            format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/{p}"),
            "networking.k8s.io/v1",
            "NetworkPolicy",
        ));
    }
    r
}

fn binding_json() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
        "metadata": {"name": "r1-alice", "uid": "ob-uid-1", "generation": 1},
        "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"}
    })
}

/// The per-owner shared objects have exactly ONE owner now. They used to be re-ensured by the
/// workspace reconciler and the environment reconciler on every pass, which is two writers for one
/// object and a namespace deleted by whichever ran last.
#[tokio::test]
async fn a_binding_ensures_one_namespace_per_team_in_use_and_reports_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        // A team workspace here, and one on ANOTHER node: the second must not make this node build
        // a namespace it does not host.
        "items": [
            ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"})),
            ws_in_team("acme", "node-a"),
            ws_in_team("elsewhere", "node-b"),
        ]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces", ws_list),
            Route { method: "PATCH", path: BINDING_STATUS.into(), status: 200, body: binding_json() },
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .chain(ns_routes("ws-acme-alice"))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    rustic_git_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == "PATCH /api/v1/namespaces/ws-alice"), "{:?}", rec.calls());
    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice");
    assert!(
        sent[0]["metadata"].get("ownerReferences").is_none(),
        "a namespace shared by every workspace this user owns must never be GC'd with one binding: {}", sent[0]
    );
    let limit = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/limitranges/slot");
    assert!(limit[0]["metadata"].get("ownerReferences").is_none(), "a quota ceiling must not vanish with a binding rewrite");
    // Everything else the binding vouches for IS owned by it, so a re-homed owner does not strand
    // a grant on the old node.
    let rb = rec.sent("PATCH", "/apis/rbac.authorization.k8s.io/v1/namespaces/ws-alice/rolebindings/api-secrets");
    assert_eq!(rb[0]["metadata"]["ownerReferences"][0]["kind"], "OwnerBinding", "{}", rb[0]);
    let acme = crd::ws_namespace("alice", "acme");
    assert_eq!(acme, "ws-acme-alice");
    assert!(rec.calls().iter().any(|c| *c == format!("PATCH /api/v1/namespaces/{acme}")), "{:?}", rec.calls());
    let stranded = crd::ws_namespace("alice", "elsewhere");
    assert!(
        !rec.calls().iter().any(|c| *c == format!("PATCH /api/v1/namespaces/{stranded}")),
        "a workspace on another node must not make namespaces here: {:?}", rec.calls()
    );
    let st = rec.sent("PATCH", BINDING_STATUS);
    assert_eq!(st.len(), 1);
    assert!(
        st[0]["status"]["conditions"].as_array().unwrap().iter()
            .any(|c| c["type"] == "NamespaceReady" && c["status"] == "True"),
        "{}", st[0]
    );
}

/// The hot loop this design has to not have: `crd::condition` stamps `lastTransitionTime` with
/// `now`, so a status write on every pass is new bytes, which fires this controller's own watch,
/// which writes again — forever, on an object nothing asked to change.
#[tokio::test]
async fn a_second_reconcile_of_a_ready_binding_writes_no_status() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces", ws_list)]
            .into_iter()
            .chain(ns_routes("ws-alice"))
            .collect(),
    );
    // What the FIRST reconcile left behind, with an older `lastTransitionTime` than `now`.
    let mut b = binding_json();
    b["status"] = serde_json::json!({
        "observedGeneration": 1,
        "conditions": [{"type": "NamespaceReady", "status": "True", "reason": "Converged",
                        "message": "namespaces exist on this node", "observedGeneration": 1,
                        "lastTransitionTime": "2020-01-01T00:00:00Z"}],
    });
    let b: crd::OwnerBinding = serde_json::from_value(b).unwrap();

    rustic_git_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    assert!(
        rec.sent("PATCH", BINDING_STATUS).is_empty(),
        "a status re-stamped with `now` is not a change: {:?}", rec.calls()
    );
}

// ── the workspace reconciler and its volume child ────────────────────────

/// The stuck pod, as a test: a workspace whose disk does not exist yet must not get a pod. The
/// symptom this fixes was a pod wedged forever on `path … does not exist`, because the workspace
/// reconciler never looked at its volume's status.
#[tokio::test]
async fn a_workspace_with_an_unready_volume_creates_no_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "working", "subvolumePresent": false}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-1", vol),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    assert!(
        !rec.calls().iter().any(|c| c.contains("/pods")),
        "no pod may exist before its disk does: {:?}",
        rec.calls()
    );
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["phase"], "creating");
    assert!(
        st.last().unwrap()["status"]["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "VolumeReady" && c["status"] == "False"),
        "{}",
        st.last().unwrap()
    );
}

/// The child is created by the parent, from the parent's placement, with an ownerReference — which
/// is what makes `DELETE workspace` reclaim the disk with no ordering logic in the API.
#[tokio::test]
async fn a_placed_workspace_creates_its_volume_child_on_its_own_node() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::not_found("/apis/rustic-git.io/v1alpha1/volumes/ws-1"),
            rustic_git_workspaces::kube_test::post(
                "/apis/rustic-git.io/v1alpha1/volumes",
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "ws-1"},
                                   "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let sent = rec.sent("POST", "/apis/rustic-git.io/v1alpha1/volumes");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["spec"]["nodeName"], "node-a", "the Volume is created FROM status.nodeName");
    assert_eq!(sent[0]["spec"]["quotaGb"], 20);
    let refs = sent[0]["metadata"]["ownerReferences"].as_array().expect("an ownerReference");
    assert_eq!(refs[0]["kind"], "Workspace");
    assert_eq!(refs[0]["name"], "ws-1");
    assert_eq!(refs[0]["controller"], true);
}

/// A release-1 object has no `storage` and names its Volume in the deprecated pointer. It must be
/// ADOPTED — never failed for the missing field, and never given a second Volume.
#[tokio::test]
async fn a_legacy_workspace_adopts_the_volume_its_spec_names() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "vol-9", "uid": "vol-uid-9"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "working", "subvolumePresent": false}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/vol-9", vol),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let mut w = workspace(serde_json::json!({}));
    w.spec.storage = None;
    w.spec.volume_ref = Some("vol-9".into());
    w.spec.node_name = Some("node-a".into());

    rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    assert!(rec.sent("POST", "/apis/rustic-git.io/v1alpha1/volumes").is_empty(), "a legacy object is adopted");
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["volumeRef"], "vol-9", "the pointers are mirrored into status");
    assert_eq!(st.last().unwrap()["status"]["nodeName"], "node-a");
}

/// A NEW object with no `storage` can never build a disk, and no retry adds a field.
#[tokio::test]
async fn a_new_workspace_without_storage_fails_permanently() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) }],
    );
    let mut w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    w.spec.storage = None;

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent: never retried");
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["phase"], "error");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "NoStorage");
}

/// Git seeding, end to end in one object: an init container that clones over SSH with the owner's
/// platform key, and no token Secret anywhere — the API named one nobody wrote and the agent could
/// not read.
#[test]
fn a_git_seeded_pod_carries_an_init_container_with_the_key_and_no_token() {
    use rustic_git_workspaces::{crd, k8s};
    let spec = crd::WorkspaceSpec {
        restore: None,
        owner: "alice".into(),
        team: String::new(),
        name: "web".into(),
        region: "r1".into(),
        image: "nginx:alpine".into(),
        storage: Some(crd::WorkspaceStorage {
            quota_gb: 20,
            source: Some(crd::VolumeSource::GitRepo { repo: "alice/site".into(), branch: "main".into() }),
        }),
        desired_state: crd::DesiredState::Running,
        resources: Default::default(),
        node_name: None,
        volume_ref: None,
        packages: vec![],
    };
    let source = spec.storage.as_ref().unwrap().source.as_ref().unwrap();
    let init = k8s::git_init_container(source, "alpine/git:2.45.2", "git.example.com", "22")
        .expect("a valid repo is accepted")
        .expect("a gitRepo source seeds with an init container");
    let pod = k8s::workspace_pod(&spec, "ws-1", &test_pod_ctx(), Some(init));

    let inits = pod.spec.as_ref().unwrap().init_containers.as_ref().expect("init containers");
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0].image.as_deref(), Some("alpine/git:2.45.2"), "pinned, so seeding works with any image");
    let mounts: Vec<&str> = inits[0].volume_mounts.as_ref().unwrap().iter().map(|m| m.mount_path.as_str()).collect();
    assert!(mounts.contains(&"/workspace"));
    assert!(mounts.contains(&k8s::USER_KEY_PATH));
    let env: std::collections::HashMap<&str, String> = inits[0]
        .env
        .as_ref()
        .unwrap()
        .iter()
        .map(|e| (e.name.as_str(), e.value.clone().unwrap_or_default()))
        .collect();
    assert_eq!(env["URL"], "ssh://git@git.example.com:22/alice/site.git");
    assert_eq!(env["BRANCH"], "main");
    assert!(env["GIT_SSH_COMMAND"].contains(k8s::USER_KEY_PATH));
    // The whole point of moving the clone into the pod: no minted credential rides along.
    let rendered = serde_json::to_string(&pod).unwrap();
    for gone in ["credentialSecret", "http.extraHeader", "x-access-token"] {
        assert!(!rendered.contains(gone), "no credential is involved any more: {gone} in {rendered}");
    }
    // Hardened exactly like the main container — a seeder is a tenant workload too.
    let main = &pod.spec.as_ref().unwrap().containers[0];
    assert_eq!(inits[0].security_context, main.security_context);
    let sc = inits[0].security_context.as_ref().unwrap();
    assert_eq!(sc.allow_privilege_escalation, Some(false));
    assert_eq!(sc.privileged, Some(false));
    assert_eq!(sc.capabilities.as_ref().unwrap().drop.as_deref(), Some(&["ALL".to_string()][..]));
    // Idempotent: a pod restart must never re-clone over a user's work.
    assert!(inits[0].command.as_ref().unwrap().join(" ").contains("ls -A /workspace"));
    // The key mount stops being optional for a seeded workspace — the clone cannot work without it.
    let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    let key = vols.iter().find(|v| v.name == "user-key").unwrap();
    assert_eq!(key.secret.as_ref().unwrap().optional, Some(false));
}

fn test_pod_ctx() -> rustic_git_workspaces::k8s::PodContext<'static> {
    rustic_git_workspaces::k8s::PodContext {
        pool: "/pool",
        node_name: "node-a",
        owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "rustic-git.io/v1alpha1".into(),
            kind: "Workspace".into(),
            name: "ws-1".into(),
            uid: "ws-uid-1".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        },
        runtime_class: None,
    }
}

/// The last gate before a repo name becomes an ssh argv. A `--branch -upload-pack=…` or an
/// `owner/name` that is neither is arbitrary command execution on the workspace pod, so it fails
/// PERMANENTLY and no pod is started for it.
#[tokio::test]
async fn a_workspace_whose_source_repo_is_not_a_name_gets_no_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20,
                 "source": {"gitRepo": {"repo": "https://evil.example.com/x", "branch": "main"}}},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-1", vol),
            ready_binding(),
            pv_route("pv-ws-1"),
            pvc_route("live-ws-1"),
            pv_route("nix-ws-1"),
            pvc_route("nix-ws-1"),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    // The profile is built first on every pass, so the source is judged on the pass after it.
    let _ = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    wait_idle(&ctx).await;
    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent: never retried");
    assert!(!rec.calls().iter().any(|c| c.contains("/pods")), "no pod for an unclonable source: {:?}", rec.calls());
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["phase"], "error");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "InvalidSource");
}

/// A child that FAILED is not a child still working: the parent surfaces the child's own reason and
/// waits for a change, instead of saying "not materialized yet" once a tick forever.
#[tokio::test]
async fn a_failed_volume_child_stops_the_parent_requeueing() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "error", "subvolumePresent": false,
                   "conditions": [{"type": "Ready", "status": "False", "reason": "NoSpace",
                                   "message": "the pool is full", "lastTransitionTime": "2026-08-27T00:00:00Z"}]}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-1", vol),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "the Volume watch re-triggers it");
    let st = rec.sent("PATCH", WS_STATUS);
    let cond = &st.last().unwrap()["status"]["conditions"][0];
    assert_eq!(cond["type"], "VolumeReady");
    assert_eq!(cond["reason"], "VolumeFailed");
    assert_eq!(cond["message"], "the pool is full", "the child's own reason, not a guess");
}

// ── snapshot requests ────────────────────────────────────────────────────

const SNAP_STATUS: &str = "/apis/rustic-git.io/v1alpha1/snapshotrequests/snap-1/status";
const VOL_GET: &str = "/apis/rustic-git.io/v1alpha1/volumes/ws-1";

fn snap_json(status: serde_json::Value) -> serde_json::Value {
    // `phase` is required by the schema and by `SnapshotRequestStatus`, so "no status yet" is
    // spelled `pending` rather than `{}` — a bare `{}` does not round-trip through the CRD type.
    let status = if status == serde_json::json!({}) { serde_json::json!({"phase": "pending"}) } else { status };
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": "snap-1", "uid": "snap-uid-1", "generation": 1,
                     "finalizers": ["rustic-git.io/snapshot"],
                     "labels": {"rustic-git.io/owner": "alice", "rustic-git.io/volume": "ws-1"}},
        // No `nodeName`: a node is a controller-owned fact and the API does not copy facts into
        // spec. The agent resolves it from the named Volume.
        "spec": {"volume": "ws-1", "message": "checkpoint"},
        "status": status,
    })
}

fn snapshot(status: serde_json::Value) -> crd::SnapshotRequest {
    serde_json::from_value(snap_json(status)).unwrap()
}

/// The Volume this request names, on the node the test's `ctx` is (`node-a`) unless told otherwise.
fn vol_on(node: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": node, "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    })
}

/// A push runs once and says what it produced. The uid-keyed `running` map is the idempotency
/// guard, exactly as for Volume work — a second reconcile of a request in flight starts nothing.
#[tokio::test]
async fn a_snapshot_request_runs_the_push_once_and_writes_done() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(VOL_GET, vol_on("node-a")),
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: snap_json(serde_json::json!({})) },
        ],
    );

    // Stand in for the push having already finished: the reconcile that OBSERVES it is what writes
    // `done`, and which pass that is depends on a thread, not on the reconcile.
    ctx.running.lock().unwrap().insert(
        "snap-uid-1".to_string(),
        (1, tokio::task::spawn_blocking(|| {
            Ok(Done { phase: crd::Phase::Done, lineage_tip: Some("layer-9".into()), restored_to: None })
        })),
    );
    wait_idle(&ctx).await;

    let action = rustic_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({"phase": "working"})), &ctx)
        .await
        .unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    let sent = rec.sent("PATCH", SNAP_STATUS);
    let last = sent.last().unwrap();
    assert_eq!(last["status"]["phase"], "done");
    assert_eq!(last["status"]["snapshotId"], "layer-9");
    assert_eq!(last["status"]["observedGeneration"], 1);
    assert!(last["status"]["at"].as_str().unwrap().contains('T'), "an rfc3339 stamp: {last}");
    assert!(
        last["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Ready" && c["status"] == "True"),
        "{last}"
    );
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");
    // Nothing outside its own object. Two controllers force-applying one Volume status under one
    // field manager prune each other's fields — the Volume's next pass would delete it anyway.
    assert!(
        !rec.calls().iter().any(|c| c.contains("/volumes/ws-1/status")),
        "the snapshot reconciler must not write the Volume's status: {:?}", rec.calls()
    );
}

/// A request whose Volume lives on another node belongs to another agent. Every agent watches every
/// request (there is no field selector), so "not mine" must be silent — a second agent writing this
/// object's status is exactly the multi-writer problem the design removes.
#[tokio::test]
async fn a_request_for_another_nodes_volume_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![rustic_git_workspaces::kube_test::get(VOL_GET, vol_on("node-b"))]);

    let action = rustic_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("PATCH")),
        "another node's request must not be touched: {:?}", rec.calls()
    );
}

/// An agent restart loses the `running` map. A request left at `working` therefore has a
/// `Progressing` condition and no handle, and there is no way to tell "crashed before starting"
/// from "crashed mid-send" — so it must NOT be re-run: `engine.push_env` would take a fresh
/// snapshot and register a SECOND commit record for one user push.
#[tokio::test]
async fn a_working_request_with_no_handle_fails_instead_of_pushing_twice() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(VOL_GET, vol_on("node-a")),
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: snap_json(serde_json::json!({})) },
        ],
    );

    let action = rustic_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({"phase": "working"})), &ctx)
        .await
        .unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "a permanent failure is not retried");
    let last = rec.sent("PATCH", SNAP_STATUS).last().unwrap().clone();
    assert_eq!(last["status"]["phase"], "error");
    assert!(
        last["status"]["conditions"].as_array().unwrap().iter()
            .any(|c| c["type"] == "Ready" && c["status"] == "False" && c["reason"] == "AgentRestarted"),
        "{last}"
    );
    assert!(ctx.running.lock().unwrap().is_empty(), "nothing was started");
    assert!(!tmp.path().join("vol/ws-1").exists(), "and no second push ran");
}

/// A request is never re-run past `done`. The record is durable and content-addressed; running it
/// again would push a second commit nobody asked for.
#[tokio::test]
async fn a_done_snapshot_request_does_nothing_on_a_second_reconcile() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let r = snapshot(serde_json::json!({"phase": "done", "snapshotId": "layer-9", "at": "2026-08-27T00:00:00Z"}));

    let action = rustic_git_agent::snapshot::apply_snapshot(&r, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.calls().is_empty(), "a finished request writes nothing — not even the Volume read: {:?}", rec.calls());
    assert!(!tmp.path().join("vol/ws-1").exists(), "and starts nothing");
}

/// Deleting a request mid-push must WAIT. This is why the request has a finalizer at all: a delete
/// during `working` would otherwise orphan a btrfs RO snapshot, a stage file, an in-flight blob
/// upload and a possible `POST /commits` with no object left to record the outcome in — and the
/// Volume's own finalizer does not cover it, because a SnapshotRequest is not the Volume's child.
#[tokio::test]
async fn deleting_a_working_request_waits_for_the_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![]);
    ctx.running.lock().unwrap().insert(
        "snap-uid-1".to_string(),
        (1, tokio::task::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(700));
            Ok(Done { phase: crd::Phase::Done, lineage_tip: None, restored_to: None })
        })),
    );
    let r = snapshot(serde_json::json!({"phase": "working"}));

    let action = rustic_git_agent::snapshot::cleanup_snapshot(&r, &ctx).await.unwrap();
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)),
        "cleanup must requeue while the push is running"
    );
    assert!(!ctx.running.lock().unwrap().is_empty(), "nothing was drained by a cleanup that waited");

    wait_idle(&ctx).await;
    rustic_git_agent::snapshot::cleanup_snapshot(&r, &ctx).await.unwrap();
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained by cleanup");
}

/// The Volume controller no longer has a push branch at all: pushing is an object with its own
/// reconciler, and `volume_work` is materialize-or-nothing.
#[tokio::test]
async fn a_volume_with_a_push_annotation_starts_no_push() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let mut v = volume(1);
    v.metadata.annotations =
        Some(std::collections::BTreeMap::from([("rustic-git.io/push-requested".to_string(), "2026-08-27T00:00:00Z".to_string())]));
    // Already observed: with the push branch gone there is nothing left for this pass to do.
    v.status = Some(crd::VolumeStatus { phase: crd::Phase::Ready, observed_generation: Some(1), subvolume_present: true, ..Default::default() });

    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "the annotation is dead weight now");
    assert!(ctx.running.lock().unwrap().is_empty(), "and nothing was started");
}

// ── the stop-before-teardown snapshot ────────────────────────────────────

const STOP_REQ: &str = "/apis/rustic-git.io/v1alpha1/snapshotrequests/stop-env-1";
const ENV_PATCH: &str = "/apis/rustic-git.io/v1alpha1/environments/env-1";
const DEP_DEL: &str = "/apis/apps/v1/namespaces/env-1/statefulsets/db";

/// A stopping environment with one service and its own volume, on this node.
fn stopping_env() -> crd::Environment {
    let mut o = env_json(serde_json::json!({"phase": "running", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));
    o["spec"]["desiredState"] = serde_json::json!("stopped");
    o["spec"]["services"] =
        serde_json::json!([{"name": "db", "image": "mongo", "command": [], "env": {}, "mounts": []}]);
    serde_json::from_value(o).unwrap()
}

fn env_vol() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "env-1", "uid": "env-vol-1"},
        "spec": {"owner": "acme", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true},
    })
}

fn stop_req(status: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": "stop-env-1", "uid": "stop-uid-1"},
        "spec": {"volume": "env-1"},
        "status": status,
    })
}

fn stop_routes(req: Option<serde_json::Value>) -> Vec<Route> {
    let mut routes = vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", env_vol()),
        Route {
            method: "PATCH",
            path: "/apis/rustic-git.io/v1alpha1/environments/env-1/status".into(),
            status: 200,
            body: env_json(serde_json::json!({})),
        },
    ];
    match req {
        Some(r) => routes.push(rustic_git_workspaces::kube_test::get(STOP_REQ, r)),
        None => routes.push(Route {
            method: "GET",
            path: STOP_REQ.into(),
            status: 404,
            // A real 404 body: `get_opt` reads the `Status` to tell "absent" from "broken".
            body: serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                                     "code": 404, "reason": "NotFound", "message": "not found"}),
        }),
    }
    routes
}

/// A stop snapshot that FAILED must not let the teardown through. An environment torn down without
/// a landed push loses its last state for good, so the services stay up and the environment says
/// why — the operator deletes and recreates the request to retry.
#[tokio::test]
async fn a_failed_stop_snapshot_tears_nothing_down() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), stop_routes(Some(stop_req(serde_json::json!({"phase": "error"})))));

    let action = rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "a failed push is not retried by us");
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("DELETE")),
        "nothing may be deleted while the push has not landed: {:?}",
        rec.calls()
    );
    let last = rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/environments/env-1/status").last().unwrap().clone();
    assert_eq!(last["status"]["phase"], "running", "the services ARE still up");
    assert!(
        last["status"]["conditions"].as_array().unwrap().iter().any(
            |c| c["type"] == "Ready" && c["status"] == "False" && c["reason"] == "StopSnapshotFailed"
        ),
        "{last}"
    );
}

/// The happy path: a `done` stop snapshot tears the services down AND deletes the request, so the
/// next stop of this environment creates a fresh one instead of finding this `done` object under
/// the same fixed name and pushing nothing.
#[tokio::test]
async fn a_landed_stop_snapshot_tears_down_and_deletes_its_request() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = stop_routes(Some(stop_req(serde_json::json!({"phase": "done", "snapshotId": "layer-1"}))));
    routes.push(Route { method: "DELETE", path: DEP_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) });
    routes.push(Route { method: "DELETE", path: STOP_REQ.into(), status: 200, body: stop_req(serde_json::json!({"phase": "done"})) });
    let (ctx, rec) = ctx(tmp.path(), routes);

    let action = rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {DEP_DEL}")), "{:?}", rec.calls());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {STOP_REQ}")), "the request must not outlive the stop: {:?}", rec.calls());
}

/// No request yet: create exactly one, and tear nothing down on this pass.
#[tokio::test]
async fn a_stop_with_no_snapshot_request_creates_one_and_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = stop_routes(None);
    routes.push(rustic_git_workspaces::kube_test::post(
        "/apis/rustic-git.io/v1alpha1/snapshotrequests",
        stop_req(serde_json::json!({"phase": "pending"})),
    ));
    let (ctx, rec) = ctx(tmp.path(), routes);

    let action = rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    let req = rec.sent("POST", "/apis/rustic-git.io/v1alpha1/snapshotrequests").remove(0);
    assert_eq!(req["metadata"]["name"], "stop-env-1");
    assert_eq!(req["spec"]["volume"], "env-1");
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
}

/// A Volume and a SnapshotRequest share no name and no ownerReference — `spec.volume` is the only
/// link — so the mapper must find requests BY THAT FIELD or a request created before its Volume
/// waits on the 15s backstop forever instead of being woken.
#[test]
fn a_volume_event_wakes_the_requests_that_name_it() {
    let mine: Arc<crd::SnapshotRequest> = Arc::new(snapshot(serde_json::json!({"phase": "pending"})));
    let other: Arc<crd::SnapshotRequest> = Arc::new(serde_json::from_value(serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": "snap-2"}, "spec": {"volume": "ws-2"}, "status": {"phase": "pending"},
    })).unwrap());

    let woken = rustic_git_agent::controller::requests_naming(&[mine, other.clone()], "ws-1");
    assert_eq!(woken.len(), 1, "only the request that names this volume");
    assert_eq!(woken[0].name, "snap-1");
    // And a volume nothing names wakes nothing — the mapper is not a "reconcile everything" hook.
    assert!(rustic_git_agent::controller::requests_naming(&[other], "ws-1").is_empty());
}

/// A push that SUCCEEDED but whose status write failed must not come back as `AgentRestarted`: the
/// bytes are in the registry and the only record of their snapshot id is the drained handle. The
/// outcome goes back in the map so the retry writes `done`, not a false permanent failure.
#[tokio::test]
async fn a_failed_status_write_replays_the_outcome_instead_of_losing_the_push() {
    let tmp = tempfile::tempdir().unwrap();
    let make = ctx;
    let (ctx, rec) = make(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(VOL_GET, vol_on("node-a")),
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 500, body: serde_json::json!({}) },
        ],
    );
    ctx.running.lock().unwrap().insert(
        "snap-uid-1".to_string(),
        (1, tokio::task::spawn_blocking(|| Ok(Done { phase: crd::Phase::Done, lineage_tip: Some("layer-9".into()), restored_to: None }))),
    );
    wait_idle(&ctx).await;

    let r = snapshot(serde_json::json!({"phase": "working"}));
    rustic_git_agent::snapshot::apply_snapshot(&r, &ctx).await.unwrap_err();
    assert!(!ctx.running.lock().unwrap().is_empty(), "the outcome must survive a failed write");
    assert_eq!(rec.sent("PATCH", SNAP_STATUS).last().unwrap()["status"]["phase"], "done");

    // The retry, against an API server that is back: `done` with the real snapshot id, never
    // `AgentRestarted`.
    let (ctx2, rec2) = make(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(VOL_GET, vol_on("node-a")),
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: snap_json(serde_json::json!({})) },
        ],
    );
    let handle = ctx.running.lock().unwrap().remove("snap-uid-1").unwrap();
    ctx2.running.lock().unwrap().insert("snap-uid-1".to_string(), handle);
    wait_idle(&ctx2).await;
    rustic_git_agent::snapshot::apply_snapshot(&r, &ctx2).await.unwrap();
    let last = rec2.sent("PATCH", SNAP_STATUS).last().unwrap().clone();
    assert_eq!(last["status"]["phase"], "done");
    assert_eq!(last["status"]["snapshotId"], "layer-9");
}

/// An environment already stopped at this generation does NOTHING. The guard matters because the
/// `stop-{env}` request is deleted after teardown: without it, the missing object reads as "no push
/// requested yet" and every later event on a stopped environment would push again, forever.
#[tokio::test]
async fn an_already_stopped_environment_pushes_nothing_and_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    // Only the two reads the pass makes before the guard; a create or a delete would 404 the mock.
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", env_vol()),
        ],
    );
    let mut e = stopping_env();
    e.status = Some(crd::EnvironmentStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(1),
        node_name: "node-a".into(),
        ..Default::default()
    });

    let action = rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("POST") || c.starts_with("DELETE")),
        "a stopped environment must not push again: {:?}",
        rec.calls()
    );
}

/// The stop request is owned by the Environment, which is the ONLY link back: an environment parked
/// at `StopSnapshotFailed` returns `await_change`, so the request's ownerReference is what the
/// environments controller's `SnapshotRequest` watch maps on to wake it.
#[tokio::test]
async fn the_stop_request_is_owned_by_its_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = stop_routes(None);
    routes.push(rustic_git_workspaces::kube_test::post(
        "/apis/rustic-git.io/v1alpha1/snapshotrequests",
        stop_req(serde_json::json!({"phase": "pending"})),
    ));
    let (ctx, rec) = ctx(tmp.path(), routes);

    rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();
    let req = rec.sent("POST", "/apis/rustic-git.io/v1alpha1/snapshotrequests").remove(0);
    let owner = &req["metadata"]["ownerReferences"][0];
    assert_eq!(owner["kind"], "Environment");
    assert_eq!(owner["name"], "env-1");
    assert_eq!(owner["controller"], true, "only a CONTROLLER ref is mapped back: {req}");
}

const ENV_STATUS_PATH: &str = "/apis/rustic-git.io/v1alpha1/environments/env-1/status";

/// An environment placed on this node authors its OWN Volume child, named after itself and
/// ownerReferenced to it — same skeleton as a workspace, so `DELETE environment` reclaims the disk.
#[tokio::test]
async fn a_placed_environment_creates_its_volume_child_on_its_own_node() {
    let tmp = tempfile::tempdir().unwrap();
    let mut fresh_vol = env_vol();
    fresh_vol["status"] = serde_json::json!({"phase": "working", "subvolumePresent": false});
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::not_found("/apis/rustic-git.io/v1alpha1/volumes/env-1"),
            // The freshly created child has no disk yet, so the pass stops at the readiness wait.
            rustic_git_workspaces::kube_test::post("/apis/rustic-git.io/v1alpha1/volumes", fresh_vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    let sent = rec.sent("POST", "/apis/rustic-git.io/v1alpha1/volumes");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["metadata"]["name"], "env-1", "the child takes the parent's name");
    assert_eq!(sent[0]["spec"]["nodeName"], "node-a", "the Volume is created FROM status.nodeName");
    let refs = sent[0]["metadata"]["ownerReferences"].as_array().expect("an ownerReference");
    assert_eq!(refs[0]["kind"], "Environment");
    assert_eq!(refs[0]["name"], "env-1");
}

/// No Deployment may exist before the disk does: a pod bound to an unmaterialized subvolume wedges
/// forever on `path … does not exist`.
#[tokio::test]
async fn an_environment_with_an_unready_volume_creates_no_deployment() {
    let tmp = tempfile::tempdir().unwrap();
    let mut vol = env_vol();
    vol["status"] = serde_json::json!({"phase": "working", "subvolumePresent": false});
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    let action = rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    assert!(
        !rec.calls().iter().any(|c| c.contains("/statefulsets")),
        "no deployment before its disk exists: {:?}",
        rec.calls()
    );
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["phase"], "creating");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "VolumeNotReady");
}

/// A release-1 environment has no `storage` and names its Volume in the deprecated pointer. It is
/// ADOPTED — never failed for the missing field, and never given a second Volume.
#[tokio::test]
async fn a_legacy_environment_adopts_the_volume_its_spec_names() {
    let tmp = tempfile::tempdir().unwrap();
    let mut vol = env_vol();
    vol["metadata"]["name"] = serde_json::json!("vol-9");
    vol["status"] = serde_json::json!({"phase": "working", "subvolumePresent": false});
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/vol-9", vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let mut e = environment(serde_json::json!({}));
    e.spec.storage = None;
    e.spec.volume_ref = Some("vol-9".into());
    e.spec.node_name = Some("node-a".into());

    rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();

    assert!(rec.sent("POST", "/apis/rustic-git.io/v1alpha1/volumes").is_empty(), "a legacy object is adopted");
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["volumeRef"], "vol-9", "the pointers are mirrored into status");
    assert_eq!(st.last().unwrap()["status"]["nodeName"], "node-a");
}

/// A NEW environment with no `storage` can never build a disk, and no retry adds a field.
#[tokio::test]
async fn a_new_environment_without_storage_fails_permanently() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let mut e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    e.spec.storage = None;

    let action = rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent: never retried");
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["phase"], "error");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "NoStorage");
}

/// A converged environment whose only status delta is `volumeRef` must still write it — the child
/// pointer is how everything else finds the disk, and a guard that ignored it left it unset forever.
#[tokio::test]
async fn an_environment_whose_only_delta_is_its_volume_ref_still_writes_status() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", env_vol()),
            rustic_git_workspaces::kube_test::get(STOP_REQ, stop_req(serde_json::json!({"phase": "done"}))),
            Route { method: "DELETE", path: DEP_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "DELETE", path: STOP_REQ.into(), status: 200, body: stop_req(serde_json::json!({"phase": "done"})) },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );
    let mut e = stopping_env();
    // Everything the guard used to compare is already correct; only `volumeRef` is missing.
    e.status = Some(crd::EnvironmentStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(1),
        node_name: "node-a".into(),
        compatible_nodes: vec!["node-a".into()],
        conditions: vec![rustic_git_workspaces::crd::condition("Ready", true, "Stopped", "pushed and stopped", 1)],
        ..Default::default()
    });
    // Not the idempotency guard's case: that one needs `observedGeneration` AND a volumeRef-free
    // status to be indistinguishable, so bump the generation to force the stop path to run.
    e.metadata.generation = Some(2);

    rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.len(), 1, "one status write: {:?}", rec.calls());
    assert_eq!(st[0]["status"]["volumeRef"], "env-1");
}

// ── in-place restore ─────────────────────────────────────────────────────

const DEP_PATCH: &str = "/apis/apps/v1/namespaces/env-1/statefulsets/db";
const POD_LIST: &str = "/api/v1/namespaces/env-1/pods";
const VOL_PATCH: &str = "/apis/rustic-git.io/v1alpha1/volumes/env-1";

const WISH_AT: &str = "2026-08-27T00:00:00Z";

fn restoring_env(restored_to: Option<&str>) -> (crd::Environment, serde_json::Value) {
    let mut o = env_json(serde_json::json!({"phase": "running", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));
    o["spec"]["services"] =
        serde_json::json!([{"name": "db", "image": "mongo", "command": [], "env": {}, "mounts": []}]);
    o["spec"]["restore"] = serde_json::json!({"snapshotId": "snap-7", "volume": "env-1",
                                              "owner": "acme", "requestedAt": WISH_AT});
    let mut vol = env_vol();
    if let Some(id) = restored_to {
        vol["status"]["restoredTo"] = serde_json::json!(id);
        vol["status"]["restoreRequestedAt"] = serde_json::json!(WISH_AT);
    }
    (serde_json::from_value(o).unwrap(), vol)
}

/// `(name, phase)` — the phase is what decides whether a pod can still be WRITING.
fn pod_list(pods: &[(&str, &str)]) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "PodList", "metadata": {"resourceVersion": "1"},
        "items": pods.iter().map(|(n, phase)| serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": n, "namespace": "env-1"},
            "status": {"phase": phase},
        })).collect::<Vec<_>>(),
    })
}

/// Never restore under a running service: the Deployments go to zero replicas and their pods have
/// to be GONE before the wish reaches the Volume. A subvolume swapped under an open database is
/// corruption nobody can attribute afterwards.
#[tokio::test]
async fn a_restore_wish_scales_the_services_to_zero_before_it_reaches_the_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(None);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", vol.clone()),
            Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
            rustic_git_workspaces::kube_test::get(POD_LIST, pod_list(&[])),
            Route { method: "PATCH", path: VOL_PATCH.into(), status: 200, body: vol },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    let action = rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    assert_eq!(rec.sent("PATCH", DEP_PATCH)[0]["spec"]["replicas"], 0);
    let calls = rec.calls();
    let scaled = calls.iter().position(|c| c == &format!("PATCH {DEP_PATCH}")).unwrap();
    let wished = calls.iter().position(|c| c == &format!("PATCH {VOL_PATCH}")).unwrap();
    assert!(scaled < wished, "the scale-down comes first: {calls:?}");
    assert_eq!(rec.sent("PATCH", VOL_PATCH)[0]["spec"]["restoreTo"]["snapshotId"], "snap-7");
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["type"], "Restoring");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["status"], "True");
}

/// A pod still terminating is a process still writing. The wish waits.
#[tokio::test]
async fn a_restore_waits_for_the_pods_to_actually_be_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(None);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
            rustic_git_workspaces::kube_test::get(POD_LIST, pod_list(&[("db-0", "Running")])),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert!(!rec.calls().iter().any(|c| c == &format!("PATCH {VOL_PATCH}")), "{:?}", rec.calls());
    assert_eq!(rec.sent("PATCH", ENV_STATUS_PATH).last().unwrap()["status"]["conditions"][0]["reason"], "Draining");
}

/// The Volume reports the wished-for snapshot live: the gate is done, so the pass falls through to
/// the ordinary converge — which re-applies every Deployment, and THAT is the scale back up. It
/// must write no second wish and scale nothing down; a gate that fired again here would be an
/// infinite restore loop, since `spec.restore` is deliberately never cleared.
#[tokio::test]
async fn a_matching_restored_to_neither_scales_down_nor_re_wishes() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(Some("snap-7"));
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    // The converge past the gate needs a namespace this mock does not answer for, so the pass
    // errors there. What is under test is everything BEFORE that point.
    let _ = rustic_git_agent::controller::apply_environment(&e, &ctx).await;
    let calls = rec.calls();
    assert!(!calls.iter().any(|c| c == &format!("PATCH {DEP_PATCH}")), "no scale-down: {calls:?}");
    assert!(!calls.iter().any(|c| c == &format!("PATCH {VOL_PATCH}")), "no second wish: {calls:?}");
    assert!(!calls.iter().any(|c| c == &format!("GET {POD_LIST}")), "the gate never ran: {calls:?}");
}

/// A finished pod is not a writer. `Succeeded`/`Failed` pods are never collected on their own, so
/// counting every pod in the namespace waits for something that will not happen — the restore hangs
/// behind a job that ended days ago.
#[tokio::test]
async fn a_finished_pod_does_not_block_the_drain() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, vol) = restoring_env(None);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", vol.clone()),
            Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
            rustic_git_workspaces::kube_test::get(POD_LIST, pod_list(&[("seed-1", "Succeeded"), ("old-1", "Failed")])),
            Route { method: "PATCH", path: VOL_PATCH.into(), status: 200, body: vol },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(rec.sent("PATCH", VOL_PATCH).len(), 1, "the drain is done: {:?}", rec.calls());
}

/// Restoring the SAME snapshot again is a legitimate ask — after undoing a restore by hand, or
/// after a bad afternoon. Comparing snapshot ids alone made the second ask a silent no-op, so the
/// guard compares the (snapshotId, requestedAt) PAIR on both sides.
#[tokio::test]
async fn a_second_wish_for_the_same_snapshot_restores_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut e, vol) = restoring_env(Some("snap-7"));
    let mut spec = e.spec.restore.clone().unwrap();
    spec.requested_at = "2026-08-28T09:00:00Z".into();
    e.spec.restore = Some(spec);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", vol.clone()),
            Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
            rustic_git_workspaces::kube_test::get(POD_LIST, pod_list(&[])),
            Route { method: "PATCH", path: VOL_PATCH.into(), status: 200, body: vol },
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    rustic_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    let sent = rec.sent("PATCH", VOL_PATCH);
    assert_eq!(sent.len(), 1, "the newer wish is a new restore: {:?}", rec.calls());
    assert_eq!(sent[0]["spec"]["restoreTo"]["requestedAt"], "2026-08-28T09:00:00Z");
}

/// The scale back up is `service_statefulset`'s own replica count — the gate does not restore it by
/// hand, the ordinary converge does.
#[test]
fn a_service_statefulset_is_one_replica() {
    let svc = rustic_git_workspaces::model::Service {
        name: "db".into(),
        image: "mongo".into(),
        command: vec![],
        env: Default::default(),
        mounts: vec![],
        ports: vec![],
    };
    let dep = rustic_git_workspaces::k8s::service_statefulset(&svc, "env-1", "acme", &test_pod_ctx()).unwrap();
    assert_eq!(dep.spec.unwrap().replicas, Some(1));
}

// ── the startup migration ────────────────────────────────────────────────

const WS_LIST: &str = "/apis/rustic-git.io/v1alpha1/workspaces";
const ENV_LIST: &str = "/apis/rustic-git.io/v1alpha1/environments";
const OLD_VOL: &str = "/apis/rustic-git.io/v1alpha1/volumes/ws-old";
const OLD_WS_STATUS: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-old/status";
const SNAP_LIST: &str = "/apis/rustic-git.io/v1alpha1/snapshotrequests";

/// A pre-migration Workspace: no status at all, and the new schema prunes the legacy
/// `spec.nodeName`/`spec.volumeRef` on read, so the Volume's own spec is the only surviving
/// pointer to its node.
fn legacy_ws_list() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [{
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-old", "uid": "ws-uid-old"},
            "spec": {"owner": "alice", "team": "", "name": "old", "region": "r1",
                     "image": "nginx:alpine", "desiredState": "running"}
        }]
    })
}

fn old_volume(owned: bool) -> serde_json::Value {
    let mut meta = serde_json::json!({"name": "ws-old", "uid": "vol-uid-old"});
    if owned {
        meta["ownerReferences"] = serde_json::json!([{
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "name": "ws-old", "uid": "ws-uid-old", "controller": true, "blockOwnerDeletion": true
        }]);
    }
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume", "metadata": meta,
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    })
}

fn empty_env_list() -> Route {
    rustic_git_workspaces::kube_test::get(
        ENV_LIST,
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "EnvironmentList", "metadata": {}, "items": []}),
    )
}

fn ws_status_ok() -> Route {
    Route {
        method: "PATCH",
        path: OLD_WS_STATUS.into(),
        status: 200,
        body: legacy_ws_list()["items"][0].clone(),
    }
}

/// Objects written before this change have a Volume with no ownerReference, no `status.nodeName`
/// and a history that exists ONLY in the registry. The migration backfills all three, so the
/// history page and restore keep working from CRs after the roll.
#[tokio::test]
async fn the_migration_adopts_the_volume_and_backfills_placement() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(WS_LIST, legacy_ws_list()),
            rustic_git_workspaces::kube_test::get(OLD_VOL, old_volume(false)),
            Route { method: "PATCH", path: OLD_VOL.into(), status: 200, body: old_volume(true) },
            ws_status_ok(),
            empty_env_list(),
        ],
    );

    rustic_git_agent::migrate::once(&ctx).await;

    let adopted = rec.sent("PATCH", OLD_VOL);
    let refs = adopted[0]["metadata"]["ownerReferences"].as_array().expect("the volume is adopted");
    assert_eq!(refs[0]["kind"], "Workspace");
    assert_eq!(refs[0]["name"], "ws-old");
    assert_eq!(refs[0]["controller"], true);
    let st = rec.sent("PATCH", OLD_WS_STATUS);
    assert_eq!(st[0]["status"]["nodeName"], "node-a", "placement is backfilled from the volume");
    assert_eq!(st[0]["status"]["compatibleNodes"], serde_json::json!(["node-a"]));
    assert_eq!(st[0]["status"]["volumeRef"], "ws-old");
}

/// The whole safety argument for running this next to live reconcilers: a second pass writes
/// nothing, so a crash mid-migration costs one extra pass and nothing else.
#[tokio::test]
async fn a_second_migration_pass_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut migrated = legacy_ws_list();
    migrated["items"][0]["status"] =
        serde_json::json!({"phase": "ready", "nodeName": "node-a", "compatibleNodes": ["node-a"], "volumeRef": "ws-old"});
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(WS_LIST, migrated),
            rustic_git_workspaces::kube_test::get(OLD_VOL, old_volume(true)),
            empty_env_list(),
        ],
    );

    rustic_git_agent::migrate::once(&ctx).await;

    assert!(rec.sent("PATCH", OLD_VOL).is_empty(), "an already-adopted volume is not re-patched");
    assert!(rec.sent("PATCH", OLD_WS_STATUS).is_empty(), "an already-placed parent is not rewritten");
}

/// A Workspace this node does not hold is left entirely alone — the migration is per node, exactly
/// like the reconcilers it feeds.
#[tokio::test]
async fn a_workspace_on_another_node_is_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let mut elsewhere = old_volume(false);
    elsewhere["spec"]["nodeName"] = "node-b".into();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(WS_LIST, legacy_ws_list()),
            rustic_git_workspaces::kube_test::get(OLD_VOL, elsewhere),
            empty_env_list(),
        ],
    );

    rustic_git_agent::migrate::once(&ctx).await;

    assert!(rec.sent("PATCH", OLD_VOL).is_empty());
    assert!(rec.sent("PATCH", OLD_WS_STATUS).is_empty());
}

/// The history half: one `SnapshotRequest` per registry commit record, and a name derived from the
/// record id so a re-run collides instead of duplicating — a 409 is success, not an error.
#[tokio::test]
async fn registry_history_becomes_one_snapshot_request_per_record_and_tolerates_a_409() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = stub_history(serde_json::json!([
        {"id": "rec-a", "state": null, "lineage": [], "region": "r1", "message": "first",
         "created_at": "2026-01-01T00:00:00Z"},
        {"id": "rec-b", "state": null, "lineage": [], "region": "r1", "message": null,
         "created_at": "2026-01-02T00:00:00Z"}
    ]))
    .await;
    let mut placed = legacy_ws_list();
    placed["items"][0]["status"] =
        serde_json::json!({"phase": "ready", "nodeName": "node-a", "compatibleNodes": ["node-a"], "volumeRef": "ws-old"});
    let created = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": "snap-ws-old-rec-a", "uid": "sr-1"}, "spec": {"volume": "ws-old"}
    });
    let (ctx, rec) = ctx_with_registry(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(WS_LIST, placed),
            rustic_git_workspaces::kube_test::get(OLD_VOL, old_volume(true)),
            rustic_git_workspaces::kube_test::post(SNAP_LIST, created.clone()),
            // The second record's object already exists — the re-run case, in one pass.
            rustic_git_workspaces::kube_test::conflict(SNAP_LIST),
            Route {
                method: "PATCH",
                path: "/apis/rustic-git.io/v1alpha1/snapshotrequests/snap-ws-old-rec-a/status".into(),
                status: 200,
                body: created.clone(),
            },
            Route {
                method: "PATCH",
                path: "/apis/rustic-git.io/v1alpha1/snapshotrequests/snap-ws-old-rec-b/status".into(),
                status: 200,
                body: created,
            },
            empty_env_list(),
        ],
        &registry,
    );

    rustic_git_agent::migrate::once(&ctx).await;

    let posts = rec.sent("POST", SNAP_LIST);
    assert_eq!(posts.len(), 2, "one request per record: {:?}", rec.calls());
    assert_eq!(posts[0]["metadata"]["name"], "snap-ws-old-rec-a");
    assert_eq!(posts[0]["spec"]["message"], "first");
    assert_eq!(posts[0]["metadata"]["labels"]["rustic-git.io/volume"], "ws-old");
    assert_eq!(posts[1]["metadata"]["name"], "snap-ws-old-rec-b");
    // The 409'd create still writes `done`: a crash between create and status would otherwise
    // leave a `pending` request that the snapshot reconciler runs as a real push.
    let conflicted = rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/snapshotrequests/snap-ws-old-rec-b/status");
    assert_eq!(conflicted.len(), 1, "a 409 on create still patches status: {:?}", rec.calls());
    assert_eq!(conflicted[0]["status"]["phase"], "done");
    let st = rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/snapshotrequests/snap-ws-old-rec-a/status");
    assert_eq!(st.len(), 1);
    assert_eq!(st[0]["status"]["phase"], "done");
    assert_eq!(st[0]["status"]["snapshotId"], "rec-a");
    assert_eq!(st[0]["status"]["lineageTip"], "rec-a");
    assert_eq!(st[0]["status"]["nodeName"], serde_json::Value::Null, "a backfilled request names no node");
}

/// A one-response HTTP stub for the registry's `history` read. Raw TCP rather than a server crate:
/// the agent's test deps do not carry one, and this is the only HTTP the migration speaks.
async fn stub_history(body: serde_json::Value) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let body = body.to_string();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                assert!(
                    req.starts_with("GET /vol-agent/alice/ws-old/history "),
                    "the migration must read the volume's history: {req}"
                );
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(body.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

/// A legacy parent whose Volume has gone is otherwise stuck forever: it still looks legacy, so the
/// claim watch will not place it and nothing ever reports the missing disk. Placement is backfilled
/// from its own deprecated `spec.nodeName` so the reconciler picks it up and says so.
#[tokio::test]
async fn a_legacy_parent_whose_volume_is_missing_is_still_placed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut list = legacy_ws_list();
    list["items"][0]["spec"]["nodeName"] = "node-a".into();
    list["items"][0]["spec"]["volumeRef"] = "ws-old".into();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(WS_LIST, list),
            rustic_git_workspaces::kube_test::not_found(OLD_VOL),
            ws_status_ok(),
            empty_env_list(),
        ],
    );

    rustic_git_agent::migrate::once(&ctx).await;

    let st = rec.sent("PATCH", OLD_WS_STATUS);
    assert_eq!(st.len(), 1, "placement falls back to the parent's own node: {:?}", rec.calls());
    assert_eq!(st[0]["status"]["nodeName"], "node-a");
    assert_eq!(st[0]["status"]["volumeRef"], "ws-old");
}

// ── the fixes from the final branch review ───────────────────────────────

/// A `cloneOf` source is resolved as a VOLUME, so it works for both parent kinds. `clone_env`
/// writes the environment's id there, and a workspace-only lookup meant a cloned environment was
/// never claimed by anyone.
fn src_volume(node: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "env-src", "uid": "env-src-uid"},
        "spec": {"owner": "acme", "team": "", "nodeName": node, "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    })
}

fn cloned_env(source: &str) -> crd::Environment {
    let mut e = environment(serde_json::json!({}));
    e.spec.storage =
        Some(crd::WorkspaceStorage { quota_gb: 20, source: Some(crd::VolumeSource::CloneOf { volume: source.into() }) });
    e
}

const ENV_STATUS: &str = "/apis/rustic-git.io/v1alpha1/environments/env-1/status";
const SRC_VOL: &str = "/apis/rustic-git.io/v1alpha1/volumes/env-src";

#[tokio::test]
async fn a_cloned_environment_is_claimed_where_its_source_volume_lives() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(SRC_VOL, src_volume("node-a")),
            Route { method: "PUT", path: ENV_STATUS.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::post(
                BINDINGS,
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-acme"},
                                   "spec": {"owner": "acme", "region": "r1", "nodeName": "node-a"}}),
            ),
        ],
    );

    rustic_git_agent::claim::claim_environment(&cloned_env("env-src"), &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", ENV_STATUS).len(), 1, "the source's disk is here: {:?}", rec.calls());
}

#[tokio::test]
async fn a_cloned_environment_is_not_claimed_off_its_sources_node() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![rustic_git_workspaces::kube_test::get(SRC_VOL, src_volume("node-b"))]);

    rustic_git_agent::claim::claim_environment(&cloned_env("env-src"), &ctx).await.unwrap();
    assert!(rec.sent("PUT", ENV_STATUS).is_empty(), "node-a holds nothing of env-src: {:?}", rec.calls());
}

/// The permanent path is a status write like any other, so it needs the same no-op guard: without
/// it every reconcile re-stamps `lastTransitionTime`, the write is its own watch event, and a
/// permanently-broken object spins against the API server until someone fixes its spec.
#[tokio::test]
async fn a_second_reconcile_of_a_settled_workspace_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let mut w = workspace(serde_json::json!({
        "phase": "error",
        "nodeName": "node-a",
        "compatibleNodes": ["node-a"],
        "conditions": [{"type": "Ready", "status": "False", "reason": "NoStorage",
                        "message": "spec.storage is required", "observedGeneration": 1,
                        "lastTransitionTime": "2026-08-27T00:00:00Z"}]
    }));
    w.spec.storage = None;

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.calls().is_empty(), "an already-settled object writes nothing: {:?}", rec.calls());
}

/// A `done` stop request that is TERMINATING is not a landed push — it is the previous stop's
/// object on its way out. Reading it as done would tear the environment down without pushing.
#[tokio::test]
async fn a_terminating_stop_request_is_treated_as_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let mut terminating = stop_req(serde_json::json!({"phase": "done", "snapshotId": "layer-1"}));
    terminating["metadata"]["deletionTimestamp"] = serde_json::json!("2026-08-27T00:00:00Z");
    terminating["metadata"]["finalizers"] = serde_json::json!(["rustic-git.io/snapshot"]);
    let mut routes = stop_routes(Some(terminating));
    routes.push(rustic_git_workspaces::kube_test::post(
        "/apis/rustic-git.io/v1alpha1/snapshotrequests",
        stop_req(serde_json::json!({"phase": "pending"})),
    ));
    let (ctx, rec) = ctx(tmp.path(), routes);

    rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("DELETE")),
        "nothing may be torn down against a terminating request: {:?}",
        rec.calls()
    );
    assert!(
        rec.calls().iter().any(|c| c == "POST /apis/rustic-git.io/v1alpha1/snapshotrequests"),
        "a fresh push is requested instead: {:?}",
        rec.calls()
    );
}

/// Stopping needs neither the disk nor the namespace — only a pod delete. Gated on the Volume, a
/// workspace whose subvolume failed could never be stopped, so it kept its pod forever.
#[tokio::test]
async fn a_stopped_workspace_with_a_broken_volume_still_loses_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let ns = crd::ws_namespace("alice", "");
    let pod_del = format!("/api/v1/namespaces/{ns}/pods/ws-1");
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "DELETE", path: pod_del.clone(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let mut w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a", "volumeRef": "ws-1"}));
    w.spec.desired_state = crd::DesiredState::Stopped;

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {pod_del}")), "{:?}", rec.calls());
    assert!(
        !rec.calls().iter().any(|c| c.contains("/volumes/")),
        "the stop must not depend on the Volume at all: {:?}",
        rec.calls()
    );
    assert_eq!(rec.sent("PATCH", WS_STATUS).last().unwrap()["status"]["phase"], "stopped");
}

/// The migration backfills PLACEMENT, not phase. Overwriting a running workspace's phase with
/// `pending` made the UI flicker "starting" on every roll.
#[tokio::test]
async fn the_migration_keeps_the_objects_existing_phase() {
    let tmp = tempfile::tempdir().unwrap();
    let mut unplaced = legacy_ws_list();
    unplaced["items"][0]["status"] = serde_json::json!({"phase": "ready"});
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(WS_LIST, unplaced),
            rustic_git_workspaces::kube_test::get(OLD_VOL, old_volume(true)),
            ws_status_ok(),
            empty_env_list(),
        ],
    );

    rustic_git_agent::migrate::once(&ctx).await;

    let st = rec.sent("PATCH", OLD_WS_STATUS);
    assert_eq!(st[0]["status"]["nodeName"], "node-a");
    assert_eq!(st[0]["status"]["phase"], "ready", "placement is backfilled, phase is left alone");
}

// ── completion wakes the reconciler ──────────────────────────────────────

/// Take one wake-up, or fail well before the 15s requeue that used to be the only path.
async fn wake<T>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("no wake-up before the timeout: the object would have waited out TICK")
        .expect("the wake channel closed")
}

/// A finished volume operation sends its own ref, so the reconcile that writes `ready` happens on
/// completion rather than on the 15s tick.
#[tokio::test]
async fn a_finished_volume_operation_wakes_its_reconciler() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let (mut vol_wakes, _snap_wakes, _ws_wakes) = ctx.wakes.lock().unwrap().take().unwrap();
    let v = volume(3);

    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));

    assert_eq!(wake(&mut vol_wakes).await.name, "vol-1");

    // The woken pass observes the handle and writes the outcome. There is no btrfs on the test
    // host, so that outcome is `error` — the `ready` half is `a_finished_operation_writes_observed_
    // generation_and_stops_requeueing`; what is under test here is that the pass happens at all.
    wait_idle(&ctx).await;
    rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    let sent = rec.sent("PATCH", VOL_STATUS);
    let last = sent.last().unwrap();
    assert_ne!(last["status"]["phase"], "working", "the wake-up pass must leave `working`: {last}");
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");
}

/// Same for a push: the wake fires on completion whatever the outcome (here the push fails — no
/// registry listens in these tests — and the next reconcile writes `error` without waiting a tick).
#[tokio::test]
async fn a_finished_push_wakes_its_reconciler() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(VOL_GET, vol_on("node-a")),
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: snap_json(serde_json::json!({})) },
        ],
    );
    let (_vol_wakes, mut snap_wakes, _ws_wakes) = ctx.wakes.lock().unwrap().take().unwrap();

    let action = rustic_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));

    assert_eq!(wake(&mut snap_wakes).await.name, "snap-1");

    wait_idle(&ctx).await;
    let action = rustic_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({"phase": "working"})), &ctx)
        .await
        .unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    let sent = rec.sent("PATCH", SNAP_STATUS);
    assert_eq!(sent.last().unwrap()["status"]["phase"], "error", "{:?}", sent.last());
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");
}

/// The success half: a finished operation whose outcome is `Ready` wakes the reconciler AND the
/// woken pass writes `ready` with the generation it ran for. Stubbed through `wake_on_finish`
/// rather than through real work, because the test host has no btrfs.
#[tokio::test]
async fn a_successful_volume_operation_wakes_and_then_writes_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let (mut vol_wakes, _snap_wakes, _ws_wakes) = ctx.wakes.lock().unwrap().take().unwrap();
    let v = volume(5);
    let handle = rustic_git_agent::controller::wake_on_finish(
        tokio::task::spawn_blocking(|| Ok(Done { phase: crd::Phase::Ready, ..Done::default() })),
        ctx.wake_volume.clone(),
        kube::runtime::reflector::ObjectRef::<crd::Volume>::new("vol-1"),
    );
    ctx.running.lock().unwrap().insert("uid-1".to_string(), (5, handle));

    assert_eq!(wake(&mut vol_wakes).await.name, "vol-1");

    wait_idle(&ctx).await;
    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    let sent = rec.sent("PATCH", VOL_STATUS);
    let last = sent.last().unwrap();
    assert_eq!(last["status"]["phase"], "ready", "{last}");
    assert_eq!(last["status"]["observedGeneration"], 5);
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");
}

/// Same for a push that lands: the wake arrives and the woken pass writes `done` with the id.
#[tokio::test]
async fn a_successful_push_wakes_and_then_writes_done() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(VOL_GET, vol_on("node-a")),
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: snap_json(serde_json::json!({})) },
        ],
    );
    let (_vol_wakes, mut snap_wakes, _ws_wakes) = ctx.wakes.lock().unwrap().take().unwrap();
    let handle = rustic_git_agent::controller::wake_on_finish(
        tokio::task::spawn_blocking(|| Ok(Done { phase: crd::Phase::Done, lineage_tip: Some("layer-9".into()), restored_to: None })),
        ctx.wake_snapshot.clone(),
        kube::runtime::reflector::ObjectRef::<crd::SnapshotRequest>::new("snap-1"),
    );
    ctx.running.lock().unwrap().insert("snap-uid-1".to_string(), (1, handle));

    assert_eq!(wake(&mut snap_wakes).await.name, "snap-1");

    wait_idle(&ctx).await;
    let action = rustic_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({"phase": "working"})), &ctx)
        .await
        .unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    let last = rec.sent("PATCH", SNAP_STATUS).last().unwrap().clone();
    assert_eq!(last["status"]["phase"], "done", "{last}");
    assert_eq!(last["status"]["snapshotId"], "layer-9");
    assert_eq!(last["status"]["observedGeneration"], 1);
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");
}

/// A snapshot outlives its `SnapshotRequest` — the env-stop request is deleted after teardown, and
/// nothing keeps a push request forever. Validating a `restoreOf` against a `done` CR therefore
/// made a deleted environment's snapshots unrestorable while their records sat untouched in the
/// registry, so the work starts with NO SnapshotRequest present at all and the registry gets to
/// answer.
#[tokio::test]
async fn a_restore_starts_without_any_snapshot_request() {
    let tmp = tempfile::tempdir().unwrap();
    // No `snapshotrequests` route is registered: a lookup would fail outright, which is the point.
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let mut v = volume(1);
    v.spec.source = Some(crd::VolumeSource::RestoreOf {
        volume: "env-gone".into(),
        snapshot_id: "snap-old".into(),
        owner: None,
        region: None,
    });

    rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();

    assert!(
        ctx.running.lock().unwrap().contains_key("uid-1"),
        "the restore work must start: {:?}",
        rec.calls()
    );
    assert!(
        !rec.calls().iter().any(|c| c.contains("snapshotrequests")),
        "the CR is the push work item, never the snapshot index: {:?}",
        rec.calls()
    );
}

/// A restore that cannot reach its snapshot's region settles: one `phase: error` status write
/// naming the region, and `await_change` — not a requeue that retries a missing Secret key every
/// 15 seconds forever. This is the loop half of the 27 Aug hang; the engine half is
/// `crates/workspaces/tests/engine_ops.rs`.
#[tokio::test]
async fn a_restore_from_an_unreachable_region_settles_and_stops_requeueing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let mut v = volume(1);
    v.spec.source = Some(crd::VolumeSource::RestoreOf {
        volume: "env-gone".into(),
        snapshot_id: "snap-old".into(),
        owner: None,
        // No `AZURE_REGION_NOWHERE_*` on this node, which is the whole point.
        region: Some("nowhere".into()),
    });

    rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    wait_idle(&ctx).await;
    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();

    assert_eq!(action, kube::runtime::controller::Action::await_change(), "a missing credential is not retryable");
    let sent = rec.sent("PATCH", VOL_STATUS);
    let last = sent.last().expect("a status write");
    assert_eq!(last["status"]["phase"], "error");
    let cond = &last["status"]["conditions"][0];
    assert_eq!(cond["reason"], "RegionUnreachable");
    assert_eq!(cond["status"], "False");
    assert!(cond["message"].as_str().unwrap().contains("nowhere"), "the condition must name it: {cond}");
}


// ── the packages step ────────────────────────────────────────────────────

/// The mocked `Ctx` every profile test wants: a fake Nix it can inspect, its own profile root, and
/// a workspace whose Volume answers ready so the pass reaches the packages step.
fn ws_ctx_with_nix(pool: &std::path::Path) -> (Arc<Ctx>, Recorder, Arc<FakeNix>) {
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    let fake = Arc::new(FakeNix::default());
    let (ctx, rec) = ctx_full(
        pool,
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-1", vol),
            ready_binding(),
            pv_route("pv-ws-1"),
            pvc_route("live-ws-1"),
            pv_route("nix-ws-1"),
            pvc_route("nix-ws-1"),
            rustic_git_workspaces::kube_test::post(
                "/api/v1/namespaces/ws-alice/pods",
                serde_json::json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws-1"}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
        "http://127.0.0.1:1",
        fake.clone(),
    );
    (ctx, rec, fake)
}

fn ready_workspace(id: &str, packages: Vec<String>) -> crd::Workspace {
    let mut o = ws_json(serde_json::json!({"phase": "creating", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));
    o["metadata"]["name"] = id.into();
    o["spec"]["packages"] = serde_json::json!(packages);
    serde_json::from_value(o).unwrap()
}

/// Apply until the profile step stops asking to be requeued: the build runs on its own thread, so
/// the pass that observes it is a later one — as with every other long operation here.
async fn apply_until_settled(w: &crd::Workspace, ctx: &Arc<Ctx>) {
    for _ in 0..4 {
        let _ = rustic_git_agent::controller::apply_workspace(w, ctx).await.unwrap();
        if ctx.running.lock().unwrap().is_empty() {
            return;
        }
        wait_idle(ctx).await;
    }
    panic!("the profile step never settled");
}

fn packages_condition(status: &serde_json::Value) -> serde_json::Value {
    status["status"]["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "PackagesReady")
        .unwrap_or_else(|| panic!("no PackagesReady condition in {status}"))
        .clone()
}

/// The profile is built from the spec, and the pod only exists once it is — a container started on
/// a stale profile is a workspace whose tools silently disagree with what it declares.
#[tokio::test]
async fn a_workspace_builds_its_profile_from_its_spec_before_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    let ws = ready_workspace("ws-1", vec!["hello".into()]);
    apply_until_settled(&ws, &ctx).await;

    let builds = fake.builds.lock().unwrap().clone();
    assert_eq!(builds.len(), 1);
    assert!(builds[0].0.contains("paths = [ pkgs.hello ];"), "{}", builds[0].0);
    assert!(builds[0].1.ends_with("ws-1.building"));
    let calls = rec.calls();
    let built = calls.iter().position(|c| c.contains("/status")).unwrap();
    let pod = calls.iter().position(|c| c.starts_with("POST") && c.contains("/pods")).unwrap();
    assert!(built < pod, "status (Building/Built) before the pod is created: {calls:?}");
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(st["status"]["packages"]["observed"][0], "hello");
    assert_eq!(packages_condition(&st)["reason"], "Built");
}

/// An empty list is still a profile: the pod mounts it as a subPath of a read-only claim, so a
/// missing link is a pod that cannot mount at all.
#[tokio::test]
async fn a_workspace_with_no_packages_still_gets_a_profile_before_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    let ws = ready_workspace("ws-1", vec![]);
    apply_until_settled(&ws, &ctx).await;

    let builds = fake.builds.lock().unwrap().clone();
    assert_eq!(builds.len(), 1, "an empty profile is still built");
    assert!(builds[0].0.contains("paths = [  ];"), "{}", builds[0].0);
    assert!(rustic_git_agent::nix::profile_exists(&ctx.profiles_dir, "ws-1"), "the link the pod mounts");
    let calls = rec.calls();
    let built = calls.iter().position(|c| c.contains("/status")).unwrap();
    let pod = calls.iter().position(|c| c.starts_with("POST") && c.contains("/pods")).unwrap();
    assert!(built < pod, "the profile exists before the pod does: {calls:?}");
}

/// The hash is what makes this idempotent: same pin, same list, a link on disk — no nix at all.
#[tokio::test]
async fn a_matching_hash_and_present_link_skip_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, fake) = ws_ctx_with_nix(tmp.path());
    let mut ws = ready_workspace("ws-1", vec!["hello".into()]);
    let pin = rustic_git_agent::nix::nixpkgs_pin();
    ws.status.as_mut().unwrap().packages = Some(rustic_git_workspaces::crd::PackagesStatus {
        observed: vec!["hello".into()],
        observed_hash: Some(rustic_git_workspaces::packages::hash(&pin, &["hello".into()])),
        profile: None,
        nixpkgs: Some(pin),
    });
    std::os::unix::fs::symlink("/tmp", rustic_git_agent::nix::profile_path(&ctx.profiles_dir, "ws-1")).unwrap();
    let _ = rustic_git_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
    assert!(fake.builds.lock().unwrap().is_empty(), "nothing to build");
}

/// A build that fails never touches the live profile: the workspace keeps the tools it had and the
/// reason is on its status, rather than a pod that cannot start. And the FAILED list is not
/// recorded as observed — doing so makes the next pass see a match and never retry.
#[tokio::test]
async fn a_failed_build_keeps_the_old_profile_and_retries_later() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    std::os::unix::fs::symlink("/tmp", rustic_git_agent::nix::profile_path(&ctx.profiles_dir, "ws-1")).unwrap();
    *fake.answer.lock().unwrap() = Err("error: attribute 'nodejs_99' missing".into());
    let ws = ready_workspace("ws-1", vec!["nodejs_99".into()]);
    apply_until_settled(&ws, &ctx).await;

    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    let c = packages_condition(&st);
    assert_eq!(c["reason"], "BuildFailed");
    assert!(c["message"].as_str().unwrap().contains("nodejs_99"));
    assert!(rustic_git_agent::nix::profile_exists(&ctx.profiles_dir, "ws-1"), "the previous profile is untouched");
    assert!(rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")), "the pod still runs on the old profile");

    // The next pass reads back the status just written — which is where recording the FAILED list
    // as observed would make it see a hash match plus a link on disk, and never retry.
    let mut o = serde_json::to_value(&ws).unwrap();
    o["status"] = st["status"].clone();
    let ws = serde_json::from_value::<crd::Workspace>(o).unwrap();
    *fake.answer.lock().unwrap() = Ok(());
    apply_until_settled(&ws, &ctx).await;
    assert_eq!(fake.builds.lock().unwrap().len(), 2, "a failure is retried");
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(st["status"]["packages"]["observed"][0], "nodejs_99", "recorded only once it built");
}

/// The API validates, but the object is not only written by the API: a name that is not an
/// attribute must be refused again here, before it can be rendered into an expression.
#[tokio::test]
async fn an_invalid_spec_entry_never_reaches_nix() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    let ws = ready_workspace("ws-1", vec!["$(id)".into()]);   // written past the API, e.g. kubectl
    let _ = rustic_git_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
    assert!(fake.builds.lock().unwrap().is_empty());
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(packages_condition(&st)["reason"], "BuildFailed");
    assert!(!rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")), "no profile ever existed, so no pod");
}
