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
    let (client, rec) = mock_client(routes);
    let engine = Engine::new(
        Pool::new(pool),
        Arc::new(object_store::memory::InMemory::new()),
        Arc::new(MemStore::new()),
        RegistryClient::new("http://127.0.0.1:1", "unused"),
    );
    (
        Arc::new(Ctx::new(
            client,
            Arc::new(engine),
            "node-a".into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            vec!["session".into(), "env".into()],
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
            rustic_git_workspaces::kube_test::get(
                "/apis/rustic-git.io/v1alpha1/ownerbindings/r1-alice",
                serde_json::json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-alice"},
                                   "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"},
                                   "status": {"conditions": [{"type": "NamespaceReady", "status": "True",
                                                              "reason": "Converged", "message": "ok",
                                                              "lastTransitionTime": "2026-08-27T00:00:00Z"}]}}),
            ),
            Route { method: "PATCH", path: "/api/v1/persistentvolumes/pv-ws-1".into(), status: 200,
                    body: serde_json::json!({"apiVersion": "v1", "kind": "PersistentVolume", "metadata": {"name": "ws-1"}}) },
            Route { method: "PATCH", path: "/api/v1/namespaces/ws-alice/persistentvolumeclaims/live-ws-1".into(), status: 200,
                    body: serde_json::json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": "ws-1"}}) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

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
            Ok(Done { phase: crd::Phase::Done, lineage_tip: Some("layer-9".into()) })
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
            Ok(Done { phase: crd::Phase::Done, lineage_tip: None })
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
}
