//! the packages step.

use super::*;


/// The mocked `Ctx` every profile test wants: a fake Nix it can inspect, its own profile root, and
/// a workspace whose Volume answers ready so the pass reaches the packages step.
pub(crate) fn ws_ctx_with_nix(pool: &std::path::Path) -> (Arc<Ctx>, Recorder, Arc<FakeNix>) {
    ws_ctx_with_ssh(pool, ssh_routes())
}

/// No host key yet, so the pass mints one.
pub(crate) fn ssh_routes() -> Vec<Route> {
    vec![
        kloudlite_workspaces::kube_test::not_found(WS_SSH_SECRET),
        kloudlite_workspaces::kube_test::post(
            "/api/v1/namespaces/ws-alice/secrets",
            serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "ws-ssh-ws-1"}}),
        ),
    ]
}

pub(crate) const WS_SSH_SECRET: &str = "/api/v1/namespaces/ws-alice/secrets/ws-ssh-ws-1";

pub(crate) fn ws_ctx_with_ssh(pool: &std::path::Path, ssh: Vec<Route>) -> (Arc<Ctx>, Recorder, Arc<FakeNix>) {
    let vol = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    let fake = Arc::new(FakeNix::default());
    let mut routes = ssh;
    routes.extend(vec![
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-1", vol),
        ready_binding(),
        ready_namespace(),
        kloudlite_workspaces::kube_test::post(
            "/api/v1/namespaces/ws-alice/pods",
            serde_json::json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws-1"}}),
        ),
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
    ]);
    // The snapshot-model checkout step is unconditional now (Task 8): pre-seed an empty worktree
    // so `Engine::checkout` converges on `WORKTREE_EXISTS` instead of shelling out to a real
    // `btrfs subvolume create` this test environment doesn't have.
    std::fs::create_dir_all(pool.join("vol/ws-1/live/ws-1")).unwrap();
    let (ctx, rec) = ctx_full(pool, routes, fake.clone());
    // The pod mounts the owner's home, so the Running arm waits for it to be Ready here.
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    (ctx, rec, fake)
}

pub(crate) fn ready_workspace(id: &str, packages: Vec<String>) -> crd::Workspace {
    let mut o = ws_json(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    o["metadata"]["name"] = id.into();
    o["spec"]["packages"] = serde_json::json!(packages);
    serde_json::from_value(o).unwrap()
}

pub(crate) const MIRROR_PATH: &str = "/nix/store/11111111111111111111111111111111-nodejs-20.20.2";
pub(crate) const LOCKED_PATH: &str = "/nix/store/00000000000000000000000000000000-nodejs-20.20.2";

/// A workspace with one pinned entry and the lock `/v1` would have written for it.
pub(crate) fn locked_workspace(store_path: &str) -> crd::Workspace {
    let mut w = ready_workspace("ws-1", vec!["nodejs@20".into()]);
    w.spec.locks = vec![crd::Lock {
        entry: "nodejs@20".into(),
        version: "20.20.2".into(),
        attr_path: "nodejs_20".into(),
        rev: "a".repeat(40),
        store_path: store_path.into(),
        resolved_at: "2026-09-08T00:00:00Z".into(),
        source: crd::LockSource::Nixhub,
    }];
    w
}

/// Apply until the profile step stops asking to be requeued: the build runs on its own thread, so
/// the pass that observes it is a later one — as with every other long operation here.
pub(crate) async fn apply_until_settled(w: &crd::Workspace, ctx: &Arc<Ctx>) -> kube::runtime::controller::Action {
    for _ in 0..4 {
        let action = kloudlite_agent::controller::apply_workspace(w, ctx).await.unwrap();
        if ctx.running.lock().unwrap().is_empty() {
            return action;
        }
        wait_idle(ctx).await;
    }
    panic!("the profile step never settled");
}

pub(crate) fn packages_condition(status: &serde_json::Value) -> serde_json::Value {
    status["status"]["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "PackagesReady")
        .unwrap_or_else(|| panic!("no PackagesReady condition in {status}"))
        .clone()
}

/// F7: the drain notice a person actually reads. `node-a` carries the decommission label, so the
/// RUNNING workspace's own reconcile stamps `Decommissioning=True/NodeLeaving` — the decommission
/// beat used to write it and this very pass, which rewrites the condition list wholesale every
/// TICK, erased it seconds later.
pub(crate) fn decommissioning_node() -> Route {
    kloudlite_workspaces::kube_test::get(
        "/api/v1/nodes/node-a",
        serde_json::json!({"apiVersion": "v1", "kind": "Node",
                           "metadata": {"name": "node-a", "labels": {crd::DECOMMISSION_LABEL: "true"}},
                           "status": {"conditions": [{"type": "Ready", "status": "True",
                                                      "lastTransitionTime": rfc3339_ago(60)}]}}),
    )
}

pub(crate) fn drain_notice(sent: &[serde_json::Value]) -> Option<serde_json::Value> {
    sent.iter()
        .flat_map(|s| s["status"]["conditions"].as_array().cloned().unwrap_or_default())
        .find(|c| c["type"] == "Decommissioning")
}

#[tokio::test]
async fn a_running_workspace_on_a_retiring_node_carries_the_drain_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ssh_routes();
    routes.push(decommissioning_node());
    let (ctx, rec, _) = ws_ctx_with_ssh(tmp.path(), routes);

    apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;

    let cond = drain_notice(&rec.sent("PATCH", WS_STATUS)).expect("the drain notice");
    assert_eq!(cond["status"], "True");
    assert_eq!(cond["reason"], "NodeLeaving");
    assert_eq!(cond["message"], "this node is being retired; stop when convenient and the next start lands elsewhere");
}

/// The same pass on a node nobody is retiring says nothing: the condition is a fact about the NODE,
/// re-read every reconcile, so removing the label removes the notice on the very next tick.
#[tokio::test]
async fn a_running_workspace_on_an_ordinary_node_carries_no_drain_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _) = ws_ctx_with_nix(tmp.path());

    apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;

    assert_eq!(drain_notice(&rec.sent("PATCH", WS_STATUS)), None);
}

/// Round 4: the reverse of the notice landing. The parents now watch their own Node, so removing
/// the decommission label re-reconciles every workspace here — and this pass must DROP the notice
/// it was carrying. It does because the running arm rebuilds the condition list wholesale and
/// `kept_conditions` carries only `PackagesReady`/`Attached` forward, which is the same property
/// that made the beat's own mark unkeepable.
#[tokio::test]
async fn a_running_workspace_drops_a_stale_drain_notice_once_the_label_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _) = ws_ctx_with_nix(tmp.path());
    let mut w = ready_workspace("ws-1", vec![]);
    let mut st = w.status.clone().unwrap_or_default();
    st.conditions.push(crd::condition("Decommissioning", true, "NodeLeaving", "this node is being retired; stop when convenient and the next start lands elsewhere", 1));
    w.status = Some(st);

    apply_until_settled(&w, &ctx).await;

    // The LAST write of the pass — the steady state a person reads. The packages step's own
    // interim writes use `replaced`, which preserves every condition by type and so carries the
    // stale one for as long as a build runs; the running arm below it is what clears it, and no
    // pass ends there.
    let last = rec.sent("PATCH", WS_STATUS).pop().expect("a status write");
    assert_eq!(drain_notice(std::slice::from_ref(&last)), None, "the notice must not outlive the label: {last}");
}

/// A STOPPED workspace never carries it, on any node. The notice asks the person to stop when
/// convenient; on one they have already stopped it is a message with nothing behind it.
#[tokio::test]
async fn a_stopped_workspace_never_carries_the_drain_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = vec![decommissioning_node()];
    routes.extend(ws_stop_routes());
    routes.push(kloudlite_workspaces::kube_test::not_found(WS_POD_DEL));
    let (ctx, rec) = ctx(tmp.path(), routes);

    let _ = kloudlite_agent::controller::apply_workspace(&stopping_ws(), &ctx).await;

    let sent = rec.sent("PATCH", WS_STATUS);
    assert!(!sent.is_empty(), "the stop arm must have written a status");
    assert_eq!(drain_notice(&sent), None, "{sent:?}");
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
    assert!(kloudlite_agent::nix::profile_exists(&ctx.profiles_dir, "ws-1"), "published as <dir>/current");
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
    assert!(kloudlite_agent::nix::profile_exists(&ctx.profiles_dir, "ws-1"), "the link the pod mounts");
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
        vec![kloudlite_workspaces::kube_test::get(
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
    let pin = kloudlite_agent::nix::nixpkgs_pin(&test_settings());
    ws.status.as_mut().unwrap().packages = Some(kloudlite_workspaces::crd::PackagesStatus {
        base: vec![],
        observed: vec!["hello".into()],
        observed_hash: Some(kloudlite_workspaces::packages::hash(&pin, &with_base(&["hello".into()]), &[])),
        profile: None,
        nixpkgs: Some(pin),
        locked: vec![],
    });
    plant_profile(&ctx, "ws-1");
    let _ = kloudlite_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
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
    assert!(kloudlite_agent::nix::profile_exists(&ctx.profiles_dir, "ws-1"), "the previous profile is untouched");
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
    let _ = kloudlite_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
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
    let _ = kloudlite_agent::controller::apply_workspace(&first, &ctx).await.unwrap();
    wait_idle(&ctx).await;
    // Every later pass sees the edited spec.
    apply_until_settled(&second, &ctx).await;

    let builds = fake.builds.lock().unwrap().clone();
    assert_eq!(builds.len(), 2, "the superseded build is discarded and the new spec built: {builds:?}");
    assert!(builds[1].contains("pkgs.jq"), "the second build is the edited list: {}", builds[1]);
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    let pin = kloudlite_agent::nix::nixpkgs_pin(&test_settings());
    assert_eq!(
        st["status"]["packages"]["observedHash"],
        kloudlite_workspaces::packages::hash(&pin, &with_base(&["hello".into(), "jq".into()]), &[]),
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
    let action = kloudlite_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();

    assert!(fake.builds.lock().unwrap().is_empty(), "nothing is built without a daemon");
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    let c = packages_condition(&st);
    assert_eq!(c["reason"], "NoNix");
    assert!(c["message"].as_str().unwrap().contains("daemon-socket"), "{c}");
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(60)));

    // With a profile already on disk the pod still runs — the tools it has keep working.
    plant_profile(&ctx, "ws-1");
    let _ = kloudlite_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
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
    let _ = kloudlite_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();

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
    let pin = kloudlite_agent::nix::nixpkgs_pin(&test_settings());
    // The disk has [hello]; the spec asks for [hello, jq]; status says Building and — correctly —
    // still names the OLD list. The Ctx is fresh: no handle, no remembered hash, as after a crash.
    plant_profile(&ctx, "ws-1");
    let mut ws = ready_workspace("ws-1", vec!["hello".into(), "jq".into()]);
    let st = ws.status.as_mut().unwrap();
    st.packages = Some(kloudlite_workspaces::crd::PackagesStatus {
        base: vec![],
        observed: vec!["hello".into()],
        observed_hash: Some(kloudlite_workspaces::packages::hash(&pin, &with_base(&["hello".into()]), &[])),
        profile: None,
        nixpkgs: Some(pin),
        locked: vec![],
    });
    st.conditions = vec![crd::condition(crd::PACKAGES_READY, false, "Building", "taking the profile through nix", 1)];
    assert!(ctx.running.lock().unwrap().is_empty());

    let _ = kloudlite_agent::controller::apply_workspace(&ws, &ctx).await.unwrap();
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
            let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
            wait_idle(&ctx).await;
            kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap()
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
    // The elapsed time is measured against the wall clock, so the two applies above add a few
    // seconds on a loaded box: assert the ten-minute bucket, not the exact second.
    let ten_minutes = fail_once(&ws, &ctx).await;
    assert!(
        (600..=660).any(|s| ten_minutes == kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(s))),
        "ten minutes in: {ten_minutes:?}"
    );
}
