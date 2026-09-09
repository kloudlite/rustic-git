//! The tests for every builder in `k8s`: one file on purpose, since most of them share the
//! `ctx`/`svc`/`ws_spec` fixtures and assert on the same rendered objects from different angles.

use super::*;
use crate::crd::DesiredState;
use crate::model::Mount;

const AGENT_RESOLV: &str = "search kube-system.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n";

/// Unattached: the workspace's own namespace leads, and everything the agent's file said about
/// nameserver, ndots and the node's suffix is carried through untouched.
#[test]
fn an_unattached_resolv_conf_is_what_kubelet_would_have_written() {
    let got = resolv_conf(AGENT_RESOLV, "ws-acme", None);
    assert_eq!(
        got,
        "search ws-acme.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
    );
}

/// Attached: the environment's namespace goes FIRST, so a name the environment defines wins
/// over one in the workspace's own namespace.
#[test]
fn an_attached_resolv_conf_searches_the_environment_first() {
    let got = resolv_conf(AGENT_RESOLV, "ws-acme", Some("env-abc"));
    assert_eq!(
        got,
        "search env-abc.svc.cluster.local ws-acme.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
    );
}

/// A cluster started with a non-default `--cluster-domain` must not get `cluster.local`
/// search entries — the domain is derived from the template, never assumed.
#[test]
fn a_non_default_cluster_domain_is_derived_from_the_template() {
    let template = "search kube-system.svc.cluster.internal svc.cluster.internal cluster.internal node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n";
    let got = resolv_conf(template, "ws-acme", Some("env-abc"));
    assert_eq!(
        got,
        "search env-abc.svc.cluster.internal ws-acme.svc.cluster.internal svc.cluster.internal cluster.internal node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
    );
}

/// A template with no search line at all still yields a usable file rather than a malformed one.
#[test]
fn a_template_without_a_search_line_gains_one() {
    let got = resolv_conf("nameserver 10.43.0.10\n", "ws-acme", Some("env-abc"));
    assert_eq!(
        got,
        "search env-abc.svc.cluster.local ws-acme.svc.cluster.local svc.cluster.local cluster.local\nnameserver 10.43.0.10\n"
    );
}

fn owner_ref() -> OwnerReference {
    OwnerReference {
        api_version: "kloudlite.io/v1alpha1".into(),
        kind: "Volume".into(),
        name: "vol-1".into(),
        uid: "uid-1".into(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Storage is mounted from the node, not claimed. Every source carries an explicit `type`: an
/// untyped hostPath creates a missing path as an empty directory, which is a wiped workspace
/// rather than a failed mount.
#[test]
fn every_volume_is_a_typed_host_path() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    assert!(
        vols.iter().all(|v| v.persistent_volume_claim.is_none()),
        "no pod claims a PVC any more"
    );
    for v in vols.iter().filter(|v| v.host_path.is_some()) {
        let h = v.host_path.as_ref().unwrap();
        // `DirectoryOrCreate` passes a presence check just as well as `Directory` does, and is
        // exactly the value this test exists to catch: it creates a missing path as an empty
        // directory, which is a silently wiped workspace on the wrong node.
        let want = if v.name == "attach" { "File" } else { "Directory" };
        assert_eq!(h.type_.as_deref(), Some(want), "hostPath {:?} must be typed {want}", v.name);
        assert!(h.path.starts_with('/'), "hostPath {:?} must be absolute", v.name);
    }
}

/// The pod's three hostPath mounts point at the paths the agent actually manages on disk.
/// `ready` has to mean a person can get in. The default image is gated on sshd accepting a
/// connection; a user's own image, whose entrypoint we do not know, is not gated at all rather
/// than held NotReady forever behind a port it may never open.
#[test]
fn the_default_image_is_ready_only_once_sshd_listens() {
    let mut spec = ws_spec();
    spec.image = crate::model::DEFAULT_WS_IMAGE.into();
    let p = workspace_pod(&spec, "ws-1", "ws-1", &ctx(), None).unwrap();
    let c = &p.spec.unwrap().containers[0];
    let probe = c.readiness_probe.as_ref().expect("the default image carries a readiness probe");
    assert_eq!(probe.tcp_socket.as_ref().unwrap().port, IntOrString::Int(22));

    let mut own = ws_spec();
    own.image = "ghcr.io/acme/devbox:1".into();
    let p = workspace_pod(&own, "ws-2", "ws-2", &ctx(), None).unwrap();
    assert!(p.spec.unwrap().containers[0].readiness_probe.is_none(), "an unknown image is not gated on a port it may never open");
}

#[test]
fn a_workspace_pods_host_paths_match_the_agents_layout() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    let path = |n: &str| {
        vols.iter().find(|v| v.name == n).unwrap_or_else(|| panic!("no {n} volume"))
            .host_path.as_ref().unwrap().path.clone()
    };
    assert_eq!(path("live"), format!("{}/vol/ws-1/live/ws-1", ctx().pool));
    assert_eq!(path("nix"), NIX_ROOT);
    assert_eq!(path("attach"), attach_file(ctx().pool, "ws-1"));
}

/// The `live` mount is the WORKTREE path, not the old single-subvolume one — and
/// `id` (volumeRef) vs `ws_id` (this workspace's own id) matter: a shared-volume clone's
/// worktree lives under the SOURCE volume's `live/`, named by the clone's own id.
#[test]
fn a_workspace_pods_live_mount_is_the_worktree_path() {
    let p = workspace_pod(&ws_spec(), "vol-1", "ws-1", &ctx(), None).unwrap();
    let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    let live = vols.iter().find(|v| v.name == "live").unwrap();
    assert_eq!(live.host_path.as_ref().unwrap().path, format!("{}/vol/vol-1/live/ws-1", ctx().pool));
    assert_eq!(live.host_path.as_ref().unwrap().type_.as_deref(), Some("Directory"));
}

/// Placement is the pod's own now that no PV carries node affinity, and it is ADDED to the
/// pool selector rather than replacing it.
#[test]
fn the_pod_selects_its_node_by_hostname() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let s = p.spec.unwrap();
    let sel = s.node_selector.expect("a node selector");
    assert_eq!(sel.get("kubernetes.io/hostname").map(String::as_str), Some("session-0"));
    assert_eq!(sel.get("kloudlite.io/pool").map(String::as_str), Some("true"));
    assert!(s.node_name.is_none(), "the scheduler still places the pod");
}

/// The env pod gets the same placement fix as the workspace pod: `service_statefulset` has no
/// PV to carry `nodeAffinity` any more either.
#[test]
fn the_service_pod_selects_its_node_by_hostname() {
    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx()).unwrap();
    let s = d.spec.unwrap().template.spec.unwrap();
    let sel = s.node_selector.expect("a node selector");
    assert_eq!(sel.get("kubernetes.io/hostname").map(String::as_str), Some("session-0"));
    assert_eq!(sel.get("kloudlite.io/pool").map(String::as_str), Some("true"));
    assert!(s.node_name.is_none(), "the scheduler still places the pod");
}

#[test]
fn a_pod_is_pinned_to_its_node_and_to_the_pool_and_nothing_else() {
    let mut spec = PodSpec::default();
    placement(&mut spec, "node-a");
    let sel = spec.node_selector.unwrap();
    assert_eq!(sel.len(), 2);
    assert_eq!(sel["kloudlite.io/pool"], "true");
    assert_eq!(sel["kubernetes.io/hostname"], "node-a");
    let tol = &spec.tolerations.unwrap()[0];
    assert_eq!(tol.key.as_deref(), Some("kloudlite.io/pool"));
    assert_eq!(spec.automount_service_account_token, Some(false));
}

/// An environment's worktree is its OWN id under whatever volume it resolved to — volume root,
/// environment leaf. For one that owns its volume the two are the same string, so the path is
/// unchanged from the workspace pod's worktree-path mount.
#[test]
fn the_service_pods_live_mount_is_the_worktree_path() {
    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx()).unwrap();
    let vols = d.spec.unwrap().template.spec.unwrap().volumes.unwrap();
    let live = vols.iter().find(|v| v.name == "live").unwrap();
    assert_eq!(live.host_path.as_ref().unwrap().path, format!("{}/vol/env-1/live/env-1", ctx().pool));
}

/// A RESTORED environment holds a SECOND worktree of the SOURCE's volume: the root comes from
/// the source, the leaf from itself, and its objects live in its own namespace. Mounting
/// `(source, source)` — what this did before the restored-environment fix — pointed two environments at one live
/// subvolume.
#[test]
fn a_restored_environments_live_mount_is_its_own_worktree_of_the_source_volume() {
    let d = service_statefulset(&svc("data", "/data"), "env-restored", "env-src", "team", &ctx()).unwrap();
    assert_eq!(
        d.metadata.namespace.as_deref(),
        Some(crate::crd::env_namespace("env-restored").as_str()),
        "its own namespace, never the source's"
    );
    let vols = d.spec.unwrap().template.spec.unwrap().volumes.unwrap();
    let live = vols.iter().find(|v| v.name == "live").unwrap();
    assert_eq!(live.host_path.as_ref().unwrap().path, format!("{}/vol/env-src/live/env-restored", ctx().pool));
}

fn ctx() -> PodContext<'static> {
    PodContext { pool: "/mnt/wspool", node_name: "session-0", owner_ref: owner_ref(), runtime_class: Some("gvisor"), default_image: "ghcr.io/kloudlite/kloudlite-workspace:deadbeef", system: None, registry_host: "registry.kloudlite.io" }
}

/// A service's own `resources` overrides the environment unit; a service with none still gets
/// the unit (`env_unit_resources()`'s 2 vCPU limit), not an empty `ResourceRequirements`.
#[test]
fn a_service_with_its_own_resources_is_rendered_with_them_and_the_unit_otherwise() {
    let mut with_res = svc("data", "/data");
    with_res.resources = Some(PodResources::default());
    let sts = service_statefulset(&with_res, "bld-alice", "bld-alice", "alice", &ctx()).unwrap();
    let c = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(c.resources.as_ref().unwrap().limits.as_ref().unwrap()["cpu"].0, "4");

    let plain = service_statefulset(&svc("data", "/data"), "e", "e", "alice", &ctx()).unwrap();
    let p = &plain.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(p.resources.as_ref().unwrap().limits.as_ref().unwrap()["cpu"].0, "2");
}

/// The namespace ceiling follows the biggest service, so a builder's 4 vCPU pod is admitted
/// where an ordinary environment stays at the unit's 2.
#[test]
fn the_limit_range_ceiling_is_the_largest_service_shape() {
    let mut big = svc("buildkit", "/cache");
    big.resources = Some(PodResources::default());
    assert_eq!(env_limit_resources(&[svc("db", "/data"), big]).cpu_limit, "4");
    assert_eq!(env_limit_resources(&[svc("db", "/data")]).cpu_limit, "2");
    assert_eq!(env_limit_resources(&[]).cpu_limit, "2");
}

/// The spike's ruling, tested: an ordinary service keeps `hardened()`'s narrow list, and a
/// builder-environment service gets root plus gvisor's exact capability set — never the union
/// of the two, and never `hardened()`'s list silently widened.
#[test]
fn a_builder_service_gets_root_and_the_gvisor_capability_list_an_ordinary_one_does_not() {
    let mut builder_ctx = ctx();
    builder_ctx.system = Some(crate::crd::BUILDER_SYSTEM);
    let sts = service_statefulset(&svc("data", "/data"), "bld-alice", "bld-alice", "alice", &builder_ctx).unwrap();
    let sc = sts.spec.unwrap().template.spec.unwrap().containers[0].security_context.clone().unwrap();
    assert_eq!(sc.run_as_user, Some(0));
    let add = sc.capabilities.unwrap().add.unwrap();
    assert_eq!(
        add,
        vec![
            "SYS_ADMIN", "CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "SETUID", "SETGID",
            "SETPCAP", "SETFCAP", "MKNOD", "SYS_CHROOT", "KILL", "NET_BIND_SERVICE", "NET_RAW",
            "AUDIT_WRITE",
        ]
    );

    let ordinary = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "alice", &ctx()).unwrap();
    let sc2 = ordinary.spec.unwrap().template.spec.unwrap().containers[0].security_context.clone().unwrap();
    assert_eq!(sc2.run_as_user, None);
    assert_eq!(
        sc2.capabilities.unwrap().add.unwrap(),
        vec!["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID", "NET_BIND_SERVICE", "SYS_CHROOT"]
    );
}

fn svc(folder: &str, path: &str) -> model::Service {
    model::Service {
        name: "web".into(),
        image: "nginx".into(),
        command: vec![],
        env: Default::default(),
        mounts: vec![Mount { folder: folder.into(), path: path.into() }],
        ports: vec![80],
        resources: None,
    }
}

fn ws_spec() -> WorkspaceSpec {
    WorkspaceSpec {
        team: String::new(),
        owner: "alice".into(),
        name: "dev".into(),
        region: "centralindia".into(),
        image: "nginx:alpine".into(),
        storage: Some(crate::crd::WorkspaceStorage { quota_gb: 10, source: None }),
        desired_state: DesiredState::Running,
        resources: PodResources::default(),
        packages: vec![],
        locks: vec![],
        attached_environment: None,
    }
}

/// Per NAMESPACE, like the home claim: a local PV binds to one claim, but one claim serves
/// every pod in the namespace. The per-workspace part is the subPath, not the object.
#[test]
fn the_attach_paths_are_per_workspace_under_the_pool() {
    assert_eq!(attach_root("/pool"), "/pool/attach");
    assert_eq!(attach_file("/pool", "ws-1"), "/pool/attach/ws-1/resolv.conf");
}

/// The mount is what makes attachment live: the agent rewrites the host file and the running
/// pod sees it. Read-only so the person in the workspace cannot point their own DNS elsewhere.
#[test]
fn a_workspace_pod_mounts_its_own_resolv_conf() {
    let spec = ws_spec();
    let pod = workspace_pod(&spec, "ws-1", "ws-1", &ctx(), None).unwrap();
    let podspec = pod.spec.unwrap();
    let vol = podspec.volumes.unwrap().into_iter().find(|v| v.name == "attach").expect("attach volume");
    let h = vol.host_path.unwrap();
    assert_eq!(h.path, attach_file(ctx().pool, "ws-1"));
    assert_eq!(h.type_.as_deref(), Some("File"));
    let mount = podspec.containers[0]
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.mount_path == "/etc/resolv.conf")
        .expect("resolv.conf mount");
    assert!(mount.sub_path.is_none(), "the volume IS the file now");
    assert_eq!(mount.read_only, Some(true));
}

#[test]
fn the_user_key_secret_carries_the_private_key_the_git_identity_and_the_old_keys_entry() {
    let m = crate::api::OwnerMaterial {
        git_name: "Alice \"Al\" Liddell".into(),
        git_email: "alice@example.com".into(),
    };
    let s = user_key_secret("alice", "ws-alice", "PRIVATE", &m, "ssh-ed25519 AAAA alice\n", "TOKEN");
    let data = s.string_data.unwrap();
    assert_eq!(data["id_ed25519"], "PRIVATE");
    // Who may ssh in is `OwnerKeys` now; this entry only keeps an old agent's pods working
    // through the rollout, and carries the same union the projection does.
    assert_eq!(data["authorized_keys"], "ssh-ed25519 AAAA alice\n");
    // A quote in a name must not end git's string early.
    assert_eq!(data["gitconfig"], "[user]\n\tname = \"Alice \\\"Al\\\" Liddell\"\n\temail = \"alice@example.com\"\n");
    assert_eq!(data["registry-token"], "TOKEN");
}

/// The build credential the docker helper reads: minted for the owner, ttl exactly what
/// `keys::write_user_key` passes (`86_400`), and verifiable with the same `Jwt` the api tier
/// signs everything else with. The ttl itself is `mint_registry`'s own contract
/// (`crates/core/src/jwt.rs`), not reworked here.
#[test]
fn the_registry_token_verifies_as_the_owner() {
    let jwt = kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap();
    let token = jwt.mint_registry("alice", "*", 86_400).unwrap();
    assert_eq!(jwt.verify_registry(&token), Some("alice".to_string()));
}

/// A team's members share one namespace and therefore ONE keys file — the pod must mount the
/// team's, not the individual's, or a teammate's key would not open the workspace they share.
#[test]
fn a_teams_pod_mounts_the_teams_keys() {
    let mut spec = ws_spec();
    spec.image = crate::model::DEFAULT_WS_IMAGE.into();
    spec.team = "acme".into();
    let pod = workspace_pod(&spec, "ws-1", "ws-1", &ctx(), None).unwrap();
    let s = pod.spec.unwrap();
    let v = s.volumes.unwrap().into_iter().find(|v| v.name == "authorized-keys").unwrap();
    assert_eq!(v.host_path.unwrap().path, keys_file(ctx().pool, "acme"));
    // The fence admits `keys/acme/…` only when the pod says it is acme's.
    assert_eq!(pod.metadata.labels.unwrap().get(TEAM_LABEL).map(String::as_str), Some("acme"));
}

#[test]
fn a_service_is_a_statefulset_with_a_stable_template() {
    let mut s = svc("data", "/data");
    s.env = [("Z", "1"), ("A", "2"), ("M", "3")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let d = service_statefulset(&s, "env-1", "env-1", "team", &ctx()).unwrap();
    let spec = d.spec.unwrap();
    assert_eq!(spec.replicas, Some(1));
    assert_eq!(spec.service_name.as_deref(), Some("web"), "the ClusterIP Service of the same name");
    let names: Vec<_> = spec.template.spec.unwrap().containers[0].env.as_ref().unwrap().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["A", "M", "Z"], "a stable template is what keeps the ReplicaSet from changing under a database");
}

#[test]
fn a_service_deployment_refuses_a_mount_that_escapes_the_subvolume() {
    let ctx = ctx();
    let ok = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx).unwrap();
    let mounts = ok.spec.as_ref().unwrap().template.spec.as_ref().unwrap().containers[0]
        .volume_mounts
        .as_ref()
        .unwrap();
    assert_eq!(mounts[0].sub_path.as_deref(), Some("volumes/data"));
    assert_eq!(mounts[0].name, "live", "a mount is a subPath of the env's one volume");

    // The C1 payload: `{"folder": "/", "path": "/host"}`. Kubernetes rejects `..` in a subPath
    // itself, but this must not lean on that — the segment is validated before it is formatted.
    for bad in ["/", "..", "a/b", "", "../../root/.ssh", "a:b"] {
        assert!(
            service_statefulset(&svc(bad, "/host"), "env-1", "env-1", "team", &ctx).is_err(),
            "folder {bad:?} must be refused"
        );
    }
    assert!(service_statefulset(&svc("data", "/data:/etc"), "env-1", "env-1", "team", &ctx).is_err());
    assert!(service_statefulset(&svc("data", "relative"), "env-1", "env-1", "team", &ctx).is_err());
}

/// Tenants share a node, so they share its kernel. A sandbox runtime puts a userspace kernel
/// between the tenant and the host one — the only thing here that turns a kernel exploit from
/// a host compromise into a sandbox escape.
///
/// Opt-in: a `runtimeClassName` naming a runtime the node lacks makes every pod fail to start,
/// so a cluster without gVisor installed must keep working.
#[test]
fn tenant_pods_run_under_the_sandbox_when_one_is_configured() {
    let ctx = ctx(); // runtime_class: Some("gvisor")
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx, None).unwrap();
    assert_eq!(p.spec.unwrap().runtime_class_name.as_deref(), Some("gvisor"));

    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx).unwrap();
    assert_eq!(
        d.spec.unwrap().template.spec.unwrap().runtime_class_name.as_deref(),
        Some("gvisor"),
        "an environment's services are tenant workloads too"
    );

    // Unset means the host kernel, not a broken pod.
    let bare = PodContext { pool: "/mnt/wspool", node_name: "session-0", owner_ref: owner_ref(), runtime_class: None, default_image: "ghcr.io/kloudlite/kloudlite-workspace:deadbeef", system: None, registry_host: "registry.kloudlite.io" };
    assert!(workspace_pod(&ws_spec(), "ws-1", "ws-1", &bare, None).unwrap().spec.unwrap().runtime_class_name.is_none());
}

#[test]
fn no_pod_this_module_builds_uses_a_claim() {
    // A PVC binds through the StorageClass and a local PV; the pods mount the host directly
    // now, so a PVC reappearing here would mean a builder regressed to the old shape.
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    for v in p.spec.unwrap().volumes.unwrap() {
        assert!(v.persistent_volume_claim.is_none(), "workspace pod must mount a hostPath, not a claim");
        // The key is a Secret, `~/workspaces` is a per-pod emptyDir (baseline allows it);
        // everything else is the workspace's data, which is a hostPath.
        assert!(v.host_path.is_some() || v.secret.is_some() || v.empty_dir.is_some());
    }
    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx()).unwrap();
    for v in d.spec.unwrap().template.spec.unwrap().volumes.unwrap() {
        assert!(v.persistent_volume_claim.is_none(), "service pod must mount a hostPath, not a claim");
    }
}

#[test]
fn a_user_pod_cannot_reach_the_api_server_or_escalate() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let s = p.spec.unwrap();
    assert_eq!(s.automount_service_account_token, Some(false));
    assert_eq!(s.restart_policy.as_deref(), Some("Always"));
    assert_eq!(
        s.node_selector.as_ref().unwrap().get("kloudlite.io/pool").map(String::as_str),
        Some("true")
    );
    // The label without the toleration schedules nothing.
    assert_eq!(s.tolerations.as_ref().unwrap()[0].key.as_deref(), Some("kloudlite.io/pool"));

    let c = &s.containers[0];
    let sc = c.security_context.as_ref().unwrap();
    assert_eq!(sc.allow_privilege_escalation, Some(false));
    // The kernel's default syscall filter. `baseline` does not demand it, so nothing else
    // would catch its removal.
    assert_eq!(sc.seccomp_profile.as_ref().unwrap().type_, "RuntimeDefault");
    let caps = sc.capabilities.as_ref().unwrap();
    assert_eq!(caps.drop.as_deref(), Some(&["ALL".to_string()][..]));
    // Only the init set, and every entry must be one PSA `baseline` permits — an add outside
    // that list is rejected by the namespace at admission, which is a pod that never starts.
    const BASELINE_ALLOWED: [&str; 13] = [
        "AUDIT_WRITE", "CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "KILL", "MKNOD",
        "NET_BIND_SERVICE", "SETFCAP", "SETGID", "SETPCAP", "SETUID", "SYS_CHROOT",
    ];
    for c in caps.add.as_deref().unwrap_or_default() {
        assert!(BASELINE_ALLOWED.contains(&c.as_str()), "{c} is not allowed under baseline");
    }

    let r = c.resources.as_ref().unwrap();
    assert!(r.requests.as_ref().unwrap().contains_key("memory"));
    assert!(r.limits.as_ref().unwrap().contains_key("memory"));
    assert!(r.requests.as_ref().unwrap().contains_key("cpu"));
    assert!(r.limits.as_ref().unwrap().contains_key("cpu"));
    // Without this a tenant can fill the node's disk, taint it `disk-pressure` and stop
    // scheduling for every other tenant on it — a node-wide denial of service from one pod.
    assert!(
        r.limits.as_ref().unwrap().contains_key("ephemeral-storage"),
        "an unbounded writable layer is a node-wide DoS"
    );
}

/// The capacity model prices a node by how many workspaces and services fit on it, and what
/// fits is decided by the REQUEST, not the limit. These numbers are therefore a pricing input,
/// not a tuning knob — drifting them silently changes what a workspace costs.
///
/// "M session" in the model is a workspace. On a 32-OCPU / 128 GB session node at 94% usable
/// memory: 120 GB ÷ 4 GB = 30 workspaces, needing 30 × 2 = 60 vCPU of the 64 available.
#[test]
fn pod_requests_match_the_capacity_model() {
    let r = PodResources::default();
    assert_eq!(r.memory_request, "4Gi", "M workspace guarantee is 4 GB");
    assert_eq!(r.memory_limit, "8Gi", "M workspace limit is 8 GB");
    assert_eq!(r.cpu_request, "2", "2 vCPU guaranteed, and deliberately not oversubscribed");
    assert_eq!(r.cpu_limit, "4");

    // An environment service: 4 GB limit packed at 1.5x oversubscription.
    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx()).unwrap();
    let res = d.spec.unwrap().template.spec.unwrap().containers[0].resources.clone().unwrap();
    let req = res.requests.unwrap();
    let lim = res.limits.unwrap();
    assert_eq!(lim.get("memory").unwrap().0, "4Gi");
    assert_eq!(req.get("memory").unwrap().0, "2730Mi", "4 GB / 1.5x oversubscription");
}

/// The slot has to be enforced by the NAMESPACE, not just by the function that builds pods.
/// A `LimitRange` is applied at admission, so it holds for a pod created by any path — a future
/// code path that forgets, a debug pod, an operator with kubectl.
#[test]
fn the_namespace_refuses_anything_larger_than_its_slot() {
    let lr = limit_range("ws-alice", "alice", "workspace", &PodResources::default(), None);
    let item = &lr.spec.unwrap().limits[0];
    assert_eq!(item.type_, "Container");

    // max is the slot's LIMIT: bursting to it is the point, exceeding it is refused.
    let max = item.max.as_ref().unwrap();
    assert_eq!(max.get("memory").unwrap().0, "8Gi");
    assert_eq!(max.get("cpu").unwrap().0, "4");

    // defaultRequest is what capacity is priced on, for anything that names no request.
    let dr = item.default_request.as_ref().unwrap();
    assert_eq!(dr.get("memory").unwrap().0, "4Gi");
    assert_eq!(dr.get("cpu").unwrap().0, "2");

    // Shared user namespace: no ownerReference, or deleting one workspace drops the ceiling
    // for every sibling.
    assert!(lr.metadata.owner_references.is_none());

    // The environment ceiling matches the unit the Deployment actually requests.
    let env = limit_range("env-1", "team", "environment", &env_unit_resources(), Some(&owner_ref()));
    let env_item = &env.spec.unwrap().limits[0];
    assert_eq!(env_item.max.as_ref().unwrap().get("memory").unwrap().0, "4Gi");
    assert_eq!(env_item.default_request.as_ref().unwrap().get("memory").unwrap().0, "2730Mi");
}

#[test]
fn a_resource_quota_caps_the_namespaces_limits() {
    let rq = resource_quota("ws-alice", "alice", "workspace", &crate::crd::default_quota(false));
    let hard = rq.spec.unwrap().hard.unwrap();
    assert_eq!(hard["limits.cpu"].0, "40");
    assert_eq!(hard["limits.memory"].0, "80Gi");
    assert_eq!(rq.metadata.labels.unwrap()["kloudlite.io/owner"], "alice");
    // No ownerReference, the same reason the namespace and the LimitRange have none: the cap
    // is shared by every workspace in here and must not vanish with any one of them.
    assert!(rq.metadata.owner_references.is_none());
}

/// The API's Secret access must be namespaced, never cluster-wide: a cluster-wide grant would
/// include every Secret in the cluster, the agent's own credentials among them.
#[test]
fn the_api_secret_grant_is_scoped_to_one_namespace() {
    let rb = api_secret_binding("ws-alice", "alice", "kloudlite-api", "kube-system", None);
    assert_eq!(rb.metadata.namespace.as_deref(), Some("ws-alice"), "a RoleBinding, not a ClusterRoleBinding");
    assert_eq!(rb.role_ref.name, "kloudlite-api-secrets");
    assert_eq!(rb.role_ref.kind, "ClusterRole", "the rules are shared; only the scope is per namespace");
    let sub = &rb.subjects.unwrap()[0];
    assert_eq!(sub.name, "kloudlite-api");
    assert_eq!(sub.namespace.as_deref(), Some("kube-system"));
    // Shared user namespace: deleting one workspace must not revoke the grant for its siblings.
    assert!(rb.metadata.owner_references.is_none());
    // The OwnerBinding, and only it, may own the grant: it has the same (owner, node) lifetime.
    let ob = OwnerReference { kind: "OwnerBinding".into(), name: "r1-alice".into(), ..Default::default() };
    let owned = api_secret_binding("ws-alice", "alice", "kloudlite-api", "kube-system", Some(&ob));
    assert_eq!(owned.metadata.owner_references.unwrap()[0].kind, "OwnerBinding");
}

/// Three things have to line up for git in a workspace to authenticate, and each fails
/// silently on its own: the mount, the 0400 mode ssh insists on, and the env var that tells
/// git which key to use.
#[test]
fn a_workspace_pod_carries_the_owners_platform_key() {
    let spec = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap().spec.unwrap();
    let v = spec.volumes.unwrap().into_iter().find(|v| v.name == "user-key").expect("volume");
    let sv = v.secret.unwrap();
    assert_eq!(sv.secret_name.as_deref(), Some(USER_KEY_SECRET));
    assert_eq!(sv.default_mode, Some(0o444), "git runs as kl and the file is root's");
    // The API writes it after the controller makes the namespace, so it can be late.
    assert_eq!(sv.optional, Some(true));
    let c = &spec.containers[0];
    assert!(c
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .any(|m| m.name == "user-key" && m.mount_path == USER_KEY_PATH));
    let env = c.env.as_ref().unwrap().iter().find(|e| e.name == "GIT_SSH_COMMAND").unwrap();
    assert!(env.value.as_ref().unwrap().contains(USER_KEY_PATH));
}

/// A private image has to be pullable in the namespace the pod runs in. The kubelet ignores a
/// named pull secret that does not exist, so referencing it unconditionally costs nothing for a
/// public image and means a namespace given a credential just works.
#[test]
fn tenant_pods_reference_the_namespace_pull_secret() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let refs = p.spec.unwrap().image_pull_secrets.unwrap();
    assert_eq!(refs[0].name, PULL_SECRET);

    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx()).unwrap();
    let refs = d.spec.unwrap().template.spec.unwrap().image_pull_secrets.unwrap();
    assert_eq!(refs[0].name, PULL_SECRET, "an env's services are where private images show up");
}

#[test]
fn a_namespace_enforces_privileged_and_audits_restricted() {
    let ns = namespace("ws-alice", "alice", "workspace", None);
    let l = ns.metadata.labels.unwrap();
    // privileged is what hostPath mounts require; audit/warn stay at restricted so the gap
    // between what's enforced and what's actually safe keeps showing up.
    assert_eq!(l.get("pod-security.kubernetes.io/enforce").map(String::as_str), Some("privileged"));
    assert_eq!(l.get("pod-security.kubernetes.io/audit").map(String::as_str), Some("restricted"));
}

#[test]
fn a_workspace_pod_mounts_the_store_and_only_its_own_profile_read_only() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let c = &p.spec.as_ref().unwrap().containers[0];
    let mounts = c.volume_mounts.as_ref().unwrap();
    let store = mounts.iter().find(|m| m.mount_path == "/nix/store").expect("store mount");
    assert_eq!(store.read_only, Some(true));
    assert_eq!(store.sub_path.as_deref(), Some("store"));
    assert_eq!(store.name, "nix");
    let prof = mounts.iter().find(|m| m.mount_path == "/nix/profile").expect("profile mount");
    assert_eq!(prof.read_only, Some(true));
    assert_eq!(prof.sub_path.as_deref(), Some("var/kloudlite/profiles/ws-1"));
    assert!(!mounts.iter().any(|m| m.mount_path == "/nix"), "never the whole store tree: other profiles and the daemon socket live there");
    let env = c.env.as_ref().unwrap();
    let get = |k: &str| env.iter().find(|e| e.name == k).and_then(|e| e.value.clone()).unwrap();
    // The MOUNT is the directory; every env points at the `current` link inside it, because a
    // subPath is resolved once at container start and a swapped link under it never lands.
    assert!(get("PATH").starts_with("/nix/profile/current/bin:"));
    assert_eq!(get("NIX_PROFILE"), "/nix/profile/current");
    assert_eq!(get("MANPATH"), "/nix/profile/current/share/man:");
    assert!(get("XDG_DATA_DIRS").starts_with("/nix/profile/current/share:"));
    let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    let nix = vols.iter().find(|v| v.name == "nix").unwrap();
    assert_eq!(nix.host_path.as_ref().unwrap().path, NIX_ROOT);
    assert!(vols.iter().all(|v| v.persistent_volume_claim.is_none()), "workspace pod must mount a hostPath, not a claim");
}

#[test]
fn a_workspace_pod_mounts_its_volume_at_workspace_and_only_there() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let s = p.spec.unwrap();
    let claims = s.volumes.as_ref().unwrap().iter().filter(|v| v.name == "live" && v.host_path.is_some());
    assert_eq!(claims.count(), 1);
    let mounts = s.containers[0].volume_mounts.as_ref().unwrap();
    assert_eq!(mounts.iter().filter(|m| m.name == "live").count(), 1, "the nginx web-root mount is gone with nginx");
    assert!(mounts.iter().any(|m| m.mount_path == "/home/kl/workspaces/dev" && m.read_only.is_none()));
}

/// The home is a PV mounted at `/home/kl` and the workspace subvolume a PV mounted INSIDE it;
/// the kubelet orders mounts by path depth, so the paths carry the order. The ssh Secret
/// mounts under `/home/kl/.ssh` land inside the home too — a Secret inside a PV is fine.
#[test]
fn a_workspace_pod_mounts_the_home_and_the_workspace_inside_it() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let s = p.spec.unwrap();
    let home = s.volumes.as_ref().unwrap().iter().find(|v| v.name == "home").expect("home volume");
    assert_eq!(home.host_path.as_ref().unwrap().path, format!("{}/homes/{}", ctx().pool, ws_spec().owner));
    let mounts = s.containers[0].volume_mounts.as_ref().unwrap();
    let home_mount = mounts.iter().find(|m| m.name == "home").expect("home mount");
    assert_eq!(home_mount.mount_path, HOME_DIR);
    assert!(home_mount.read_only.is_none(), "dotfiles are written by the person");
    assert!(home_mount.sub_path.is_none());
    // Without this a node-side remount strands every running pod on the detached mount
    // ("Network is unreachable" on every path under $HOME) until the pod is recreated.
    assert_eq!(home_mount.mount_propagation.as_deref(), Some("HostToContainer"));
    let live = mounts.iter().find(|m| m.name == "live").unwrap();
    assert!(live.mount_path.starts_with(&format!("{HOME_DIR}/")), "the workspace is INSIDE the home: {}", live.mount_path);
    assert!(AUTHORIZED_KEYS_PATH.starts_with(HOME_DIR));
    // A custom image gets the home too: it is the person's, not the image's.
    let mut custom = ws_spec();
    custom.image = "ghcr.io/someone/theirs:1".into();
    let s = workspace_pod(&custom, "ws-1", "ws-1", &ctx(), None).unwrap().spec.unwrap();
    assert!(s.volumes.as_ref().unwrap().iter().any(|v| v.name == "home"));
}

/// H1: the home's hostPath must be its WORKTREE path (`{pool}/vol/{home}/live/{home}`), same
/// split `live_worktree_volume` already gives the workspace's own mount — not the old-layout
/// directory, which after a migration holds the worktree ONE level down and would hide
/// dotfiles / snapshot nothing new.
/// A shared-volume clone's pod is named after the WORKSPACE, not the volume it shares. Naming
/// it after the volume made every clone claim its source's pod name: the clone adopted the
/// source's running pod, its `podRef` pointed at another workspace's shell (the gateway dials
/// `podRef`), and stopping the clone deleted the source's pod.
#[test]
fn a_clones_pod_is_named_after_the_workspace_not_the_shared_volume() {
    let p = workspace_pod(&ws_spec(), "vol-1", "ws-clone", &ctx(), None).unwrap();
    assert_eq!(p.metadata.name.as_deref(), Some("ws-clone"), "the pod is this workspace's");
    assert_eq!(
        p.metadata.labels.as_ref().unwrap().get(WORKSPACE_LABEL).map(String::as_str),
        Some("ws-clone"),
        "the workspace label names the workspace the pod IS"
    );
    let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    let path = |n: &str| vols.iter().find(|v| v.name == n).unwrap().host_path.as_ref().unwrap().path.clone();
    assert!(path("live").ends_with("/vol-1/live/ws-clone"), "worktree: volume root, workspace leaf");
    assert!(path("attach").contains("/attach/ws-clone/"), "resolv.conf is per workspace");
}

#[test]
fn the_home_is_the_shared_nfs_path_and_caches_are_local() {
    let pod = workspace_pod(&ws_spec(), "vol-1", "ws-1", &ctx(), None).unwrap();
    let s = pod.spec.unwrap();
    let vols = s.volumes.unwrap();
    let path = |n: &str| vols.iter().find(|v| v.name == n).unwrap().host_path.as_ref().unwrap().path.clone();
    assert_eq!(path("home"), format!("{}/homes/{}", ctx().pool, ws_spec().owner));
    assert_eq!(path("homecache"), format!("{}/homecache/{}", ctx().pool, ws_spec().owner));
    let mounts = s.containers[0].volume_mounts.clone().unwrap();
    let sub = |mp: &str| mounts.iter().find(|m| m.mount_path == mp).map(|m| (m.name.clone(), m.sub_path.clone()));
    assert_eq!(sub(HOME_CACHE_DIR), Some(("homecache".into(), Some("cache".into()))));
    assert_eq!(sub("/home/kl/.cargo/registry"), Some(("homecache".into(), Some("cargo-registry".into()))));
    assert_eq!(sub("/home/kl/.vscode-server"), Some(("homecache".into(), Some("vscode-server".into()))));
    assert_eq!(sub("/home/kl/.cursor-server"), Some(("homecache".into(), Some("cursor-server".into()))));
    assert_eq!(sub(HOME_STATE_DIR), Some(("homecache".into(), Some("state".into()))));
}

#[test]
fn the_login_env_redirects_every_cache_and_pins_histfile_local() {
    let env = login_env("ws-1", "acme", "registry.kloudlite.io");
    let get = |n: &str| env.iter().find(|e| e.name == n).unwrap().value.clone().unwrap();
    assert_eq!(get("XDG_CACHE_HOME"), format!("{HOME_CACHE_DIR}/xdg"));
    assert_eq!(get("HISTFILE"), format!("{HOME_STATE_DIR}/shell_history"));
    for (var, sub) in [
        ("npm_config_cache", "npm"), ("PNPM_STORE_DIR", "pnpm"), ("BUN_INSTALL_CACHE_DIR", "bun"),
        ("CARGO_TARGET_DIR", "cargo-target"), ("RUSTUP_HOME", "rustup"), ("GOMODCACHE", "gomod"),
        ("GRADLE_USER_HOME", "gradle"), ("UV_CACHE_DIR", "uv"), ("PIP_CACHE_DIR", "pip"),
        ("DENO_DIR", "deno"), ("PLAYWRIGHT_BROWSERS_PATH", "playwright"),
    ] {
        assert_eq!(get(var), format!("{HOME_CACHE_DIR}/{sub}"), "{var}");
    }
    // The configs half of the author's rule: cargo credentials and a person's GOPATH/src stay
    // on the shared home, so neither var may be redirected onto the disposable volume.
    for var in ["CARGO_HOME", "GOPATH"] {
        assert!(env.iter().all(|e| e.name != var), "{var} must stay on the shared home");
    }
}

/// Four things have to line up for `ssh kl@workspace` to work, and each fails silently on
/// its own: sshd as the container's process, its host key, the owner's authorized_keys where
/// the config says to look, and the modes sshd refuses to start (or to authenticate) without.
#[test]
fn the_default_image_runs_sshd_with_its_own_host_key_and_the_owners_keys() {
    let mut spec = ws_spec();
    spec.image = crate::model::DEFAULT_WS_IMAGE.into();
    let s = workspace_pod(&spec, "ws-1", "ws-1", &ctx(), None).unwrap().spec.unwrap();
    let c = &s.containers[0];
    let cmd = c.command.as_ref().unwrap();
    assert_eq!(cmd[0], "/bin/sh");
    assert!(
        cmd[2].trim_end().ends_with(&format!("exec {}/bin/sshd -D -e -f {SSHD_DIR}/sshd_config", crate::packages::PROFILE_LINK)),
        "{}",
        cmd[2]
    );
    // sshd exits on a missing privsep directory or a missing `sshd` user, and stock alpine has
    // neither.
    // The accounts and chroot dir are the image's (Dockerfile `workspace`), not the prelude's.
    assert!(!cmd[2].contains("adduser"), "{}", cmd[2]);
    assert_eq!(c.image.as_deref(), Some("ghcr.io/kloudlite/kloudlite-workspace:deadbeef"), "the pinned image, not the marker");
    assert_eq!(c.ports.as_ref().unwrap()[0].container_port, 22);

    let vols = s.volumes.as_ref().unwrap();
    let host = vols.iter().find(|v| v.name == "ws-ssh").expect("host key volume").secret.clone().unwrap();
    assert_eq!(host.secret_name.as_deref(), Some("ws-ssh-ws-1"));
    // sshd refuses a host key that is group- or world-readable and exits; the config it reads
    // is not a secret and stays readable.
    assert_eq!(host.default_mode, Some(0o400));
    let config_item = host.items.as_ref().unwrap().iter().find(|i| i.key == "sshd_config").expect("config item");
    assert_eq!(config_item.mode, Some(0o444));

    let keys = vols.iter().find(|v| v.name == "authorized-keys").expect("authorized_keys volume").host_path.clone().unwrap();
    assert_eq!(keys.path, keys_file(ctx().pool, "alice"), "the agent's rendered file for this pod's namespace");
    // `File`, so the kubelet refuses the pod rather than inventing an empty directory where
    // the owner's keys should be — the reconcile parks it on `KeysNotReady` before that.
    assert_eq!(keys.type_.as_deref(), Some("File"));

    let mounts = c.volume_mounts.as_ref().unwrap();
    let ssh = mounts.iter().find(|m| m.name == "ws-ssh").unwrap();
    assert_eq!(ssh.mount_path, SSHD_DIR);
    assert_eq!(ssh.read_only, Some(true));
    let ak = mounts.iter().find(|m| m.name == "authorized-keys").unwrap();
    // The volume IS the file, so no subPath: a subPath mount would never see the agent's
    // in-place rewrite, and a key added in the UI would need a pod recreate to take effect.
    assert_eq!(ak.mount_path, AUTHORIZED_KEYS_PATH);
    assert_eq!(ak.sub_path, None);
    assert_eq!(ak.read_only, Some(true));
    // Where sshd is told to look has to be where the mount actually puts it.
    assert!(sshd_config("dev", "acme", "registry.kloudlite.io").contains(&format!("AuthorizedKeysFile {AUTHORIZED_KEYS_PATH}")));
    // The mount's parent directories are the node's, not `kl`'s; without this every key is
    // refused as "bad ownership or modes".
    assert!(sshd_config("dev", "acme", "registry.kloudlite.io").contains("StrictModes no\n"));
    // The account sshd lets in: fixed uid, unlocked, owning the volume; and the key it reads.
    let prelude = &cmd[2];
    // `-h`: the tree is the person's between starts, and a planted symlink must not hand root's
    // chown a target outside it (the same hole the home seed closed by running as kl).
    assert!(prelude.contains("chown -Rh 1000:1000 /home/kl/workspaces/dev"), "{prelude}");
    assert!(!prelude.contains("chown -R 1000:1000"), "{prelude}");
    // Never `-R` over the home: `.ssh` is a read-only mount, and under `set -e` one EROFS
    // from chown is a pod that never starts.
    assert!(!prelude.contains("-R 1000:1000 $H"), "{prelude}");
    // Root's part ends where `su` begins. Below `$H` the person owns the tree between starts,
    // so a root `chown`/`mkdir`/redirect there follows whatever symlink they planted. The
    // closed list below — not a prefix, so a new path cannot be smuggled onto an existing
    // line — is every path root may touch: mountpoints and the `.cargo` parent the kubelet
    // makes root-owned for one. Adding to it is the moment to re-read `prelude`'s doc comment.
    const ROOT_CHOWNS: [&str; 3] =
        ["chown 1000:1000 $H $H/workspaces", "chown -h 1000:1000 $H/.cargo $H/.cargo/registry", "chown 1000:1000 $H/.local"];
    let su_at = prelude.lines().position(|l| l.starts_with("su kl -s /bin/sh <<'SEED'")).expect("seed runs as kl");
    let root: Vec<&str> = prelude.lines().take(su_at).collect();
    // Both must actually be there: dropping the second leaves `~/.cargo` unwritable by kl.
    for want in ROOT_CHOWNS {
        assert!(root.contains(&want), "{root:?}");
    }
    for l in &root {
        // Root writes only to /etc (the container's own filesystem); nothing under $H.
        assert!(!l.contains("$H/") || ROOT_CHOWNS.contains(l), "root must not write under $H: {l}");
        assert!(!l.contains("> /home"), "root must not write under the home: {l}");
        assert!(!l.starts_with("chown") || ROOT_CHOWNS.contains(l), "root chown below the mountpoints: {l}");
    }
    let seed_end = prelude.lines().position(|l| l == "SEED").expect("heredoc terminator at column 0");
    assert!(prelude.lines().skip(su_at + 1).take(seed_end - su_at - 1).any(|l| l.starts_with("mkdir -p $H/")), "{prelude}");
    // `~/workspaces` is the pod's own emptyDir: root chowns that mount point and nothing else.
    assert!(prelude.contains("chown 1000:1000 $H $H/workspaces\n"), "{prelude}");
    assert!(prelude.lines().nth(seed_end + 1).unwrap().starts_with("chown -Rh 1000:1000 /home/kl/workspaces/"), "{prelude}");
    // The prompt and the profile's PATH, for both shells; the greeting replaces alpine's.
    assert!(prelude.contains("starship init zsh"), "{prelude}");
    // Coloured `ls` in both shells: coreutils' ls is plain until LS_COLORS and --color say
    // otherwise, and a login that cannot tell a directory from a file feels broken.
    assert!(prelude.contains("dircolors -b"), "{prelude}");
    assert!(prelude.contains("ls --color=auto"), "{prelude}");
    // Seeded once: a person's own edits to their rc files must survive a restart.
    assert!(prelude.contains("[ -e $H/.config/zsh/.zshrc ] ||"), "{prelude}");
    // Run the rc-seeding lines for real: the quoting inside printf is the thing that breaks.
    let home = tempfile::tempdir().unwrap();
    let seed: String = prelude
        .lines()
        .filter(|l| l.contains("printf") || l.starts_with("H=") || l.starts_with("mkdir -p $H"))
        .map(|l| l.replacen("H=/home/kl", &format!("H={}", home.path().display()), 1))
        .collect::<Vec<_>>()
        .join("\n");
    let ok = std::process::Command::new("sh").arg("-c").arg(&seed).status().map(|s| s.success());
    assert_eq!(ok.ok(), Some(true), "seed lines do not run:\n{seed}");
    let zshrc = std::fs::read_to_string(home.path().join(".config/zsh/.zshrc")).unwrap();
    assert!(zshrc.contains("eval \"$(dircolors -b)\"\n") && zshrc.contains("alias ls=\"ls --color=auto\""), "{zshrc}");
    assert!(zshrc.contains("zstyle \":completion:*\" list-colors \"${(s.:.)LS_COLORS}\""), "{zshrc}");
    let fish = std::fs::read_to_string(home.path().join(".config/fish/config.fish")).unwrap();
    assert!(fish.contains("set -gx LS_COLORS (dircolors -b | string match -r \"LS_COLORS=.([^']*)\")[2]\n"), "{fish}");
    assert!(fish.contains("starship init fish | source\n"), "{fish}");
    assert!(prelude.contains("starship init fish | source"), "{prelude}");
    // It is a shell script assembled from string pieces; the one check that catches a broken
    // heredoc or an unbalanced quote before a pod does.
    let ok = std::process::Command::new("sh").arg("-n").arg("-c").arg(prelude).status().map(|s| s.success());
    assert_eq!(ok.ok(), Some(true), "prelude does not parse:\n{prelude}");
    // Non-interactive logins (`ssh ws cmd`, sftp, editors' remote helpers) read no rc file,
    // so the profile's PATH has to come from sshd itself.
    let cfg = sshd_config("dev", "acme", "registry.kloudlite.io");
    // Exactly one SetEnv line, carrying every variable: sshd ignores a second one.
    assert_eq!(cfg.matches("SetEnv ").count(), 1, "{cfg}");
    let line = cfg.lines().find(|l| l.starts_with("SetEnv ")).unwrap();
    assert!(line.contains("\"PATH=/nix/profile/current/bin:"), "{line}");
    assert!(line.contains("\"KL_WORKSPACE=/home/kl/workspaces/dev\"") && line.contains("\"KL_WORKSPACE_NAME=dev\""), "{line}");
    // The platform rc files: interactive-only cd into the workspace, starship names it.
    assert!(prelude.contains("> /etc/zshrc") && prelude.contains("> /etc/fish/conf.d/kl.fish") && prelude.contains("> /etc/starship.toml"), "{prelude}");
    // In the platform file, not the seeded `.zshrc`: a home seeded before this line existed
    // never gets a second seed, and without `compinit` zsh falls back to its primitive
    // completer, which appends the match to the word instead of replacing it ("cacargo").
    // The dump goes to the per-node cache dir, never the shared home.
    assert!(prelude.contains("autoload -Uz compinit && compinit -d"), "{prelude}");
    // The build profile: files, not env, so it runs as a subprocess of both rc files rather
    // than needing to be sourced — see kl-build.sh's own doc for why.
    assert!(prelude.contains("[ -r /etc/profile.d/kl-build.sh ] && sh /etc/profile.d/kl-build.sh"), "{prelude}");
    assert!(prelude.contains("test -r /etc/profile.d/kl-build.sh; and sh /etc/profile.d/kl-build.sh"), "{prelude}");
    // `~/.local` is created ROOT-owned by the kubelet as the parent of the `.local/state`
    // mount point, so without this fish cannot create `.local/share` and refuses to save
    // history ("Permission denied"); the same for anything else that keeps XDG data.
    assert!(prelude.contains("chown 1000:1000 $H/.local\n"), "{prelude}");
    assert!(prelude.contains("[[ -o interactive ]] || return 0"), "{prelude}");
    // zsh finds its rc under `~/.config` only if the LOGIN is told so; the entrypoint's env
    // does not reach an ssh session.
    assert!(line.contains("\"ZDOTDIR=/home/kl/.config/zsh\""), "{line}");
    assert!(line.contains("\"LANG=C.UTF-8\""), "{line}");
    assert!(line.contains("\"GIT_SSH_COMMAND=ssh -i /etc/kloudlite/ssh/id_ed25519 "), "{line}");
    assert!(line.contains("\"GIT_CONFIG_SYSTEM=/etc/kloudlite/ssh/gitconfig\""), "{line}");
    // ...and the pod entrypoint sees the identical list.
    let names: Vec<&str> = c.env.as_ref().unwrap().iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"GIT_CONFIG_SYSTEM") && names.contains(&"PATH") && names.contains(&"GIT_SSH_COMMAND"), "{names:?}");
    assert_eq!(s.hostname.as_deref(), Some("ws"));
    // No fsGroup: it would re-mode the host key Secret too, and sshd refuses a host key
    // anyone but its owner can read.
    assert!(s.security_context.as_ref().and_then(|s| s.fs_group).is_none());
    // The existing git mount must stay where GIT_SSH_COMMAND points.
    assert!(mounts.iter().any(|m| m.name == "user-key" && m.mount_path == USER_KEY_PATH));
    // Storage is mounted from the node, not claimed — nothing here may grow a PVC.
    assert!(vols.iter().all(|v| v.persistent_volume_claim.is_none()));
}

#[test]
fn a_custom_image_keeps_its_entrypoint_and_gets_no_sshd() {
    let mut spec = ws_spec();
    spec.image = "ghcr.io/acme/dev:1".into();
    let s = workspace_pod(&spec, "ws-1", "ws-1", &ctx(), None).unwrap().spec.unwrap();
    assert!(s.containers[0].command.is_none(), "a user image keeps its entrypoint");
    assert!(s.containers[0].ports.is_none());
    assert!(s.volumes.as_ref().unwrap().iter().all(|v| v.name != "ws-ssh" && v.name != "authorized-keys"));
}

/// The host key Secret is per workspace and dies with it — a clone gets its own.
#[test]
fn a_workspaces_host_key_lives_and_dies_with_it() {
    let s = ws_ssh_secret("ws-1", "dev", "ws-alice", "alice", &owner_ref(), "PRIVATE", "ssh-ed25519 AAAA ws", "registry.kloudlite.io");
    assert_eq!(s.metadata.name.as_deref(), Some("ws-ssh-ws-1"));
    assert_eq!(s.metadata.namespace.as_deref(), Some("ws-alice"));
    assert_eq!(s.metadata.owner_references.unwrap()[0].controller, Some(true));
    let d = s.string_data.unwrap();
    assert_eq!(d["ssh_host_ed25519_key"], "PRIVATE");
    assert_eq!(d["ssh_host_ed25519_key.pub"], "ssh-ed25519 AAAA ws");
    // The config names the key and the keys file by absolute path, and turns passwords off:
    // the container runs as root, and the login is `kl` — never root.
    let cfg = &d["sshd_config"];
    assert!(cfg.contains(&format!("HostKey {SSHD_DIR}/ssh_host_ed25519_key")), "{cfg}");
    assert!(cfg.contains("AuthorizedKeysFile /home/kl/.ssh/authorized_keys"), "{cfg}");
    assert!(cfg.contains("PermitRootLogin no\n"), "{cfg}");
    assert!(cfg.contains("AllowUsers kl\n"), "{cfg}");
    assert!(cfg.contains("PasswordAuthentication no"), "{cfg}");
}

/// The gate is the only thing that may reach a builder, and a workspace reaching the gate is
/// the only new hole this task opens — one peer, both selectors, same AND trap as
/// `allow_gateway_ingress`.
#[test]
fn only_the_builder_gate_may_be_reached_on_egress() {
    let p = builder_gate_egress("ws-alice", "alice", &owner_ref());
    assert_eq!(p.metadata.name.as_deref(), Some("allow-builder-gate"));
    assert_eq!(p.metadata.namespace.as_deref(), Some("ws-alice"));
    assert_eq!(p.metadata.owner_references.unwrap()[0].controller, Some(true));
    let spec = p.spec.unwrap();
    assert_eq!(spec.policy_types.as_ref().unwrap(), &vec!["Egress".to_string()], "never an ingress hole here");
    let pod_sel = spec.pod_selector.unwrap();
    assert!(pod_sel.match_labels.is_none() && pod_sel.match_expressions.is_none(), "every pod in the namespace");
    let rule = &spec.egress.as_ref().unwrap()[0];
    let to = rule.to.as_ref().unwrap();
    assert_eq!(to.len(), 1, "one peer: namespace AND pod, not namespace OR pod");
    let ns = to[0].namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(ns["kubernetes.io/metadata.name"], "kloudlite-system");
    let pod = to[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(pod["app"], "kloudlite-builder-gate");
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(1234)));
}

/// Only the gate may dial buildkit — never any other pod, in this namespace or any other.
#[test]
fn only_the_builder_gate_may_reach_buildkit() {
    let p = builder_gate_ingress("env-bld-alice", "alice", &owner_ref());
    assert_eq!(p.metadata.name.as_deref(), Some("allow-builder-gate"));
    assert_eq!(p.metadata.namespace.as_deref(), Some("env-bld-alice"));
    assert_eq!(p.metadata.owner_references.unwrap()[0].controller, Some(true));
    let spec = p.spec.unwrap();
    assert_eq!(spec.policy_types.as_ref().unwrap(), &vec!["Ingress".to_string()], "never an egress hole here");
    let sel = spec.pod_selector.unwrap().match_labels.clone().unwrap();
    assert_eq!(sel[SERVICE_LABEL], "buildkit", "only the buildkit service pod, not the whole namespace");
    let rule = &spec.ingress.as_ref().unwrap()[0];
    let from = rule.from.as_ref().unwrap();
    assert_eq!(from.len(), 1, "one peer: namespace AND pod, not namespace OR pod");
    let ns = from[0].namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(ns["kubernetes.io/metadata.name"], "kloudlite-system");
    let pod = from[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(pod["app"], "kloudlite-builder-gate");
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(1234)));
}

/// Port 22 is open to exactly one peer. Without the namespace half every tenant's own pods
/// could label themselves `app=kloudlite-gateway` and reach each other's sshd.
#[test]
fn only_the_gateway_may_reach_port_22() {
    let p = allow_gateway_ingress("ws-alice", "alice", &owner_ref());
    assert_eq!(p.metadata.name.as_deref(), Some("allow-gateway-ssh"));
    let spec = p.spec.unwrap();
    assert_eq!(spec.policy_types.as_ref().unwrap(), &vec!["Ingress".to_string()], "never an egress hole");
    let rule = &spec.ingress.as_ref().unwrap()[0];
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(22)));
    let from = rule.from.as_ref().unwrap();
    assert_eq!(from.len(), 1, "one peer: namespace AND pod, not namespace OR pod");
    let ns = from[0].namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(ns["kubernetes.io/metadata.name"], GATEWAY_NAMESPACE);
    assert_eq!(GATEWAY_NAMESPACE, "kloudlite-system", "deploy/k3s/gateway.yaml puts the gateway here; keep them equal");
    let pod = from[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(pod["app"], "kloudlite-gateway");
}

/// The grant selects the POD, never the namespace: an owner's workspaces share a namespace, so
/// a namespace-wide rule would open every workspace they have to the environment.
#[test]
fn the_attachment_egress_selects_one_workspace_pod() {
    let p = attach_egress("ws-acme", "ws-1", "env-abc", "acme", &owner_ref());
    assert_eq!(p.metadata.name.as_deref(), Some("attach-ws-1"));
    assert_eq!(p.metadata.namespace.as_deref(), Some("ws-acme"));
    let spec = serde_json::to_value(p.spec.unwrap()).unwrap();
    assert_eq!(spec["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
    assert_eq!(spec["policyTypes"], serde_json::json!(["Egress"]));
    assert_eq!(
        spec["egress"][0]["to"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
        "env-abc"
    );
}

/// The environment side names both the namespace and the pod: a namespace selector alone would
/// admit every workspace of every owner who happens to share that namespace.
#[test]
fn the_attachment_ingress_names_the_namespace_and_the_pod() {
    let p = attach_ingress("env-abc", "ws-acme", "ws-1", "acme", &owner_ref());
    assert_eq!(p.metadata.namespace.as_deref(), Some("env-abc"));
    let spec = serde_json::to_value(p.spec.unwrap()).unwrap();
    let from = &spec["ingress"][0]["from"][0];
    assert_eq!(from["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"], "ws-acme");
    assert_eq!(from["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
    assert_eq!(spec["policyTypes"], serde_json::json!(["Ingress"]));
}

#[test]
fn every_child_object_cascades_on_delete() {
    // Reclamation via garbage collection rather than cleanup code that can be skipped or crash
    // halfway. If this regresses, deleting a workspace leaks its pod or namespace.
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    assert_eq!(p.metadata.owner_references.unwrap()[0].controller, Some(true));
    assert_eq!(namespace("env-1", "team", "environment", Some(&owner_ref())).metadata.owner_references.unwrap().len(), 1);
    for pol in default_policies("env-1", "team", &owner_ref()) {
        assert_eq!(pol.metadata.owner_references.unwrap().len(), 1);
    }

    // The shared user namespace must NOT cascade: it outlives any one workspace, and an owner
    // reference here would delete every sibling when one workspace goes.
    let shared = namespace("ws-alice", "alice", "workspace", None);
    assert!(
        shared.metadata.owner_references.is_none(),
        "a user's workspace namespace is shared infrastructure and must not be garbage-collected"
    );
}

/// `allow-dns` reached every pod in kube-system on 53 — the agent's own DaemonSet included —
/// where only CoreDNS was ever meant. One peer, both selectors: the two-peer form would mean
/// "all of kube-system OR every k8s-app=kube-dns pod anywhere", which is wider than what it
/// replaces (see `attach_egress`'s comment on the same trap).
#[test]
fn allow_dns_reaches_coredns_only() {
    let p = default_policies("ws-alice", "alice", &owner_ref())
        .into_iter()
        .find(|p| p.metadata.name.as_deref() == Some("allow-dns"))
        .expect("allow-dns");
    let to = &p.spec.as_ref().unwrap().egress.as_ref().unwrap()[0].to.as_ref().unwrap();
    assert_eq!(to.len(), 1, "one peer, or the selectors are an OR");
    let peer = &to[0];
    assert_eq!(
        peer.namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap()["kubernetes.io/metadata.name"],
        "kube-system",
        "the namespace selector must survive alongside the pod selector, not be replaced by it"
    );
    assert_eq!(
        peer.pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap()["k8s-app"],
        "kube-dns"
    );
    // Both selectors in one peer are ANDed by Kubernetes: a pod in kube-system without
    // k8s-app=kube-dns (the agent, say) must not match. Two peers would OR them instead and
    // let exactly this pod through — the regression this test exists to catch.
    let ns_labels = std::collections::BTreeMap::from([("kubernetes.io/metadata.name".to_string(), "kube-system".to_string())]);
    let other_pod_labels = std::collections::BTreeMap::from([("app".to_string(), "kloudlite-agent".to_string())]);
    let matches = |sel: &k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector, labels: &std::collections::BTreeMap<String, String>| {
        sel.match_labels.as_ref().unwrap().iter().all(|(k, v)| labels.get(k) == Some(v))
    };
    assert!(matches(peer.namespace_selector.as_ref().unwrap(), &ns_labels), "kube-system namespace must match");
    assert!(!matches(peer.pod_selector.as_ref().unwrap(), &other_pod_labels), "a non-CoreDNS kube-system pod must not match");
}

#[test]
fn an_environment_namespace_denies_by_default_and_still_resolves_dns() {
    let pols = default_policies("env-1", "team", &owner_ref());
    let names: Vec<_> = pols.iter().filter_map(|p| p.metadata.name.as_deref()).collect();
    assert_eq!(names, vec!["default-deny", "allow-dns", "allow-internet-egress", "allow-same-namespace"]);

    let deny = pols[0].spec.as_ref().unwrap();
    assert_eq!(deny.policy_types.as_ref().unwrap().len(), 2, "deny must cover BOTH directions");
    assert!(deny.ingress.is_none() && deny.egress.is_none(), "a rule here would stop it denying");

    let dns = pols[1].spec.as_ref().unwrap().egress.as_ref().unwrap();
    assert!(dns[0].ports.as_ref().unwrap().iter().any(|p| p.port == Some(IntOrString::Int(53))));
}

/// A workspace has to reach npm and GitHub, but "allow egress" written the obvious way
/// (`0.0.0.0/0`) also opens `169.254.169.254` — the cloud metadata service, which on Azure
/// hands out the NODE's managed identity. That is an escape from the cluster, not the
/// namespace, so the internet rule must be an allow-list with holes punched out.
#[test]
fn internet_egress_never_reaches_the_metadata_service_or_the_cluster() {
    let pols = default_policies("ws-alice", "alice", &owner_ref());
    let net = pols.iter().find(|p| p.metadata.name.as_deref() == Some("allow-internet-egress")).unwrap();
    let rules = net.spec.as_ref().unwrap().egress.as_ref().unwrap();
    let block = rules[0].to.as_ref().unwrap()[0].ip_block.as_ref().unwrap();
    assert_eq!(block.cidr, "0.0.0.0/0");
    let except = block.except.as_ref().unwrap();

    // The metadata service, and every private range the cluster lives on.
    for cidr in ["169.254.0.0/16", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
        assert!(except.contains(&cidr.to_string()), "{cidr} must be excluded from egress");
    }
    // Egress-only: this rule must never become an ingress hole.
    assert_eq!(net.spec.as_ref().unwrap().policy_types.as_ref().unwrap(), &vec!["Egress".to_string()]);
}

#[test]
fn a_service_gets_a_clusterip_for_each_declared_port() {
    let s = service_clusterip(&svc("data", "/data"), "env-1", "team", &owner_ref(), false).unwrap();
    let spec = s.spec.unwrap();
    let ports = spec.ports.unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].port, 80);
    assert_eq!(ports[0].target_port, Some(IntOrString::Int(80)));
    // The selector must match the Deployment's template labels or the Service selects nothing
    // and the name resolves to a black hole.
    assert_eq!(spec.selector.unwrap().get(SERVICE_LABEL).map(String::as_str), Some("web"));
}

fn intercept(ports: &[(u16, u16)]) -> crate::crd::Intercept {
    crate::crd::Intercept {
        service: "web".into(),
        workspace: "ws-1".into(),
        ports: ports.iter().map(|(s, w)| crate::crd::PortMap { service: *s, workspace: *w }).collect(),
    }
}

fn two_port_svc() -> model::Service {
    let mut s = svc("data", "/data");
    s.ports = vec![80, 5432];
    s
}

/// The Service keeps the dialled number and the slice carries the mapped one; they are joined
/// by the `p{port}` name, so a rename here breaks the remap silently.
#[test]
fn the_slice_delivers_a_mapped_port_and_the_service_still_dials_the_declared_one() {
    let s = two_port_svc();
    let ic = intercept(&[(80, 3000)]);
    let sl = intercept_slice(&s, "env-1", "team", &owner_ref(), &ic, Some("10.42.3.231"));
    assert_eq!(sl.metadata.name.as_deref(), Some("web-intercept"));
    assert_eq!(sl.metadata.namespace.as_deref(), Some("env-1"));
    assert_eq!(sl.metadata.labels.as_ref().unwrap()["kubernetes.io/service-name"], "web");
    assert_eq!(sl.address_type, "IPv4");

    let ports = sl.ports.unwrap();
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0].name.as_deref(), Some("p80"));
    assert_eq!(ports[0].port, Some(3000), "the mapped port");
    // Unmapped: answered on its own number.
    assert_eq!(ports[1].name.as_deref(), Some("p5432"));
    assert_eq!(ports[1].port, Some(5432));

    assert_eq!(sl.endpoints[0].addresses, vec!["10.42.3.231".to_string()]);

    let dialled = service_clusterip(&s, "env-1", "team", &owner_ref(), true).unwrap().spec.unwrap();
    let dialled = dialled.ports.unwrap();
    assert_eq!(dialled[0].name.as_deref(), Some("p80"), "the join is by NAME");
    assert_eq!(dialled[0].port, 80, "what callers dial never changes");
}

/// No pod means no address, but the slice still has to exist with its ports: an intercepted
/// Service with no endpoints refuses connections, which is right, while a slice with stale
/// endpoints sends traffic to whoever holds that IP next.
#[test]
fn a_slice_with_no_pod_ip_has_ports_but_no_endpoints() {
    let sl = intercept_slice(&two_port_svc(), "env-1", "team", &owner_ref(), &intercept(&[]), None);
    assert!(sl.endpoints.is_empty());
    assert_eq!(sl.ports.unwrap().len(), 2);
}

/// A Service that keeps its selector has its endpoints overwritten by Kubernetes, and the
/// selector can only match pods in the environment's own namespace — the intercept would
/// silently never take effect.
#[test]
fn an_intercepted_service_has_no_selector() {
    let s = two_port_svc();
    let on = service_clusterip(&s, "env-1", "team", &owner_ref(), true).unwrap();
    assert!(on.spec.unwrap().selector.is_none());
    let off = service_clusterip(&s, "env-1", "team", &owner_ref(), false).unwrap();
    assert!(off.spec.unwrap().selector.is_some());
}

/// Same AND-not-OR rule as the attach pair: two peers would open the whole workspace namespace
/// to the environment, plus any pod anywhere carrying that workspace label.
#[test]
fn the_intercept_policies_name_one_peer_each() {
    let r = owner_ref();
    let eg = intercept_egress("env-abc", "ws-acme", "ws-1", "acme", &r);
    assert_eq!(eg.metadata.name.as_deref(), Some("intercept-ws-1"));
    assert_eq!(eg.metadata.namespace.as_deref(), Some("env-abc"));
    let spec = serde_json::to_value(eg.spec.unwrap()).unwrap();
    let to = spec["egress"][0]["to"].as_array().unwrap();
    assert_eq!(to.len(), 1, "two peers is an OR, not an AND");
    assert_eq!(to[0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"], "ws-acme");
    assert_eq!(to[0]["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
    assert_eq!(spec["policyTypes"], serde_json::json!(["Egress"]));

    let ing = intercept_ingress("ws-acme", "env-abc", "ws-1", "acme", &r);
    assert_eq!(ing.metadata.namespace.as_deref(), Some("ws-acme"));
    let spec = serde_json::to_value(ing.spec.unwrap()).unwrap();
    // Its peer is a whole namespace, so the scoping to this one pod is the top-level selector.
    assert_eq!(spec["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
    assert_eq!(spec["ingress"][0]["from"].as_array().unwrap().len(), 1);
    assert_eq!(spec["policyTypes"], serde_json::json!(["Ingress"]));
}

#[test]
fn a_service_with_no_ports_gets_no_clusterip() {
    let mut s = svc("worker", "/data");
    s.ports.clear();
    // An empty `ports` list on a k8s Service is rejected by the API server, so a portless
    // service must not produce one at all — its StatefulSet still runs, it is just unreachable
    // by name, which is correct for something that listens on nothing.
    assert!(service_clusterip(&s, "env-1", "team", &owner_ref(), false).is_none());
}

/// `spec.name` is spliced into a root `/bin/sh -c` prelude, the sshd `SetEnv` list and the
/// container's `mount_path`. `/v1` checks it, but a Workspace written by any other path — a
/// restored backup, a migration, an operator with kubectl — reaches this builder directly,
/// which is exactly why `git_init_container` and `service_statefulset` both re-check.
#[test]
fn workspace_pod_refuses_a_name_that_is_not_a_name() {
    let ctx = PodContext {
        pool: "/wspool",
        node_name: "node-a",
        owner_ref: owner_ref(),
        runtime_class: None,
        default_image: "img:1",
        system: None,
        registry_host: "registry.kloudlite.io",
    };
    for hostile in ["../../etc", "a; touch /pwned", "", "..", "x'\nchown 0 /", &"n".repeat(64)] {
        let spec: crate::crd::WorkspaceSpec = serde_json::from_value(serde_json::json!({
            "owner": "alice", "team": "", "name": hostile, "region": "r1",
            "image": "", "packages": [], "desiredState": "running",
        }))
        .unwrap();
        assert!(workspace_pod(&spec, "vol-1", "ws-1", &ctx, None).is_err(), "accepted {hostile:?}");
    }
}

/// The init image is root and never reads anything but `id_ed25519` — least privilege says
/// it should not be ABLE to read `registry-token` even though it never would, and it must not
/// share a volume name with the main container's unrestricted view of the same Secret.
#[test]
fn only_the_seed_key_is_visible_to_the_git_seed_container() {
    let source = crate::crd::VolumeSource::GitRepo { repo: "acme/dev".into(), branch: "main".into() };
    let init = git_init_container(&source, "alpine/git:1", "git.khost.dev", "22").unwrap().unwrap();
    let pod = workspace_pod(&ws_spec(), "vol-1", "ws-1", &ctx(), Some(init)).unwrap();
    let s = pod.spec.unwrap();

    let seed_mount = s.init_containers.unwrap()[0].volume_mounts.clone().unwrap();
    let seed_vol_name = seed_mount.iter().find(|m| m.mount_path == USER_KEY_PATH).unwrap().name.clone();
    assert_ne!(seed_vol_name, "user-key", "the init container must not share the main container's volume");

    let volumes = s.volumes.unwrap();
    let seed_vol = volumes.iter().find(|v| v.name == seed_vol_name).unwrap();
    let items = seed_vol.secret.as_ref().unwrap().items.as_ref().expect("scoped by items");
    assert_eq!(items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), vec!["id_ed25519"]);

    // The main container's own mount is untouched: still the full Secret, no `items`.
    let main_mount = s.containers[0].volume_mounts.clone().unwrap();
    let main_vol_name = main_mount.iter().find(|m| m.mount_path == USER_KEY_PATH).unwrap().name.clone();
    assert_eq!(main_vol_name, "user-key");
    let main_vol = volumes.iter().find(|v| v.name == "user-key").unwrap();
    assert!(main_vol.secret.as_ref().unwrap().items.is_none());
}

/// The ordinary name still builds, and still mounts where it always did.
#[test]
fn workspace_pod_accepts_a_real_name() {
    let ctx = PodContext {
        pool: "/wspool",
        node_name: "node-a",
        owner_ref: owner_ref(),
        runtime_class: None,
        default_image: "img:1",
        system: None,
        registry_host: "registry.kloudlite.io",
    };
    let spec: crate::crd::WorkspaceSpec = serde_json::from_value(serde_json::json!({
        "owner": "alice", "team": "", "name": "my-ws", "region": "r1",
        "image": "", "packages": [], "desiredState": "running",
    }))
    .unwrap();
    let pod = workspace_pod(&spec, "vol-1", "ws-1", &ctx, None).expect("a real name builds");
    let mounts = pod.spec.unwrap().containers[0].volume_mounts.clone().unwrap();
    assert!(mounts.iter().any(|m| m.mount_path == workspace_dir("my-ws")));
}

/// ONE peer, always. `namespaceSelector` and `podSelector` in one element of `from`/`to` is an
/// AND; split across two elements it is an OR, and every sshd in the cluster becomes reachable
/// by any pod that labels itself correctly. The functions say so; this is what holds them to it.
#[test]
fn every_grant_ands_its_namespace_and_pod_selectors_in_one_peer() {
    let r = owner_ref();
    let cases: Vec<(&str, NetworkPolicy, &str)> = vec![
        ("attach_ingress", attach_ingress("env-1", "ws-alice", "ws-1", "alice", &r), "ingress"),
        ("allow_gateway_ingress", allow_gateway_ingress("ws-alice", "alice", &r), "ingress"),
        ("attach_egress", attach_egress("ws-alice", "ws-1", "env-1", "alice", &r), "egress"),
    ];
    for (name, pol, dir) in cases {
        let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
        let rules = spec[dir].as_array().unwrap_or_else(|| panic!("{name}: no {dir}"));
        assert_eq!(rules.len(), 1, "{name}: one rule");
        let peers = rules[0][if dir == "ingress" { "from" } else { "to" }].as_array().unwrap();
        assert_eq!(peers.len(), 1, "{name}: two peers is an OR, not an AND: {peers:?}");
        // And the selectors that must be there ARE there — a single peer with only a
        // namespaceSelector would pass the count above while opening the whole namespace.
        if name != "attach_egress" {
            assert!(peers[0].get("podSelector").is_some(), "{name}: no podSelector");
        } else {
            // Its peer has no podSelector (it targets a whole namespace), so the scoping to
            // this one workspace pod is the policy's own top-level podSelector instead.
            assert_eq!(spec["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
        }
        assert!(peers[0].get("namespaceSelector").is_some(), "{name}: no namespaceSelector");
    }
}

/// The gateway hole is port 22 and nothing else, from the gateway namespace and nothing else.
#[test]
fn the_gateway_hole_is_one_port_from_one_place() {
    let pol = allow_gateway_ingress("ws-alice", "alice", &owner_ref());
    let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
    let rule = &spec["ingress"][0];
    assert_eq!(rule["ports"], serde_json::json!([{"protocol": "TCP", "port": 22}]));
    assert_eq!(
        rule["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
        GATEWAY_NAMESPACE
    );
    assert_eq!(rule["from"][0]["podSelector"]["matchLabels"]["app"], "kloudlite-gateway");
}

/// `169.254.0.0/16` is the one that matters: on Azure `169.254.169.254` hands out the NODE's
/// managed identity to anything that asks, which is a full escape from the cluster. RFC 1918
/// covers pod, service and node networks without this code knowing their numbers.
#[test]
fn internet_egress_excludes_the_metadata_service_and_all_of_rfc_1918() {
    let pol = allow_internet_egress("ws-alice", "alice", &owner_ref());
    let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
    let block = &spec["egress"][0]["to"][0]["ipBlock"];
    assert_eq!(block["cidr"], "0.0.0.0/0");
    let except: Vec<String> =
        block["except"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    for want in ["169.254.0.0/16", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
        assert!(except.contains(&want.to_string()), "{want} is not excluded: {except:?}");
    }
    assert_eq!(spec["egress"].as_array().unwrap().len(), 1, "one rule; a second would union it open");
}
