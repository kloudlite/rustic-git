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
use std::sync::Arc;

const VOL_STATUS: &str = "/apis/rustic-git.io/v1alpha1/volumes/vol-1/status";

/// A fake `Nix` that records the expressions it was asked to build and answers as told. It
/// returns a STORE PATH, as the real one does: the link and the publish are the reconciler's job,
/// not nix's, because `nix -o`'s auto GC root does not survive the rename.
struct FakeNix {
    builds: std::sync::Mutex<Vec<String>>,
    answer: std::sync::Mutex<Result<(), String>>,
    ping: std::sync::Mutex<Result<(), String>>,
    /// Run while a build is "in flight", so a test can change the spec mid-build.
    on_build: std::sync::Mutex<Option<Box<dyn Fn() + Send>>>,
}
impl Default for FakeNix {
    fn default() -> Self {
        FakeNix {
            builds: std::sync::Mutex::new(Vec::new()),
            answer: std::sync::Mutex::new(Ok(())),
            ping: std::sync::Mutex::new(Ok(())),
            on_build: std::sync::Mutex::new(None),
        }
    }
}
#[async_trait::async_trait]
impl rustic_git_agent::nix::Nix for FakeNix {
    async fn build(&self, expr: &str, _: std::time::Duration) -> Result<std::path::PathBuf, String> {
        self.builds.lock().unwrap().push(expr.to_string());
        if let Some(f) = self.on_build.lock().unwrap().take() {
            f();
        }
        let r = self.answer.lock().unwrap().clone();
        r.map(|()| std::path::PathBuf::from("/tmp"))
    }
    async fn ping(&self) -> Result<(), String> { self.ping.lock().unwrap().clone() }
    async fn collect_garbage(&self) -> Result<u64, String> { Ok(0) }
}

/// A profile as a finished build leaves it: the directory the pod mounts, with `current` inside.
/// The list the node actually hashes: the platform base set first, then the workspace's own.
fn with_base(own: &[String]) -> Vec<String> {
    let base = rustic_git_agent::nix::base_packages();
    let mut all = base.clone();
    all.extend(own.iter().filter(|p| !base.contains(p)).cloned());
    all
}

fn plant_profile(ctx: &Arc<Ctx>, id: &str) {
    std::fs::create_dir_all(rustic_git_agent::nix::profile_dir(&ctx.profiles_dir, id)).unwrap();
    std::os::unix::fs::symlink("/tmp", rustic_git_agent::nix::profile_path(&ctx.profiles_dir, id)).unwrap();
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
    ctx_full(pool, routes, Arc::new(FakeNix::default()))
}

/// The one constructor: every test's profile root is a directory under its own pool tempdir, so no
/// test can reach the node's real `/nix` and none of them race each other over it.
fn ctx_full(pool: &std::path::Path, routes: Vec<Route>, nix: Arc<FakeNix>) -> (Arc<Ctx>, Recorder) {
    ctx_with_homes_export(pool, routes, nix, Some("test:/".into()))
}

/// The `WS_HOMES_EXPORT`-unset variant: a node with no shared-home mount, which every workspace
/// reconcile must park on rather than start a pod against.
fn ctx_without_homes_export(pool: &std::path::Path, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
    ctx_with_homes_export(pool, routes, Arc::new(FakeNix::default()), None)
}

fn ctx_with_homes_export(pool: &std::path::Path, mut routes: Vec<Route>, nix: Arc<FakeNix>, homes_export: Option<String>) -> (Arc<Ctx>, Recorder) {
    // Every reconcile now unconditionally may ask "does this volume have commits yet"
    // (`claim::commit_placement`/`has_commits`, the checkout/migrate step) — a call no test fixture
    // needed before the commit model became the only model (Task 8). Appended AFTER the caller's
    // own routes, so a test that mocks its own `/snapshots` response (exact history, retention,
    // …) still hits that one first; this is only the default "nothing here yet" answer for every
    // test that never cared about snapshots at all.
    routes.push(rustic_git_workspaces::kube_test::get(
        "/apis/rustic-git.io/v1alpha1/snapshots",
        serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": []}),
    ));
    // Likewise the stop's flush gate: a workspace stop now waits until another node holds its
    // final sync point, so the default answer for every test that is not ABOUT the flush is
    // "already cut, already replicated". Appended last, so a flush test's own routes win.
    routes.push(rustic_git_workspaces::kube_test::get(
        WS_STOP_REQ,
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                           "metadata": {"name": "stop-ws-1", "uid": "stop-ws-uid", "creationTimestamp": rfc3339_ago(60)},
                           "spec": {"volume": "ws-1", "owner": "alice", "worktree": "ws-1", "transient": true},
                           "status": {"phase": "ready", "readyAt": rfc3339_ago(30)}}),
    ));
    routes.push(rustic_git_workspaces::kube_test::get(
        REPLICAS,
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplicaList",
                           "metadata": {"resourceVersion": "1"},
                           "items": [{"apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                                      "metadata": {"name": "ws-1.node-b", "uid": "vr-b"},
                                      "spec": {"volume": "ws-1", "node": "node-b"},
                                      "status": {"phase": "Synced", "branches": {}, "lastSyncAt": rfc3339_ago(1)}}]}),
    ));
    routes.push(Route { method: "DELETE", path: WS_STOP_REQ.into(), status: 200, body: serde_json::json!({"kind": "Status"}) });
    let (client, rec) = mock_client(routes);
    // Best effort: one test hands a plain file as its "pool" on purpose.
    let profiles = pool.join("profiles");
    let _ = std::fs::create_dir_all(&profiles);
    let engine = Engine::new(Pool::new(pool));
    // Ctx::new reads the pinned default image from the environment, as the agent does.
    std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
    (
        Arc::new(Ctx::new(
            client,
            Arc::new(engine),
            "node-a".into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            vec!["session".into(), "env".into()],
            homes_export,
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

/// An agent that rolls mid-operation loses the handle but keeps the `observedGeneration` its
/// pass already stamped: the object is left `Working` with nothing running. The next reconcile
/// must re-run the pass rather than treat the generation as done, or the volume stays `Working`
/// forever while its data is perfectly healthy.
#[tokio::test]
async fn a_working_volume_with_nothing_running_is_re_run_not_left_stranded() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let mut v = volume(7);
    v.status = Some(rustic_git_workspaces::crd::VolumeStatus {
        phase: rustic_git_workspaces::crd::Phase::Working,
        observed_generation: Some(7),
        subvolume_present: true,
        ..Default::default()
    });

    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_ne!(
        action,
        kube::runtime::controller::Action::await_change(),
        "a stranded Working volume must be picked back up, not awaited forever"
    );
    assert!(!ctx.running.lock().unwrap().is_empty(), "the recovery pass must actually start work");
    wait_idle(&ctx).await;
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
        format!("/apis/rustic-git.io/v1alpha1/ownerbindings/{}", crd::binding_name("r1", "alice")),
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                           "metadata": {"name": "r1-alice"},
                           "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"},
                           "status": {"conditions": [{"type": "NamespaceReady", "status": "True",
                                                      "reason": "Converged", "message": "ok",
                                                      "lastTransitionTime": "2026-08-27T00:00:00Z"}]}}),
    )
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
/// by a controller. Everything downstream (the Volume's node, the pod's hostPath and nodeSelector)
/// is derived from this one field.
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


/// The owner→node pin is gone: the home is a directory on a region-shared NFS mount every node
/// serves, so a binding naming another node no longer says anything about where this object's data
/// is. Placement is `may_claim` alone — and the binding, which still ensures namespaces, is now
/// reconciled on every node.
#[tokio::test]
async fn a_binding_on_another_node_no_longer_blocks_a_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(
                format!("/apis/rustic-git.io/v1alpha1/ownerbindings/{}", crd::binding_name("r1", "alice")),
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-alice"},
                                   "spec": {"owner": "alice", "region": "r1", "nodeName": "node-b"}}),
            ),
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );

    rustic_git_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();

    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "a binding elsewhere no longer defers the claim: {:?}", rec.calls());
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
}

/// A node that cannot serve homes must not claim at all: `apply_workspace` would park the object
/// at `HomeNotReady` forever, and nothing ever un-places a live node's claim.
#[tokio::test]
async fn a_node_without_a_homes_export_does_not_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx_without_homes_export(tmp.path(), vec![]);

    rustic_git_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "no claim without a shared home: {:?}", rec.calls());
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

/// Losing the race must be a REAL conflict. `Patch::Apply(..).force()` never conflicts — it is the
/// wrong primitive for the one write in this system that must race — so the claim is an optimistic
/// write carrying the object's `resourceVersion`, and a 409 means another node won.
///
/// A 409 is not assumed to mean "placed": the claim RE-READS and runs the same decision again, so
/// a peer that only widened `compatibleNodes` does not scare this node off a claim it may still
/// make. Here the peer really did place it, so the re-read decides "leave it alone".
///
/// The loser must also not create the OwnerBinding: only the node whose claim actually won should
/// ever write it, or two nodes race to author the owner's one binding object.
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
        source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into(), commit: None }),
    });

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(
        rec.sent("PUT", WS_STATUS).is_empty(),
        "node-a does not hold ws-src's disk and must not claim its clone: {:?}", rec.calls()
    );
}

// ── commit-model placement ──────────────────────────────────────────────

const SNAPSHOTS_LIST: &str = "/apis/rustic-git.io/v1alpha1/snapshots";

fn commit_list_of(kind: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({"apiVersion": "v1", "kind": format!("{kind}List"), "items": items})
}

fn snapshot_cr(name: &str, volume: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name, "uid": "snap-uid"},
        "spec": {"volume": volume, "owner": "alice", "worktree": "ws-1", "parent": "", "pinned": false},
        "status": {"phase": "ready"},
    })
}

fn volume_replica(volume: &str, node: &str, phase: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": crd::replica_name(volume, node), "uid": "vr-uid"},
        "spec": {"volume": volume, "node": node},
        "status": {"phase": phase, "branches": {}},
    })
}

/// A workspace whose volume already has commits and whose replica on THIS node reports Synced is
/// claimed exactly like the old `compatibleNodes` arm — ruling A.
#[tokio::test]
async fn commit_model_a_synced_replica_claims_a_workspace_with_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: commit_list_of("Snapshot", vec![snapshot_cr("vol-1-a", "vol-1")]) },
            rustic_git_workspaces::kube_test::get(
                format!("/apis/rustic-git.io/v1alpha1/volumereplicas/{}", crd::replica_name("vol-1", "node-a")),
                volume_replica("vol-1", "node-a", "Synced"),
            ),
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "Synced: claimed");
}

/// The same volume, but this node's replica is Syncing (or absent) — ruling A's other half: no
/// claim, and the object is left unplaced for whichever node IS Synced.
#[tokio::test]
async fn commit_model_a_syncing_replica_does_not_claim_a_workspace_with_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: commit_list_of("Snapshot", vec![snapshot_cr("vol-1-a", "vol-1")]) },
            rustic_git_workspaces::kube_test::not_found(format!(
                "/apis/rustic-git.io/v1alpha1/volumereplicas/{}",
                crd::replica_name("vol-1", "node-a")
            )),
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "no replica on this node: never started dataless");
}

/// A volume with ZERO `Snapshot` CRs is the bootstrap case (ruling B) — claimable by any pool
/// node with no replica at all, same as the old empty-`compatibleNodes` arm.
#[tokio::test]
async fn commit_model_a_zero_commit_volume_is_claimable_as_bootstrap() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: commit_list_of("Snapshot", vec![]) },
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "zero commits: bootstrap, claimable");
}

/// A brand-new workspace has no child `Volume` at all yet (`volumeRef` unset) — the same bootstrap
/// case, reached with no Snapshot list at all since there is no volume to list.
#[tokio::test]
async fn commit_model_a_workspace_with_no_volume_yet_is_claimable_as_bootstrap() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );

    rustic_git_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1);
    assert!(!rec.calls().iter().any(|c| c.starts_with(&format!("GET {SNAPSHOTS_LIST}"))), "nothing to list without a volume");
}

/// F1: the write a claim makes is a PUT of the WHOLE status subresource — building it from only
/// the 4 fields `decide` cares about would silently erase `head`, `volumeRef` and everything else
/// a prior life on another node already put there. This is exactly the shape `unclaim_dead_nodes`
/// leaves behind: `nodeName` cleared, everything else intact.
#[tokio::test]
async fn f1_reclaiming_an_unclaimed_workspace_preserves_head_and_volume_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            binding_route(),
        ],
    );
    let w = workspace(serde_json::json!({
        "phase": "ready", "nodeName": "", "compatibleNodes": [],
        "volumeRef": "vol-1", "head": "vol-1-commit-a",
        "packages": {"base": [], "observed": [], "observedHash": "h1", "profile": "/nix/store/x"},
    }));

    rustic_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"]["nodeName"], "node-a", "reclaimed by this node");
    assert_eq!(sent[0]["status"]["volumeRef"], "vol-1", "F1: volumeRef must survive the claim write");
    assert_eq!(sent[0]["status"]["head"], "vol-1-commit-a", "F1: head must survive the claim write");
    assert_eq!(sent[0]["status"]["packages"]["profile"], "/nix/store/x", "F1: nothing else in status is wiped either");
}

/// F4: a `Snapshot`-list error must never read as "bootstrap, claim it" — that is the exact
/// never-started-dataless failure the commit-model arm exists to prevent.
#[tokio::test]
async fn f4_a_snapshot_list_error_claims_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 500, body: serde_json::json!({"message": "etcd is down"}) }],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    let result = rustic_git_agent::claim::claim_workspace(&w, &ctx).await;
    assert!(result.is_err(), "a Snapshot-list error must not resolve to a claim decision");
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "nothing is claimed on a listing error");
}

/// F4's other half: a `VolumeReplica`-get error must not read as "no replica, so not Synced,
/// so no claim" either — that HAPPENS to be the safe answer for this arm, but the point is the
/// error must propagate rather than being silently folded into a boolean.
#[tokio::test]
async fn f4_a_volume_replica_get_error_claims_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: commit_list_of("Snapshot", vec![snapshot_cr("vol-1-a", "vol-1")]) },
            Route {
                method: "GET",
                path: format!("/apis/rustic-git.io/v1alpha1/volumereplicas/{}", crd::replica_name("vol-1", "node-a")),
                status: 500,
                body: serde_json::json!({"message": "etcd is down"}),
            },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "volumeRef": "vol-1"}));

    let result = rustic_git_agent::claim::claim_workspace(&w, &ctx).await;
    assert!(result.is_err(), "a VolumeReplica-get error must not resolve to a claim decision");
    assert!(rec.sent("PUT", WS_STATUS).is_empty(), "nothing is claimed on a lookup error");
}

/// F2: `status.head` has no writer yet in this task (Task 5 commits, Task 6 clones/restores) — a
/// workspace whose volume already has commits but whose OWN `head` is still `None` must not be
/// handed an empty bootstrap worktree. It waits, and no `checkout` (and so no worktree dir) ever
/// happens.
#[tokio::test]
async fn f2_head_none_with_commits_present_requeues_without_a_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ssh_routes();
    // TWICE: a re-host pass lists snapshots once for `latest_transient` and once for
    // `has_commits`, and this mock walks a path's routes in order — with one route the second
    // listing would fall through to the empty default and read as a zero-commit bootstrap.
    for _ in 0..2 {
        routes.push(Route {
            method: "GET",
            path: SNAPSHOTS_LIST.into(),
            status: 200,
            body: commit_list_of("Snapshot", vec![snapshot_cr("ws-1-a", "ws-1")]),
        });
    }
    let (ctx, rec, _fake) = ws_ctx_with_ssh(tmp.path(), routes);
    // `ws_ctx_with_ssh` pre-seeds an empty worktree so tests that expect a normal checkout don't
    // need real btrfs; THIS test's whole point is that no checkout may happen at all, so undo
    // that seeding before exercising it.
    std::fs::remove_dir_all(tmp.path().join("vol/ws-1/live/ws-1")).unwrap();

    let action = rustic_git_agent::controller::apply_workspace(&ready_workspace("ws-1", vec![]), &ctx).await.unwrap();

    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)), "waits for T5/T6 to record a head");
    assert!(!rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")), "no pod without a resolved head: {:?}", rec.calls());
    assert!(!tmp.path().join("vol/ws-1/live/ws-1").exists(), "no bootstrap worktree either");
}

/// The re-host fixture: a placed workspace whose `status.head` is `ws-1-aaaaaaaa`, no worktree on
/// this pool (that is what makes it a re-host), and a snapshot listing of that head plus two sync
/// points — ordered so a pick by listing order, or a last-one-wins pick, lands on the wrong object.
/// `present` names the `snap/` dirs this node actually holds.
async fn rehost_outcome(tmp: &std::path::Path, present: &[&str]) -> String {
    let transient = |name: &str, generation: &str| {
        let mut s = snapshot_cr(name, "ws-1");
        s["spec"]["transient"] = true.into();
        s["metadata"]["annotations"] = serde_json::json!({"rustic-git.io/synced-generation": generation});
        s
    };
    let mut routes = ssh_routes();
    routes.push(Route {
        method: "GET",
        path: SNAPSHOTS_LIST.into(),
        status: 200,
        body: commit_list_of(
            "Snapshot",
            vec![snapshot_cr("ws-1-aaaaaaaa", "ws-1"), transient("sync-ws-1-bbbbbbbb", "9"), transient("sync-ws-1-cccccccc", "4")],
        ),
    });
    let (ctx, _rec, _fake) = ws_ctx_with_ssh(tmp, routes);
    std::fs::remove_dir_all(tmp.join("vol/ws-1/live/ws-1")).unwrap();
    for name in present {
        std::fs::create_dir_all(tmp.join("vol/ws-1/snap").join(name)).unwrap();
    }
    let mut w = ready_workspace("ws-1", vec![]);
    w.status.as_mut().unwrap().head = Some("ws-1-aaaaaaaa".into());
    // Which snapshot the checkout was asked for is read off WHICH failure comes back, and that is
    // sharp on any platform: a source that is not on the pool fails `NO_SUCH_RECORD` before any
    // shell-out, and a source that IS there gets past that check — succeeding on a btrfs node and
    // dying in `spawn btrfs` on one without, but never as `NO_SUCH_RECORD`.
    rustic_git_agent::controller::apply_workspace(&w, &ctx).await.err().map(|e| e.0).unwrap_or_default()
}

/// Re-host: a node that has never run this worktree starts from the newest SYNC POINT, not from
/// `status.head` — the sync beat replicated it after the last commit, so the loss window on a node
/// death is one `WS_SYNC_SECS`. Only the gen-9 sync point is on the pool, and the head commit is
/// NOT, so picking the head (or the older gen-4 point) is a `NO_SUCH_RECORD`.
#[tokio::test]
async fn a_workspace_starting_on_a_new_node_checks_out_its_latest_sync_point_over_its_head() {
    let tmp = tempfile::tempdir().unwrap();

    let outcome = rehost_outcome(tmp.path(), &["sync-ws-1-bbbbbbbb"]).await;

    assert_ne!(
        outcome,
        rustic_git_workspaces::engine::ops::NO_SUCH_RECORD,
        "the checkout must have been asked for the local sync point, not the absent head commit"
    );
}

/// The other half, and the reason `latest_transient` intersects with `local_commits`: a replica one
/// pull cycle behind sees a `Ready` transient whose subvolume has not landed here yet. Checking
/// that out is a PERMANENT `NO_SUCH_RECORD` with no fallback, where `head` — which this node DOES
/// hold — would have started the worktree perfectly well. Neither sync point is on the pool here,
/// so a sharp fall back to the head is the only acceptable outcome.
#[tokio::test]
async fn a_sync_point_this_node_has_not_pulled_yet_falls_back_to_the_head() {
    let tmp = tempfile::tempdir().unwrap();

    let outcome = rehost_outcome(tmp.path(), &["ws-1-aaaaaaaa"]).await;

    assert_ne!(
        outcome,
        rustic_git_workspaces::engine::ops::NO_SUCH_RECORD,
        "an unpulled sync point must not be checked out; the local head must be"
    );
}

const WS_CLONE_OBJ: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-clone";
const WS_1_OBJ: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1";

/// A shared-volume clone workspace (`cloneOf { commit: Some(_) }`), whose worktree lives under
/// the SOURCE volume's `live/`, not its own.
fn clone_workspace() -> crd::Workspace {
    let mut w = workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-src"}));
    w.metadata.name = Some("ws-clone".into());
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "vol-src".into(), commit: Some("vol-src-abcd".into()) }),
    });
    w
}

/// (i) Task 7a finding 1 / item 3: a fresh clone reconcile ADDS the finalizer. kube-rs's
/// `finalizer()` combinator patches it on and returns `await_change()` WITHOUT running `Apply` at
/// all ("No point applying here, since the patch will cause a new reconciliation") — so this needs
/// no other route.
#[tokio::test]
async fn a_clone_reconcile_adds_the_worktree_finalizer() {
    let tmp = tempfile::tempdir().unwrap();
    let route = Route { method: "PATCH", path: WS_CLONE_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-src"})) };
    let (ctx, rec) = ctx(tmp.path(), vec![route]);

    rustic_git_agent::controller::reconcile_workspace(Arc::new(clone_workspace()), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WS_CLONE_OBJ).len(), 1, "the finalizer-add patch");
}

/// (ii) An OWNED workspace (no `cloneOf`, or one with no `commit` — the pre-commit-model
/// full-copy clone) never grows the finalizer: `reconcile_workspace` must route it straight to
/// `apply_workspace` without ever touching `WS_1_OBJ` — no route is registered for it, so a
/// mistaken PATCH there would 404 and fail this test on its own.
#[tokio::test]
async fn an_owned_workspace_reconcile_does_not_add_the_finalizer() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ws_stop_routes();
    routes.push(Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) });
    let (ctx, rec) = ctx(tmp.path(), routes);
    let mut w = stopping_ws();
    w.status.as_mut().unwrap().pod_ref = None;
    assert!(w.spec.storage.as_ref().and_then(|s| s.source.as_ref()).is_none());

    rustic_git_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert!(rec.calls().iter().all(|c| c != &format!("PATCH {WS_1_OBJ}")), "no finalizer patch: {:?}", rec.calls());
}

/// (iii) A deleting clone that already carries the finalizer: `reconcile_workspace` runs the
/// `Cleanup` arm (which calls `drop_worktree` — proved for real by
/// `engine_commit.rs`'s `drop_worktree_deletes_the_subvolume_and_is_ok_on_absent_retry`; here the
/// worktree is simply absent, so `drop_worktree`'s own no-op-on-absent path keeps this a pure loop
/// test), then removes the finalizer via kube-rs's Test+Remove JSON patch.
#[tokio::test]
async fn a_deleting_clones_reconcile_drops_its_worktree_then_removes_the_finalizer() {
    let tmp = tempfile::tempdir().unwrap();
    // `ws_lock` needs `vol/` to exist to create its lock file — normally left behind by an
    // earlier checkout; nothing else in this pure-loop test ever creates it.
    std::fs::create_dir_all(tmp.path().join("vol")).unwrap();
    let route = Route { method: "PATCH", path: WS_CLONE_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-src"})) };
    let (ctx, rec) = ctx(tmp.path(), vec![route]);
    let mut w = clone_workspace();
    w.metadata.finalizers = Some(vec![crd::WORKTREE_FINALIZER.to_string()]);
    w.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));

    rustic_git_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WS_CLONE_OBJ).len(), 1, "the finalizer-remove patch");
}

/// (iv) A non-clone workspace that still carries a finalizer left by an earlier pass (a rollback,
/// or a respec away from `cloneOf`) and is deleting: `reconcile_workspace` must still enter the
/// wrapper — the guard is "nothing to add AND nothing to remove", not "not a clone" alone — run
/// the no-op-on-disk Cleanup arm (`cleanup_workspace_worktree` skips anything that isn't a shared
/// clone), and remove the finalizer, or the object is stranded in Terminating forever.
#[tokio::test]
async fn a_reconcile_of_an_already_finalized_deleting_non_clone_workspace_removes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let route = Route { method: "PATCH", path: WS_1_OBJ.into(), status: 200, body: ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"})) };
    let (ctx, rec) = ctx(tmp.path(), vec![route]);
    let mut w = workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"}));
    w.metadata.finalizers = Some(vec![crd::WORKTREE_FINALIZER.to_string()]);
    w.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));

    rustic_git_agent::controller::reconcile_workspace(Arc::new(w), ctx).await.unwrap();

    assert_eq!(rec.sent("PATCH", WS_1_OBJ).len(), 1, "the finalizer-remove patch");
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

fn binding_status() -> String {
    format!("/apis/rustic-git.io/v1alpha1/ownerbindings/{}/status", crd::binding_name("r1", "alice"))
}

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
        // The agent's own per-namespace host-key grant, in place of `secrets` cluster-wide.
        ok(
            format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings/agent-secrets"),
            "rbac.authorization.k8s.io/v1",
            "RoleBinding",
        ),
    ];
    for p in ["default-deny", "allow-dns", "allow-same-namespace", "allow-internet-egress", "allow-gateway-ssh"] {
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
        "metadata": {"name": crd::binding_name("r1", "alice"), "uid": "ob-uid-1", "generation": 1},
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
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .chain(ns_routes(&crd::ws_namespace("alice", "acme")))
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
    assert!(rec.calls().iter().any(|c| *c == format!("PATCH /api/v1/namespaces/{acme}")), "{:?}", rec.calls());
    let stranded = crd::ws_namespace("alice", "elsewhere");
    assert!(
        !rec.calls().iter().any(|c| *c == format!("PATCH /api/v1/namespaces/{stranded}")),
        "a workspace on another node must not make namespaces here: {:?}", rec.calls()
    );
    let st = rec.sent("PATCH", &binding_status());
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
        rec.sent("PATCH", &binding_status()).is_empty(),
        "a status re-stamped with `now` is not a change: {:?}", rec.calls()
    );
}


fn home_vol_json(quota: u64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "home-alice", "uid": "home-uid-1", "generation": 1,
                     "ownerReferences": [{"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                                          "name": crd::binding_name("r1", "alice"), "uid": "ob-uid-1",
                                          "controller": true, "blockOwnerDeletion": true}]},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": quota},
        "status": {"phase": "ready", "subvolumePresent": true},
    })
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

/// `heal_labels` is what makes an object written by any other path (a restored backup, kubectl)
/// listable: the labels are a view of `spec.owner`, and a reconcile re-stamps them from it. Seeded
/// with a label naming the wrong owner, the FIRST thing the pass does is patch it back.
#[tokio::test]
async fn a_wrong_owner_label_is_re_stamped_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    const WS: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1";
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: WS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let mut j = ws_json(serde_json::json!({"phase": "stopped", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));
    j["metadata"]["labels"]["rustic-git.io/owner"] = serde_json::json!("mallory");
    j["spec"]["desiredState"] = serde_json::json!("stopped");
    let w: crd::Workspace = serde_json::from_value(j).unwrap();

    rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", WS);
    assert_eq!(sent.len(), 1, "one label patch: {:?}", rec.calls());
    assert_eq!(sent[0]["metadata"]["labels"]["rustic-git.io/owner"], "alice", "{}", sent[0]);
    assert_eq!(sent[0]["metadata"]["labels"]["rustic-git.io/kind"], "workspace");
    assert!(sent[0].get("spec").is_none(), "labels only — a controller never writes spec: {}", sent[0]);
    assert_eq!(rec.calls()[0], format!("PATCH {WS}"), "healed before anything else: {:?}", rec.calls());
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
        packages: vec![],
        attached_environment: None,
    };
    let source = spec.storage.as_ref().unwrap().source.as_ref().unwrap();
    let init = k8s::git_init_container(source, "alpine/git:2.45.2", "git.example.com", "22")
        .expect("a valid repo is accepted")
        .expect("a gitRepo source seeds with an init container");
    let pod = k8s::workspace_pod(&spec, "ws-1", "ws-1", &test_pod_ctx(), Some(init));

    let inits = pod.spec.as_ref().unwrap().init_containers.as_ref().expect("init containers");
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0].image.as_deref(), Some("alpine/git:2.45.2"), "pinned, so seeding works with any image");
    let mounts: Vec<&str> = inits[0].volume_mounts.as_ref().unwrap().iter().map(|m| m.mount_path.as_str()).collect();
    assert!(mounts.contains(&"/workspace"), "the seeder mounts the volume at its own fixed path");
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
        default_image: "ghcr.io/kloudlite/rustic-git-workspace:deadbeef",
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
    // The commit-model checkout step runs before this gate is ever reached; pre-seed an empty
    // worktree so it converges on `WORKTREE_EXISTS` instead of shelling to a real `btrfs`.
    std::fs::create_dir_all(tmp.path().join("vol/ws-1/live/ws-1")).unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-1", vol),
            ready_binding(),
            rustic_git_workspaces::kube_test::not_found(WS_SSH_SECRET),
            rustic_git_workspaces::kube_test::post(
                "/api/v1/namespaces/ws-alice/secrets",
                serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "ws-ssh-ws-1"}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    // The profile is built first on every pass, so the source is judged on the pass after it.
    let _ = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    wait_idle(&ctx).await;
    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent: never retried");
    assert!(!rec.calls().iter().any(|c| c.contains("/pods")), "no pod for an unclonable source: {:?}", rec.calls());
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["phase"], "error");
    // By type, not by index: the settle keeps `PackagesReady` (and `Attached`) ahead of it now.
    let conds = st.last().unwrap()["status"]["conditions"].as_array().unwrap().clone();
    assert!(conds.iter().any(|c| c["reason"] == "InvalidSource"), "{conds:?}");
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


// Fixture helpers still used by surviving tests below (the object-store push/stop tests that
// used to sit here are deleted — Task 8).

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

fn stop_commit(status: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        // `creationTimestamp` is what the whole-wait bound measures from — an hour ago, so a test
        // that wants the bound to bite only has to set the timeout, and one that does not is
        // unaffected because it never waits on the bound at all.
        "metadata": {"name": "stop-env-1", "uid": "stop-uid-1", "creationTimestamp": rfc3339_ago(3600)},
        "spec": {"volume": "env-1", "owner": "acme", "worktree": "env-1", "transient": true},
        "status": status,
    })
}

fn stopping_ws() -> crd::Workspace {
    let mut o = ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "compatibleNodes": ["node-a"],
                                            "volumeRef": "ws-1", "podRef": "ws-alice/ws-1"}));
    o["spec"]["desiredState"] = serde_json::json!("stopped");
    serde_json::from_value(o).unwrap()
}

fn ws_stop_routes() -> Vec<Route> {
    vec![Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) }]
}

// ── the stop-before-teardown snapshot ────────────────────────────────────

const STOP_REQ: &str = "/apis/rustic-git.io/v1alpha1/snapshots/stop-env-1";
const ENV_PATCH: &str = "/apis/rustic-git.io/v1alpha1/environments/env-1";
const DEP_DEL: &str = "/apis/apps/v1/namespaces/env-1/statefulsets/db";
const REPLICAS: &str = "/apis/rustic-git.io/v1alpha1/volumereplicas";
const WS_STOP_REQ: &str = "/apis/rustic-git.io/v1alpha1/snapshots/stop-ws-1";

/// `WS_STOP_FLUSH_TIMEOUT_SECS` is process-global and `flush_timeout()` reads it at call time, so
/// every test that SETS it takes this lock. The coupling is one-way and worth stating: a test that
/// never sets the var still races one that does, so any test whose outcome depends on the timeout
/// must take the lock and set the value it wants explicitly rather than relying on the default.
/// Every other stop test is deliberately timeout-independent (already cut, already replicated).
static FLUSH_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sets the timeout for the guard's lifetime and restores the default (unset) on drop.
struct FlushTimeout(std::sync::MutexGuard<'static, ()>);

impl FlushTimeout {
    fn set(secs: &str) -> Self {
        let g = FLUSH_ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("WS_STOP_FLUSH_TIMEOUT_SECS", secs);
        FlushTimeout(g)
    }
}

impl Drop for FlushTimeout {
    fn drop(&mut self) {
        std::env::remove_var("WS_STOP_FLUSH_TIMEOUT_SECS");
    }
}

fn rfc3339_ago(secs: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
}

/// A `VolumeReplicaList` as the flush gate lists it.
fn replica_list(rows: &[(&str, &str, Option<&str>)]) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplicaList",
        "metadata": {"resourceVersion": "1"},
        "items": rows.iter().map(|(node, phase, last)| serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": crd::replica_name("env-1", node), "uid": format!("vr-{node}")},
            "spec": {"volume": "env-1", "node": node},
            "status": {"phase": phase, "branches": {}, "lastSyncAt": last},
        })).collect::<Vec<_>>(),
    })
}

/// Everything a stopping environment touches before the flush gate: the drain, and the volume.
fn env_flush_routes(stop: serde_json::Value, replicas: serde_json::Value) -> Vec<Route> {
    vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "PATCH", path: DEP_PATCH.into(), status: 200, body: serde_json::json!({"kind": "StatefulSet"}) },
        rustic_git_workspaces::kube_test::get(POD_LIST, pod_list(&[])),
        rustic_git_workspaces::kube_test::get(STOP_REQ, stop),
        rustic_git_workspaces::kube_test::get(REPLICAS, replicas),
        Route { method: "DELETE", path: DEP_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: STOP_REQ.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
    ]
}

/// Ready is not landed. The whole point of the final sync point is that another node holds it, so
/// a cut only this node has leaves the services up — tearing down here would strand the last
/// minutes of work on one disk.
#[tokio::test]
async fn a_stop_waits_for_a_replica_to_hold_the_flush() {
    let _env = FlushTimeout::set("600");
    let tmp = tempfile::tempdir().unwrap();
    let mut ready = stop_commit(serde_json::json!({"phase": "ready", "readyAt": rfc3339_ago(5)}));
    // Freshly created, so the whole-wait bound is nowhere near — this test is about the replica,
    // not the timeout.
    ready["metadata"]["creationTimestamp"] = serde_json::json!(rfc3339_ago(5));
    let (ctx, rec) = ctx(tmp.path(), env_flush_routes(ready, replica_list(&[("node-a", "Synced", Some(&rfc3339_ago(1)))])));

    rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();

    assert!(!rec.calls().iter().any(|c| c == &format!("DELETE {DEP_DEL}")), "only this node holds it: {:?}", rec.calls());
    assert_eq!(rec.sent("PATCH", ENV_STATUS_PATH).last().unwrap()["status"]["conditions"][0]["reason"], "FlushBeforeStop");
}

/// Another node reporting `Synced` at or after the cut became Ready is the landing signal.
#[tokio::test]
async fn a_stop_proceeds_once_another_replica_is_synced_after_the_cut() {
    let _env = FlushTimeout::set("600");
    let tmp = tempfile::tempdir().unwrap();
    let ready = stop_commit(serde_json::json!({"phase": "ready", "readyAt": rfc3339_ago(30)}));
    // An offset other than Z on purpose: these two stamps are parsed as instants, never compared
    // as strings — lexically this one sorts BEFORE the `readyAt` above.
    let replicas = replica_list(&[
        ("node-a", "Synced", Some(&rfc3339_ago(60))),
        ("node-b", "Synced", Some(&(chrono::Utc::now() - chrono::Duration::seconds(1)).with_timezone(&chrono::FixedOffset::east_opt(5 * 3600).unwrap()).to_rfc3339())),
    ]);
    let (ctx, rec) = ctx(tmp.path(), env_flush_routes(ready, replicas));

    rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {DEP_DEL}")), "replicated: teardown proceeds: {:?}", rec.calls());
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "Stopped");
}

/// A stop that can never be replicated must still finish — a workspace nobody can stop is worse
/// than one whose last sync point lives on a single node — but it says so.
#[tokio::test]
async fn a_stop_tears_down_after_the_flush_timeout_with_a_condition() {
    let _env = FlushTimeout::set("0");
    let tmp = tempfile::tempdir().unwrap();
    let ready = stop_commit(serde_json::json!({"phase": "ready", "readyAt": rfc3339_ago(5)}));
    let (ctx, rec) = ctx(tmp.path(), env_flush_routes(ready, replica_list(&[("node-a", "Synced", Some(&rfc3339_ago(1)))])));

    rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {DEP_DEL}")), "past the bound: {:?}", rec.calls());
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["phase"], "stopped");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "FlushUnreplicated");
}

/// The bound covers the WHOLE wait, not just the replicated leg. `commit_worktree` is keep-biased
/// and retries a failing cut `Working` forever, so a worktree whose snapshot can never be taken
/// would otherwise park its parent's teardown for good — the pod kept alive by a gate that can
/// never open.
#[tokio::test]
async fn a_stop_whose_cut_never_becomes_ready_tears_down_after_the_timeout() {
    let _env = FlushTimeout::set("0");
    let tmp = tempfile::tempdir().unwrap();
    let wedged = stop_commit(serde_json::json!({"phase": "working"}));
    let (ctx, rec) = ctx(tmp.path(), env_flush_routes(wedged, replica_list(&[])));

    rustic_git_agent::controller::apply_environment(&stopping_env(), &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {DEP_DEL}")), "a wedged cut must not park: {:?}", rec.calls());
    let st = rec.sent("PATCH", ENV_STATUS_PATH);
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "FlushUnreplicated");
}

/// The workspace half of the same gate: the pod is what keeps the worktree changing, so the cut
/// happens first and the delete waits on it landing.
#[tokio::test]
async fn a_workspace_stop_cuts_a_sync_point_before_deleting_the_pod() {
    let _env = FlushTimeout::set("600");
    let tmp = tempfile::tempdir().unwrap();
    let (ctx1, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::not_found(WS_STOP_REQ),
            rustic_git_workspaces::kube_test::post(
                "/apis/rustic-git.io/v1alpha1/snapshots",
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                                   "metadata": {"name": "stop-ws-1", "uid": "stop-ws-uid"},
                                   "spec": {"volume": "ws-1", "owner": "alice", "worktree": "ws-1", "transient": true}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        ],
    );

    rustic_git_agent::controller::apply_workspace(&stopping_ws(), &ctx1).await.unwrap();
    let cut = rec.sent("POST", "/apis/rustic-git.io/v1alpha1/snapshots");
    assert_eq!(cut.len(), 1, "one sync point: {:?}", rec.calls());
    assert_eq!(cut[0]["spec"]["transient"], true, "a sync point, not a commit a user sees");
    assert_eq!(cut[0]["spec"]["worktree"], "ws-1", "the PARENT's name, not the volume's");
    assert!(!rec.calls().iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")), "no delete before it lands: {:?}", rec.calls());

    // Second pass: the cut is Ready and node-b holds it.
    drop(rec);
    let tmp2 = tempfile::tempdir().unwrap();
    let replicas = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplicaList", "metadata": {"resourceVersion": "1"},
        "items": [{"apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
                   "metadata": {"name": crd::replica_name("ws-1", "node-b"), "uid": "vr-b"},
                   "spec": {"volume": "ws-1", "node": "node-b"},
                   "status": {"phase": "Synced", "branches": {}, "lastSyncAt": rfc3339_ago(1)}}],
    });
    let (ctx2, rec) = ctx(
        tmp2.path(),
        vec![
            rustic_git_workspaces::kube_test::get(
                WS_STOP_REQ,
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                                   "metadata": {"name": "stop-ws-1", "uid": "stop-ws-uid"},
                                   "spec": {"volume": "ws-1", "owner": "alice", "worktree": "ws-1", "transient": true},
                                   "status": {"phase": "ready", "readyAt": rfc3339_ago(30)}}),
            ),
            rustic_git_workspaces::kube_test::get(REPLICAS, replicas),
            Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "DELETE", path: WS_STOP_REQ.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    rustic_git_agent::controller::apply_workspace(&stopping_ws(), &ctx2).await.unwrap();
    let calls = rec.calls();
    assert!(calls.iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")), "landed: the pod goes: {calls:?}");
    assert_eq!(rec.sent("PATCH", WS_STATUS).last().unwrap()["status"]["phase"], "stopped");
}














const WS_POD_DEL: &str = "/api/v1/namespaces/ws-alice/pods/ws-1";














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
            rustic_git_workspaces::kube_test::get(STOP_REQ, stop_commit(serde_json::json!({"phase": "done"}))),
            Route { method: "DELETE", path: DEP_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "DELETE", path: STOP_REQ.into(), status: 200, body: stop_commit(serde_json::json!({"phase": "done"})) },
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

/// A granted wish stays in `spec.restore` forever, so the gate meets it on every pass. It may
/// INITIALIZE `head` — once — and must never re-derive it afterwards: a push advances `head` to a
/// new commit, and a gate that compared `head` against the wish would stamp it straight back, so
/// an environment that was ever restored could never move past its restore point. What shipped
/// did exactly that.
#[tokio::test]
async fn a_granted_wish_never_drags_head_back_off_a_pushed_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut e, vol) = restoring_env(Some("snap-7"));
    // The state after a push: the wish was applied and recorded long ago, and `head` has since
    // moved on to a commit the commit reconciler cut.
    let mut st = e.status.clone().unwrap_or_default();
    st.head = Some("env-1-aaaaaaaa".into());
    st.restored_to = Some("snap-7".into());
    st.restore_requested_at = Some(WISH_AT.into());
    e.status = Some(st);
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", vol),
            Route { method: "PATCH", path: ENV_STATUS_PATH.into(), status: 200, body: env_json(serde_json::json!({})) },
        ],
    );

    // As in the sibling test above, the converge past the gate needs a namespace this mock does
    // not answer for; what is under test is every status write before that point.
    let _ = rustic_git_agent::controller::apply_environment(&e, &ctx).await;
    for w in rec.sent("PATCH", ENV_STATUS_PATH) {
        let head = w["status"]["head"].as_str();
        assert_ne!(head, Some("snap-7"), "the granted wish must not drag `head` back: {w}");
    }
}

/// The other half: a wish this environment has NOT recorded is applied — `head` is initialized to
/// the restore point and the wish is recorded, so the pass above can tell "applied" from "fresh".
#[tokio::test]
async fn a_freshly_granted_wish_initializes_head_and_is_recorded() {
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

    let _ = rustic_git_agent::controller::apply_environment(&e, &ctx).await;
    let sent = rec.sent("PATCH", ENV_STATUS_PATH);
    let first = sent.first().expect("the grant writes status");
    assert_eq!(first["status"]["head"], "snap-7");
    assert_eq!(first["status"]["restoredTo"], "snap-7");
    assert_eq!(first["status"]["restoreRequestedAt"], WISH_AT);
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
        Some(crd::WorkspaceStorage { quota_gb: 20, source: Some(crd::VolumeSource::CloneOf { volume: source.into(), commit: None }) });
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


/// Stopping needs neither the disk nor the namespace — only a pod delete. Gated on the Volume, a
/// workspace whose subvolume failed could never be stopped, so it kept its pod forever.
#[tokio::test]
async fn a_stopped_workspace_with_a_broken_volume_still_loses_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let ns = crd::ws_namespace("alice", "");
    let pod_del = format!("/api/v1/namespaces/{ns}/pods/ws-1");
    // An EXPLICIT wedged cut, not the fixture's default "already replicated" answer: a broken
    // volume is exactly the case whose `commit_worktree` never succeeds, and a stop gated on that
    // cut is how this invariant regresses. The bound is what saves it, so the timeout is set here.
    let _env = FlushTimeout::set("0");
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get(
                WS_STOP_REQ,
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                                   "metadata": {"name": "stop-ws-1", "uid": "wedged", "creationTimestamp": rfc3339_ago(3600)},
                                   "spec": {"volume": "ws-1", "owner": "alice", "worktree": "ws-1", "transient": true},
                                   "status": {"phase": "working"}}),
            ),
            Route { method: "DELETE", path: pod_del.clone(), status: 200, body: serde_json::json!({"kind": "Status"}) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    // With a `podRef`: a workspace that never ran skips the flush entirely, which would make this
    // test pass without ever exercising the gate.
    let mut w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a", "volumeRef": "ws-1",
                                             "podRef": "ws-alice/ws-1"}));
    w.spec.desired_state = crd::DesiredState::Stopped;

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {pod_del}")), "{:?}", rec.calls());
    assert!(
        !rec.calls().iter().any(|c| c.contains("/volumes/")),
        "the stop must not depend on the Volume at all: {:?}",
        rec.calls()
    );
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["phase"], "stopped");
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "FlushUnreplicated");
}

/// The home is on the shared NFS mount now (spec 2026-09-01): a stop deletes the pod straight
/// away, with no `stop-home-{ws}` snapshot request gating it.
#[tokio::test]
async fn a_stop_deletes_the_pod_without_any_home_push_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let w = stopping_ws();
    rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")));
    assert!(rec.calls().iter().all(|c| !c.contains("snapshots/stop-home")), "no stop-home gate: {:?}", rec.calls());
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
    let (mut vol_wakes, _ws_wakes) = ctx.wakes.lock().unwrap().take().unwrap();
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


/// The success half: a finished operation whose outcome is `Ready` wakes the reconciler AND the
/// woken pass writes `ready` with the generation it ran for. Stubbed through `wake_on_finish`
/// rather than through real work, because the test host has no btrfs.
#[tokio::test]
async fn a_successful_volume_operation_wakes_and_then_writes_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let (mut vol_wakes, _ws_wakes) = ctx.wakes.lock().unwrap().take().unwrap();
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





// ── the packages step ────────────────────────────────────────────────────

/// The mocked `Ctx` every profile test wants: a fake Nix it can inspect, its own profile root, and
/// a workspace whose Volume answers ready so the pass reaches the packages step.
fn ws_ctx_with_nix(pool: &std::path::Path) -> (Arc<Ctx>, Recorder, Arc<FakeNix>) {
    ws_ctx_with_ssh(pool, ssh_routes())
}

/// No host key yet, so the pass mints one.
fn ssh_routes() -> Vec<Route> {
    vec![
        rustic_git_workspaces::kube_test::not_found(WS_SSH_SECRET),
        rustic_git_workspaces::kube_test::post(
            "/api/v1/namespaces/ws-alice/secrets",
            serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "ws-ssh-ws-1"}}),
        ),
    ]
}

const WS_SSH_SECRET: &str = "/api/v1/namespaces/ws-alice/secrets/ws-ssh-ws-1";

fn ws_ctx_with_ssh(pool: &std::path::Path, ssh: Vec<Route>) -> (Arc<Ctx>, Recorder, Arc<FakeNix>) {
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    let fake = Arc::new(FakeNix::default());
    let mut routes = ssh;
    routes.extend(vec![
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-1", vol),
        ready_binding(),
        rustic_git_workspaces::kube_test::post(
            "/api/v1/namespaces/ws-alice/pods",
            serde_json::json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws-1"}}),
        ),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ]);
    // The commit-model checkout step is unconditional now (Task 8): pre-seed an empty worktree
    // so `Engine::checkout` converges on `WORKTREE_EXISTS` instead of shelling out to a real
    // `btrfs subvolume create` this test environment doesn't have.
    std::fs::create_dir_all(pool.join("vol/ws-1/live/ws-1")).unwrap();
    let (ctx, rec) = ctx_full(pool, routes, fake.clone());
    // The pod mounts the owner's home, so the Running arm waits for it to be Ready here.
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
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
async fn apply_until_settled(w: &crd::Workspace, ctx: &Arc<Ctx>) -> kube::runtime::controller::Action {
    for _ in 0..4 {
        let action = rustic_git_agent::controller::apply_workspace(w, ctx).await.unwrap();
        if ctx.running.lock().unwrap().is_empty() {
            return action;
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
    assert!(builds[0].contains("pkgs.git pkgs.openssh") && builds[0].ends_with("pkgs.hello ]; }"), "base set first, then the workspace's own: {}", builds[0]);
    assert!(rustic_git_agent::nix::profile_exists(&ctx.profiles_dir, "ws-1"), "published as <dir>/current");
    let calls = rec.calls();
    let built = calls.iter().position(|c| c.contains("/status")).unwrap();
    let pod = calls.iter().position(|c| c.starts_with("POST") && c.contains("/pods")).unwrap();
    assert!(built < pod, "status (Building/Built) before the pod is created: {calls:?}");
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(st["status"]["packages"]["observed"][0], "hello");
    assert_eq!(packages_condition(&st)["reason"], "Built");
}

/// An empty list is still a profile: the pod mounts it as a subPath of the read-only `nix`
/// hostPath, so a missing link is a pod that cannot mount at all.
#[tokio::test]
async fn a_workspace_with_no_packages_still_gets_a_profile_before_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    let ws = ready_workspace("ws-1", vec![]);
    apply_until_settled(&ws, &ctx).await;

    let builds = fake.builds.lock().unwrap().clone();
    assert_eq!(builds.len(), 1, "an empty profile is still built");
    assert!(builds[0].contains("pkgs.git") && !builds[0].contains("pkgs.hello"), "the base set alone: {}", builds[0]);
    assert!(rustic_git_agent::nix::profile_exists(&ctx.profiles_dir, "ws-1"), "the link the pod mounts");
    let calls = rec.calls();
    let built = calls.iter().position(|c| c.contains("/status")).unwrap();
    let pod = calls.iter().position(|c| c.starts_with("POST") && c.contains("/pods")).unwrap();
    assert!(built < pod, "the profile exists before the pod does: {calls:?}");
}

/// A pod started before its host key Secret exists mounts nothing at `/etc/ssh` and sshd dies on
/// boot, so the Secret has to be there first — and the public half has to reach status, which is
/// the only place the CLI can learn the key to pin.
#[tokio::test]
async fn a_workspace_gets_a_host_key_secret_before_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _fake) = ws_ctx_with_nix(tmp.path());
    let ws = ready_workspace("ws-1", vec![]);
    apply_until_settled(&ws, &ctx).await;

    let calls = rec.calls();
    let secret = calls.iter().position(|c| c == "POST /api/v1/namespaces/ws-alice/secrets").expect("host key Secret created");
    let pod = calls.iter().position(|c| c.starts_with("POST") && c.contains("/pods")).expect("pod created");
    assert!(secret < pod, "the Secret exists before the pod does: {calls:?}");

    let sent = rec.sent("POST", "/api/v1/namespaces/ws-alice/secrets");
    let body = &sent[0];
    assert_eq!(body["metadata"]["name"], "ws-ssh-ws-1");
    // The real generated key, not a fake: what lands in the Secret must be what sshd reads.
    let private = body["stringData"]["ssh_host_ed25519_key"].as_str().unwrap();
    assert!(private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"), "{private}");
    let public = body["stringData"]["ssh_host_ed25519_key.pub"].as_str().unwrap();
    assert!(public.starts_with("ssh-ed25519 "), "{public}");
    assert!(body["stringData"]["sshd_config"].as_str().unwrap().contains("HostKey"));

    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(st["status"]["sshHostKey"], public, "the public half, on status: {st}");
}

/// A recreated pod must keep the key its users have pinned, so an existing Secret is read, never
/// regenerated.
#[tokio::test]
async fn an_existing_host_key_is_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _fake) = ws_ctx_with_ssh(
        tmp.path(),
        vec![rustic_git_workspaces::kube_test::get(
            WS_SSH_SECRET,
            serde_json::json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": {"name": "ws-ssh-ws-1", "namespace": "ws-alice"},
                // As the API server hands them back: base64 of "ssh-ed25519 OLDPUB ws".
                "data": {"ssh_host_ed25519_key.pub": "c3NoLWVkMjU1MTkgT0xEUFVCIHdz"},
            }),
        )],
    );
    let ws = ready_workspace("ws-1", vec![]);
    apply_until_settled(&ws, &ctx).await;

    assert!(
        !rec.calls().iter().any(|c| c == "POST /api/v1/namespaces/ws-alice/secrets"),
        "an existing key is never replaced: {:?}",
        rec.calls()
    );
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(st["status"]["sshHostKey"], "ssh-ed25519 OLDPUB ws");
}

/// The hash is what makes this idempotent: same pin, same list, a link on disk — no nix at all.
#[tokio::test]
async fn a_matching_hash_and_present_link_skip_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, fake) = ws_ctx_with_nix(tmp.path());
    let mut ws = ready_workspace("ws-1", vec!["hello".into()]);
    let pin = rustic_git_agent::nix::nixpkgs_pin();
    ws.status.as_mut().unwrap().packages = Some(rustic_git_workspaces::crd::PackagesStatus {
        base: vec![],
        observed: vec!["hello".into()],
        observed_hash: Some(rustic_git_workspaces::packages::hash(&pin, &with_base(&["hello".into()]))),
        profile: None,
        nixpkgs: Some(pin),
    });
    plant_profile(&ctx, "ws-1");
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
    plant_profile(&ctx, "ws-1");
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

/// THE lost-edit bug: a PATCH that lands while the build runs must not be published as if it were
/// the new spec. The build that finished belongs to the OLD list; publishing it and stamping the
/// NEW hash makes every later pass see a match and never rebuild — the workspace is permanently
/// short a package it asked for.
#[tokio::test]
async fn a_spec_change_during_a_build_is_rebuilt_not_published_under_the_new_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    let first = ready_workspace("ws-1", vec!["hello".into()]);
    let second = ready_workspace("ws-1", vec!["hello".into(), "jq".into()]);

    // Pass one starts the build for [hello]; the edit lands before it completes.
    let _ = rustic_git_agent::controller::apply_workspace(&first, &ctx).await.unwrap();
    wait_idle(&ctx).await;
    // Every later pass sees the edited spec.
    apply_until_settled(&second, &ctx).await;

    let builds = fake.builds.lock().unwrap().clone();
    assert_eq!(builds.len(), 2, "the superseded build is discarded and the new spec built: {builds:?}");
    assert!(builds[1].contains("pkgs.jq"), "the second build is the edited list: {}", builds[1]);
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    let pin = rustic_git_agent::nix::nixpkgs_pin();
    assert_eq!(
        st["status"]["packages"]["observedHash"],
        rustic_git_workspaces::packages::hash(&pin, &with_base(&["hello".into(), "jq".into()])),
        "the recorded hash is the one that was actually built"
    );
    assert_eq!(packages_condition(&st)["reason"], "Built");
}

/// A daemon that is down is this node's fault, not the package list's: its own reason, no build
/// attempted, and a workspace that already has a profile still gets its pod.
#[tokio::test]
async fn a_dead_daemon_is_no_nix_and_never_a_build() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    *fake.ping.lock().unwrap() = Err("cannot connect to /nix/var/nix/daemon-socket/socket".into());
    let ws = ready_workspace("ws-1", vec!["hello".into()]);
    let action = rustic_git_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();

    assert!(fake.builds.lock().unwrap().is_empty(), "nothing is built without a daemon");
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    let c = packages_condition(&st);
    assert_eq!(c["reason"], "NoNix");
    assert!(c["message"].as_str().unwrap().contains("daemon-socket"), "{c}");
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(60)));

    // With a profile already on disk the pod still runs — the tools it has keep working.
    plant_profile(&ctx, "ws-1");
    let _ = rustic_git_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
    assert!(rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")));
}

/// A stop must not erase what the packages step said: dropping `PackagesReady` left the web
/// showing "installing packages…" for a workspace that is simply off.
#[tokio::test]
async fn stopping_a_workspace_keeps_its_packages_condition() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _fake) = ws_ctx_with_nix(tmp.path());
    let mut ws = ready_workspace("ws-1", vec!["hello".into()]);
    ws.spec.desired_state = crd::DesiredState::Stopped;
    ws.status.as_mut().unwrap().conditions =
        vec![crd::condition(crd::PACKAGES_READY, true, "Built", "profile is on disk", 1)];
    let _ = rustic_git_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();

    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(st["status"]["phase"], "stopped");
    assert_eq!(packages_condition(&st)["reason"], "Built");
}

/// The RESTART half of the mid-build bug: while a build runs, status must keep saying what is on
/// the DISK. Recording the new hash under `Building` and then dying before the publish leaves a
/// status that matches the spec next to the previous profile — every later pass sees a hash match
/// and skips the build forever.
#[tokio::test]
async fn a_build_interrupted_by_a_restart_is_started_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, fake) = ws_ctx_with_nix(tmp.path());
    let pin = rustic_git_agent::nix::nixpkgs_pin();
    // The disk has [hello]; the spec asks for [hello, jq]; status says Building and — correctly —
    // still names the OLD list. The Ctx is fresh: no handle, no remembered hash, as after a crash.
    plant_profile(&ctx, "ws-1");
    let mut ws = ready_workspace("ws-1", vec!["hello".into(), "jq".into()]);
    let st = ws.status.as_mut().unwrap();
    st.packages = Some(rustic_git_workspaces::crd::PackagesStatus {
        base: vec![],
        observed: vec!["hello".into()],
        observed_hash: Some(rustic_git_workspaces::packages::hash(&pin, &with_base(&["hello".into()]))),
        profile: None,
        nixpkgs: Some(pin),
    });
    st.conditions = vec![crd::condition(crd::PACKAGES_READY, false, "Building", "taking the profile through nix", 1)];
    assert!(ctx.running.lock().unwrap().is_empty());

    let _ = rustic_git_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
    // The build runs on a blocking thread; on a loaded CI box it has not always STARTED by the
    // time the pass returns, and the fake records a build only when it runs.
    wait_idle(&ctx).await;
    let builds = fake.builds.lock().unwrap().clone();
    assert_eq!(builds.len(), 1, "the interrupted build is started again");
    assert!(builds[0].contains("pkgs.jq"), "{}", builds[0]);
}

/// A build that keeps failing must not be retried every minute forever: the requeue grows with how
/// long the workspace has been in `BuildFailed`, and a spec edit (the only real fix) is an event
/// that wakes the reconcile regardless.
#[tokio::test]
async fn a_failing_build_backs_off_from_a_minute_towards_an_hour() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, fake) = ws_ctx_with_nix(tmp.path());
    *fake.answer.lock().unwrap() = Err("error: attribute 'nodejs_99' missing".into());
    let ws = ready_workspace("ws-1", vec!["nodejs_99".into()]);   // nothing on disk to fall back to

    let fail_once = |w: &crd::Workspace, ctx: &Arc<Ctx>| {
        let w = w.clone();
        let ctx = ctx.clone();
        async move {
            let _ = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
            wait_idle(&ctx).await;
            rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap()
        }
    };
    assert_eq!(
        fail_once(&ws, &ctx).await,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(60)),
        "the first failure retries at the floor"
    );

    // Ten minutes in the failed state: the retry is ten minutes out.
    let mut ws = ws;
    let mut c = crd::condition(crd::PACKAGES_READY, false, "BuildFailed", "error: attribute 'nodejs_99' missing", 1);
    c.last_transition_time = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
        k8s_openapi::jiff::Timestamp::now() - std::time::Duration::from_secs(600),
    );
    ws.status.as_mut().unwrap().conditions = vec![c];
    assert_eq!(
        fail_once(&ws, &ctx).await,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(600))
    );
}

// ── what the node asks the API server for ────────────────────────────────

/// Every watch `run` opens is scoped to this node, and the ones that cannot be (a request names
/// no node) are label-selected down to the objects this node acts on. The mock answers every list
/// with nothing and every watch with a body the watcher cannot parse, so the controllers spin up,
/// ask, and back off — which is enough to see the selectors they ask WITH.
#[tokio::test(flavor = "multi_thread")]
async fn every_watch_is_scoped_to_this_node_or_label_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let list = |kind: &str| {
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": format!("{kind}List"),
                           "metadata": {"resourceVersion": "1"}, "items": []})
    };
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes", list("Volume")),
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces", list("Workspace")),
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/environments", list("Environment")),
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/ownerbindings", list("OwnerBinding")),
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/snapshots", list("Snapshot")),
            rustic_git_workspaces::kube_test::get("/api/v1/pods", list("Pod")),
            rustic_git_workspaces::kube_test::get("/apis/apps/v1/statefulsets", list("StatefulSet")),
        ],
    );
    let running = tokio::spawn(rustic_git_agent::controller::run(ctx));
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    running.abort();

    let reqs = rec.requests();
    let of = |path: &str| -> Vec<String> { reqs.iter().filter(|r| r.starts_with(&format!("GET {path}?"))).cloned().collect() };
    let volumes = of("/apis/rustic-git.io/v1alpha1/volumes");
    assert!(!volumes.is_empty(), "the Volume watch never opened: {reqs:?}");
    // The heartbeat's capped list is the one unscoped Volume request there is.
    for r in volumes.iter().filter(|r| !r.contains("limit=1&") && !r.ends_with("limit=1")) {
        assert!(r.contains("fieldSelector=spec.nodeName%3Dnode-a"), "an unscoped Volume request: {r}");
    }
    let parents = [of("/apis/rustic-git.io/v1alpha1/workspaces"), of("/apis/rustic-git.io/v1alpha1/environments")].concat();
    assert!(!parents.is_empty(), "no parent watch opened: {reqs:?}");
    for r in &parents {
        assert!(r.contains("fieldSelector=status.nodeName%3D"), "an unscoped parent request: {r}");
    }
    for r in of("/apis/apps/v1/statefulsets") {
        assert!(r.contains("labelSelector=rustic-git.io%2Fkind%3Denvironment"), "every StatefulSet in the cluster: {r}");
    }
    let snaps = of("/apis/rustic-git.io/v1alpha1/snapshots");
    assert!(
        snaps.iter().any(|r| r.contains("labelSelector=rustic-git.io%2Fstop-of")),
        "the env controller's stop-push watch is not label-selected: {snaps:?}"
    );
}


/// A converged workspace reconciles on every pod event, and a converged pass must not re-apply
/// its children — only the pod is read every time, to observe liveness.
#[tokio::test]
async fn a_converged_workspace_does_not_re_apply_its_children_on_the_next_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _fake) = ws_ctx_with_nix(tmp.path());
    let ws = ready_workspace("ws-1", vec![]);
    apply_until_settled(&ws, &ctx).await;
    // The fixture never feeds a pass's status back into `ws`, so every other pass restarts the
    // (instant) fake profile build and returns before the pod; the passes that do reach the pod
    // are the converged ones, and it is those that must apply nothing. How many passes that
    // takes depends on timing (a slow CI runner is still mid-build after settling), so keep
    // going until two of them have been seen.
    let pod_get = "GET /api/v1/namespaces/ws-alice/pods/ws-1";
    let mut converged: Vec<Vec<String>> = Vec::new();
    for _ in 0..10 {
        let before = rec.calls().len();
        let _ = rustic_git_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
        let pass: Vec<String> = rec.calls()[before..].to_vec();
        if pass.iter().any(|c| c == pod_get) {
            converged.push(pass);
            if converged.len() == 2 {
                break;
            }
        } else {
            wait_idle(&ctx).await;
        }
    }
    assert_eq!(converged.len(), 2, "never saw two converged passes: {:?}", rec.calls());
}

/// The shared home replaces the home Volume (spec 2026-09-01): a node with no `WS_HOMES_EXPORT`
/// has nowhere to mount an owner's home, so it must park the workspace rather than start a pod
/// that would hostPath an empty local dir in the home's place.
#[tokio::test]
async fn a_node_without_a_homes_export_parks_the_workspace_instead_of_starting_a_pod() {
    let tmp = tempfile::tempdir().unwrap();
    // resolve_volume and the namespace-ready check both run before the homes-export gate, so
    // this fixture still needs a Ready Volume and a Ready binding to reach it — same shapes as
    // `ws_ctx_with_ssh`'s, minus the SSH/pod routes the homes-export gate never lets it reach.
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    let routes = vec![
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-1", vol),
        ready_binding(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx_without_homes_export(tmp.path(), routes);
    let w = ready_workspace("ws-1", vec![]);

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "HomeNotReady");
    assert!(rec.calls().iter().all(|c| !c.contains("/pods")), "no pod while unmounted: {:?}", rec.calls());
}

// ── attachment ───────────────────────────────────────────────────────────

/// The workspace-side objects an attachment adds, on top of `ws_ctx_with_nix`'s: the shared attach
/// claim, and both halves of the grant answered with themselves.
fn attach_routes() -> Vec<Route> {
    let np = |ns: &str| Route {
        method: "PATCH",
        path: format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/attach-ws-1"),
        status: 200,
        body: serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                                 "metadata": {"name": "attach-ws-1"}}),
    };
    vec![
        rustic_git_workspaces::kube_test::not_found(WS_SSH_SECRET),
        rustic_git_workspaces::kube_test::post(
            "/api/v1/namespaces/ws-alice/secrets",
            serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "ws-ssh-ws-1"}}),
        ),
        np("ws-alice"),
        np("env-abc"),
    ]
}

fn env_route(id: &str, region: &str) -> Route {
    rustic_git_workspaces::kube_test::get(
        format!("/apis/rustic-git.io/v1alpha1/environments/{id}"),
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Environment",
            "metadata": {"name": id, "uid": "env-uid-1", "generation": 1},
            "spec": {"owner": "alice", "name": "api", "region": region, "services": [],
                     "desiredState": "running"},
        }),
    )
}

fn attached_workspace(env_id: &str) -> crd::Workspace {
    let mut w = ready_workspace("ws-1", vec![]);
    w.spec.attached_environment = Some(env_id.into());
    w
}

fn attached_condition(rec: &Recorder) -> serde_json::Value {
    let st = rec.sent("PATCH", WS_STATUS).last().expect("a status write").clone();
    st["status"]["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "Attached")
        .unwrap_or_else(|| panic!("no Attached condition in {st}"))
        .clone()
}

/// Attaching writes both halves of the grant. The file itself is asserted by the k8s tests — here
/// what matters is that the reconcile reaches the policies at all, and before the pod.
#[tokio::test]
async fn an_attached_workspace_gets_both_halves_of_the_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(env_route("env-abc", "r1"));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    apply_until_settled(&attached_workspace("env-abc"), &ctx).await;

    let calls = rec.calls();
    let policy = |ns: &str| format!("PATCH /apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/attach-ws-1");
    let ws_half = calls.iter().position(|c| *c == policy("ws-alice")).expect("workspace-side policy");
    let env_half = calls.iter().position(|c| *c == policy("env-abc")).expect("environment-side policy");
    let pod = calls.iter().position(|c| c.starts_with("POST") && c.contains("/pods")).unwrap();
    assert!(ws_half < pod && env_half < pod, "the grant lands before the pod: {calls:?}");

    // A `subPath` whose target is missing becomes a directory: the file exists before the pod.
    let written = std::fs::read_to_string(rustic_git_workspaces::k8s::attach_file(&ctx.pool, "ws-1")).unwrap();
    assert!(written.contains("env-abc.svc."), "the environment leads the search line: {written}");

    // The environment-side half is owned by the ENVIRONMENT: an ownerReference cannot cross
    // namespaces, so a Workspace ref there would never be collected.
    let sent = rec.sent("PATCH", "/apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies/attach-ws-1");
    assert_eq!(sent.last().unwrap()["metadata"]["ownerReferences"][0]["kind"], "Environment");
    assert_eq!(attached_condition(&rec)["status"], "True");
    assert_eq!(attached_condition(&rec)["message"], "env-abc");
}

/// A stale id is not an error. `/v1` clears the field when an environment is deleted, but a crash
/// mid-delete must degrade to "not attached" rather than leaving a grant pointing at nothing.
#[tokio::test]
async fn a_workspace_attached_to_a_missing_environment_reconciles_unattached() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(rustic_git_workspaces::kube_test::not_found("/apis/rustic-git.io/v1alpha1/environments/env-gone"));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    apply_until_settled(&attached_workspace("env-gone"), &ctx).await;

    assert!(
        !rec.calls().iter().any(|c| c.contains("/networkpolicies/attach-ws-1") && c.starts_with("PATCH")),
        "no grant for an environment that is not there: {:?}",
        rec.calls()
    );
    let written = std::fs::read_to_string(rustic_git_workspaces::k8s::attach_file(&ctx.pool, "ws-1")).unwrap();
    assert!(!written.contains("env-"), "no search domain either: {written}");
    let cond = attached_condition(&rec);
    assert_eq!(cond["status"], "False");
    assert_eq!(cond["reason"], "EnvironmentNotFound", "the refusal is reported, not silent");
}

/// A different region is a different cluster: no route, no DNS. Refused by the reconciler as well
/// as by `/v1`, because a spec can arrive by any path.
#[tokio::test]
async fn a_cross_region_attachment_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(env_route("env-abc", "other-region"));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    apply_until_settled(&attached_workspace("env-abc"), &ctx).await;

    assert!(
        !rec.calls().iter().any(|c| c.contains("/networkpolicies/attach-ws-1") && c.starts_with("PATCH")),
        "no grant across a region boundary: {:?}",
        rec.calls()
    );
    assert_eq!(attached_condition(&rec)["reason"], "RegionMismatch");
}

/// An unattached workspace has no `Attached` condition at all, and its grant is deleted — detach is
/// the same reconcile with the field cleared.
#[tokio::test]
async fn an_unattached_workspace_reports_nothing_and_deletes_its_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), attach_routes());
    let _ = rustic_git_agent::controller::apply_workspace(&ready_workspace("ws-1", vec![]), &ctx).await.unwrap();

    assert!(
        rec.calls().iter().any(|c| *c == "DELETE /apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/attach-ws-1"),
        "the grant is deleted by name: {:?}",
        rec.calls()
    );
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert!(
        !st["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Attached"),
        "not attached is not a condition: {st}"
    );
}

/// The status this workspace would carry after a pass attached to `env_id` — the `Attached`
/// message is where the previous environment's namespace is read back from.
fn was_attached_to(env_id: &str) -> crd::Workspace {
    let mut w = ready_workspace("ws-1", vec![]);
    let mut st = w.status.unwrap_or_default();
    st.conditions.push(crd::condition("Attached", true, "Converged", env_id, 1));
    w.status = Some(st);
    w
}

/// Detaching, and re-attaching elsewhere, must collect the ingress in the OLD environment's
/// namespace. Left behind it is a dormant cross-namespace grant that goes live again the moment
/// anything re-adds an egress with the same workspace id.
#[tokio::test]
async fn detaching_deletes_the_grant_in_the_old_environments_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(env_route("env-def", "r1"));
    routes.push(Route {
        method: "PATCH",
        path: "/apis/networking.k8s.io/v1/namespaces/env-def/networkpolicies/attach-ws-1".into(),
        status: 200,
        body: serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                                 "metadata": {"name": "attach-ws-1"}}),
    });
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    let stale = "DELETE /apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies/attach-ws-1";

    // Cleared: both halves go.
    apply_until_settled(&was_attached_to("env-abc"), &ctx).await;
    let after_detach = rec.calls().iter().filter(|c| *c == stale).count();
    assert!(after_detach > 0, "the old environment's half: {:?}", rec.calls());
    assert!(rec
        .calls()
        .iter()
        .any(|c| c == "DELETE /apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/attach-ws-1"));

    // Re-attached elsewhere: the new grant is applied and the old namespace is still cleaned up.
    let mut moved = was_attached_to("env-abc");
    moved.spec.attached_environment = Some("env-def".into());
    apply_until_settled(&moved, &ctx).await;
    assert!(rec.calls().iter().filter(|c| *c == stale).count() > after_detach, "on the re-attach too");
    assert!(rec
        .calls()
        .iter()
        .any(|c| c == "PATCH /apis/networking.k8s.io/v1/namespaces/env-def/networkpolicies/attach-ws-1"));
}

/// A pod created before this feature shipped has no attach volume, and `create_if_absent` never
/// replaces it — so the file and the policies this pass writes reach nothing. The condition is
/// gated on the LIVE pod rather than on the spec, because "attached" that resolves nothing is a
/// success the user cannot see through.
fn workspace_pod_json(volumes: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws-1"},
        "spec": {"volumes": volumes},
        "status": {"conditions": [{"type": "Ready", "status": "True",
                                   "lastTransitionTime": "2026-08-30T00:00:00Z"}]},
    })
}

#[tokio::test]
async fn an_attached_workspace_whose_pod_predates_the_mount_does_not_report_attached() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(env_route("env-abc", "r1"));
    routes.push(rustic_git_workspaces::kube_test::get(
        "/api/v1/namespaces/ws-alice/pods/ws-1",
        workspace_pod_json(serde_json::json!([{"name": "home", "persistentVolumeClaim": {"claimName": "home"}}])),
    ));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);

    apply_until_settled(&attached_workspace("env-abc"), &ctx).await;

    let cond = attached_condition(&rec);
    assert_eq!(cond["status"], "False", "a pod with no attach mount resolves nothing: {cond}");
    assert_eq!(cond["reason"], "PodPredatesAttachment");
    assert!(cond["message"].as_str().unwrap().contains("stop and start"), "{cond}");
}

/// The same pass on a pod that DOES carry the mount reports the attachment, addressed by the bare
/// environment id the next pass reads back.
#[tokio::test]
async fn an_attached_workspace_whose_pod_carries_the_mount_reports_attached() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(env_route("env-abc", "r1"));
    routes.push(rustic_git_workspaces::kube_test::get(
        "/api/v1/namespaces/ws-alice/pods/ws-1",
        workspace_pod_json(serde_json::json!([{"name": "attach", "hostPath": {"path": "/pool/attach/ws-1/resolv.conf", "type": "File"}}])),
    ));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);

    apply_until_settled(&attached_workspace("env-abc"), &ctx).await;

    assert_eq!(attached_condition(&rec)["status"], "True");
    assert_eq!(attached_condition(&rec)["message"], "env-abc");
}

/// A stop between the attach and the detach must not lose the grant's address. `ws_conditions`
/// rebuilds the condition list on every stop, so an `Attached` dropped there is finding 1 coming
/// back through a different door — the ingress stranded in `env-abc` with nothing left that knows
/// where it is.
#[tokio::test]
async fn a_stop_between_the_attach_and_the_detach_still_collects_the_old_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), attach_routes());

    // Stop while attached: this pass rewrites the whole condition list.
    let mut stopping = was_attached_to("env-abc");
    stopping.spec.desired_state = crd::DesiredState::Stopped;
    rustic_git_agent::controller::apply_workspace(&stopping, &ctx).await.unwrap();
    let stopped = rec.sent("PATCH", WS_STATUS).last().expect("a status write")["status"].clone();

    // Detach, starting from exactly the status that stop wrote — not from a hand-built one.
    let mut detached: crd::Workspace = serde_json::from_value(ws_json(stopped)).unwrap();
    detached.spec.attached_environment = None;
    apply_until_settled(&detached, &ctx).await;

    assert!(
        rec.calls().iter().any(|c| c == "DELETE /apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies/attach-ws-1"),
        "the stop must carry the environment id through: {:?}",
        rec.calls()
    );
}

/// The invariant, not one site: any pass that rebuilds the condition list must carry `Attached`
/// through, because a detach after it is what collects the grant in the old environment's
/// namespace. A volume wait — a node reboot, a restore, a re-materialize — is the cheapest such
/// pass to force; the stop path is covered above, and both go through `ws_conditions`.
#[tokio::test]
async fn a_volume_wait_between_the_attach_and_the_detach_still_collects_the_old_grant() {
    let tmp = tempfile::tempdir().unwrap();
    // First read of the Volume is NOT ready, so this pass settles into a wait and writes status;
    // the fixture's own ready route answers every read after it.
    let mut routes = vec![rustic_git_workspaces::kube_test::get(
        "/apis/rustic-git.io/v1alpha1/volumes/ws-1",
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
            "status": {"phase": "creating", "subvolumePresent": false}
        }),
    )];
    routes.extend(attach_routes());
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);

    rustic_git_agent::controller::apply_workspace(&was_attached_to("env-abc"), &ctx).await.unwrap();
    let waited = rec.sent("PATCH", WS_STATUS).last().expect("a status write")["status"].clone();

    let mut detached: crd::Workspace = serde_json::from_value(ws_json(waited)).unwrap();
    detached.spec.attached_environment = None;
    apply_until_settled(&detached, &ctx).await;

    assert!(
        rec.calls().iter().any(|c| c == "DELETE /apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies/attach-ws-1"),
        "the wait must carry the environment id through: {:?}",
        rec.calls()
    );
}

/// The whole point: a workspace whose inputs another workspace already built on this node reaches
/// PackagesReady without nix being asked to evaluate anything.
#[tokio::test]
async fn a_workspace_whose_inputs_are_already_built_does_not_invoke_nix() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    // Seed the index as a previous build would have.
    let store = ctx.profiles_dir.join("seeded-store-path");
    std::fs::create_dir_all(&store).unwrap();
    let pin = rustic_git_agent::nix::nixpkgs_pin();
    let hash = rustic_git_workspaces::packages::hash(&pin, &with_base(&["hello".into()]));
    rustic_git_agent::nix::record_index(&ctx.profiles_dir, &hash, &store).unwrap();

    let ws = ready_workspace("ws-1", vec!["hello".into()]);
    apply_until_settled(&ws, &ctx).await;

    assert!(fake.builds.lock().unwrap().is_empty(), "an indexed profile must not be rebuilt");
    assert_eq!(
        std::fs::read_link(rustic_git_agent::nix::profile_path(&ctx.profiles_dir, "ws-1")).unwrap(),
        store,
        "the workspace's own link points at the shared store path"
    );
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(packages_condition(&st)["status"], "True", "ready on the cached profile: {st}");
    assert_eq!(st["status"]["packages"]["observedHash"], hash, "so the per-workspace skip hits next pass");
}

/// A dangling entry must not short-circuit the build, or the pod gets a profile with no bin.
#[tokio::test]
async fn an_index_entry_pointing_at_nothing_still_builds() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, fake) = ws_ctx_with_nix(tmp.path());
    let pin = rustic_git_agent::nix::nixpkgs_pin();
    let hash = rustic_git_workspaces::packages::hash(&pin, &with_base(&["hello".into()]));
    rustic_git_agent::nix::record_index(&ctx.profiles_dir, &hash, &ctx.profiles_dir.join("gone")).unwrap();

    let ws = ready_workspace("ws-1", vec!["hello".into()]);
    apply_until_settled(&ws, &ctx).await;

    assert_eq!(fake.builds.lock().unwrap().len(), 1, "a miss builds");
}

/// A real build feeds the index, which is what makes the SECOND workspace's hit possible.
#[tokio::test]
async fn a_finished_build_is_recorded_under_its_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, _fake) = ws_ctx_with_nix(tmp.path());
    let ws = ready_workspace("ws-1", vec!["hello".into()]);
    apply_until_settled(&ws, &ctx).await;

    let pin = rustic_git_agent::nix::nixpkgs_pin();
    let hash = rustic_git_workspaces::packages::hash(&pin, &with_base(&["hello".into()]));
    assert_eq!(
        rustic_git_agent::nix::indexed(&ctx.profiles_dir, &hash),
        Some(std::path::PathBuf::from("/tmp")),
        "the store path the build produced"
    );
}

/// The pod is created unconditionally once the workspace's volume and profile are ready — no
/// storage-binding gate any more: a `hostPath` needs no binding, and a missing directory is a
/// mount failure the pod reports, not a race the controller prevents.
#[tokio::test]
async fn a_workspace_gets_its_pod_once_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _fake) = ws_ctx_with_ssh(tmp.path(), ssh_routes());
    apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;
    assert!(rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")), "{:?}", rec.calls());
    // Storage is the node's filesystem now: no reconcile writes a PersistentVolume or a claim.
    assert!(
        rec.calls().iter().all(|c| !c.contains("persistentvolume")),
        "no PV or PVC traffic at all: {:?}", rec.calls()
    );
}

/// A workspace whose `live/{id}` worktree already exists on this node (a
/// re-reconcile after an earlier pass materialized it, or a pod restarting on the same disk)
/// converges through `WORKTREE_EXISTS` rather than erroring — the pass still reaches the pod, so
/// materialization never blocks a workspace whose worktree is already there. Real btrfs is not
/// available in this test environment, so this is the one commit-model path exercisable here: the
/// `Engine::checkout` call, without ever shelling out, because `dst.exists()` is checked first.
#[tokio::test]
async fn commit_model_checkout_converges_on_an_existing_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/ws-1/live/ws-1")).unwrap();
    let mut routes = ssh_routes();
    routes.push(Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: commit_list_of("Snapshot", vec![]) });
    let (ctx, rec, _fake) = ws_ctx_with_ssh(tmp.path(), routes);

    apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;

    assert!(
        rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")),
        "an already-materialized worktree must not block the pod: {:?}", rec.calls()
    );
}

/// The environment side of the same bootstrap: Task 4 wired the checkout arm
/// into `apply_workspace` only and left the Environment path to this task (`run_environment`'s
/// twin block, added beside `apply_workspace`'s). Same convergence trick as the workspace test
/// above — the `live/{id}` worktree already exists, so `Engine::checkout` converges through
/// `WORKTREE_EXISTS` without ever shelling to real btrfs — and the same zero-commit volume, so
/// the `HeadUnknown` gate never engages. Reaching the Namespace `ensure` call (the very next thing
/// `run_environment` does) is the proof the checkout arm did not block the pass.
#[tokio::test]
async fn commit_model_environment_bootstrap_materializes_its_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/env-1/live/env-1")).unwrap();
    let routes = vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: commit_list_of("Snapshot", vec![]) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    // The pass runs past the Namespace `ensure` call and then fails on the next unmocked route
    // (NetworkPolicy, RoleBinding, ...) — that error is not the point; the point is that the
    // checkout arm let it get this far at all instead of parking at `HeadUnknown` or a btrfs error.
    let _ = rustic_git_agent::controller::apply_environment(&e, &ctx).await;

    assert!(
        rec.calls().iter().any(|c| c.starts_with("PATCH") && c.contains("/namespaces/env-1")),
        "the worktree materialized and the pass reached namespace reconciliation: {:?}", rec.calls()
    );
}

// ── commit-model clone/restore (Task 6b) ────────────────────────────────

/// A `Ready` `Snapshot` of the volume a `cloneOf` names — the precondition `commit_ready` checks
/// before ever letting a clone check out. Kept separate from `snapshot_cr` (worktree/parent don't
/// matter here) so the volume and phase are the only things a test has to vary.
fn ready_commit(name: &str, volume: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name, "uid": "commit-uid"},
        "spec": {"volume": volume, "owner": "alice", "worktree": volume, "parent": "", "pinned": false},
        "status": {"phase": "ready"},
    })
}

/// `check_source` proves the clone SOURCE object exists (Workspace, then Environment) before
/// anything else — independent of, and ahead of, the volume-level checks below.
fn source_workspace_exists(id: &str) -> Route {
    rustic_git_workspaces::kube_test::get(
        format!("/apis/rustic-git.io/v1alpha1/workspaces/{id}"),
        serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
                           "metadata": {"name": id},
                           "spec": {"owner": "alice", "team": "", "name": id, "region": "r1",
                                    "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"},
                           "status": {"phase": "ready", "nodeName": "node-a", "compatibleNodes": ["node-a"]}}),
    )
}

fn ready_source_volume(id: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": id, "uid": "src-vol-uid"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true},
    })
}

/// A workspace whose `cloneOf` carries a graft commit and no worktree yet — a fresh clone.
fn cloned_workspace(commit: &str, head: Option<&str>) -> crd::Workspace {
    let mut status = serde_json::json!({"phase": "creating", "nodeName": "node-a", "compatibleNodes": ["node-a"]});
    if let Some(h) = head {
        status["head"] = serde_json::json!(h);
    }
    let mut w = workspace(status);
    w.spec.storage = Some(crd::WorkspaceStorage {
        quota_gb: 20,
        source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into(), commit: Some(commit.into()) }),
    });
    w
}

/// A clone with no head of its own yet checks out the GRAFTED commit (never bootstraps empty next
/// to the source's real history) and records it as its own `head` on the very first pass — the
/// same preserve-pattern write `snapshot::advance_head` uses for a push, so retention's
/// `worktree_heads` sees it from here on. `resolve_volume` also proves clone PLACEMENT here: the
/// SOURCE's volume (`ws-src`), not a freshly created child, is what gets read — the route list has
/// no `POST /volumes` at all, so `ensure_child_volume` was never called.
#[tokio::test]
async fn commit_model_clone_checks_out_its_graft_commit_and_records_it_as_head() {
    let tmp = tempfile::tempdir().unwrap();
    // The worktree name is the WORKSPACE's own id, on the SOURCE volume's snap tree — never
    // `vol/ws-1/...`, which would be a fresh (and wrong) child volume.
    std::fs::create_dir_all(tmp.path().join("vol/ws-src/live/ws-1")).unwrap();
    let routes = vec![
        source_workspace_exists("ws-src"),
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-src", ready_source_volume("ws-src")),
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/snapshots/ws-src-aaaaaaaa", ready_commit("ws-src-aaaaaaaa", "ws-src")),
        ready_binding(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = cloned_workspace("ws-src-aaaaaaaa", None);

    // The pass runs past the checkout arm and then fails on the next unmocked route (namespace,
    // profile, ...) — not the point here; the head write already landed by then.
    let _ = rustic_git_agent::controller::apply_workspace(&w, &ctx).await;

    let sent = rec.sent("PATCH", WS_STATUS);
    assert!(
        sent.iter().any(|s| s["status"]["head"] == "ws-src-aaaaaaaa"),
        "the graft commit must be recorded as this clone's own head: {sent:?}"
    );
    assert!(!rec.calls().iter().any(|c| c.contains("POST") && c.contains("/volumes")), "a shared-volume clone creates no child Volume");
}

/// A clone naming a commit that is not a `Ready` `Snapshot` of its source volume — swept by
/// retention, or simply never existed — settles PERMANENTLY with its own reason, distinct from a
/// bad clone SOURCE (`NoSuchSource`, settled earlier by `check_source`): retrying at TICK would
/// spin on the same missing snapshot forever.
#[tokio::test]
async fn commit_model_clone_with_a_missing_commit_settles_as_no_such_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let routes = vec![
        source_workspace_exists("ws-src"),
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-src", ready_source_volume("ws-src")),
        rustic_git_workspaces::kube_test::not_found("/apis/rustic-git.io/v1alpha1/snapshots/ws-src-gone"),
        ready_binding(),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = cloned_workspace("ws-src-gone", None);

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "settled permanently, not requeued");

    let sent = rec.sent("PATCH", WS_STATUS);
    let last = sent.last().expect("a status write");
    assert_eq!(last["status"]["phase"], "error");
    let cond = &last["status"]["conditions"][0];
    assert_eq!(cond["reason"], "NoSuchCommit");
}

/// A clone that already has its own `head` (it pushed since being grafted) never re-derives it
/// from `cloneOf` — the graft is a ONE-TIME starting point, not a value this pass keeps re-reading.
#[tokio::test]
async fn commit_model_clone_with_a_head_of_its_own_does_not_rewrite_it() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/ws-src/live/ws-1")).unwrap();
    let routes = vec![
        source_workspace_exists("ws-src"),
        rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/volumes/ws-src", ready_source_volume("ws-src")),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let w = cloned_workspace("ws-src-aaaaaaaa", Some("ws-1-own-commit"));

    let _ = rustic_git_agent::controller::apply_workspace(&w, &ctx).await;

    // No snapshot GET at all: an already-owned head skips `commit_ready`'s validation of the
    // graft commit entirely, and no status write ever names the graft commit as `head`.
    assert!(!rec.calls().iter().any(|c| c.contains("/snapshots/")), "{:?}", rec.calls());
    assert!(rec.sent("PATCH", WS_STATUS).iter().all(|s| s["status"]["head"] != "ws-src-aaaaaaaa"));
}

/// Restore-in-place never touches the registry (the old `get_history`/`restore` HTTP calls, gone
/// with the object-store subsystem) — the checkout-swap (`Engine::swap_worktree`) is entirely
/// local. Real btrfs is unavailable in this test environment, so the swap itself errors past the
/// point this asserts; the point is that nothing here ever reaches for a network call.
#[tokio::test]
async fn commit_model_restore_in_place_never_calls_the_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "env-1", "uid": "vol-uid-1", "generation": 2},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20,
                 "restoreTo": {"snapshotId": "env-1-bbbbbbbb", "volume": "env-1", "requestedAt": "2026-09-01T00:00:00Z"}},
        "status": {"phase": "ready", "subvolumePresent": true},
    });
    let routes = vec![
        Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/env-1/status".into(), status: 200, body: vol.clone() },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let v: crd::Volume = serde_json::from_value(vol).unwrap();

    let _ = rustic_git_agent::controller::apply_volume(&v, &ctx).await;
    wait_idle(&ctx).await;

    assert!(
        !rec.calls().iter().any(|c| c.contains("get_history") || c.contains("registry")),
        "commit-model restore must never fetch from the registry: {:?}", rec.calls()
    );
}

/// The sync beat is keep-biased about not knowing: `Engine::generation` shells out to `btrfs
/// subvolume show`, which cannot work here, so this asserts the beat WARNS AND CREATES NOTHING on
/// a generation error — cutting on "we do not know" would cut a redundant sync point every single
/// pass. The decision itself (has the generation moved?) is a pure function tested in `sync.rs`;
/// no fake-`generation` seam is worth carrying for a second test of it.
#[tokio::test]
async fn the_sync_beat_cuts_a_transient_only_when_the_worktree_generation_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a",
                                             "volumeRef": "vol-1", "podRef": "ws-1"}))]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces", ws_list),
            rustic_git_workspaces::kube_test::get(
                "/apis/rustic-git.io/v1alpha1/environments",
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "EnvironmentList",
                                   "metadata": {}, "items": []}),
            ),
        ],
    );

    rustic_git_agent::sync::sync_beat(&ctx).await;

    assert!(
        rec.calls().iter().any(|c| c == "GET /apis/rustic-git.io/v1alpha1/snapshots"),
        "the beat must look for this worktree's existing sync point: {:?}", rec.calls()
    );
    assert!(
        rec.sent("POST", SNAPSHOTS_LIST).is_empty(),
        "an unreadable generation must cut nothing: {:?}", rec.sent("POST", SNAPSHOTS_LIST)
    );
}
