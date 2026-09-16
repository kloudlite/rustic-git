//! `harness-bench` as the SECOND container of a bench workspace's pod, and the gateway's hole to
//! reach it.
//!
//! A bench is a Workspace with `spec.bench` set (`crd::is_bench`), so it has a volume, a home, a
//! `user-key` and sshd like any other; nothing here rebuilds those. `bench_container` adds only
//! what `harness-bench` itself needs, and its data lives at `{workspace_dir}/.bench` INSIDE the
//! btrfs subvolume, so it is snapshotted, replicated and pushed with the workspace instead of
//! sitting in a folder of its own on the shared home.
//!
//! The pod's `restartPolicy` is the workspace's `Always`, so the idle/locked channel is per
//! CONTAINER now (`status.containerStatuses[name=bench]`), not a pod phase: idle keeps serving and
//! reports through the readiness probe, a held lock exits 75 and the kubelet backs off.

use super::*;
use k8s_openapi::api::core::v1::{EnvVarSource, ExecAction, ObjectFieldSelector};

pub const BENCH_PORT: u16 = 7789;
pub const BENCH_DIR: &str = "/bench";
pub const BENCH_CONTAINER: &str = "bench";
pub const BENCH_POD: &str = "bench";
pub const BENCH_TOOL_PATH: &str = "/etc/kloudlite/bench-tool";


/// Where a bench keeps its sessions, locks and per-workspace state, relative to the workspace
/// directory. A dot name so it is out of the way, and `deploy/workspace-image/gitignore-global`
/// ignores it globally — the person's repo lives in the same tree and must never see it.
pub const BENCH_SUBDIR: &str = ".bench";


/// The `harness-bench` container of a bench workspace's pod.
///
/// It runs the binary directly, not through a shell: there is nothing to seed (the workspace
/// container's prelude owns the home) and `exec`ing through `/bin/sh` only hides the exit code
/// the agent reads off `containerStatuses`.
///
/// `resources` is `model::bench_container_resources()` and NEVER `spec.resources`: that field
/// sizes the `workspace` container the person works in, and a bench that shrank because somebody
/// sized their workspace small would OOM mid-turn. The same function `quota` charges, so what
/// runs and what is billed cannot drift apart.
pub fn bench_container(ws_id: &str, spec: &WorkspaceSpec, image: &str, idle_secs: u64, api_url: &str, registry_host: &str) -> Container {
    let dir = workspace_dir(&spec.name);
    let var = |n: &str, v: String| EnvVar { name: n.into(), value: Some(v), ..Default::default() };
    let model = spec.bench.as_ref().map(|b| b.model.clone()).unwrap_or_default();
    let mut env = vec![
        var("KL_OWNER", spec.owner.clone()),
        // The SPACE slug, exactly as the workspace container's `login_env` spells it (a personal
        // space folds to the handle): two containers of one pod must never disagree on which team
        // they are in.
        var("KL_TEAM", crate::crd::space_slug(&spec.owner, &spec.team)),
        var("KL_BENCH", ws_id.to_string()),
        // Same two names the workspace container carries, so `kl` and the tool server agree with
        // the bench about which workspace this is and where it lives.
        var("KL_WORKSPACE_ID", ws_id.to_string()),
        var("KL_WORKSPACE", dir.clone()),
        var("KL_MODEL", model),
        var("KL_REGISTRY_HOST", registry_host.to_string()),
        var("KL_BENCH_IDLE_SECS", idle_secs.to_string()),
        // The PATH to the tool token, never the token: env shows up in `ps e`, crash dumps and
        // child processes, and a file the api refreshes in place stays current without a restart.
        var("KL_TOOL_TOKEN_FILE", format!("{BENCH_TOOL_PATH}/token")),
        var("KLOUDLITE_OTLP_URL", OTLP_URL.to_string()),
        var("OTEL_SERVICE_NAME", "harness-bench".to_string()),
        EnvVar {
            name: "NODE_NAME".into(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector { field_path: "spec.nodeName".into(), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        },
        var("HOME", HOME_DIR.to_string()),
        var("LANG", "C.UTF-8".to_string()),
    ];
    // Unset rather than empty when the agent has no `WS_API_URL`: the tools then fail closed.
    if !api_url.is_empty() {
        env.push(var("KL_API_URL", api_url.to_string()));
    }

    // sshd is the other container's; without it nothing here chroots, so the one capability the
    // workspace adds for privilege separation stays dropped.
    let mut security = hardened();
    if let Some(caps) = security.capabilities.as_mut().and_then(|c| c.add.as_mut()) {
        caps.retain(|c| c != "SYS_CHROOT");
    }

    Container {
        name: BENCH_CONTAINER.to_string(),
        image: Some(image.to_string()),
        command: Some(vec![
            "harness-bench".to_string(),
            "--dir".to_string(),
            format!("{dir}/{BENCH_SUBDIR}"),
            "--idle-secs".to_string(),
            idle_secs.to_string(),
        ]),
        env: Some(env),
        volume_mounts: Some(vec![
            VolumeMount { name: "home".to_string(), mount_path: HOME_DIR.to_string(), mount_propagation: Some("HostToContainer".to_string()), ..Default::default() },
            // The LIVE worktree at the same path the workspace container sees it, which is what
            // puts `.bench` inside the subvolume rather than beside it.
            VolumeMount { name: "live".to_string(), mount_path: dir, ..Default::default() },
            VolumeMount { name: "user-key".to_string(), mount_path: USER_KEY_PATH.to_string(), read_only: Some(true), ..Default::default() },
            VolumeMount { name: "bench-tool".to_string(), mount_path: BENCH_TOOL_PATH.to_string(), read_only: Some(true), ..Default::default() },
            VolumeMount { name: "tmp".to_string(), mount_path: "/tmp".to_string(), ..Default::default() },
        ]),
        resources: Some(quantities(&crate::model::bench_container_resources())),
        security_context: Some(security),
        // `--ping` does GET /healthz on `BENCH_PORT`, but that request isn't counted as a
        // client by the idle clock `harness-bench` keeps — only WebSockets count — so the
        // probe never keeps the bench awake. It is also the idle channel: `--ping` exits
        // non-zero once the bench has gone idle, which is the verdict the agent reads.
        readiness_probe: Some(Probe {
            exec: Some(ExecAction { command: Some(vec!["harness-bench".to_string(), "--ping".to_string()]) }),
            period_seconds: Some(5),
            timeout_seconds: Some(3),
            ..Default::default()
        }),
        // So a lock holder's name reaches `status.containerStatuses[].state.terminated.message`.
        termination_message_policy: Some("File".to_string()),
        ..Default::default()
    }
}


/// The gateway's hole to `BENCH_PORT`, one per namespace, selecting the pair's bench pod by both
/// its id and `kind=bench` — a namespace holds the person's ordinary workspaces too, and none of
/// them listens here.
// ponytail: AKS runs no network policy engine; the fence holds on the k3s regions where benches run
pub fn allow_gateway_bench(namespace: &str, id: &str) -> NetworkPolicy {
    NetworkPolicy {
        metadata: ObjectMeta {
            name: Some("allow-gateway-bench".to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(
            serde_json::from_value(json!({
                "podSelector": { "matchLabels": { WORKSPACE_LABEL: id, KIND_LABEL: "bench" } },
                "policyTypes": ["Ingress"],
                "ingress": [{
                    "from": [{
                        "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": GATEWAY_NAMESPACE } },
                        "podSelector": { "matchLabels": { "app": "kloudlite-gateway" } },
                    }],
                    "ports": [{ "protocol": "TCP", "port": BENCH_PORT as i32 }],
                }],
            }))
            .expect("static NetworkPolicy spec"),
        ),
    }
}


/// `{pool}/homes/.benches/{team}/{owner}` — under the region-shared home export, never on a
/// node's local btrfs: a bench has no volume, so there is nothing for a per-node hostPath to
/// pin. `.benches` keeps this out of the flat `{pool}/homes/{owner}` namespace a workspace's own
/// home occupies. Every segment goes through `model::validate_mount`, the same check a bind mount
/// gets, because this becomes a `hostPath` the same way: `team`/`owner` come from the CRD, not
/// from a client body /v1 has already checked, so nothing here may assume they are already safe.
pub fn bench_folder(pool: &str, team: &str, owner: &str) -> Result<String, String> {
    for segment in [team, owner] {
        model::validate_mount(&model::Mount { folder: segment.to_string(), path: BENCH_DIR.to_string() })?;
    }
    Ok(format!("{pool}/homes/.benches/{team}/{owner}"))
}
