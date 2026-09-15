//! A person's space follows one environment (`controller::space`): the resolver, the converge
//! every pod of the space runs, the legacy per-pod grant it collects, and the unknown-cache rule.

use super::*;

const WS_EGRESS: &str = "/apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/space-env";

fn env_ingress(env: &str) -> String {
    format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/space-ws-alice", crd::env_namespace(env))
}

fn np(path: String, name: &str) -> Route {
    kloudlite_workspaces::kube_test::patch(path, serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": name}}))
}

/// The workspace-side objects a space adds on top of `ws_ctx_with_ssh`'s: the host key and both
/// halves of the grant for `env-abc` and `env-def`, answered with themselves.
pub(crate) fn attach_routes() -> Vec<Route> {
    let mut r = ssh_routes();
    r.push(np(WS_EGRESS.into(), "space-env"));
    r.push(np(env_ingress("env-abc"), "space-ws-alice"));
    r.push(np(env_ingress("env-def"), "space-ws-alice"));
    r
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

/// A choice as `/v1` writes it, with the uid the policies' owner reference needs.
pub(crate) fn space(owner: &str, team: &str, env: &str) -> crd::SpaceEnvironment {
    let mut s = crd::space_environment(owner, team, env);
    s.metadata.uid = Some(format!("space-uid-{owner}-{team}"));
    s
}

pub(crate) fn attached_condition(rec: &Recorder) -> Option<serde_json::Value> {
    let st = rec.sent("PATCH", WS_STATUS).last().expect("a status write").clone();
    st["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Attached").cloned()
}

fn resolv(ctx: &Arc<Ctx>, id: &str) -> String {
    std::fs::read_to_string(kloudlite_workspaces::k8s::attach_file(&ctx.pool, id)).unwrap()
}

/// One choice answers for every pod of the space — any number of workspaces, the bench, a kind
/// added later — and for nobody else: a teammate's space in the same team is its own.
#[tokio::test]
async fn one_choice_answers_for_every_pod_of_the_space_and_no_teammate() {
    use kloudlite_agent::controller::space::{space_environment, SpaceEnv};
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![]);
    ctx.remember_spaces(vec![space("alice", "acme", "env-abc")]);
    let chosen = |owner: &str, team: &str| match space_environment(&ctx, owner, team, None) {
        SpaceEnv::Known(c) => c.map(|c| c.environment),
        SpaceEnv::Unknown => panic!("listed"),
    };
    // A workspace, a second workspace and the bench of alice in acme all ask with (alice, acme).
    assert_eq!(chosen("alice", "acme").as_deref(), Some("env-abc"));
    assert_eq!(chosen("Alice", "acme").as_deref(), Some("env-abc"), "handles fold like ws_namespace");
    assert_eq!(chosen("bob", "acme"), None, "a teammate chooses independently");
    assert_eq!(chosen("alice", ""), None, "the personal space is its own");
}

/// The cache is known and holds no choice: an object still carrying the retired field resolves
/// through it (the migration window). A choice, once written, wins over the field; an unlisted
/// cache is Unknown whatever the field says.
#[tokio::test]
async fn the_retired_field_is_a_fallback_only_while_the_cache_is_known_and_empty() {
    use kloudlite_agent::controller::space::{space_environment, SpaceEnv};
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx_unlisted(tmp.path(), vec![]);
    assert_eq!(space_environment(&ctx, "alice", "", Some("env-old")), SpaceEnv::Unknown);
    ctx.remember_spaces(vec![]);
    match space_environment(&ctx, "alice", "", Some("env-old")) {
        SpaceEnv::Known(Some(c)) => assert!(c.environment == "env-old" && c.space.is_none()),
        other => panic!("{other:?}"),
    }
    ctx.remember_spaces(vec![space("alice", "", "env-new")]);
    match space_environment(&ctx, "alice", "", Some("env-old")) {
        SpaceEnv::Known(Some(c)) => assert_eq!(c.environment, "env-new"),
        other => panic!("{other:?}"),
    }
}

/// The agent renders the pod's `/etc/resolv.conf` and writes the `Attached` condition, and
/// NOTHING else: both halves of the grant belong to `kloudlite-controller` since stage 1 of
/// the cluster-controller split. Two writers across a roll is the one thing that release must
/// never have.
#[tokio::test]
async fn the_agent_writes_no_network_policy_for_a_space() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(env_route("env-abc", "r1"));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    ctx.remember_spaces(vec![space("alice", "", "env-abc")]);
    apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;

    let calls = rec.calls();
    assert!(!calls.iter().any(|c| c.contains("networkpolicies")), "the agent must make no NetworkPolicy call: {calls:?}");
    assert!(resolv(&ctx, "ws-1").contains("env-abc.svc."), "{}", resolv(&ctx, "ws-1"));
    let cond = attached_condition(&rec).expect("Attached");
    assert_eq!((cond["status"].as_str(), cond["reason"].as_str(), cond["message"].as_str()), (Some("True"), Some("Space"), Some("env-abc")));
}

/// A switch rewrites the SAME file (the running pod holds the inode); the grants follow the switch
/// in the controller, never here.
#[tokio::test]
async fn a_switch_rewrites_resolv_conf_in_place() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.extend([env_route("env-abc", "r1"), env_route("env-def", "r1")]);
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    ctx.remember_spaces(vec![space("alice", "", "env-abc")]);
    apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;
    let path = kloudlite_workspaces::k8s::attach_file(&ctx.pool, "ws-1");
    let inode = std::fs::metadata(&path).unwrap().ino();

    ctx.remember_spaces(vec![space("alice", "", "env-def")]);
    apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode, "written in place, never renamed");
    assert!(resolv(&ctx, "ws-1").contains("env-def.svc.") && !resolv(&ctx, "ws-1").contains("env-abc"));
    assert!(!rec.calls().iter().any(|c| c.contains("networkpolicies")), "{:?}", rec.calls());
}

/// The environment's own prune never deletes a `space-*` ingress, whatever the space points at: the
/// controller owns it and deletes it on a transition.
#[tokio::test]
async fn the_environment_never_prunes_a_space_grant() {
    for spaces in [Some("env-other"), Some("env-1"), None] {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![kloudlite_workspaces::kube_test::get(
            "/apis/networking.k8s.io/v1/namespaces/env-1/networkpolicies",
            serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicyList", "metadata": {},
                               "items": [{"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": "space-ws-alice"}}]}),
        )];
        let (ctx, rec) = ctx_unlisted(tmp.path(), routes);
        if let Some(env) = spaces {
            ctx.remember_spaces(vec![space("alice", "", env)]);
        }
        let _ = kloudlite_agent::controller::apply_environment(&environment(serde_json::json!({"phase": "creating", "nodeName": "node-a"})), &ctx).await;
        let did = rec.calls().contains(&"DELETE /apis/networking.k8s.io/v1/namespaces/env-1/networkpolicies/space-ws-alice".to_string());
        assert!(!did, "space -> {spaces:?}: {:?}", rec.calls());
    }
}

/// An unlisted cache touches nothing: no policy written or deleted, an existing resolv.conf left as
/// it is, and the last recorded `Attached` kept — read as "no environment" it would strip DNS from
/// every pod in the region on every agent restart.
#[tokio::test]
async fn an_unlisted_space_cache_rewrites_and_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh_unlisted(tmp.path(), attach_routes());
    let path = kloudlite_workspaces::k8s::attach_file(&ctx.pool, "ws-1");
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    std::fs::write(&path, "search env-abc.svc.cluster.local\n").unwrap();
    let mut w = ready_workspace("ws-1", vec![]);
    w.status.get_or_insert_with(Default::default).conditions = vec![crd::condition(crd::ATTACHED, true, "Space", "env-abc", 1)];
    apply_until_settled(&w, &ctx).await;

    assert!(!rec.calls().iter().any(|c| c.contains("/networkpolicies/space") || c.contains("/networkpolicies/attach-")), "{:?}", rec.calls());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "search env-abc.svc.cluster.local\n");
    assert_eq!(attached_condition(&rec).expect("kept")["message"], "env-abc");
}

/// The legacy per-pod pair an older build wrote is collected once — both halves, by the id its
/// `Attached` condition recorded — and a pass already in the new shape deletes nothing.
#[tokio::test]
async fn the_legacy_per_pod_grant_is_collected_once() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(env_route("env-abc", "r1"));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    ctx.remember_spaces(vec![space("alice", "", "env-abc")]);
    let mut w = ready_workspace("ws-1", vec![]);
    w.status.get_or_insert_with(Default::default).conditions = vec![crd::condition(crd::ATTACHED, true, "Converged", "env-abc", 1)];
    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    let calls = rec.calls();
    for ns in ["ws-alice", "env-abc"] {
        assert!(calls.contains(&format!("DELETE /apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/attach-ws-1")), "{ns}: {calls:?}");
    }

    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), { let mut r = attach_routes(); r.push(env_route("env-abc", "r1")); r });
    ctx.remember_spaces(vec![space("alice", "", "env-abc")]);
    w.status.get_or_insert_with(Default::default).conditions = vec![crd::condition(crd::ATTACHED, true, "Space", "env-abc", 1)];
    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE") && c.contains("networkpolicies")), "{:?}", rec.calls());
}

/// A choice naming a gone environment, or one in another region, reports why and grants nothing.
#[tokio::test]
async fn a_missing_or_cross_region_environment_is_reported_and_grants_nothing() {
    for (route, reason) in [
        (kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/environments/env-abc"), "EnvironmentNotFound"),
        (env_route("env-abc", "other-region"), "RegionMismatch"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut routes = attach_routes();
        routes.push(route);
        let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
        ctx.remember_spaces(vec![space("alice", "", "env-abc")]);
        apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;
        assert!(!rec.calls().iter().any(|c| c.starts_with("PATCH") && c.contains("/networkpolicies/space")), "{:?}", rec.calls());
        assert!(!resolv(&ctx, "ws-1").contains("env-"), "{}", resolv(&ctx, "ws-1"));
        let cond = attached_condition(&rec).expect("reported");
        assert_eq!((cond["status"].as_str(), cond["reason"].as_str()), (Some("False"), Some(reason)));
    }
}

/// No choice and never attached: no condition and no DELETE on every pass (2026-09-12). A cleared
/// choice drops the condition and leaves `space-env` to the controller.
#[tokio::test]
async fn no_choice_reports_nothing_and_a_cleared_choice_deletes_no_space_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), attach_routes());
    kloudlite_agent::controller::apply_workspace(&ready_workspace("ws-1", vec![]), &ctx).await.unwrap();
    assert!(!rec.calls().iter().any(|c| c.contains("networkpolicies/space") || c.contains("networkpolicies/attach")), "{:?}", rec.calls());
    assert!(attached_condition(&rec).is_none());

    let mut w = ready_workspace("ws-1", vec![]);
    w.status.get_or_insert_with(Default::default).conditions = vec![crd::condition(crd::ATTACHED, true, "Space", "env-abc", 1)];
    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(!rec.calls().iter().any(|c| c.contains("networkpolicies")), "{:?}", rec.calls());
    assert!(attached_condition(&rec).is_none(), "the condition goes with the choice");
}

/// A pod whose space points at an environment in another region settles into `RegionMismatch`, and
/// a settled pod issues no DELETE on any later pass (2026-09-12).
#[tokio::test]
async fn a_refused_pod_reconciled_twice_issues_no_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.push(env_route("env-abc", "other-region"));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    ctx.remember_spaces(vec![space("alice", "", "env-abc")]);
    let mut w = ready_workspace("ws-1", vec![]);
    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    let st = rec.sent("PATCH", WS_STATUS).last().unwrap()["status"].clone();
    w.status = Some(serde_json::from_value(st).unwrap());
    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(attached_condition(&rec).unwrap()["reason"], "RegionMismatch");
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE") && c.contains("networkpolicies")), "{:?}", rec.calls());
}

fn legacy_np(ns: &str) -> Route {
    np(format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/attach-ws-1"), "attach-ws-1")
}

/// Under the field fallback a pod manages only its OWN per-pod pair: `space-env` is the whole
/// namespace's, and a sibling on a different field — or a detached one — must never touch it.
#[tokio::test]
async fn the_field_fallback_manages_only_the_pods_own_legacy_pair() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.extend([env_route("env-abc", "r1"), legacy_np("ws-alice"), legacy_np("env-abc"), Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1".into(), status: 200, body: ws_json(serde_json::json!({})) }]);
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    let mut w = ready_workspace("ws-1", vec![]);
    w.spec.attached_environment = Some("env-abc".into());
    apply_until_settled(&w, &ctx).await;
    let calls = rec.calls();
    assert!(calls.contains(&"PATCH /apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/attach-ws-1".to_string()), "{calls:?}");
    assert!(calls.contains(&"PATCH /apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies/attach-ws-1".to_string()), "{calls:?}");
    assert!(!calls.iter().any(|c| c.contains("/space-")), "the fallback never touches the namespace pair: {calls:?}");
    assert_eq!(attached_condition(&rec).unwrap()["reason"], "Converged");

    // A detached sibling carrying an old True condition: its own pair goes, `space-env` stays.
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), attach_routes());
    let mut sib = ready_workspace("ws-1", vec![]);
    sib.status.get_or_insert_with(Default::default).conditions = vec![crd::condition(crd::ATTACHED, true, "Converged", "env-abc", 1)];
    kloudlite_agent::controller::apply_workspace(&sib, &ctx).await.unwrap();
    let calls = rec.calls();
    assert!(calls.contains(&"DELETE /apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/attach-ws-1".to_string()), "{calls:?}");
    assert!(!calls.iter().any(|c| c.contains("space-env")), "{calls:?}");
}

/// A dropped legacy pair is re-applied when the fallback needs it again in the same process:
/// the delete forgets `ensure`'s memory, or the 600 s skip would leave the pair missing.
#[tokio::test]
async fn a_dropped_legacy_pair_is_reapplied_when_needed_again() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = attach_routes();
    routes.extend([env_route("env-abc", "r1"), legacy_np("ws-alice"), legacy_np("env-abc"), Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1".into(), status: 200, body: ws_json(serde_json::json!({})) }]);
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    let egress = "/apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/attach-ws-1";
    let ingress = "/apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies/attach-ws-1";
    let mut w = ready_workspace("ws-1", vec![]);
    w.spec.attached_environment = Some("env-abc".into());
    apply_until_settled(&w, &ctx).await;
    let (e1, i1) = (rec.sent("PATCH", egress).len(), rec.sent("PATCH", ingress).len());
    assert!(e1 > 0 && i1 > 0, "{:?}", rec.calls());

    let mut detached = ready_workspace("ws-1", vec![]);
    detached.status.get_or_insert_with(Default::default).conditions = vec![crd::condition(crd::ATTACHED, true, "Converged", "env-abc", 1)];
    kloudlite_agent::controller::apply_workspace(&detached, &ctx).await.unwrap();
    assert!(rec.calls().contains(&format!("DELETE {egress}")), "{:?}", rec.calls());

    apply_until_settled(&w, &ctx).await;
    assert!(rec.sent("PATCH", egress).len() > e1, "egress re-applied: {:?}", rec.calls());
    assert!(rec.sent("PATCH", ingress).len() > i1, "ingress re-applied: {:?}", rec.calls());
}

/// Once the migration has settled an object, its still-set field is ignored: a choice the person
/// cleared cannot come back through a clear that failed or was deferred.
#[tokio::test]
async fn a_settled_field_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), { let mut r = attach_routes(); r.push(Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-1".into(), status: 200, body: ws_json(serde_json::json!({})) }); r });
    let mut w = ready_workspace("ws-1", vec![]);
    w.spec.attached_environment = Some("env-abc".into());
    w.metadata.annotations = Some([(crd::SPACE_MIGRATED_ANNOTATION.to_string(), "true".to_string())].into());
    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(!rec.calls().iter().any(|c| c.contains("networkpolicies")), "{:?}", rec.calls());
    assert!(attached_condition(&rec).is_none());
}

pub(crate) fn workspace_pod_json(volumes: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws-1"},
        "spec": {"volumes": volumes},
        "status": {"conditions": [{"type": "Ready", "status": "True",
                                   "lastTransitionTime": "2026-08-30T00:00:00Z"}]},
    })
}

/// A pod created before the resolv.conf mount existed resolves nothing, so it does not report
/// `Attached=True`; one that carries the mount does.
#[tokio::test]
async fn attached_is_reported_only_for_a_pod_that_carries_the_mount() {
    for (volumes, status) in [
        (serde_json::json!([{"name": "home", "persistentVolumeClaim": {"claimName": "home"}}]), "False"),
        (serde_json::json!([{"name": "attach", "hostPath": {"path": "/pool/attach/ws-1/resolv.conf", "type": "File"}}]), "True"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut routes = attach_routes();
        routes.push(env_route("env-abc", "r1"));
        routes.push(kloudlite_workspaces::kube_test::get("/api/v1/namespaces/ws-alice/pods/ws-1", workspace_pod_json(volumes)));
        let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
        ctx.remember_spaces(vec![space("alice", "", "env-abc")]);
        apply_until_settled(&ready_workspace("ws-1", vec![]), &ctx).await;
        let cond = attached_condition(&rec).expect("Attached");
        assert_eq!(cond["status"], status, "{cond}");
        if status == "False" {
            assert_eq!(cond["reason"], "PodPredatesAttachment");
        }
    }
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
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-otlp", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
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
        Route { method: "PATCH", path: format!("/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/allow-otlp", crd::env_namespace("env-1")), status: 200, body: serde_json::json!({"kind": "NetworkPolicy"}) },
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
