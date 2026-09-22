//! Tests for the bench pod builder: its own `/home/kl` Volume mount, no bench-folder or NFS home
//! left, the read-only harness for a departed member, and the gateway-only ingress policy.

use super::*;
use crate::crd::{Bench, BenchAccess, BenchSpec, BenchStatus};
use crate::k8s::{bench_ingress_policy, bench_pod, BENCH_DIR};

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
fn a_bench_pod_mounts_exactly_its_own_live_worktree_at_home() {
    let b = fixture_bench("alice", "acme", DesiredState::Running);
    let p = bench_pod(&b, "bench-1", "/wspool", None, "cr.example", 300, "").unwrap();
    assert_eq!(p.metadata.namespace.as_deref(), Some(crate::crd::ws_namespace("alice", "acme").as_str()));
    let spec = p.spec.unwrap();
    let vols = spec.volumes.as_ref().unwrap();
    let paths: Vec<String> = vols.iter().filter_map(|v| v.host_path.as_ref().map(|h| h.path.clone())).collect();
    assert!(paths.contains(&"/wspool/vol/bench-1/live/bench-1".to_string()), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains("homes") || p.contains(".benches")), "no shared-home path left: {paths:?}");
    assert!(vols.iter().all(|v| v.name != "home" && v.name != "bench-folder"));
    assert!(vols.iter().any(|v| v.name == "tmp" && v.empty_dir.is_some()));

    let c = &spec.containers[0];
    assert_eq!(
        c.command.as_deref(),
        Some(&["sh".to_string(), "-c".to_string(), "mkdir -p \"$KL_BENCH_DIR\" && exec harness-bench".to_string()][..])
    );
    let mounts = c.volume_mounts.as_ref().unwrap();
    assert!(mounts.iter().any(|m| m.name == "live" && m.mount_path == "/home/kl"));
    assert!(mounts.iter().all(|m| m.name != "bench-folder" && m.name != "home"));
    let bench_dir_env = c.env.as_ref().unwrap().iter().find(|e| e.name == "KL_BENCH_DIR").unwrap();
    assert_eq!(bench_dir_env.value.as_deref(), Some(BENCH_DIR));
    assert_eq!(c.readiness_probe.as_ref().unwrap().timeout_seconds, Some(3), "a slow Node start must not flap the bench unready");
    // The engine credentials come from the bench-only Secret, never user-key (mounted whole
    // into every workspace pod), and are optional so a fleet without the Secret still starts.
    let engine_names = ["TYPESAFE_API_KEY", "JEVHARN_API_KEY", "JEVHARN_MODEL", "JEVHARN_BASE_URL"];
    for name in engine_names {
        let ev = c.env.as_ref().unwrap().iter().find(|e| e.name == name).unwrap();
        let sel = ev.value_from.as_ref().unwrap().secret_key_ref.as_ref().unwrap();
        assert_eq!(sel.name, "bench-engine");
        assert_eq!(sel.optional, Some(true));
    }
}

#[test]
fn a_departed_members_bench_runs_the_reader_and_every_bench_may_exit_idle() {
    let mut b = fixture_bench("alice", "acme", DesiredState::Running);
    b.spec.access = crate::crd::BenchAccess::ReadOnly;
    let spec = bench_pod(&b, "bench-1", "/wspool", None, "cr", 420, "").unwrap().spec.unwrap();
    let c = &spec.containers[0];
    assert_eq!(c.command.as_ref().unwrap().last().map(String::as_str), Some("mkdir -p \"$KL_BENCH_DIR\" && exec harness-bench --read-only"));
    assert_eq!(spec.restart_policy.as_deref(), Some("OnFailure"), "exit 0 is idle and must not restart");
    let idle = c.env.as_ref().unwrap().iter().find(|e| e.name == "KL_BENCH_IDLE_SECS").unwrap();
    assert_eq!(idle.value.as_deref(), Some("420"));
}

#[test]
fn kompress_url_env_is_present_only_when_the_region_set_one() {
    let b = fixture_bench("alice", "acme", DesiredState::Running);
    let without = bench_pod(&b, "bench-1", "/wspool", None, "cr", 300, "").unwrap();
    let c = &without.spec.unwrap().containers[0];
    assert!(c.env.as_ref().unwrap().iter().all(|e| e.name != "KL_KOMPRESS_URL"));

    let with = bench_pod(&b, "bench-1", "/wspool", None, "cr", 300, "http://kloudlite-kompress.kloudlite-system:8787").unwrap();
    let c = &with.spec.unwrap().containers[0];
    let ev = c.env.as_ref().unwrap().iter().find(|e| e.name == "KL_KOMPRESS_URL").unwrap();
    assert_eq!(ev.value.as_deref(), Some("http://kloudlite-kompress.kloudlite-system:8787"));
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
