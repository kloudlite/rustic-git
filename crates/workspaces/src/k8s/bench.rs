//! `sessions` as a bench workspace's ONE container (owner ruling 2026-09-25: no shell sidecar, no
//! workspace container on a bench pod), and the gateway's hole to reach it.
//!
//! A bench is a Workspace with `spec.bench` set (`crd::is_bench`), so it has a volume, a home and a
//! `user-key` like any other; nothing here rebuilds those. `bench_container` adds only what the
//! bench image needs, and its data lives at `~/.bench` INSIDE the btrfs subvolume (the volume IS
//! the home, 2026-09-22), so it is snapshotted, replicated and pushed with the workspace.
//!
//! The image runs `runsvdir` as pid 1 (R2/R3 of the 2026-10-05 bench plan, same shape as the
//! workspace image's own `prelude`), supervising four runit services under `/etc/kl/sv`: the agent
//! CLI in a `tmux` session, `ttyd` attached to it, `sshd` (its host key persists at `~/.ssh-host`,
//! logins land in the same tmux session via `ForceCommand`) and the `sessions` node service, a
//! loopback-only HTTP server on `127.0.0.1:8917` that sshd (`BENCH_PORT`) and ttyd (`BENCH_TERM_PORT`) sit beside, not
//! on. A crash in any one is restarted by runit in place — there is no second pod phase to fall
//! back to.
//!
//! The pod's `restartPolicy` is the workspace's `Always`, so the idle/locked channel is per
//! CONTAINER now (`status.containerStatuses[name=sessions]`), not a pod phase: idle keeps serving
//! and reports through the readiness probe (`sessions --ping` against the idle tracking `/idle`
//! exposes), a stale/never-up service fails the probe and the kubelet never marks it ready.

use super::*;
use k8s_openapi::api::core::v1::{EnvVarSource, ExecAction, ObjectFieldSelector};

pub const BENCH_PORT: u16 = 7789;
/// ttyd's own port — a second gateway hole beside `BENCH_PORT`, same pod.
pub const BENCH_TERM_PORT: u16 = 7681;
/// The container every session's agent runs in. Named `sessions` since 2026-09-17 (spec §2.2): a
/// bench pod has no shell and no workspace container at all (owner ruling 2026-09-25) — it is
/// the ONLY container, so "the bench container" now means the whole pod.
pub const BENCH_CONTAINER: &str = "sessions";
pub const BENCH_TOOL_PATH: &str = "/etc/kloudlite/bench-tool";


/// Where a bench keeps its sessions, locks and per-workspace state, relative to the home. A dot name so it is out of the way, and `deploy/workspace-image/gitignore-global`
/// ignores it globally — the person's repo lives in the same tree and must never see it.
pub const BENCH_SUBDIR: &str = ".bench";


/// The `sessions` container of a bench workspace's pod: `runsvdir` as pid 1, supervising the
/// agent CLI (tmux), ttyd, sshd and the node `sessions` service (see module docs).
///
/// `resources` is `model::bench_container_resources()` and NEVER `spec.resources`: that field
/// sizes the `workspace` container the person works in, and a bench that shrank because somebody
/// sized their workspace small would OOM mid-turn. The same function `quota` charges, so what
/// runs and what is billed cannot drift apart.
#[allow(clippy::too_many_arguments)]
pub fn bench_container(
    ws_id: &str,
    spec: &WorkspaceSpec,
    image: &str,
    idle_secs: u64,
    api_url: &str,
    registry_host: &str,
    provider_url: &str,
) -> Container {
    let var = |n: &str, v: String| EnvVar { name: n.into(), value: Some(v), ..Default::default() };
    let model = spec.bench.as_ref().map(|b| b.model.clone()).unwrap_or_default();
    let mut env = vec![
        var("KL_OWNER", spec.owner.clone()),
        // The SPACE slug, exactly as the workspace container's `login_env` spells it (a personal
        // space folds to the handle): two containers of one pod must never disagree on which team
        // they are in.
        var("KL_TEAM", crate::crd::space_slug(&spec.owner, &spec.team)),
        var("KL_BENCH", ws_id.to_string()),
        // Same name the workspace container carries, so `kl` and the tool server agree on which
        // workspace this is. Never `KL_WORKSPACE`: the mod reads that var to mean "I am a
        // per-workspace session" (`bench/mod/hooks/register.tsx`), and the main agent CLI is not
        // one — setting it here made the main session believe it was scoped to a workspace.
        var("KL_WORKSPACE_ID", ws_id.to_string()),
        var("KL_REGISTRY_HOST", registry_host.to_string()),
        var("KL_BENCH_IDLE_SECS", idle_secs.to_string()),
        // The PATH to the tool token, never the token: env shows up in `ps e`, crash dumps and
        // child processes, and a file the api refreshes in place stays current without a restart.
        var("KL_TOOL_TOKEN_FILE", format!("{BENCH_TOOL_PATH}/token")),
        var("CLAUDE_CODE_PLUGIN_DIRS", "/opt/kl/mod".to_string()),
        var("KLOUDLITE_OTLP_URL", OTLP_URL.to_string()),
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
    // From `spec.bench.model` only when set: an unset model leaves the agent CLI on its own
    // default rather than stamping an empty override.
    if !model.is_empty() {
        env.push(var("ANTHROPIC_MODEL", model));
    }
    // `ClusterSettings.benchProviderUrl`, omitted when empty so the agent CLI's own default (talk
    // to Anthropic directly) applies. The token itself is never an env var — `apiKeyHelper` in
    // `/etc/claude-code/managed-settings.json` reads it off the mounted `user-key` Secret, never
    // stamped here (env shows up in `ps e`, crash dumps, child processes).
    if !provider_url.is_empty() {
        env.push(var("ANTHROPIC_BASE_URL", provider_url.to_string()));
    }

    Container {
        name: BENCH_CONTAINER.to_string(),
        image: Some(image.to_string()),
        command: Some(vec!["runsvdir".to_string(), "/etc/kl/sv".to_string()]),
        env: Some(env),
        ports: Some(vec![
            ContainerPort { container_port: BENCH_PORT as i32, name: Some("ssh".into()), ..Default::default() },
            ContainerPort { container_port: BENCH_TERM_PORT as i32, name: Some("ttyd".into()), ..Default::default() },
        ]),
        volume_mounts: Some(vec![
            // The bench's OWN volume, which is its home (2026-09-22): `.bench/` and the person's
            // own agent-session state live there, read by the agent CLI itself and listed by no
            // tool. The person's workspaces are other volumes and never mounted here.
            VolumeMount { name: "live".to_string(), mount_path: HOME_DIR.to_string(), ..Default::default() },
            VolumeMount { name: "user-key".to_string(), mount_path: USER_KEY_PATH.to_string(), read_only: Some(true), ..Default::default() },
            VolumeMount { name: "bench-tool".to_string(), mount_path: BENCH_TOOL_PATH.to_string(), read_only: Some(true), ..Default::default() },
            // `sshd`'s login must authenticate the same way an ordinary workspace does: the
            // owner's own key, projected by the agent from `OwnerKeys` (see `k8s/attach.rs`).
            VolumeMount { name: "authorized-keys".into(), mount_path: AUTHORIZED_KEYS_PATH.into(), read_only: Some(true), ..Default::default() },
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
        security_context: Some(hardened()),
        // `--ping` is the `sessions` node service's own health check (Task 2): exit 2 when no
        // server is listening on 127.0.0.1:8917 yet, 0 once it is. It never counts as a client of
        // the idle clock the mod/session service keeps — only a real WebSocket does — so the probe
        // never keeps a bench awake.
        readiness_probe: Some(Probe {
            exec: Some(ExecAction { command: Some(vec!["node".to_string(), "/opt/kl/sessions/main.ts".to_string(), "--ping".to_string()]) }),
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
        // idleness at all. 60 x 2 s is the budget from `running` to first serve.
        startup_probe: Some(Probe {
            exec: Some(ExecAction { command: Some(vec!["node".to_string(), "/opt/kl/sessions/main.ts".to_string(), "--ping".to_string()]) }),
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
                    "ports": [
                        { "protocol": "TCP", "port": BENCH_PORT as i32 },
                        { "protocol": "TCP", "port": BENCH_TERM_PORT as i32 },
                    ],
                }],
            }))
            .expect("static NetworkPolicy spec"),
        ),
    }
}
