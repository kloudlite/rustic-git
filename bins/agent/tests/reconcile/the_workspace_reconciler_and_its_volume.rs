//! the workspace reconciler and its volume child.

use super::*;


/// The stuck pod, as a test: a workspace whose disk does not exist yet must not get a pod. The
/// symptom this fixes was a pod wedged forever on `path … does not exist`, because the workspace
/// reconciler never looked at its volume's status.
#[tokio::test]
async fn a_workspace_with_an_unready_volume_creates_no_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "working", "subvolumePresent": false}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-1", vol),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
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
            kloudlite_workspaces::kube_test::not_found("/apis/kloudlite.io/v1alpha1/volumes/ws-1"),
            kloudlite_workspaces::kube_test::post(
                "/apis/kloudlite.io/v1alpha1/volumes",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "ws-1"},
                                   "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let sent = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/volumes");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["spec"]["nodeName"], "node-a", "the Volume is created FROM status.nodeName");
    assert_eq!(sent[0]["spec"]["quotaGb"], 20);
    let refs = sent[0]["metadata"]["ownerReferences"].as_array().expect("an ownerReference");
    assert_eq!(refs[0]["kind"], "Workspace");
    assert_eq!(refs[0]["name"], "ws-1");
    assert_eq!(refs[0]["controller"], true);
}

/// Two up-to-date nodes race for a released volume: the CAS picks one, and the LOSER has to
/// notice. Today it writes `Degraded/NodeMismatch` and sits in `error` forever, holding a
/// `status.nodeName` that contradicts the volume — so the winner's own sweep never sees it as
/// unplaced and nothing ever starts it. Clearing my own claim is the whole fix.
#[tokio::test]
async fn a_mismatch_against_a_live_owner_un_places_me() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                "/apis/kloudlite.io/v1alpha1/volumes/ws-1",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "ws-1", "uid": "uid-1"},
                                   "spec": {"owner": "alice", "nodeName": "node-b", "region": "r1", "quotaGb": 20}}),
            ),
            kloudlite_workspaces::kube_test::get(
                "/api/v1/nodes/node-b",
                serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "node-b"},
                                   "status": {"conditions": [{"type": "Ready", "status": "True",
                                                              "lastTransitionTime": rfc3339_ago(60)}]}}),
            ),
            // A guarded write is a PUT of the whole status subresource — never a forced PATCH,
            // which would let this un-place race and silently clobber node-b's own claim.
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({
        "phase": "creating", "nodeName": "node-a", "volumeRef": "ws-1",
        "head": "ws-1-aaaaaaaa", "conditions": [{"type": "PackagesReady", "status": "True", "reason": "Built",
                                                  "message": "ok", "lastTransitionTime": "2026-08-27T00:00:00Z"}],
    }));

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"]["nodeName"], "", "I un-place myself so the real owner reclaims it");
    assert_eq!(sent[0]["status"]["volumeRef"], "ws-1", "the guarded write merges onto the fetched status, not a bare patch");
    assert_eq!(sent[0]["status"]["head"], "ws-1-aaaaaaaa", "unrelated status fields ride along untouched");
    assert!(
        sent[0]["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "PackagesReady"),
        "kept conditions survive alongside the new NodeMismatch one: {:?}", sent[0]["status"]["conditions"]
    );
    assert_ne!(action, kube::runtime::controller::Action::await_change(), "and come back rather than awaiting a change I just caused");
}

/// When the owner is DEAD the arm stays exactly as it was: refuse and wait. Un-placing here would
/// be a second thing allowed to release a volume, and the per-volume sweep is the only one — it
/// is the thing that knows whether a running sibling still pins it.
#[tokio::test]
async fn a_mismatch_against_a_dead_owner_still_refuses_and_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get(
                "/apis/kloudlite.io/v1alpha1/volumes/ws-1",
                serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "ws-1", "uid": "uid-1"},
                                   "spec": {"owner": "alice", "nodeName": "node-b", "region": "r1", "quotaGb": 20}}),
            ),
            kloudlite_workspaces::kube_test::get(
                "/api/v1/nodes/node-b",
                serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "node-b"},
                                   "status": {"conditions": [{"type": "Ready", "status": "False",
                                                              "lastTransitionTime": rfc3339_ago(2000)}]}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a", "volumeRef": "ws-1"}));

    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", WS_STATUS);
    assert_eq!(sent[0]["status"]["nodeName"], "node-a", "a dead owner's volume is the sweep's to release, not mine");
    assert_eq!(sent[0]["status"]["conditions"][0]["reason"], "NodeMismatch");
}

/// `heal_labels` is what makes an object written by any other path (a restored backup, kubectl)
/// listable: the labels are a view of `spec.owner`, and a reconcile re-stamps them from it. Seeded
/// with a label naming the wrong owner, the FIRST thing the pass does is patch it back.
#[tokio::test]
async fn a_wrong_owner_label_is_re_stamped_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    const WS: &str = "/apis/kloudlite.io/v1alpha1/workspaces/ws-1";
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: WS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let mut j = ws_json(serde_json::json!({"phase": "stopped", "nodeName": "node-a"}));
    j["metadata"]["labels"]["kloudlite.io/owner"] = serde_json::json!("mallory");
    j["spec"]["desiredState"] = serde_json::json!("stopped");
    let w: crd::Workspace = serde_json::from_value(j).unwrap();

    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", WS);
    assert_eq!(sent.len(), 1, "one label patch: {:?}", rec.calls());
    assert_eq!(sent[0]["metadata"]["labels"]["kloudlite.io/owner"], "alice", "{}", sent[0]);
    assert_eq!(sent[0]["metadata"]["labels"]["kloudlite.io/kind"], "workspace");
    assert!(sent[0].get("spec").is_none(), "labels only — a controller never writes spec: {}", sent[0]);
    // After the one self-dead read every reconcile now makes (`controller::i_am_dead`), and
    // before anything else.
    assert_eq!(rec.calls()[1], format!("PATCH {WS}"), "healed before anything else: {:?}", rec.calls());
}

/// `ATTACHED_ENV_LABEL` is `spec.attachedEnvironment`'s listing view (`delete_env`'s sweep selects
/// on it, since a teammate's workspace may be attached to an environment it does not own — an
/// owner label cannot stand in for it). A reconcile re-stamps it from spec exactly as it does the
/// owner label.
#[tokio::test]
async fn a_stale_attached_env_label_is_re_stamped_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    const WS: &str = "/apis/kloudlite.io/v1alpha1/workspaces/ws-1";
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PATCH", path: WS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let mut j = ws_json(serde_json::json!({"phase": "stopped", "nodeName": "node-a"}));
    j["spec"]["desiredState"] = serde_json::json!("stopped");
    j["spec"]["attachedEnvironment"] = serde_json::json!("env-1");
    j["metadata"]["labels"]["kloudlite.io/attached-environment"] = serde_json::json!("env-stale");
    let w: crd::Workspace = serde_json::from_value(j).unwrap();

    kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", WS);
    assert_eq!(sent.len(), 1, "owner/kind/team already match spec, only the attached-env label heals: {:?}", rec.calls());
    assert_eq!(sent[0]["metadata"]["labels"]["kloudlite.io/attached-environment"], "env-1", "{}", sent[0]);
    assert!(sent[0].get("spec").is_none(), "labels only — a controller never writes spec: {}", sent[0]);
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

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
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
    use kloudlite_workspaces::{crd, k8s};
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
        locks: vec![],
        attached_environment: None,
    };
    let source = spec.storage.as_ref().unwrap().source.as_ref().unwrap();
    let init = k8s::git_init_container(source, "alpine/git:2.45.2", "git.example.com", "22")
        .expect("a valid repo is accepted")
        .expect("a gitRepo source seeds with an init container");
    let pod = k8s::workspace_pod(&spec, "ws-1", "ws-1", &test_pod_ctx(), Some(init)).unwrap();

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
    // ssh refuses root's own key at the volume's 0444, so the seeder clones with a 0600 copy it
    // installs first; the key path it reads is the mounted one, the one it uses is the copy.
    let cmd = inits[0].command.as_ref().unwrap().join(" ");
    assert!(cmd.contains(&format!("install -m 600 {}/id_ed25519", k8s::USER_KEY_PATH)), "{cmd}");
    assert!(cmd.contains("GIT_SSH_COMMAND=\"ssh -i /tmp/seed_key"), "{cmd}");
    assert!(!env.contains_key("GIT_SSH_COMMAND"), "the env would point ssh at the refused 0444 file");
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

pub(crate) fn test_pod_ctx() -> kloudlite_workspaces::k8s::PodContext<'static> {
    kloudlite_workspaces::k8s::PodContext {
        default_image: "ghcr.io/kloudlite/kloudlite-workspace:deadbeef",
        pool: "/pool",
        node_name: "node-a",
        owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "kloudlite.io/v1alpha1".into(),
            kind: "Workspace".into(),
            name: "ws-1".into(),
            uid: "ws-uid-1".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        },
        runtime_class: None,
        system: None,
        registry_host: "registry.kloudlite.io",
    }
}

/// The last gate before a repo name becomes an ssh argv. A `--branch -upload-pack=…` or an
/// `owner/name` that is neither is arbitrary command execution on the workspace pod, so it fails
/// PERMANENTLY and no pod is started for it.
#[tokio::test]
async fn a_workspace_whose_source_repo_is_not_a_name_gets_no_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20,
                 "source": {"gitRepo": {"repo": "https://evil.example.com/x", "branch": "main"}}},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    // The snapshot-model checkout step runs before this gate is ever reached; pre-seed an empty
    // worktree so it converges on `WORKTREE_EXISTS` instead of shelling to a real `btrfs`.
    std::fs::create_dir_all(tmp.path().join("vol/ws-1/live/ws-1")).unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-1", vol),
            ready_binding(),
            ready_namespace(),
            kloudlite_workspaces::kube_test::not_found(WS_SSH_SECRET),
            kloudlite_workspaces::kube_test::post(
                "/api/v1/namespaces/ws-alice/secrets",
                serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "ws-ssh-ws-1"}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    // The profile is built first on every pass, so the source is judged on the pass after it.
    let _ = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    wait_idle(&ctx).await;
    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
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
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "error", "subvolumePresent": false,
                   "conditions": [{"type": "Ready", "status": "False", "reason": "NoSpace",
                                   "message": "the pool is full", "lastTransitionTime": "2026-08-27T00:00:00Z"}]}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/volumes/ws-1", vol),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"phase": "creating", "nodeName": "node-a"}));

    let action = kloudlite_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
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
        Some(std::collections::BTreeMap::from([("kloudlite.io/push-requested".to_string(), "2026-08-27T00:00:00Z".to_string())]));
    // Already observed: with the push branch gone there is nothing left for this pass to do.
    v.status = Some(crd::VolumeStatus { phase: crd::Phase::Ready, observed_generation: Some(1), subvolume_present: true, ..Default::default() });

    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "the annotation is dead weight now");
    assert!(ctx.running.lock().unwrap().is_empty(), "and nothing was started");
}


// Fixture helpers still used by surviving tests below (the object-store push/stop tests that
// used to sit here are deleted — Task 8).

/// A stopping environment with one service and its own volume, on this node.
pub(crate) fn stopping_env() -> crd::Environment {
    let mut o = env_json(serde_json::json!({"phase": "running", "nodeName": "node-a"}));
    o["spec"]["desiredState"] = serde_json::json!("stopped");
    o["spec"]["services"] =
        serde_json::json!([{"name": "db", "image": "mongo", "command": [], "env": {}, "mounts": []}]);
    serde_json::from_value(o).unwrap()
}

pub(crate) fn env_vol() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "env-1", "uid": "env-vol-1"},
        "spec": {"owner": "acme", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true},
    })
}

pub(crate) fn stop_snapshot(status: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
        // `creationTimestamp` is what the whole-wait bound measures from — an hour ago, so a test
        // that wants the bound to bite only has to set the timeout, and one that does not is
        // unaffected because it never waits on the bound at all.
        "metadata": {"name": "stop-env-1-1", "uid": "stop-uid-1", "creationTimestamp": rfc3339_ago(3600)},
        "spec": {"volume": "env-1", "owner": "acme", "worktree": "env-1", "transient": true},
        "status": status,
    })
}

pub(crate) fn stopping_ws() -> crd::Workspace {
    let mut o = ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a",
                                            "volumeRef": "ws-1", "podRef": "ws-alice/ws-1"}));
    o["spec"]["desiredState"] = serde_json::json!("stopped");
    serde_json::from_value(o).unwrap()
}

pub(crate) fn ws_stop_routes() -> Vec<Route> {
    vec![Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) }]
}
