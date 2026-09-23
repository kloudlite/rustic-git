//! `harness-bench` as the SECOND container of a bench workspace's pod, and the gateway's hole to
//! reach it.
//!
//! A bench is a Workspace with `spec.bench` set (`crd::is_bench`), so it has a volume, a home, a
//! `user-key` and sshd like any other; nothing here rebuilds those. `bench_container` adds only
//! what `harness-bench` itself needs, and its data lives at `~/.bench` INSIDE the btrfs subvolume
//! (the volume IS the home, 2026-09-22), so it is snapshotted, replicated and pushed with the
//! workspace.
//!
//! The pod's `restartPolicy` is the workspace's `Always`, so the idle/locked channel is per
//! CONTAINER now (`status.containerStatuses[name=bench]`), not a pod phase: idle keeps serving and
//! reports through the readiness probe, a held lock exits 75 and the kubelet backs off.

use super::*;
use k8s_openapi::api::core::v1::{EnvVarSource, ExecAction, ObjectFieldSelector, SecretKeySelector};

pub const BENCH_PORT: u16 = 7789;
/// The container every session's pi runs in. Named `sessions` since 2026-09-17 (spec §2.2): a
/// bench pod is `sessions` + `shell` and has no workspace container at all, so "the bench
/// container" no longer means anything a person could point at.
pub const BENCH_CONTAINER: &str = "sessions";
pub const BENCH_TOOL_PATH: &str = "/etc/kloudlite/bench-tool";


/// Where a bench keeps its sessions, locks and per-workspace state, relative to the home. A dot name so it is out of the way, and `deploy/workspace-image/gitignore-global`
/// ignores it globally — the person's repo lives in the same tree and must never see it.
pub const BENCH_SUBDIR: &str = ".bench";


fn bench_engine_var(key: &str) -> EnvVar {
    EnvVar {
        name: key.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector { name: BENCH_ENGINE_SECRET.to_string(), key: key.to_string(), optional: Some(true) }),
            ..Default::default()
        }),
        ..Default::default()
    }
}


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
#[allow(clippy::too_many_arguments)]
pub fn bench_container(ws_id: &str, spec: &WorkspaceSpec, image: &str, idle_secs: u64, kompress_url: &str, api_url: &str, registry_host: &str) -> Container {
    let data = format!("{HOME_DIR}/{BENCH_SUBDIR}");
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
        var("KL_WORKSPACE", WORKSPACE_DIR.to_string()),
        var("KL_MODEL", model),
        var("KL_REGISTRY_HOST", registry_host.to_string()),
        var("KL_BENCH_IDLE_SECS", idle_secs.to_string()),
        // Where pi keeps `auth.json` (the person's provider keys), its settings and everything
        // else it writes. Its default is `$HOME/.pi/agent`, and the sessions container HAS NO HOME
        // MOUNT since 2026-09-17 (spec §2.2) — so every bench on the fleet lost its keys at once
        // and every session answered "No API key found for deepseek" (hourly 00:25, 2026-09-18).
        //
        // `.bench/` is exactly where bench state belongs (spec §3.2 item 5): the harness's own
        // store, inside the bench's volume, so the keys are snapshotted, replicated and moved with
        // the bench like its transcripts — and read by the harness, never by a tool.
        var("PI_CODING_AGENT_DIR", format!("{data}/pi")),
        // The PATH to the tool token, never the token: env shows up in `ps e`, crash dumps and
        // child processes, and a file the api refreshes in place stays current without a restart.
        var("KL_TOOL_TOKEN_FILE", format!("{BENCH_TOOL_PATH}/token")),
        var("KLOUDLITE_OTLP_URL", OTLP_URL.to_string()),
        var("OTEL_SERVICE_NAME", "harness-bench".to_string()),
        // The pod's OWN address, for the one thing in the pod the sessions container must reach:
        // the SHELL sidecar's ttyd on `SHELL_PORT`, in the container beside it. 127.0.0.1 would
        // work for a sidecar in the same network namespace, but the bench splices a terminal by
        // ADDRESS and the same code path serves a workspace's shell on another pod — so it is
        // handed the address rather than a special case (harness 067661a8).
        EnvVar {
            name: "KL_POD_IP".into(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector { field_path: "status.podIP".into(), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        },
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
        // The sys-1 engine's credentials, from the bench-only `bench-engine` Secret (never
        // `user-key`, which every workspace pod mounts whole). `optional: true` so a fleet without
        // the entries still starts the pod and `harness/bench/src/runtime.ts` reports the missing
        // key itself rather than the pod hitting CreateContainerConfigError.
        bench_engine_var("TYPESAFE_API_KEY"),
        bench_engine_var("JEVHARN_API_KEY"),
        bench_engine_var("JEVHARN_MODEL"),
        bench_engine_var("JEVHARN_BASE_URL"),
    ];
    // Unset rather than empty when the agent has no `WS_API_URL`: the tools then fail closed.
    if !api_url.is_empty() {
        env.push(var("KL_API_URL", api_url.to_string()));
    }
    // Empty means no Kompress service in this region; the engine then falls back to its
    // rule-based compressors.
    if !kompress_url.is_empty() {
        env.push(var("KL_KOMPRESS_URL", kompress_url.to_string()));
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
            data,
            "--idle-secs".to_string(),
            idle_secs.to_string(),
        ]),
        env: Some(env),
        volume_mounts: Some(vec![
            // The bench's OWN volume, which is its home (2026-09-22): `.bench/` (plans, tasks,
            // transcripts, memory) lives there, read by the harness itself and listed by no tool.
            // The person's workspaces are other volumes and never mounted here.
            VolumeMount { name: "live".to_string(), mount_path: HOME_DIR.to_string(), ..Default::default() },
            VolumeMount { name: "user-key".to_string(), mount_path: USER_KEY_PATH.to_string(), read_only: Some(true), ..Default::default() },
            VolumeMount { name: "bench-tool".to_string(), mount_path: BENCH_TOOL_PATH.to_string(), read_only: Some(true), ..Default::default() },
            VolumeMount { name: "tmp".to_string(), mount_path: "/tmp".to_string(), ..Default::default() },
            // The same `/etc/resolv.conf` the workspace container mounts: the bench runs the
            // person's tools, so it must resolve an attached environment's services by bare name
            // exactly as their shell does. The volume IS the file, so no subPath.
            VolumeMount { name: "attach".into(), mount_path: "/etc/resolv.conf".into(), read_only: Some(true), ..Default::default() },
        ]),
        resources: Some(quantities_with_disk(
            &crate::model::bench_container_resources(),
            crate::model::BENCH_EPHEMERAL.0,
            crate::model::BENCH_EPHEMERAL.1,
        )),
        security_context: Some(security),
        // `--ping` does GET /healthz on `BENCH_PORT`, but that request isn't counted as a
        // client by the idle clock `harness-bench` keeps — only WebSockets count — so the
        // probe never keeps the bench awake. It is also the idle channel: `--ping` exits
        // non-zero once the bench has gone idle, which is the verdict the agent reads.
        readiness_probe: Some(Probe {
            exec: Some(ExecAction { command: Some(vec!["harness-bench".to_string(), "--ping".to_string()]) }),
            // 2 s, not 5: the pod is Ready when this first passes, and every second here is a
            // second the desktop waits on 202 (a wake measured 17 s on 2026-09-17, 5 of them here).
            period_seconds: Some(2),
            timeout_seconds: Some(2),
            ..Default::default()
        }),
        // Readiness alone cannot tell "asleep" from "never came up": the kubelet collapses every
        // non-zero `--ping` exit to `ready=false`, and the agent would call a bench that runs but
        // never serves idle after 15 s, delete its pod and hide the fault behind a normal phase.
        // `started` flips once, on the first successful ping, and is the agent's gate on believing
        // idleness at all. 24 x 5 s is the budget from `running` to first serve.
        startup_probe: Some(Probe {
            exec: Some(ExecAction { command: Some(vec!["harness-bench".to_string(), "--ping".to_string()]) }),
            period_seconds: Some(2),
            failure_threshold: Some(60),
            timeout_seconds: Some(2),
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
