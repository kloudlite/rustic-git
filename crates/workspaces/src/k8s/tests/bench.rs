//! Tests for the bench pod builder: its own folder, no worktree, the read-only harness for a
//! departed member, and the gateway-only ingress policy.

use super::*;
use crate::crd::{Bench, BenchAccess, BenchSpec, BenchStatus};
use crate::k8s::{bench_folder, bench_ingress_policy, bench_pod};

fn fixture_bench(owner: &str, team: &str, state: DesiredState) -> Bench {
    let mut b = Bench::new(
        "bench-1",
        BenchSpec {
            owner: owner.to_string(),
            team: team.to_string(),
            image: "cr.example/bench:latest".to_string(),
            model: "sonnet".to_string(),
            desired_state: state,
            access: BenchAccess::Full,
            wake_at: None,
            resources: Default::default(),
            attached_environment: None,
        },
    );
    b.status = Some(BenchStatus { node_name: "n1".to_string(), ..Default::default() });
    b
}

#[test]
fn a_bench_pod_mounts_only_its_own_folder_and_no_worktree() {
    let b = fixture_bench("alice", "acme", DesiredState::Running);
    let p = bench_pod(&b, "bench-1", "/wspool", None, "cr.example", "https://api.example", 300).unwrap();
    assert_eq!(p.metadata.namespace.as_deref(), Some(crate::crd::ws_namespace("alice", "acme").as_str()));
    let spec = p.spec.unwrap();
    let paths: Vec<String> = spec.volumes.as_ref().unwrap().iter()
        .filter_map(|v| v.host_path.as_ref().map(|h| h.path.clone())).collect();
    assert!(paths.contains(&"/wspool/homes/.benches/acme/alice".to_string()));
    assert!(paths.contains(&"/wspool/homes/alice".to_string()));
    assert!(!paths.iter().any(|p| p.contains("/vol/") || p.contains("homecache") || p.ends_with("/.benches") || p.ends_with("/acme")));
    let c = &spec.containers[0];
    assert!(matches!(c.command.as_deref(), Some([sh, dash_c, p]) if sh == "/bin/sh" && dash_c == "-c" && p.ends_with("exec harness-bench\n")));
    assert!(c.volume_mounts.as_ref().unwrap().iter().any(|m| m.mount_path == "/bench"));
    assert_eq!(c.readiness_probe.as_ref().unwrap().timeout_seconds, Some(3), "a slow Node start must not flap the bench unready");
}

#[test]
fn every_bench_runs_the_harness_and_may_exit_idle() {
    let b = fixture_bench("alice", "acme", DesiredState::Running);
    let spec = bench_pod(&b, "bench-1", "/wspool", None, "cr", "", 420).unwrap().spec.unwrap();
    let c = &spec.containers[0];
    let cmd = c.command.as_ref().unwrap();
    assert_eq!(cmd[0], "/bin/sh");
    // `exec`, so harness-bench is pid 1 and gets the kubelet's TERM — and so an idle exit 0 is
    // its own, not a shell's.
    assert!(cmd[2].ends_with("exec harness-bench\n"), "{}", cmd[2]);
    // Seeded once: the home is persistent, so a person's own edits survive the next bench.
    assert!(cmd[2].contains("[ -e /home/kl/.config/zsh/.zshrc ] ||"), "{}", cmd[2]);
    assert!(cmd[2].contains("starship init zsh"), "{}", cmd[2]);
    assert_eq!(spec.restart_policy.as_deref(), Some("OnFailure"), "exit 0 is idle and must not restart");
    let idle = c.env.as_ref().unwrap().iter().find(|e| e.name == "KL_BENCH_IDLE_SECS").unwrap();
    assert_eq!(idle.value.as_deref(), Some("420"));
}

#[test]
fn a_folder_segment_that_escapes_is_refused_before_it_becomes_a_hostpath() {
    assert!(bench_folder("/wspool", "..", "alice").is_err());
    assert!(bench_folder("/wspool", "acme", "a/b").is_err());
    assert!(bench_folder("/wspool", "acme", ".").is_err());
    assert!(bench_pod(&fixture_bench("alice", "../x", DesiredState::Running), "b", "/wspool", None, "cr", "", 300).is_err());
}

#[test]
fn only_the_gateway_may_reach_the_bench_port() {
    let np = bench_ingress_policy("ws-alice", "bench-1");
    let spec = np.spec.unwrap();
    assert_eq!(spec.pod_selector.unwrap().match_labels.unwrap()[WORKSPACE_LABEL], "bench-1");
    let rule = &spec.ingress.unwrap()[0];
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(BENCH_PORT as i32)));
    assert_eq!(rule.from.as_ref().unwrap().len(), 1);
}

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


#[test]
fn a_bench_pod_mounts_the_tool_secret_optional_and_read_only() {
    let spec = bench_pod(&fixture_bench("alice", "acme", DesiredState::Running), "bench-1", "/wspool", None, "cr", "https://api.example", 300).unwrap().spec.unwrap();
    let v = spec.volumes.unwrap().into_iter().find(|v| v.name == "bench-tool").expect("volume");
    let sv = v.secret.unwrap();
    assert_eq!(sv.secret_name.as_deref(), Some(crate::k8s::BENCH_TOOL_SECRET));
    assert_eq!(sv.optional, Some(true));
    assert_eq!(sv.default_mode, Some(0o444));
    let m = spec.containers[0].volume_mounts.as_ref().unwrap().iter().find(|m| m.name == "bench-tool").expect("mount").clone();
    assert_eq!(m.mount_path, "/etc/kloudlite/bench-tool");
    assert_eq!(m.read_only, Some(true));
}

#[test]
fn a_bench_pod_carries_only_the_token_path_in_env() {
    let b = fixture_bench("alice", "acme", DesiredState::Running);
    let env = bench_pod(&b, "bench-1", "/wspool", None, "cr", "https://api.example", 300).unwrap().spec.unwrap().containers[0].env.clone().unwrap();
    assert!(env.iter().all(|e| !e.value.as_deref().unwrap_or_default().contains("eyJ")), "no token in env");
    let get = |n: &str| env.iter().find(|e| e.name == n).and_then(|e| e.value.clone());
    assert_eq!(get("KL_TOOL_TOKEN_FILE").as_deref(), Some("/etc/kloudlite/bench-tool/token"));
    assert_eq!(get("KL_API_URL").as_deref(), Some("https://api.example"));
    // The image's uid-1000 passwd shell is bash; zsh is what the prompt and `/etc/zsh/zshrc` are
    // built for, so the bench names it rather than leaving `/pty` to the passwd default.
    assert_eq!(get("SHELL").as_deref(), Some("/bin/zsh"));
    assert_eq!(get("ZDOTDIR").as_deref(), Some("/home/kl/.config/zsh"));
    assert_eq!(get("HISTFILE").as_deref(), Some("/home/kl/.local/state/zsh_history"));
    let bare = bench_pod(&b, "bench-1", "/wspool", None, "cr", "", 300).unwrap().spec.unwrap().containers[0].env.clone().unwrap();
    assert!(!bare.iter().any(|e| e.name == "KL_API_URL"), "no api url means unset, so tools fail closed");
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


#[test]
fn a_bench_pod_carries_both_containers_and_the_tool_secret_optional() {
    let p = workspace_pod(&bench_ws_spec(), "ws-1", "bench-1", &ctx(), None, Some(("cr.example/bench:v9", 420))).unwrap();
    assert_eq!(p.metadata.labels.as_ref().unwrap()[KIND_LABEL], "bench");
    let spec = p.spec.unwrap();
    let names: Vec<&str> = spec.containers.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["workspace", "bench"], "the bench rides alongside sshd, it does not replace it");
    // The pod stays a workspace pod: sshd keeps it alive, so an exited bench container is the
    // kubelet's to restart, not a pod phase.
    assert_eq!(spec.restart_policy.as_deref(), Some("Always"));

    let v = spec.volumes.as_ref().unwrap().iter().find(|v| v.name == "bench-tool").expect("volume");
    let sv = v.secret.clone().unwrap();
    assert_eq!(sv.secret_name.as_deref(), Some(crate::k8s::BENCH_TOOL_SECRET));
    assert_eq!(sv.optional, Some(true));
    assert_eq!(sv.default_mode, Some(0o444));
    assert!(spec.volumes.as_ref().unwrap().iter().any(|v| v.name == "tmp"));

    let c = &spec.containers[1];
    assert_eq!(c.image.as_deref(), Some("cr.example/bench:v9"));
    assert_eq!(
        c.command.as_deref().unwrap(),
        ["harness-bench", "--dir", "/home/kl/workspaces/bench/.bench", "--idle-secs", "420"],
        "the session folder is inside the btrfs subvolume, so it is snapshotted with the workspace"
    );
    let m = |name: &str| c.volume_mounts.as_ref().unwrap().iter().find(|m| m.name == name).cloned().expect(name);
    assert_eq!(m("live").mount_path, workspace_dir("bench"), "the same path the workspace container sees");
    assert_eq!(m("bench-tool").read_only, Some(true));
    assert_eq!(m("user-key").read_only, Some(true));
    assert_eq!(m("home").mount_propagation.as_deref(), Some("HostToContainer"));
    // sshd is the other container's, so the one capability it needs stays dropped here.
    let caps = c.security_context.as_ref().unwrap().capabilities.clone().unwrap();
    assert!(!caps.add.unwrap().contains(&"SYS_CHROOT".to_string()));
    assert_eq!(c.readiness_probe.as_ref().unwrap().period_seconds, Some(5));

    let get = |n: &str| c.env.as_ref().unwrap().iter().find(|e| e.name == n).and_then(|e| e.value.clone());
    assert_eq!(get("KL_TOOL_TOKEN_FILE").as_deref(), Some("/etc/kloudlite/bench-tool/token"));
    assert_eq!(get("KL_WORKSPACE_ID").as_deref(), Some("bench-1"));
    assert_eq!(get("KL_WORKSPACE").as_deref(), Some(workspace_dir("bench").as_str()));
    assert_eq!(get("KL_MODEL").as_deref(), Some("sonnet"));
    assert_eq!(get("KL_BENCH_IDLE_SECS").as_deref(), Some("420"));
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
