//! attachment.

use super::*;


/// The workspace-side objects an attachment adds, on top of `ws_ctx_with_nix`'s: the shared attach
/// claim, and both halves of the grant answered with themselves.
pub(crate) fn attach_routes() -> Vec<Route> {
    let np = |ns: &str| Route {
        method: "PATCH",
        path: format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/attach-ws-1"),
        status: 200,
        body: serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                                 "metadata": {"name": "attach-ws-1"}}),
    };
    vec![
        kloudlite_workspaces::kube_test::not_found(WS_SSH_SECRET),
        kloudlite_workspaces::kube_test::post(
            "/api/v1/namespaces/ws-alice/secrets",
            serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "ws-ssh-ws-1"}}),
        ),
        np("ws-alice"),
        np("env-abc"),
        // `attached_workspace` sets `spec.attachedEnvironment` with no label to match, so the
        // reconcile's `heal_attached_label` patches it back in on the first pass.
        Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1".into(), status: 200, body: ws_json(serde_json::json!({})) },
    ]
}

pub(crate) fn env_route(id: &str, region: &str) -> Route {
    kloudlite_workspaces::kube_test::get(
        format!("/apis/kloudlite.io/v1alpha1/environments/{id}"),
        serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
            "metadata": {"name": id, "uid": "env-uid-1", "generation": 1},
            "spec": {"owner": "alice", "name": "api", "region": region, "services": [],
                     "desiredState": "running"},
        }),
    )
}

pub(crate) fn attached_workspace(env_id: &str) -> crd::Workspace {
    let mut w = ready_workspace("ws-1", vec![]);
    w.spec.attached_environment = Some(env_id.into());
    w
}

pub(crate) fn attached_condition(rec: &Recorder) -> serde_json::Value {
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
    let written = std::fs::read_to_string(kloudlite_workspaces::k8s::attach_file(&ctx.pool, "ws-1")).unwrap();
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
    routes.push(kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/environments/env-gone"));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    apply_until_settled(&attached_workspace("env-gone"), &ctx).await;

    assert!(
        !rec.calls().iter().any(|c| c.contains("/networkpolicies/attach-ws-1") && c.starts_with("PATCH")),
        "no grant for an environment that is not there: {:?}",
        rec.calls()
    );
    let written = std::fs::read_to_string(kloudlite_workspaces::k8s::attach_file(&ctx.pool, "ws-1")).unwrap();
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

/// An unattached workspace has no `Attached` condition at all — and no grant is deleted, because
/// one was never recorded (2026-09-12). This DELETE used to run on every reconcile of every
/// workspace that has never been attached, against a policy that has never existed.
#[tokio::test]
async fn a_workspace_that_was_never_attached_reports_nothing_and_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), attach_routes());
    let _ = kloudlite_agent::controller::apply_workspace(&ready_workspace("ws-1", vec![]), &ctx).await.unwrap();

    assert!(
        !rec.calls().iter().any(|c| c.contains("networkpolicies/attach-ws-1")),
        "nothing was ever attached, so there is nothing to delete: {:?}",
        rec.calls()
    );
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert!(
        !st["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Attached"),
        "not attached is not a condition: {st}"
    );
}

/// The detach the DELETE exists for: the field is cleared but the LAST pass recorded an
/// attachment, so the workspace-side grant goes by name.
#[tokio::test]
async fn a_detached_workspace_deletes_the_grant_it_was_recorded_as_holding() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), attach_routes());
    let mut w = ready_workspace("ws-1", vec![]);
    let st = w.status.get_or_insert_with(Default::default);
    st.conditions = vec![crd::condition(crd::ATTACHED, true, "Converged", "env-1", 1)];
    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    assert!(
        rec.calls().iter().any(|c| *c == "DELETE /apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/attach-ws-1"),
        "the grant is deleted by name: {:?}",
        rec.calls()
    );
}

/// The status this workspace would carry after a pass attached to `env_id` — the `Attached`
/// message is where the previous environment's namespace is read back from.
pub(crate) fn was_attached_to(env_id: &str) -> crd::Workspace {
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
pub(crate) fn workspace_pod_json(volumes: serde_json::Value) -> serde_json::Value {
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
    routes.push(kloudlite_workspaces::kube_test::get(
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
    routes.push(kloudlite_workspaces::kube_test::get(
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
    kloudlite_agent::controller::apply_workspace(&stopping, &ctx).await.unwrap();
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
    let mut routes = vec![kloudlite_workspaces::kube_test::get(
        "/apis/kloudlite.io/v1alpha1/volumes/ws-1",
        serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
            "status": {"phase": "creating", "subvolumePresent": false}
        }),
    )];
    routes.extend(attach_routes());
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);

    kloudlite_agent::controller::apply_workspace(&was_attached_to("env-abc"), &ctx).await.unwrap();
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
    let pin = kloudlite_agent::nix::nixpkgs_pin(&test_settings());
    let hash = kloudlite_workspaces::packages::hash(&pin, &with_base(&["hello".into()]), &[]);
    kloudlite_agent::nix::record_index(&ctx.profiles_dir, &hash, &store).unwrap();

    let ws = ready_workspace("ws-1", vec!["hello".into()]);
    apply_until_settled(&ws, &ctx).await;

    assert!(fake.builds.lock().unwrap().is_empty(), "an indexed profile must not be rebuilt");
    assert_eq!(
        std::fs::read_link(kloudlite_agent::nix::profile_path(&ctx.profiles_dir, "ws-1")).unwrap(),
        store,
        "the workspace's own link points at the shared store path"
    );
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(packages_condition(&st)["status"], "True", "ready on the cached profile: {st}");
    assert_eq!(st["status"]["packages"]["observedHash"], hash, "so the per-workspace skip hits next pass");
}

/// The lock's bytes come from the cache and the expression takes the path verbatim: the pinned
/// nixpkgs has some other version of nodejs, and evaluating `pkgs.nodejs` would install that one.
#[tokio::test]
async fn a_locked_package_is_copied_from_the_cache_and_built_from_its_store_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    apply_until_settled(&locked_workspace(LOCKED_PATH), &ctx).await;

    assert_eq!(fake.copies.lock().unwrap().as_slice(), [LOCKED_PATH.to_string()]);
    let builds = fake.builds.lock().unwrap().clone();
    assert!(builds[0].contains(&format!("(builtins.storePath \"{LOCKED_PATH}\")")), "{builds:?}");
    assert!(!builds[0].contains("pkgs.nodejs@20"), "the raw entry must never reach nix: {builds:?}");

    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(packages_condition(&st)["status"], "True", "{st}");
    assert_eq!(
        st["status"]["packages"]["locked"],
        serde_json::json!([{"entry": "nodejs@20", "version": "20.20.2", "rev": "a".repeat(40)}]),
        "the observed half of the lock: {st}"
    );
}

/// A mirror lock names a revision, not a path. It is evaluated to one and then copied like any
/// other lock — leaving the `getFlake` in the build expression would let nix build whatever it
/// found in that nixpkgs from source, which is the whole thing this design refuses.
#[tokio::test]
async fn a_mirror_lock_is_evaluated_to_a_path_copied_and_then_substituted() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, fake) = ws_ctx_with_nix(tmp.path());
    apply_until_settled(&locked_workspace(""), &ctx).await;

    assert_eq!(fake.evals.lock().unwrap().as_slice(), [("a".repeat(40), "nodejs_20".to_string())]);
    assert_eq!(fake.copies.lock().unwrap().as_slice(), [MIRROR_PATH.to_string()], "the evaluated path is pulled");
    let builds = fake.builds.lock().unwrap().clone();
    assert!(builds[0].contains(&format!("(builtins.storePath \"{MIRROR_PATH}\")")), "{builds:?}");
    assert!(!builds[0].contains("getFlake \"github:NixOS/nixpkgs/aaaa"), "no second nixpkgs in the build: {builds:?}");
}

/// A lock outlives the entry that made it. Dropping `nodejs@20` from the list must drop nodejs,
/// not keep installing it from a lock nobody asked for any more.
#[tokio::test]
async fn a_lock_for_an_entry_no_longer_in_the_list_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    let mut w = locked_workspace(LOCKED_PATH);
    w.spec.packages = vec!["hello".into()]; // the pin is gone; its lock is not
    apply_until_settled(&w, &ctx).await;

    assert!(fake.copies.lock().unwrap().is_empty(), "a stale lock is not pulled");
    let builds = fake.builds.lock().unwrap().clone();
    assert!(!builds[0].contains("storePath"), "nor built: {builds:?}");
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    assert_eq!(st["status"]["packages"]["observedHash"], kloudlite_workspaces::packages::hash(
        &kloudlite_agent::nix::nixpkgs_pin(&test_settings()), &with_base(&["hello".into()]), &[]),
        "the hash is the one a workspace that never had the lock would have: {st}");
    assert!(st["status"]["packages"]["locked"].is_null(), "and status does not report it: {st}");
}

/// There is no source-build fallback for a locked path: a path the cache lacks is a sentence to
/// the person, under its own reason, and nix is never asked to build anything.
#[tokio::test]
async fn a_lock_the_cache_does_not_have_is_reported_and_never_built() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    *fake.copy_answer.lock().unwrap() = Err("NotCached: path does not exist".into());
    apply_until_settled(&locked_workspace(LOCKED_PATH), &ctx).await;

    assert!(fake.builds.lock().unwrap().is_empty(), "the copy failed, so nothing is built");
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap().clone();
    let c = packages_condition(&st);
    assert_eq!(c["status"], "False");
    assert_eq!(c["reason"], "NotCached", "{st}");
    let msg = c["message"].as_str().unwrap();
    assert!(msg.contains("nodejs@20") && msg.contains("20.20.2"), "names the entry and version: {msg}");
}

/// Nothing but a spec edit can make the cache hold that path, so retrying on a timer is load for
/// nothing — the workspace waits for a change, as an unresolved entry does.
#[tokio::test]
async fn a_not_cached_lock_waits_for_a_spec_edit_rather_than_retrying() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, fake) = ws_ctx_with_nix(tmp.path());
    *fake.copy_answer.lock().unwrap() = Err("NotCached: path does not exist".into());
    let action = apply_until_settled(&locked_workspace(LOCKED_PATH), &ctx).await;
    assert_eq!(action, kube::runtime::controller::Action::await_change());
}

/// Only `/v1` resolves a version — the agent has no internet. An `@` entry that arrived without
/// its lock (a restored backup, a `kubectl edit`) waits rather than guessing.
#[tokio::test]
async fn a_pinned_entry_with_no_lock_is_unresolved_not_built() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    apply_until_settled(&ready_workspace("ws-1", vec!["nodejs@20".into()]), &ctx).await;

    assert!(fake.builds.lock().unwrap().is_empty());
    assert!(fake.copies.lock().unwrap().is_empty());
    let c = packages_condition(&rec.sent("PATCH", WS_STATUS).last().unwrap().clone());
    assert_eq!(c["reason"], "Unresolved");
}

/// A pin in the platform's BASE list is nobody's spec edit to fix: `/v1` locks `spec.packages`
/// and nothing else, so blaming the workspace's own list would send a person hunting through
/// entries that are all fine.
#[tokio::test]
async fn a_pinned_base_entry_is_the_operators_error_not_an_unresolved_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, fake) = ws_ctx_with_nix(tmp.path());
    let mut s = (*ctx.settings.load()).clone();
    s.base_packages = "nodejs@20".into();
    ctx.settings.store(s);

    apply_until_settled(&ready_workspace("ws-1", vec!["jq".into()]), &ctx).await;

    assert!(fake.builds.lock().unwrap().is_empty());
    let c = packages_condition(&rec.sent("PATCH", WS_STATUS).last().unwrap().clone());
    assert_eq!(c["reason"], "BuildFailed", "{c}");
    assert_ne!(c["reason"], "Unresolved");
    assert!(c["message"].as_str().unwrap().starts_with("base packages: "), "{c}");
}

/// A dangling entry must not short-circuit the build, or the pod gets a profile with no bin.
#[tokio::test]
async fn an_index_entry_pointing_at_nothing_still_builds() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec, fake) = ws_ctx_with_nix(tmp.path());
    let pin = kloudlite_agent::nix::nixpkgs_pin(&test_settings());
    let hash = kloudlite_workspaces::packages::hash(&pin, &with_base(&["hello".into()]), &[]);
    kloudlite_agent::nix::record_index(&ctx.profiles_dir, &hash, &ctx.profiles_dir.join("gone")).unwrap();

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

    let pin = kloudlite_agent::nix::nixpkgs_pin(&test_settings());
    let hash = kloudlite_workspaces::packages::hash(&pin, &with_base(&["hello".into()]), &[]);
    assert_eq!(
        kloudlite_agent::nix::indexed(&ctx.profiles_dir, &hash),
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
/// available in this test environment, so this is the one snapshot-model path exercisable here: the
/// `Engine::checkout` call, without ever shelling out, because `dst.exists()` is checked first.
///
/// IMPLICITLY GATED: pre-created directories stand in for subvolumes, so no `btrfs` binary is
/// invoked. A change that makes the engine actually shell out will pass here and fail on a node.
#[tokio::test]
async fn snapshot_model_checkout_converges_on_an_existing_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/ws-1/live/ws-1")).unwrap();
    let mut routes = ssh_routes();
    routes.push(Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![]) });
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
/// `WORKTREE_EXISTS` without ever shelling to real btrfs — and the same zero-snapshot volume, so
/// the `HeadUnknown` gate never engages. Reaching the Namespace `ensure` call (the very next thing
/// `run_environment` does) is the proof the checkout arm did not block the pass.
///
/// IMPLICITLY GATED: pre-created directories stand in for subvolumes, so no `btrfs` binary is
/// invoked. A change that makes the engine actually shell out will pass here and fail on a node.
#[tokio::test]
async fn snapshot_model_environment_bootstrap_materializes_its_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/env-1/live/env-1")).unwrap();
    let routes = vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![]) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    // The pass runs past the Namespace `ensure` call and then fails on the next unmocked route
    // (NetworkPolicy, RoleBinding, ...) — that error is not the point; the point is that the
    // checkout arm let it get this far at all instead of parking at `HeadUnknown` or a btrfs error.
    let _ = kloudlite_agent::controller::apply_environment(&e, &ctx).await;

    assert!(
        rec.calls().iter().any(|c| c.starts_with("PATCH") && c.contains("/namespaces/env-1")),
        "the worktree materialized and the pass reached namespace reconciliation: {:?}", rec.calls()
    );
}

/// The hidden per-owner builder environment gets the one ingress hole that admits the gate to its
/// buildkit service. Same "run far enough, check the call landed" shape as the bootstrap test
/// above — the pass fails on the next unmocked route, which is not the point.
#[tokio::test]
async fn a_builder_environment_opens_ingress_to_the_gate() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/env-1/live/env-1")).unwrap();
    let mut ej = env_json(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));
    ej["spec"]["system"] = serde_json::json!("builder");
    let routes = vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![]) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "Namespace"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/default-deny", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-dns", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-internet-egress", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-same-namespace", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-builder-gate", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let e: crd::Environment = serde_json::from_value(ej).unwrap();

    let _ = kloudlite_agent::controller::apply_environment(&e, &ctx).await;

    assert!(
        rec.calls().iter().any(|c| c == &format!(
            "PATCH /apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-builder-gate",
            crd::env_namespace("env-1")
        )),
        "the builder environment must open the gate's one ingress hole: {:?}", rec.calls()
    );
}

/// An ordinary environment has no buildkit service, so it gets no ingress hole for the gate.
#[tokio::test]
async fn an_ordinary_environment_opens_no_ingress_to_the_gate() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("vol/env-1/live/env-1")).unwrap();
    let routes = vec![
        Route { method: "PATCH", path: ENV_PATCH.into(), status: 200, body: env_json(serde_json::json!({})) },
        kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/env-1", env_vol()),
        Route { method: "GET", path: SNAPSHOTS_LIST.into(), status: 200, body: snapshot_list_of("Snapshot", vec![]) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "Namespace"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/default-deny", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-dns", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-internet-egress", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-same-namespace", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
        Route { method: "PATCH", path: format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{}/rolebindings/api-secrets", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "RoleBinding"}) },
        Route { method: "PATCH", path: format!("/api/v1/namespaces/{}/limitranges/slot", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "LimitRange"}) },
    ];
    let (ctx, rec) = ctx(tmp.path(), routes);
    let e = environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    let _ = kloudlite_agent::controller::apply_environment(&e, &ctx).await;

    assert!(
        !rec.calls().iter().any(|c| c.contains("/networkpolicies/allow-builder-gate")),
        "an ordinary environment must open no path to any builder: {:?}", rec.calls()
    );
}
