//! The node controller's three load-bearing behaviours, against a mocked API server
//! (`kloudlite_workspaces::kube_test`) — no cluster, no btrfs.
//!
//! These are deliberately about the *loop*, not about btrfs: what the reconcile starts, what it
//! refuses to start twice, and what it never deletes. The btrfs half is covered by the engine's own
//! loopback tests and by `tests/ws_e2e.sh` against real k3s.

use kloudlite_agent::controller::{Ctx, Done};
use kloudlite_core::settings::LiveSettings;
use kloudlite_workspaces::crd;
use kloudlite_workspaces::engine::{Engine, Pool};
use kloudlite_workspaces::kube_test::{mock_client, Recorder, Route};
use kloudlite_workspaces::settings::AgentSettings;
use std::sync::Arc;

mod placement_claims;
mod capacity;
mod snapshot_model_placement;
mod parent_deletion_and_the_volume;
mod owner_bindings_and_quota;
mod the_workspace_reconciler_and_its_volume;
mod the_stop_before_teardown_snapshot;
mod in_place_restore;
mod the_fixes_from_the_final_branch_review;
mod completion_wakes_the_reconciler;
mod the_packages_step;
mod what_the_node_asks_the_api_server_for;
mod attachment;
mod snapshot_model_clone_restore_task_6b;
mod starts_spread_the_owner_gives_a_movable;
mod filter_foreign_snapshots_against_the_nod;
mod the_agent_decides_from_stores;
#[allow(unused_imports)]
use attachment::*;
#[allow(unused_imports)]
use capacity::*;
#[allow(unused_imports)]
use completion_wakes_the_reconciler::*;
#[allow(unused_imports)]
use filter_foreign_snapshots_against_the_nod::*;
#[allow(unused_imports)]
use in_place_restore::*;
#[allow(unused_imports)]
use placement_claims::*;
#[allow(unused_imports)]
use snapshot_model_clone_restore_task_6b::*;
#[allow(unused_imports)]
use snapshot_model_placement::*;
#[allow(unused_imports)]
use parent_deletion_and_the_volume::*;
#[allow(unused_imports)]
use owner_bindings_and_quota::*;
#[allow(unused_imports)]
use starts_spread_the_owner_gives_a_movable::*;
#[allow(unused_imports)]
use the_fixes_from_the_final_branch_review::*;
#[allow(unused_imports)]
use the_packages_step::*;
#[allow(unused_imports)]
use the_stop_before_teardown_snapshot::*;
#[allow(unused_imports)]
use the_workspace_reconciler_and_its_volume::*;
#[allow(unused_imports)]
use what_the_node_asks_the_api_server_for::*;
#[allow(unused_imports)]
use the_agent_decides_from_stores::*;

fn test_settings() -> LiveSettings<AgentSettings> {
    LiveSettings::new(AgentSettings::from_env())
}

const VOL_STATUS: &str = "/apis/kloudlite.io/v1alpha1/volumes/vol-1/status";

/// A fake `Nix` that records the expressions it was asked to build and answers as told. It
/// returns a STORE PATH, as the real one does: the link and the publish are the reconciler's job,
/// not nix's, because `nix -o`'s auto GC root does not survive the rename.
struct FakeNix {
    builds: std::sync::Mutex<Vec<String>>,
    /// The store paths `copy_from_cache` was asked for, and the answer it gives.
    copies: std::sync::Mutex<Vec<String>>,
    copy_answer: std::sync::Mutex<Result<(), String>>,
    /// The `(rev, attr)` pairs a mirror lock was evaluated for, and the path it answers with.
    evals: std::sync::Mutex<Vec<(String, String)>>,
    eval_answer: std::sync::Mutex<Result<String, String>>,
    answer: std::sync::Mutex<Result<(), String>>,
    ping: std::sync::Mutex<Result<(), String>>,
    /// Run while a build is "in flight", so a test can change the spec mid-build.
    on_build: std::sync::Mutex<Option<Box<dyn Fn() + Send>>>,
}
impl Default for FakeNix {
    fn default() -> Self {
        FakeNix {
            builds: std::sync::Mutex::new(Vec::new()),
            copies: std::sync::Mutex::new(Vec::new()),
            copy_answer: std::sync::Mutex::new(Ok(())),
            evals: std::sync::Mutex::new(Vec::new()),
            eval_answer: std::sync::Mutex::new(Ok(MIRROR_PATH.into())),
            answer: std::sync::Mutex::new(Ok(())),
            ping: std::sync::Mutex::new(Ok(())),
            on_build: std::sync::Mutex::new(None),
        }
    }
}
#[async_trait::async_trait]
impl kloudlite_agent::nix::Nix for FakeNix {
    async fn build(&self, expr: &str, _: std::time::Duration) -> Result<std::path::PathBuf, String> {
        self.builds.lock().unwrap().push(expr.to_string());
        if let Some(f) = self.on_build.lock().unwrap().take() {
            f();
        }
        let r = self.answer.lock().unwrap().clone();
        r.map(|()| std::path::PathBuf::from("/tmp"))
    }
    async fn eval_out_path(&self, rev: &str, attr: &str, _: std::time::Duration) -> Result<String, String> {
        self.evals.lock().unwrap().push((rev.to_string(), attr.to_string()));
        self.eval_answer.lock().unwrap().clone()
    }
    async fn copy_from_cache(&self, store_path: &str, _: std::time::Duration) -> Result<(), String> {
        self.copies.lock().unwrap().push(store_path.to_string());
        self.copy_answer.lock().unwrap().clone()
    }
    async fn ping(&self) -> Result<(), String> { self.ping.lock().unwrap().clone() }
    async fn collect_garbage(&self) -> Result<u64, String> { Ok(0) }
}

/// A profile as a finished build leaves it: the directory the pod mounts, with `current` inside.
/// The list the node actually hashes: the platform base set first, then the workspace's own.
fn with_base(own: &[String]) -> Vec<String> {
    let base = kloudlite_agent::nix::base_packages(&test_settings());
    let mut all = base.clone();
    all.extend(own.iter().filter(|p| !base.contains(p)).cloned());
    all
}

fn plant_profile(ctx: &Arc<Ctx>, id: &str) {
    std::fs::create_dir_all(kloudlite_agent::nix::profile_dir(&ctx.profiles_dir, id)).unwrap();
    std::os::unix::fs::symlink("/tmp", kloudlite_agent::nix::profile_path(&ctx.profiles_dir, id)).unwrap();
}

fn patch_ok(path: &str) -> Route {
    Route { method: "PATCH", path: path.into(), status: 200, body: volume_json(1) }
}

fn volume_json(generation: i64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1",
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
    // A literal address, never a name: the agent resolves the export host at boot, and a name
    // that happens to resolve on a laptop (`test`) is NXDOMAIN inside the cluster, where these
    // tests also run — 160 of them failed there on "Name or service not known".
    ctx_with_homes_export(pool, routes, nix, Some("127.0.0.1:/".into()))
}

/// The `WS_HOMES_EXPORT`-unset variant: a node with no shared-home mount, which every workspace
/// reconcile must park on rather than start a pod against.
fn ctx_without_homes_export(pool: &std::path::Path, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
    ctx_with_homes_export(pool, routes, Arc::new(FakeNix::default()), None)
}

fn ctx_with_homes_export(pool: &std::path::Path, routes: Vec<Route>, nix: Arc<FakeNix>, homes_export: Option<String>) -> (Arc<Ctx>, Recorder) {
    ctx_on_node("node-a", pool, routes, nix, homes_export)
}

/// The same fixture as some OTHER node — what the hand-off half of a capacity decline needs: one
/// node with no room, and a second one that takes the parent it left unplaced.
fn ctx_on_node(node: &str, pool: &std::path::Path, mut routes: Vec<Route>, nix: Arc<FakeNix>, homes_export: Option<String>) -> (Arc<Ctx>, Recorder) {
    // Every reconcile now unconditionally may ask "does this volume have snapshots yet"
    // (`claim::placement`/`has_snapshots`, the checkout/migrate step) — a call no test fixture
    // needed before the snapshot model became the only model (Task 8). Appended AFTER the caller's
    // own routes, so a test that mocks its own `/snapshots` response (exact history, retention,
    // …) still hits that one first; this is only the default "nothing here yet" answer for every
    // test that never cared about snapshots at all.
    routes.push(kloudlite_workspaces::kube_test::get(
        "/apis/kloudlite.io/v1alpha1/snapshots",
        serde_json::json!({"apiVersion": "v1", "kind": "SnapshotList", "items": []}),
    ));
    // The delete path now asks "does an unmaterialized rescue clone name one of these cuts"
    // (`snapshot::seeded_from_cuts`) before it deletes any of them, so every finalizer fixture
    // needs an answer. Appended after the caller's own routes: "no clone is seeding from anything"
    // is only the default for the tests that are not about that rule.
    routes.push(kloudlite_workspaces::kube_test::get(
        "/apis/kloudlite.io/v1alpha1/volumes",
        serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeList",
                           "metadata": {"resourceVersion": "1"}, "items": []}),
    ));
    // Likewise the stop's flush gate: a workspace stop now waits until another node holds its
    // final sync point, so the default answer for every test that is not ABOUT the flush is
    // "already cut, already replicated". Appended last, so a flush test's own routes win.
    routes.push(kloudlite_workspaces::kube_test::get(
        WS_STOP_REQ,
        serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
                           "metadata": {"name": "stop-ws-1-1", "uid": "stop-ws-uid", "creationTimestamp": rfc3339_ago(60)},
                           "spec": {"volume": "ws-1", "owner": "alice", "worktree": "ws-1", "transient": true},
                           "status": {"phase": "ready", "readyAt": rfc3339_ago(30)}}),
    ));
    routes.push(kloudlite_workspaces::kube_test::get(
        REPLICAS,
        serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplicaList",
                           "metadata": {"resourceVersion": "1"},
                           "items": [{"apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
                                      "metadata": {"name": "ws-1.node-b", "uid": "vr-b"},
                                      "spec": {"volume": "ws-1", "node": "node-b"},
                                      "status": {"phase": "Synced", "branches": {}, "lastSyncAt": rfc3339_ago(1)}}]}),
    ));
    // The claim asks whether THIS node is placeable at all (dead or decommissioning takes no new
    // work), so every fixture needs a Ready node-a. SKIPPED when the test brought its own: the mock
    // walks same-path routes in order and repeats the last, so merely appending this would answer
    // "Ready and unlabelled" from the second pass onward — silently un-draining a node midway
    // through a multi-pass test.
    if !routes.iter().any(|r| r.method == "GET" && r.path == format!("/api/v1/nodes/{node}")) {
        routes.push(kloudlite_workspaces::kube_test::get(
            format!("/api/v1/nodes/{node}"),
            serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": node},
                               // Allocatable, because a FRESH claim now checks capacity: an
                               // 8-vCPU/32 GiB node, room for several default workspaces.
                               "status": {"allocatable": {"cpu": "8", "memory": "33554432Ki"},
                                          "conditions": [{"type": "Ready", "status": "True",
                                                          "lastTransitionTime": rfc3339_ago(60)}]}}),
        ));
    }
    // The other half of that check: what is already scheduled here, and what has been CLAIMED
    // here but has no pod yet. Empty unless the test says otherwise, same skip rule as the node.
    // `/api/v1/nodes` too: with the two parent lists answering "empty" instead of 404, a stop's
    // `wake_peers` now gets far enough to ask who its peers are.
    for (path, kind) in [("/api/v1/pods", "Pod"), ("/api/v1/nodes", "Node"), (WORKSPACES_LIST, "Workspace"), (ENVIRONMENTS_LIST, "Environment")] {
        if !routes.iter().any(|r| r.method == "GET" && r.path == path) {
            routes.push(kloudlite_workspaces::kube_test::get(
                path,
                serde_json::json!({"apiVersion": "v1", "kind": format!("{kind}List"), "metadata": {}, "items": []}),
            ));
        }
    }
    let (client, rec) = mock_client(routes);
    // Best effort: one test hands a plain file as its "pool" on purpose.
    let profiles = pool.join("profiles");
    let _ = std::fs::create_dir_all(&profiles);
    let engine = Engine::new(Pool::new(pool));
    // Ctx::new reads the pinned default image from the environment, as the agent does.
    std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/kloudlite-workspace:deadbeef");
    (
        Arc::new(Ctx::new(
            client,
            Arc::new(engine),
            node.into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            true,
            homes_export,
            "registry.kloudlite.io".into(),
            nix,
            profiles,
            test_settings(),
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
    // Held open by a channel the test owns rather than a 2 s sleep (2026-09-12): the operation is
    // in flight for exactly as long as this scope, so the assertion never races a timer.
    let (_hold, block) = std::sync::mpsc::channel::<()>();
    ctx.running.lock().unwrap().insert(
        "uid-1".to_string(),
        (1, tokio::task::spawn_blocking(move || {
            let _ = block.recv();
            Ok(Done::default())
        })),
    );

    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
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
        (7, tokio::task::spawn_blocking(|| Ok(Done { phase: kloudlite_workspaces::crd::Phase::Ready, ..Done::default() }))),
    );

    wait_idle(&ctx).await;

    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    let sent = rec.sent("PATCH", VOL_STATUS);
    assert_eq!(sent.len(), 1, "exactly one status write");
    assert_eq!(sent[0]["status"]["observedGeneration"], 7);
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");

    // The guard against the classic hot loop: the same status, computed again, is not rewritten.
    let mut observed = v.clone();
    observed.status = serde_json::from_value(sent[0]["status"].clone()).unwrap();
    let action = kloudlite_agent::controller::apply_volume(&observed, &ctx).await.unwrap();
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
    v.status = Some(kloudlite_workspaces::crd::VolumeStatus {
        phase: kloudlite_workspaces::crd::Phase::Working,
        observed_generation: Some(7),
        subvolume_present: true,
        ..Default::default()
    });

    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_ne!(
        action,
        kube::runtime::controller::Action::await_change(),
        "a stranded Working volume must be picked back up, not awaited forever"
    );
    assert!(!ctx.running.lock().unwrap().is_empty(), "the recovery pass must actually start work");
    wait_idle(&ctx).await;
}

/// The dead-node sweep writes `Unavailable` as a status change only, so when the owner returns
/// `observedGeneration` is still current. The pass must run anyway, or the volume — and every
/// workspace on it — stays "not materialized" until someone edits the object by hand.
#[tokio::test]
async fn an_unavailable_volume_is_re_run_when_its_owner_reconciles_it_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let mut v = volume(7);
    v.status = Some(kloudlite_workspaces::crd::VolumeStatus {
        phase: kloudlite_workspaces::crd::Phase::Unavailable,
        observed_generation: Some(7),
        subvolume_present: true,
        conditions: vec![crd::condition("Available", false, "NodeDead", "owner node-a is dead", 7)],
        ..Default::default()
    });

    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_ne!(
        action,
        kube::runtime::controller::Action::await_change(),
        "a returned owner must re-run its Unavailable volume, not await a spec change that never comes"
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

    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));

    // Let the doomed operation finish, then observe it.
    wait_idle(&ctx).await;
    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
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
    use kloudlite_workspaces::model::{EnvState, WsState};

    // Grepped from controller/workspace.rs. Volume phases are excluded deliberately: a Volume is never
    // projected into a doc, so its vocabulary is its own.
    use kloudlite_workspaces::crd::Phase;
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

    // A push still in flight for this volume: held by a channel, not a 700 ms sleep (2026-09-12),
    // so "still running when cleanup ran" is a fact and not a timing bet. Dropping `hold` below
    // is what ends it, so the second half of the test runs against a genuinely finished handle.
    let (hold, block) = std::sync::mpsc::channel::<()>();
    ctx.running.lock().unwrap().insert(
        "uid-1".to_string(),
        (1, tokio::task::spawn_blocking(move || {
            let _ = block.recv();
            Ok(Done::default())
        })),
    );

    let action = kloudlite_agent::controller::cleanup_volume(&v, &ctx).await.unwrap();
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)),
        "cleanup must requeue while an operation is running, not reclaim the subvolume"
    );
    // Still held: nothing was drained by a cleanup that decided to wait.
    assert!(!ctx.running.lock().unwrap().is_empty());

    // Once it finishes, the same call drains the handle and proceeds instead of requeueing
    // forever — while deleting, the finalizer routes every pass here, so nothing else could.
    drop(hold);
    wait_idle(&ctx).await;
    kloudlite_agent::controller::cleanup_volume(&v, &ctx).await.unwrap();
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained by cleanup");
}
