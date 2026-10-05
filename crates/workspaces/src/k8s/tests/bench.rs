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


// --- a bench IS a workspace, but its ONE container, not a workspace pod's shape ---

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
/// container and no shell at all (owner ruling 2026-09-25) — `sessions` is its only container —
/// so that prelude never runs, and the worktree
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
    let p = workspace_pod(&spec, "ws-1", "bench-1", &ctx(), None, Some(("cr.example/bench:v9", 420))).unwrap();
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
fn a_bench_pod_carries_its_one_container_and_the_tool_secret_optional() {
    let p = workspace_pod(&bench_ws_spec(), "ws-1", "bench-1", &ctx(), None, Some(("cr.example/bench:v9", 420))).unwrap();
    assert_eq!(p.metadata.labels.as_ref().unwrap()[KIND_LABEL], "bench");
    let spec = p.spec.unwrap();
    let names: Vec<&str> = spec.containers.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["sessions"], "a bench pod is the sessions container, nothing else (owner ruling 2026-09-25: no shell)");
    // The pod stays a workspace pod: sshd would keep an ordinary one alive, but a bench pod has no
    // sshd either — the kubelet restarts the exited bench container itself, not a pod phase.
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
    // No second container: no shell beside it, no ttyd port, no sidecar home mount.
    assert_eq!(spec.containers.len(), 1);
    // sshd never runs on a bench pod, so the one capability it needs stays dropped here too.
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
    // No `KL_POD_IP` any more (owner ruling 2026-09-25): there is no shell sidecar to dial, so a
    // downward-API pod address would have no reader.
    assert!(c.env.as_ref().unwrap().iter().all(|e| e.name != "KL_POD_IP"), "no shell sidecar left to dial");
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
