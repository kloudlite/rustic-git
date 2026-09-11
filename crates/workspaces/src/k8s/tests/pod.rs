//! The workspace Pod: volumes, placement, sandbox, security context, sshd, keys, names.

use super::*;


/// Storage is mounted from the node, not claimed. Every source carries an explicit `type`: an
/// untyped hostPath creates a missing path as an empty directory, which is a wiped workspace
/// rather than a failed mount.
#[test]
pub(crate) fn every_volume_is_a_typed_host_path() {
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
pub(crate) fn the_default_image_is_ready_only_once_sshd_listens() {
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
pub(crate) fn a_workspace_pods_host_paths_match_the_agents_layout() {
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
pub(crate) fn a_workspace_pods_live_mount_is_the_worktree_path() {
    let p = workspace_pod(&ws_spec(), "vol-1", "ws-1", &ctx(), None).unwrap();
    let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    let live = vols.iter().find(|v| v.name == "live").unwrap();
    assert_eq!(live.host_path.as_ref().unwrap().path, format!("{}/vol/vol-1/live/ws-1", ctx().pool));
    assert_eq!(live.host_path.as_ref().unwrap().type_.as_deref(), Some("Directory"));
}


/// Placement is the pod's own now that no PV carries node affinity, and it is ADDED to the
/// pool selector rather than replacing it.
#[test]
pub(crate) fn the_pod_selects_its_node_by_hostname() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let s = p.spec.unwrap();
    let sel = s.node_selector.expect("a node selector");
    assert_eq!(sel.get("kubernetes.io/hostname").map(String::as_str), Some("session-0"));
    assert_eq!(sel.get("kloudlite.io/pool").map(String::as_str), Some("true"));
    assert!(s.node_name.is_none(), "the scheduler still places the pod");
}


#[test]
pub(crate) fn a_pod_is_pinned_to_its_node_and_to_the_pool_and_nothing_else() {
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


#[test]
pub(crate) fn the_user_key_secret_carries_the_private_key_the_git_identity_and_the_old_keys_entry() {
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
pub(crate) fn the_registry_token_verifies_as_the_owner() {
    let jwt = kloudlite_core::jwt::Jwt::new("test-secret-that-is-at-least-32-bytes-long").unwrap();
    let token = jwt.mint_registry("alice", "*", 86_400).unwrap();
    assert_eq!(jwt.verify_registry(&token), Some("alice".to_string()));
}


/// A team's members share one namespace and therefore ONE keys file — the pod must mount the
/// team's, not the individual's, or a teammate's key would not open the workspace they share.
#[test]
pub(crate) fn a_teams_pod_mounts_the_teams_keys() {
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


/// Tenants share a node, so they share its kernel. A sandbox runtime puts a userspace kernel
/// between the tenant and the host one — the only thing here that turns a kernel exploit from
/// a host compromise into a sandbox escape.
///
/// Opt-in: a `runtimeClassName` naming a runtime the node lacks makes every pod fail to start,
/// so a cluster without gVisor installed must keep working.
#[test]
pub(crate) fn tenant_pods_run_under_the_sandbox_when_one_is_configured() {
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
pub(crate) fn no_pod_this_module_builds_uses_a_claim() {
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
pub(crate) fn a_user_pod_cannot_reach_the_api_server_or_escalate() {
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
pub(crate) fn pod_requests_match_the_capacity_model() {
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


/// Three things have to line up for git in a workspace to authenticate, and each fails
/// silently on its own: the mount, the 0400 mode ssh insists on, and the env var that tells
/// git which key to use.
#[test]
pub(crate) fn a_workspace_pod_carries_the_owners_platform_key() {
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
pub(crate) fn tenant_pods_reference_the_namespace_pull_secret() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let refs = p.spec.unwrap().image_pull_secrets.unwrap();
    assert_eq!(refs[0].name, PULL_SECRET);

    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx()).unwrap();
    let refs = d.spec.unwrap().template.spec.unwrap().image_pull_secrets.unwrap();
    assert_eq!(refs[0].name, PULL_SECRET, "an env's services are where private images show up");
}


#[test]
pub(crate) fn a_workspace_pod_mounts_the_store_and_only_its_own_profile_read_only() {
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
pub(crate) fn a_workspace_pod_mounts_its_volume_at_workspace_and_only_there() {
    let p = workspace_pod(&ws_spec(), "ws-1", "ws-1", &ctx(), None).unwrap();
    let s = p.spec.unwrap();
    let claims = s.volumes.as_ref().unwrap().iter().filter(|v| v.name == "live" && v.host_path.is_some());
    assert_eq!(claims.count(), 1);
    let mounts = s.containers[0].volume_mounts.as_ref().unwrap();
    // One mount of the WHOLE subvolume, at the workspace path; every other `live` mount is a
    // `.cache/` subPath (the registry, the editor servers) and never the root again.
    let whole: Vec<_> = mounts.iter().filter(|m| m.name == "live" && m.sub_path.is_none()).collect();
    assert_eq!(whole.len(), 1, "the nginx web-root mount is gone with nginx");
    assert!(mounts.iter().all(|m| m.name != "live" || m.sub_path.is_none() || m.sub_path.as_deref().unwrap().starts_with(".cache/")));
    assert!(mounts.iter().any(|m| m.mount_path == "/home/kl/workspaces/dev" && m.read_only.is_none()));
}


/// The home is a PV mounted at `/home/kl` and the workspace subvolume a PV mounted INSIDE it;
/// the kubelet orders mounts by path depth, so the paths carry the order. The ssh Secret
/// mounts under `/home/kl/.ssh` land inside the home too — a Secret inside a PV is fine.
#[test]
pub(crate) fn a_workspace_pod_mounts_the_home_and_the_workspace_inside_it() {
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
pub(crate) fn a_clones_pod_is_named_after_the_workspace_not_the_shared_volume() {
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
pub(crate) fn the_home_is_the_shared_nfs_path_and_caches_are_local() {
    let pod = workspace_pod(&ws_spec(), "vol-1", "ws-1", &ctx(), None).unwrap();
    let s = pod.spec.unwrap();
    let vols = s.volumes.unwrap();
    let path = |n: &str| vols.iter().find(|v| v.name == n).unwrap().host_path.as_ref().unwrap().path.clone();
    assert_eq!(path("home"), format!("{}/homes/{}", ctx().pool, ws_spec().owner));
    assert_eq!(path("homecache"), format!("{}/homecache/{}", ctx().pool, ws_spec().owner));
    let mounts = s.containers[0].volume_mounts.clone().unwrap();
    let sub = |mp: &str| mounts.iter().find(|m| m.mount_path == mp).map(|m| (m.name.clone(), m.sub_path.clone()));
    assert_eq!(sub(HOME_CACHE_DIR), Some(("homecache".into(), Some("cache".into()))));
    for (path, dir) in [("/home/kl/.cargo/registry", "cargo-registry"), ("/home/kl/.vscode-server", "vscode-server"), ("/home/kl/.cursor-server", "cursor-server"), ("/home/kl/.zed_server", "zed-server"), ("/home/kl/.windsurf-server", "windsurf-server"), ("/home/kl/.jetbrains", "jetbrains")] {
        assert_eq!(sub(path), Some(("live".into(), Some(format!(".cache/{dir}")))), "{path} travels with the tree");
    }
    assert_eq!(sub(HOME_STATE_DIR), Some(("homecache".into(), Some("state".into()))));
}


#[test]
pub(crate) fn the_login_env_redirects_every_cache_and_pins_histfile_local() {
    let env = login_env("ws-1", "acme", "registry.kloudlite.io");
    let get = |n: &str| env.iter().find(|e| e.name == n).unwrap().value.clone().unwrap();
    assert_eq!(get("HISTFILE"), format!("{HOME_STATE_DIR}/shell_history"));
    // Every cache lives WITH the workspace, under `{ws}/.cache/`, so a clone or a restore arrives
    // warm — and never at a tool's own `./target`, which a repository may version.
    let ws = workspace_dir("ws-1");
    for (var, sub) in [
        ("XDG_CACHE_HOME", "xdg"), ("npm_config_cache", "npm"), ("PNPM_STORE_DIR", "pnpm"), ("BUN_INSTALL_CACHE_DIR", "bun"),
        ("RUSTUP_HOME", "rustup"), ("GOMODCACHE", "gomod"), ("UV_CACHE_DIR", "uv"), ("PIP_CACHE_DIR", "pip"),
        ("DENO_DIR", "deno"), ("YARN_CACHE_FOLDER", "yarn"), ("COMPOSER_CACHE_DIR", "composer"), ("NUGET_PACKAGES", "nuget"),
    ] {
        assert_eq!(get(var), format!("{ws}/.cache/{sub}"), "{var}");
    }
    assert_eq!(get("MAVEN_OPTS"), format!("-Dmaven.repo.local={ws}/.cache/m2"));
    // Only what must not travel stays node-local.
    assert_eq!(get("TMPDIR"), format!("{HOME_CACHE_DIR}/tmp"));
    assert_eq!(get("CARGO_TARGET_DIR"), format!("{ws}/.cache/cargo-target"));
    assert_eq!(get("GOCACHE"), format!("{ws}/.cache/go-build"));
    assert_eq!(get("PLAYWRIGHT_BROWSERS_PATH"), format!("{ws}/.cache/ms-playwright"));
    // Config, the home's half: cargo and gradle credentials, a person's GOPATH/src.
    assert_eq!(get("GRADLE_USER_HOME"), format!("{HOME_DIR}/.gradle"));
    for var in ["CARGO_HOME", "GOPATH"] {
        assert!(env.iter().all(|e| e.name != var), "{var} must stay on the shared home");
    }
    assert_eq!(get("DO_NOT_TRACK"), "1");
}

/// The global git ignore: appended to the person's own file exactly once, never a per-repo line.
#[test]
pub(crate) fn the_prelude_appends_the_global_git_ignore_once() {
    let prelude = prelude("ws-1");
    assert!(prelude.contains("grep -qF '# kloudlite: derived state' $H/.config/git/ignore 2>/dev/null || cat /etc/kloudlite/gitignore-global >> $H/.config/git/ignore"), "{prelude}");
    let shipped = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/workspace-image/gitignore-global")).unwrap();
    assert_eq!(shipped, "# kloudlite: derived state the platform places inside a workspace directory\n.cache/\ngraft/\n.direnv/\n");
}


/// Four things have to line up for `ssh kl@workspace` to work, and each fails silently on
/// its own: sshd as the container's process, its host key, the owner's authorized_keys where
/// the config says to look, and the modes sshd refuses to start (or to authenticate) without.
#[test]
pub(crate) fn the_default_image_runs_sshd_with_its_own_host_key_and_the_owners_keys() {
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
pub(crate) fn a_custom_image_keeps_its_entrypoint_and_gets_no_sshd() {
    let mut spec = ws_spec();
    spec.image = "ghcr.io/acme/dev:1".into();
    let s = workspace_pod(&spec, "ws-1", "ws-1", &ctx(), None).unwrap().spec.unwrap();
    assert!(s.containers[0].command.is_none(), "a user image keeps its entrypoint");
    assert!(s.containers[0].ports.is_none());
    assert!(s.volumes.as_ref().unwrap().iter().all(|v| v.name != "ws-ssh" && v.name != "authorized-keys"));
}


/// The host key Secret is per workspace and dies with it — a clone gets its own.
#[test]
pub(crate) fn a_workspaces_host_key_lives_and_dies_with_it() {
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


#[test]
pub(crate) fn every_child_object_cascades_on_delete() {
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


/// `spec.name` is spliced into a root `/bin/sh -c` prelude, the sshd `SetEnv` list and the
/// container's `mount_path`. `/v1` checks it, but a Workspace written by any other path — a
/// restored backup, a migration, an operator with kubectl — reaches this builder directly,
/// which is exactly why `git_init_container` and `service_statefulset` both re-check.
#[test]
pub(crate) fn workspace_pod_refuses_a_name_that_is_not_a_name() {
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
pub(crate) fn only_the_seed_key_is_visible_to_the_git_seed_container() {
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
pub(crate) fn workspace_pod_accepts_a_real_name() {
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
