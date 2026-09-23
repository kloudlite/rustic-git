//! Tests for what a bench workspace adds to an ordinary workspace pod: the `harness-bench`
//! container, the tool-token Secret and the gateway-only hole to `BENCH_PORT`.

use super::*;

#[test]
fn bench_tool_secret_carries_token_and_exp_only() {
    let s = crate::k8s::bench_tool_secret("wt-alice-acme", "tok", 42);
    assert_eq!(s.metadata.name.as_deref(), Some(crate::k8s::BENCH_TOOL_SECRET));
    assert_eq!(s.metadata.namespace.as_deref(), Some("wt-alice-acme"));
    let data = s.string_data.unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data["token"], "tok");
    let ann = s.metadata.annotations.unwrap();
    assert_eq!(ann.len(), 1);
    assert_eq!(ann["kloudlite.io/exp"], "42");
    assert!(s.metadata.labels.is_none() && s.metadata.owner_references.is_none());
}


// --- a bench IS a workspace: the second container of an ordinary workspace pod ---

fn bench_ws_spec() -> WorkspaceSpec {
    WorkspaceSpec {
        team: "acme".into(),
        name: "bench".into(),
        bench: Some(crate::crd::BenchOptions { model: "sonnet".into(), wake_at: None }),
        ..ws_spec()
    }
}


/// NOTHING in a bench pod chowns its worktree, so the worktree must arrive owned by the tenant.
///
/// An ordinary workspace pod gets away with a root-owned one because its `workspace` container's
/// prelude runs `chown -Rh 1000 {HOME_DIR}` on every start. A bench pod has no workspace
/// container — `sessions` + `shell` since spec §2.2 — so that prelude never runs, and the worktree
/// `btrfs subvolume create` left owned by root stayed that way: `harness-bench` died on
/// `EACCES: mkdir '/home/kl/workspaces/bench/.bench'` and every team bench crash-looped
/// (2026-09-18). `Engine::checkout` hands a fresh subvolume to uid 1000 now.
///
/// This test is the guard on the ASSUMPTION rather than on the chown itself: if a prelude ever
/// comes back to a bench pod, or the containers change again, whoever does it should see why the
/// engine-side chown exists.
#[test]
fn a_bench_pod_has_no_prelude_to_chown_its_worktree() {
    let spec = bench_ws_spec();
    let p = workspace_pod(&spec, "ws-1", "bench-1", &ctx(), None, Some(("cr.example/bench:v9", 420, ""))).unwrap();
    let pod = p.spec.unwrap();
    // The bench's own container runs the binary directly — no shell, so no seeding of any kind.
    let sessions = pod.containers.iter().find(|c| c.name == "sessions").expect("the sessions container");
    assert_eq!(sessions.command.as_ref().unwrap()[0], "harness-bench");
    // And no container on this pod chowns anything, nor is there an init container that could.
    for c in pod.containers.iter().chain(pod.init_containers.iter().flatten()) {
        let argv = c.command.clone().unwrap_or_default().join(" ") + " " + &c.args.clone().unwrap_or_default().join(" ");
        assert!(!argv.contains("chown"), "{} chowns something; the engine-side chown may be redundant", c.name);
    }
    // The worktree it must be able to write is the `live` mount, which IS the home.
    let dir = crate::k8s::HOME_DIR;
    let live = sessions
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.name == "live")
        .expect("the bench holds its own worktree");
    assert_eq!(live.mount_path, dir);
    // `.bench` is made INSIDE it by the harness, as uid 1000 — the mkdir that was refused.
    assert_eq!(sessions.command.as_ref().unwrap()[2], format!("{dir}/{}", crate::k8s::BENCH_SUBDIR));
}


#[test]
fn a_bench_pod_carries_both_containers_and_the_tool_secret_optional() {
    let p = workspace_pod(&bench_ws_spec(), "ws-1", "bench-1", &ctx(), None, Some(("cr.example/bench:v9", 420, ""))).unwrap();
    assert_eq!(p.metadata.labels.as_ref().unwrap()[KIND_LABEL], "bench");
    let spec = p.spec.unwrap();
    let names: Vec<&str> = spec.containers.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["sessions", "shell"], "a bench pod is the sessions container and a terminal, nothing else");
    // The pod stays a workspace pod: sshd keeps it alive, so an exited bench container is the
    // kubelet's to restart, not a pod phase.
    assert_eq!(spec.restart_policy.as_deref(), Some("Always"));

    let v = spec.volumes.as_ref().unwrap().iter().find(|v| v.name == "bench-tool").expect("volume");
    let sv = v.secret.clone().unwrap();
    assert_eq!(sv.secret_name.as_deref(), Some(crate::k8s::BENCH_TOOL_SECRET));
    assert_eq!(sv.optional, Some(true));
    assert_eq!(sv.default_mode, Some(0o444));
    assert!(spec.volumes.as_ref().unwrap().iter().any(|v| v.name == "tmp"));

    let c = &spec.containers[0];
    assert_eq!(c.image.as_deref(), Some("cr.example/bench:v9"));
    assert_eq!(
        c.command.as_deref().unwrap(),
        ["harness-bench", "--dir", "/home/kl/.bench", "--idle-secs", "420"],
        "the session folder is inside the btrfs subvolume, so it is snapshotted with the workspace"
    );
    let m = |name: &str| c.volume_mounts.as_ref().unwrap().iter().find(|m| m.name == name).cloned().expect(name);
    assert_eq!(m("live").mount_path, crate::k8s::HOME_DIR, "the bench's own volume, where `.bench/` lives");
    assert_eq!(m("bench-tool").read_only, Some(true));
    assert_eq!(m("user-key").read_only, Some(true));
    // The SHELL beside it gets a scratch home of its own, never the worktree: no code in a shell.
    let shell = &spec.containers[1];
    assert_eq!(shell.name, crate::k8s::SHELL_CONTAINER);
    let sm = |name: &str| shell.volume_mounts.as_ref().unwrap().iter().find(|m| m.name == name).cloned().expect(name);
    assert_eq!(sm("shell-home").mount_path, crate::k8s::HOME_DIR);
    assert!(shell.volume_mounts.as_ref().unwrap().iter().all(|m| m.name != "live"), "the shell must not see the worktree");
    assert_eq!(shell.ports.as_ref().unwrap()[0].container_port, crate::k8s::SHELL_PORT as i32);
    // sshd is the other container's, so the one capability it needs stays dropped here.
    let caps = c.security_context.as_ref().unwrap().capabilities.clone().unwrap();
    assert!(!caps.add.unwrap().contains(&"SYS_CHROOT".to_string()));
    assert_eq!(c.readiness_probe.as_ref().unwrap().period_seconds, Some(2));
    // What the SCHEDULER packs against: a node process, not a second workspace. The two
    // containers together used to request 4 CPU, and a second bench on an 8-core node sat
    // `Pending`/`Insufficient cpu` with every `bench.*` probe timing out (fleet, 2026-09-17).
    let r = c.resources.as_ref().unwrap();
    let req = r.requests.as_ref().unwrap();
    let lim = r.limits.as_ref().unwrap();
    assert_eq!(req["cpu"].0, "250m");
    assert_eq!(req["memory"].0, "512Mi");
    assert_eq!(req["ephemeral-storage"].0, "512Mi");
    // The burst a turn may take is still a real one.
    assert_eq!(lim["cpu"].0, "2");
    assert_eq!(lim["memory"].0, "4Gi");
    assert_eq!(lim["ephemeral-storage"].0, "2Gi");
    // And the SHELL beside it is a terminal's worth and no more: it is on every pod in the fleet,
    // so its request is multiplied by all of them.
    let sh = spec.containers[1].resources.as_ref().unwrap().requests.as_ref().unwrap();
    assert_eq!(sh["cpu"].0, crate::model::shell_container_resources().cpu_request);
    assert_eq!(sh["memory"].0, crate::model::shell_container_resources().memory_request);

    let get = |n: &str| c.env.as_ref().unwrap().iter().find(|e| e.name == n).and_then(|e| e.value.clone());
    assert_eq!(get("KL_TOOL_TOKEN_FILE").as_deref(), Some("/etc/kloudlite/bench-tool/token"));
    assert_eq!(get("KL_WORKSPACE_ID").as_deref(), Some("bench-1"));
    assert_eq!(get("KL_WORKSPACE").as_deref(), Some(crate::k8s::WORKSPACE_DIR));
    assert_eq!(get("KL_MODEL").as_deref(), Some("sonnet"));
    assert_eq!(get("KL_BENCH_IDLE_SECS").as_deref(), Some("420"));
    // pi's state directory, under `.bench/` in the bench's own volume, so the keys travel with it.
    assert_eq!(
        get("PI_CODING_AGENT_DIR").as_deref(),
        Some("/home/kl/.bench/pi")
    );
    // The pod's own address, from the downward API: the bench dials the shell sidecar beside it at
    // `{KL_POD_IP}:7790`, and without it the splice would look for a terminal on loopback and find
    // none (harness 067661a8).
    let pod_ip = c.env.as_ref().unwrap().iter().find(|e| e.name == "KL_POD_IP").expect("KL_POD_IP");
    assert_eq!(
        pod_ip.value_from.as_ref().and_then(|f| f.field_ref.as_ref()).map(|f| f.field_path.as_str()),
        Some("status.podIP")
    );
    assert!(pod_ip.value.is_none(), "an address is the kubelet's to fill in, never a literal");
    assert!(c.env.as_ref().unwrap().iter().all(|e| !e.value.as_deref().unwrap_or_default().contains("eyJ")), "no token in env");
}


#[test]
fn only_the_gateway_may_reach_a_bench_workspace_pod() {
    let np = allow_gateway_bench("wt-alice-acme", "bench-1");
    assert_eq!(np.metadata.name.as_deref(), Some("allow-gateway-bench"));
    let spec = np.spec.unwrap();
    let sel = spec.pod_selector.unwrap().match_labels.unwrap();
    assert_eq!(sel[WORKSPACE_LABEL], "bench-1");
    assert_eq!(sel[KIND_LABEL], "bench", "a sibling workspace in the same namespace gets no hole");
    let rule = &spec.ingress.unwrap()[0];
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(BENCH_PORT as i32)));
    assert_eq!(rule.from.as_ref().unwrap().len(), 1);
}
