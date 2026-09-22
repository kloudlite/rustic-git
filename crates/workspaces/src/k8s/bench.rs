//! The bench pod: one container, no btrfs worktree, no sshd — a bench has no volume, so nothing
//! here has a subvolume path, a homecache, a host key Secret or a git seed init container. What
//! IS carried from a workspace: the shared home (removed in Task 4), the `user-key` Secret projected
//! the same way, the attach `resolv.conf`, and the hardened security context. `harness-bench`
//! itself owns the model runtime and the idle clock; this module only shapes the pod around it.

use super::*;
use crate::crd::{Bench, BenchAccess};
use k8s_openapi::api::core::v1::{EnvVarSource, ExecAction, ObjectFieldSelector, SecretKeySelector};

pub const BENCH_PORT: u16 = 7789;
pub const BENCH_DIR: &str = "/bench";
pub const BENCH_CONTAINER: &str = "bench";
pub const BENCH_POD: &str = "bench";


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


/// An env var pulling one engine credential out of `BENCH_ENGINE_SECRET`. `optional: true` — see
/// the comment where these are used.
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


/// The bench's one pod. `idle_secs` is the region's `benchIdleSecs`, stamped in at create so a
/// live setting change never reaches a running pod mid-session — the same `Mark::Live`-vs-`Boot`
/// split as everywhere else in `k8s`: this value takes effect only on the pod's next create.
pub fn bench_pod(b: &Bench, id: &str, pool: &str, runtime_class: Option<&str>, registry_host: &str, idle_secs: u64, kompress_url: &str) -> Result<Pod, String> {
    let owner = &b.spec.owner;
    let team = &b.spec.team;
    let folder = bench_folder(pool, team, owner)?;
    let ns = crate::crd::ws_namespace(owner, team);

    let command = match b.spec.access {
        BenchAccess::Full => vec!["harness-bench".to_string()],
        BenchAccess::ReadOnly => vec!["harness-bench".to_string(), "--read-only".to_string()],
    };
    let var = |n: &str, v: String| EnvVar { name: n.into(), value: Some(v), ..Default::default() };

    let mut env = vec![
        var("KL_OWNER", owner.clone()),
        var("KL_TEAM", team.clone()),
        var("KL_BENCH", id.to_string()),
        var("KL_MODEL", b.spec.model.clone()),
        var("KL_REGISTRY_HOST", registry_host.to_string()),
        var("KL_BENCH_IDLE_SECS", idle_secs.to_string()),
    ];
    // Empty means no Kompress service in this region; the engine client falls back to the
    // rule-based compressors when `KL_KOMPRESS_URL` is unset entirely.
    if !kompress_url.is_empty() {
        env.push(var("KL_KOMPRESS_URL", kompress_url.to_string()));
    }
    env.extend([
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
        // The sys-1 engine's credentials, from the bench-only `bench-engine` Secret
        // (never `user-key`, which every workspace pod mounts whole). `optional: true`
        // so a fleet without these entries still starts the pod —
        // `harness/bench/src/runtime.ts::makeTurn` then reports "TYPESAFE_API_KEY is not
        // set" itself rather than the pod hitting CreateContainerConfigError.
        bench_engine_var("TYPESAFE_API_KEY"),
        bench_engine_var("JEVHARN_API_KEY"),
        bench_engine_var("JEVHARN_MODEL"),
        bench_engine_var("JEVHARN_BASE_URL"),
    ]);

    let mut pod_spec = PodSpec {
        containers: vec![Container {
            name: BENCH_CONTAINER.to_string(),
            image: Some(b.spec.image.clone()),
            command: Some(command),
            env: Some(env),
            volume_mounts: Some(vec![
                VolumeMount { name: "home".to_string(), mount_path: HOME_DIR.to_string(), mount_propagation: Some("HostToContainer".to_string()), ..Default::default() },
                VolumeMount { name: "bench-folder".to_string(), mount_path: BENCH_DIR.to_string(), ..Default::default() },
                VolumeMount { name: "user-key".to_string(), mount_path: USER_KEY_PATH.to_string(), read_only: Some(true), ..Default::default() },
                VolumeMount { name: "attach".into(), mount_path: "/etc/resolv.conf".into(), read_only: Some(true), ..Default::default() },
                VolumeMount { name: "tmp".to_string(), mount_path: "/tmp".to_string(), ..Default::default() },
            ]),
            resources: Some(quantities(&b.spec.resources)),
            security_context: Some(hardened()),
            // `--ping` does GET /healthz on `BENCH_PORT`, but that request isn't counted as a
            // client by the idle clock `harness-bench` keeps — only WebSockets count — so the
            // probe never keeps the bench awake.
            readiness_probe: Some(Probe {
                exec: Some(ExecAction { command: Some(vec!["harness-bench".to_string(), "--ping".to_string()]) }),
                period_seconds: Some(5),
                timeout_seconds: Some(3),
                ..Default::default()
            }),
            termination_message_policy: Some("File".to_string()),
            ..Default::default()
        }],
        volumes: Some(vec![
            // removed in Task 4: bench still carries the old shared-NFS home shape;
            // Task 4 reshapes the bench onto its own volume.
            host_dir("home", format!("{pool}/homes/{owner}")),
            Volume { name: "bench-folder".to_string(), host_path: Some(HostPathVolumeSource { path: folder, type_: Some("Directory".into()) }), ..Default::default() },
            user_key_volume(true),
            attach_volume(pool, id),
            Volume { name: "tmp".to_string(), empty_dir: Some(Default::default()), ..Default::default() },
        ]),
        image_pull_secrets: Some(vec![LocalObjectReference { name: PULL_SECRET.to_string() }]),
        // An idle exit (0) leaves the pod `Succeeded` for the agent to remove; a held lock (75)
        // or a crash restarts it — either way there is no reason to restart on a clean idle exit.
        restart_policy: Some("OnFailure".to_string()),
        runtime_class_name: runtime_class.map(str::to_string),
        ..Default::default()
    };
    placement(&mut pod_spec, &b.status.as_ref().map(|s| s.node_name.clone()).unwrap_or_default());

    let owner_ref = OwnerReference {
        api_version: "kloudlite.io/v1alpha1".into(),
        kind: "Bench".into(),
        name: b.metadata.name.clone().unwrap_or_default(),
        uid: b.metadata.uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    };
    let mut m = meta(BENCH_POD, Some(&ns), owner, "bench", &owner_ref);
    if let Some(l) = m.labels.as_mut() {
        l.insert(TEAM_LABEL.to_string(), team.clone());
        l.insert(WORKSPACE_LABEL.to_string(), id.to_string());
    }
    Ok(Pod { metadata: m, spec: Some(pod_spec), ..Default::default() })
}


/// Admits `BENCH_PORT` only from the gateway's pods (`app: kloudlite-gateway` in
/// `GATEWAY_NAMESPACE`, read from `deploy/k3s/gateway.yaml` — never guessed), the same shape as
/// `allow_gateway_ingress`'s port-22 hole for a workspace.
// ponytail: AKS runs no network policy engine; the fence holds on the k3s regions where benches run
pub fn bench_ingress_policy(namespace: &str, id: &str) -> NetworkPolicy {
    NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(format!("bench-{id}")),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(
            serde_json::from_value(json!({
                "podSelector": { "matchLabels": { WORKSPACE_LABEL: id } },
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
