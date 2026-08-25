//! Pure builders from the domain types to Kubernetes objects.
//!
//! No client, no I/O, no environment reads — every input arrives as an argument, which is what
//! makes the security-relevant paths here exhaustively testable.
//!
//! # Why local PersistentVolumes and not hostPath
//!
//! A workspace is a btrfs subvolume on one node, so the naive expression is a `hostPath` mount. It
//! was rejected for two reasons, both verified against the live cluster rather than assumed:
//!
//! * **Pod Security Admission forbids it.** `hostPath` is refused by BOTH `restricted` and
//!   `baseline` ("hostPath volumes (volume \"v\")"), so any namespace running user workloads would
//!   have to be `privileged` — surrendering namespace-level enforcement entirely for every pod,
//!   forever, to express one mount.
//! * **It makes placement an assertion instead of a constraint.** With `hostPath` the pod must name
//!   its node and be right; with a `local` PV the PV carries `nodeAffinity` and the SCHEDULER
//!   enforces it. A pod that cannot be placed stays Pending with a reason, instead of running on a
//!   node where the data is not.
//!
//! `persistentVolumeClaim` is an allowed volume type under `restricted`, so one static `local` PV
//! per volume gives the same bytes with none of that cost.
//!
//! # Why `baseline` and not `restricted`
//!
//! `restricted` additionally demands `runAsNonRoot`, and the default workspace image runs as root
//! (`nginx:alpine` fails with `container has runAsNonRoot and image will run as root`), as do the
//! common database images an environment is made of. `baseline` blocks what actually lets a
//! container escape — hostPath, privileged, hostNetwork/PID/IPC, dangerous capabilities — while
//! leaving root INSIDE the container, which a dev workspace genuinely needs. `restricted` is
//! recorded as warn+audit so the violations are visible without being fatal, and a namespace whose
//! images allow it can be raised to enforce individually.

use crate::crd::{PodResources, WorkspaceSpec};
use crate::model;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EnvVar, LocalVolumeSource, Namespace,
    NodeSelectorRequirement, NodeSelectorTerm, PersistentVolume, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, PersistentVolumeSpec, Pod,
    PodSpec, PodTemplateSpec, ResourceRequirements, SecurityContext, Service as CoreService,
    ServicePort, ServiceSpec, Toleration, Volume, VolumeMount, VolumeNodeAffinity,
    VolumeResourceRequirements,
};
use k8s_openapi::api::networking::v1::{
    NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer,
    NetworkPolicyPort, NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;

pub const OWNER_LABEL: &str = "rustic-git.io/owner";
pub const KIND_LABEL: &str = "rustic-git.io/kind";
pub const SERVICE_LABEL: &str = "rustic-git.io/service";
/// The one StorageClass these PVs bind through. `no-provisioner` + `WaitForFirstConsumer`: nothing
/// is provisioned dynamically, and binding is deferred until a pod exists so the scheduler can
/// consider the PV's node affinity instead of binding first and discovering the conflict after.
pub const STORAGE_CLASS: &str = "rustic-git-local";
/// The label naming which workspace a pod belongs to. Load-bearing since workspaces share a
/// namespace: an attachment selects on it, so without it a grant would reach every workspace the
/// user owns.
pub const WORKSPACE_LABEL: &str = "rustic-git.io/workspace";

/// The PVC name for a volume. Per-volume, not fixed: a user's workspaces share one namespace, so a
/// single `live` claim would be one claim fought over by every workspace they own.
pub fn claim_name(id: &str) -> String {
    format!("live-{id}")
}

pub struct PodContext<'a> {
    /// The btrfs pool root on the node, e.g. `/wspool-prod`. Only the PV needs it — a pod refers to
    /// its claim, never to a path.
    pub pool: &'a str,
    pub node_name: &'a str,
    pub owner_ref: OwnerReference,
}

fn labels(owner: &str, kind: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (OWNER_LABEL.to_string(), owner.to_string()),
        (KIND_LABEL.to_string(), kind.to_string()),
    ])
}

fn meta(name: &str, ns: Option<&str>, owner: &str, kind: &str, owner_ref: &OwnerReference) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: ns.map(str::to_string),
        labels: Some(labels(owner, kind)),
        // Deletion cascades through garbage collection rather than through cleanup code that can be
        // skipped, crash halfway, or be forgotten by a new code path.
        owner_references: Some(vec![owner_ref.clone()]),
        ..Default::default()
    }
}

/// `ws-{id}` / `env-{id}`, labelled for the policies that select it and for Pod Security Admission.
///
/// See the module docs for why this is `baseline` rather than `restricted`.
pub fn namespace(name: &str, owner: &str, kind: &str, owner_ref: Option<&OwnerReference>) -> Namespace {
    let mut l = labels(owner, kind);
    l.insert("pod-security.kubernetes.io/enforce".into(), "baseline".into());
    // Not fatal, but recorded: if an image ever CAN run non-root, these tell us so.
    l.insert("pod-security.kubernetes.io/warn".into(), "restricted".into());
    l.insert("pod-security.kubernetes.io/audit".into(), "restricted".into());
    Namespace {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(l),
            // `None` for a user's shared workspace namespace: an ownerReference here would make
            // deleting ONE workspace garbage-collect the namespace and every sibling workspace in
            // it. It is shared infrastructure — created on demand, left behind when empty. An
            // environment namespace does own its objects, because there it really is one-to-one.
            owner_references: owner_ref.map(|r| vec![r.clone()]),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The PV name for a volume id. Cluster-scoped, so it carries the id rather than living in a
/// namespace that already implies it.
pub fn pv_name(id: &str) -> String {
    format!("pv-{id}")
}

/// A statically provisioned `local` PV over one btrfs subvolume.
///
/// `Retain`, never `Delete`: the reclaim policy decides what happens to a user's data when their
/// claim goes away, and `Delete` would hand that decision to the kubelet. Reclaiming a subvolume is
/// the controller's job, done deliberately, after the finalizer says the bytes are gone.
pub fn local_pv(id: &str, owner: &str, quota_gb: u64, ctx: &PodContext) -> PersistentVolume {
    PersistentVolume {
        metadata: ObjectMeta {
            name: Some(pv_name(id)),
            labels: Some(labels(owner, "volume")),
            owner_references: Some(vec![ctx.owner_ref.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeSpec {
            capacity: Some(BTreeMap::from([(
                "storage".to_string(),
                Quantity(format!("{quota_gb}Gi")),
            )])),
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            persistent_volume_reclaim_policy: Some("Retain".to_string()),
            storage_class_name: Some(STORAGE_CLASS.to_string()),
            local: Some(LocalVolumeSource {
                path: format!("{}/vol/{}/live", ctx.pool, id),
                ..Default::default()
            }),
            // This is what replaces naming a node on the pod: the scheduler will only place a pod
            // using this claim onto this node, and says so when it cannot.
            node_affinity: Some(VolumeNodeAffinity {
                required: Some(k8s_openapi::api::core::v1::NodeSelector {
                    node_selector_terms: vec![NodeSelectorTerm {
                        match_expressions: Some(vec![NodeSelectorRequirement {
                            key: "kubernetes.io/hostname".to_string(),
                            operator: "In".to_string(),
                            values: Some(vec![ctx.node_name.to_string()]),
                        }]),
                        ..Default::default()
                    }],
                }),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The claim binding a namespace to its one PV.
///
/// `volume_name` is set explicitly: without it the claim would bind to whichever PV of this class
/// happens to fit, which for per-workspace storage means someone else's data.
pub fn claim(ns: &str, id: &str, owner: &str, quota_gb: u64, owner_ref: &OwnerReference) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: meta(&claim_name(id), Some(ns), owner, "volume", owner_ref),
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            storage_class_name: Some(STORAGE_CLASS.to_string()),
            volume_name: Some(pv_name(id)),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(format!("{quota_gb}Gi")),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn quantities(res: &PodResources) -> ResourceRequirements {
    // Requests AND limits on every user container: requests are what the scheduler packs against,
    // limits are what stops one workspace eating a node its neighbours share.
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_request.clone())),
            ("memory".to_string(), Quantity(res.memory_request.clone())),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_limit.clone())),
            ("memory".to_string(), Quantity(res.memory_limit.clone())),
        ])),
        ..Default::default()
    }
}

/// What `baseline` does not enforce but we can still apply per container.
///
/// `run_as_non_root` is deliberately absent — see the module docs: forcing it would break the
/// zero-configuration default image and most database images an environment is built from.
fn hardened() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            // Drop everything, then add back only what an ordinary image needs to INITIALISE.
            // `drop: ALL` alone is not deployable for images users actually bring: the default
            // workspace image dies at startup with
            //   nginx: [emerg] chown("/var/cache/nginx/client_temp", 101) failed (1: Operation not permitted)
            // because its entrypoint runs as root, chowns its cache dirs and drops to the nginx
            // user — the same shape postgres, mongo and most official images use. Observed on the
            // cluster, not theorised.
            //
            // Every one of these is on Pod Security Admission `baseline`'s allowed-add list, so the
            // namespace still rejects the dangerous ones (SYS_ADMIN, NET_RAW, SYS_PTRACE and the
            // rest) — which is the property that actually matters. This is "the container runtime's
            // ordinary default, stated explicitly" rather than a widening of it.
            add: Some(
                ["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID", "NET_BIND_SERVICE"]
                    .iter()
                    .map(|c| c.to_string())
                    .collect(),
            ),
        }),
        privileged: Some(false),
        ..Default::default()
    }
}

fn claim_volume(id: &str) -> Volume {
    Volume {
        // The in-pod volume name stays constant; only the CLAIM it resolves to varies per volume.
        name: "live".to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: claim_name(id),
            read_only: Some(false),
        }),
        ..Default::default()
    }
}

/// Keep the pod on its role's nodes and tolerate that role's taint.
///
/// The node itself is chosen by the PV's affinity, not here — this only expresses "session pods run
/// on session nodes". The toleration is not optional: the label without it schedules nothing.
fn placement(spec: &mut PodSpec, role: &str) {
    // One label KEY per role (`rustic-git.io/session`, `rustic-git.io/env`) rather than one shared
    // key with the role as its value. A label key holds a single value, so `role=session` and
    // `role=env` are mutually exclusive and no node could ever serve both — which made a
    // single-node install impossible, and produced an unschedulable pod whose data was on one node
    // and whose selector demanded another:
    //   1 node(s) didn't match PersistentVolume's node affinity
    //   1 node(s) didn't match Pod's node affinity/selector
    // Separate keys let a small or CI cluster put both roles on one box and a large one keep them
    // apart, with no change to this code.
    spec.node_selector = Some(BTreeMap::from([(
        format!("rustic-git.io/{role}"),
        "true".to_string(),
    )]));
    spec.tolerations = Some(vec![Toleration {
        key: Some(format!("rustic-git.io/{role}")),
        operator: Some("Exists".to_string()),
        effect: Some("NoSchedule".to_string()),
        ..Default::default()
    }]);
    // A user workload has no business talking to the API server.
    spec.automount_service_account_token = Some(false);
}

/// The workspace's one pod.
pub fn workspace_pod(spec: &WorkspaceSpec, id: &str, ctx: &PodContext) -> Pod {
    let _ = ctx.node_name; // placement rides on the PV; kept in context for the PV builder
    let mut pod_spec = PodSpec {
        containers: vec![Container {
            name: "workspace".to_string(),
            image: Some(spec.image.clone()),
            // The double mount is deliberate, carried over from the container-era agent:
            // `/workspace` is the generic contract every image can rely on, while mounting the SAME
            // volume read-only at nginx's web root means the default image serves the workspace's
            // own files with zero configuration instead of an empty landing page.
            volume_mounts: Some(vec![
                VolumeMount {
                    name: "live".to_string(),
                    mount_path: "/workspace".to_string(),
                    ..Default::default()
                },
                VolumeMount {
                    name: "live".to_string(),
                    mount_path: "/usr/share/nginx/html".to_string(),
                    read_only: Some(true),
                    ..Default::default()
                },
            ]),
            resources: Some(quantities(&spec.resources)),
            security_context: Some(hardened()),
            ..Default::default()
        }],
        volumes: Some(vec![claim_volume(id)]),
        // What `--restart unless-stopped` became: stopping is expressed by deleting the pod, not by
        // a policy the kubelet interprets.
        restart_policy: Some("Always".to_string()),
        ..Default::default()
    };
    placement(&mut pod_spec, "session");
    let mut m = meta(
        id,
        Some(&crate::crd::ws_namespace(&spec.owner)),
        &spec.owner,
        "workspace",
        &ctx.owner_ref,
    );
    // Which workspace this pod IS. Siblings share the namespace, so an attachment grant that named
    // only the namespace would reach all of them; this label is what keeps it to one.
    if let Some(l) = m.labels.as_mut() {
        l.insert(WORKSPACE_LABEL.to_string(), id.to_string());
    }
    Pod { metadata: m, spec: Some(pod_spec), ..Default::default() }
}

/// One Deployment per service in an environment.
///
/// **Every mount goes through `validate_mount` here.** An environment has ONE volume, and each
/// declared mount is a folder inside it, expressed as a `subPath` on the shared claim. Kubernetes
/// rejects `..` in a subPath itself, but this does not lean on that: a folder is validated as a
/// single safe segment before it is ever formatted into one.
pub fn service_deployment(
    svc: &model::Service,
    env_id: &str,
    owner: &str,
    ctx: &PodContext,
) -> Result<Deployment, String> {
    let mut mounts = Vec::new();
    for m in &svc.mounts {
        model::validate_mount(m)?;
        mounts.push(VolumeMount {
            name: "live".to_string(),
            mount_path: m.path.clone(),
            sub_path: Some(format!("volumes/{}", m.folder)),
            ..Default::default()
        });
    }

    let mut sel = labels(owner, "environment");
    sel.insert(SERVICE_LABEL.to_string(), svc.name.clone());

    let mut pod_spec = PodSpec {
        containers: vec![Container {
            name: svc.name.clone(),
            image: Some(svc.image.clone()),
            command: (!svc.command.is_empty()).then(|| svc.command.clone()),
            env: Some(
                svc.env
                    .iter()
                    .map(|(k, v)| EnvVar {
                        name: k.clone(),
                        value: Some(v.clone()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ports: Some(
                svc.ports
                    .iter()
                    .map(|p| ContainerPort {
                        container_port: *p as i32,
                        ..Default::default()
                    })
                    .collect(),
            ),
            volume_mounts: (!mounts.is_empty()).then_some(mounts),
            resources: Some(quantities(&PodResources {
                cpu_request: "250m".into(),
                cpu_limit: "2".into(),
                memory_request: "512Mi".into(),
                memory_limit: "4Gi".into(),
            })),
            security_context: Some(hardened()),
            ..Default::default()
        }],
        volumes: Some(vec![claim_volume(env_id)]),
        ..Default::default()
    };
    placement(&mut pod_spec, "env");

    Ok(Deployment {
        metadata: meta(
            &svc.name,
            Some(&crate::crd::env_namespace(env_id)),
            owner,
            "environment",
            &ctx.owner_ref,
        ),
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(sel.clone()),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(sel),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// The ClusterIP that gives a service its DNS name — what makes `mongodb://db:27017` resolve from a
/// sibling service, and from an attached workspace on another node.
pub fn service_clusterip(
    svc: &model::Service,
    env_id: &str,
    owner: &str,
    owner_ref: &OwnerReference,
) -> CoreService {
    let mut sel = labels(owner, "environment");
    sel.insert(SERVICE_LABEL.to_string(), svc.name.clone());
    CoreService {
        metadata: meta(
            &svc.name,
            Some(&crate::crd::env_namespace(env_id)),
            owner,
            "environment",
            owner_ref,
        ),
        spec: Some(ServiceSpec {
            selector: Some(sel),
            ports: Some(
                svc.ports
                    .iter()
                    .map(|p| ServicePort {
                        name: Some(format!("p{p}")),
                        port: *p as i32,
                        target_port: Some(IntOrString::Int(*p as i32)),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn policy(name: &str, ns: &str, owner: &str, owner_ref: &OwnerReference, spec: NetworkPolicySpec) -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta(name, Some(ns), owner, "policy", owner_ref),
        spec: Some(spec),
    }
}

/// The three policies every namespace gets: deny everything, allow DNS out, allow the namespace to
/// talk to itself.
///
/// Generated rather than rendered from YAML so there is exactly one definition of the isolation
/// rule. Order does not matter — NetworkPolicies are additive, and the default-deny is expressed by
/// selecting every pod with no rules rather than by precedence.
pub fn default_policies(ns: &str, owner: &str, owner_ref: &OwnerReference) -> Vec<NetworkPolicy> {
    let all_pods = LabelSelector::default();
    vec![
        policy(
            "default-deny",
            ns,
            owner,
            owner_ref,
            NetworkPolicySpec {
                pod_selector: Some(all_pods.clone()),
                policy_types: Some(vec!["Ingress".into(), "Egress".into()]),
                ..Default::default()
            },
        ),
        policy(
            "allow-dns",
            ns,
            owner,
            owner_ref,
            NetworkPolicySpec {
                pod_selector: Some(all_pods.clone()),
                policy_types: Some(vec!["Egress".into()]),
                // To CoreDNS specifically, by its namespace's well-known label. Without this rule
                // every lookup fails, which is the most common way a default-deny namespace looks
                // like "the network is broken".
                egress: Some(vec![NetworkPolicyEgressRule {
                    to: Some(vec![NetworkPolicyPeer {
                        namespace_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                "kubernetes.io/metadata.name".to_string(),
                                "kube-system".to_string(),
                            )])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ports: Some(vec![
                        NetworkPolicyPort {
                            protocol: Some("UDP".into()),
                            port: Some(IntOrString::Int(53)),
                            ..Default::default()
                        },
                        NetworkPolicyPort {
                            protocol: Some("TCP".into()),
                            port: Some(IntOrString::Int(53)),
                            ..Default::default()
                        },
                    ]),
                }]),
                ..Default::default()
            },
        ),
        policy(
            "allow-same-namespace",
            ns,
            owner,
            owner_ref,
            NetworkPolicySpec {
                pod_selector: Some(all_pods),
                policy_types: Some(vec!["Ingress".into(), "Egress".into()]),
                // An environment's services must reach each other — that is what an environment IS.
                ingress: Some(vec![NetworkPolicyIngressRule {
                    from: Some(vec![NetworkPolicyPeer {
                        pod_selector: Some(LabelSelector::default()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                egress: Some(vec![NetworkPolicyEgressRule {
                    to: Some(vec![NetworkPolicyPeer {
                        pod_selector: Some(LabelSelector::default()),
                        ..Default::default()
                    }]),
                    ports: None,
                }]),
            },
        ),
    ]
}

/// One policy per attachment, in the ENVIRONMENT's namespace, keyed by the workspace namespace's
/// name label.
///
/// Attaching is an authorization decision made in `/v1` against team membership; this only
/// expresses a decision already taken. Deleting the policy is what detaching means.
pub fn attach_policy(
    env_ns: &str,
    ws_ns: &str,
    ws_id: &str,
    owner: &str,
    owner_ref: &OwnerReference,
) -> NetworkPolicy {
    policy(
        &format!("attach-{ws_id}"),
        env_ns,
        owner,
        owner_ref,
        NetworkPolicySpec {
            pod_selector: Some(LabelSelector::default()),
            policy_types: Some(vec!["Ingress".into()]),
            ingress: Some(vec![NetworkPolicyIngressRule {
                // BOTH selectors in ONE peer, which ANDs them. Two peers would OR, and a bare
                // namespace selector would grant every workspace the owner has — because a user's
                // workspaces now SHARE a namespace, naming the namespace alone is exactly the
                // over-grant this has to avoid.
                from: Some(vec![NetworkPolicyPeer {
                    namespace_selector: Some(LabelSelector {
                        match_labels: Some(BTreeMap::from([(
                            "kubernetes.io/metadata.name".to_string(),
                            ws_ns.to_string(),
                        )])),
                        ..Default::default()
                    }),
                    pod_selector: Some(LabelSelector {
                        match_labels: Some(BTreeMap::from([(
                            WORKSPACE_LABEL.to_string(),
                            ws_id.to_string(),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
    )
}

/// The egress counterpart, in the WORKSPACE's namespace: `default_policies` denies egress except
/// DNS and same-namespace, so an attachment needs a hole punched at both ends.
pub fn attach_egress_policy(ws_ns: &str, env_ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &format!("attach-{env_ns}"),
        ws_ns,
        owner,
        owner_ref,
        NetworkPolicySpec {
            pod_selector: Some(LabelSelector::default()),
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![NetworkPolicyPeer {
                    namespace_selector: Some(LabelSelector {
                        match_labels: Some(BTreeMap::from([(
                            "kubernetes.io/metadata.name".to_string(),
                            env_ns.to_string(),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::DesiredState;
    use crate::model::Mount;

    fn owner_ref() -> OwnerReference {
        OwnerReference {
            api_version: "rustic-git.io/v1alpha1".into(),
            kind: "Volume".into(),
            name: "vol-1".into(),
            uid: "uid-1".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    fn ctx() -> PodContext<'static> {
        PodContext { pool: "/mnt/wspool", node_name: "session-0", owner_ref: owner_ref() }
    }

    fn svc(folder: &str, path: &str) -> model::Service {
        model::Service {
            name: "web".into(),
            image: "nginx".into(),
            command: vec![],
            env: Default::default(),
            mounts: vec![Mount { folder: folder.into(), path: path.into() }],
            ports: vec![80],
        }
    }

    fn ws_spec() -> WorkspaceSpec {
        WorkspaceSpec {
            owner: "alice".into(),
            name: "dev".into(),
            region: "centralindia".into(),
            image: "nginx:alpine".into(),
            volume_ref: "vol-1".into(),
            node_name: "session-0".into(),
            desired_state: DesiredState::Running,
            resources: PodResources::default(),
        }
    }

    #[test]
    fn a_service_deployment_refuses_a_mount_that_escapes_the_subvolume() {
        let ctx = ctx();
        let ok = service_deployment(&svc("data", "/data"), "env-1", "team", &ctx).unwrap();
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
                service_deployment(&svc(bad, "/host"), "env-1", "team", &ctx).is_err(),
                "folder {bad:?} must be refused"
            );
        }
        assert!(service_deployment(&svc("data", "/data:/etc"), "env-1", "team", &ctx).is_err());
        assert!(service_deployment(&svc("data", "relative"), "env-1", "team", &ctx).is_err());
    }

    #[test]
    fn no_pod_this_module_builds_uses_a_hostpath() {
        // hostPath is refused by PSA baseline AND restricted, so a single one here would force the
        // whole namespace to `privileged` — the regression this module exists to prevent.
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx());
        for v in p.spec.unwrap().volumes.unwrap() {
            assert!(v.host_path.is_none(), "workspace pod must mount a claim, not a hostPath");
            assert!(v.persistent_volume_claim.is_some());
        }
        let d = service_deployment(&svc("data", "/data"), "env-1", "team", &ctx()).unwrap();
        for v in d.spec.unwrap().template.spec.unwrap().volumes.unwrap() {
            assert!(v.host_path.is_none(), "service pod must mount a claim, not a hostPath");
        }
    }

    #[test]
    fn the_volume_pins_the_node_and_never_deletes_the_data() {
        let pv = local_pv("ws-1", "alice", 20, &ctx());
        let spec = pv.spec.unwrap();
        assert_eq!(spec.local.as_ref().unwrap().path, "/mnt/wspool/vol/ws-1/live");
        // Retain, never Delete: reclaiming a user's subvolume is a deliberate controller action,
        // not something the kubelet does when a claim goes away.
        assert_eq!(spec.persistent_volume_reclaim_policy.as_deref(), Some("Retain"));

        // The scheduler enforces placement from this, which is why the pod no longer names a node.
        let term = &spec.node_affinity.unwrap().required.unwrap().node_selector_terms[0];
        let e = &term.match_expressions.as_ref().unwrap()[0];
        assert_eq!(e.key, "kubernetes.io/hostname");
        assert_eq!(e.values.as_deref(), Some(&["session-0".to_string()][..]));

        let p = workspace_pod(&ws_spec(), "ws-1", &ctx());
        assert!(
            p.spec.unwrap().node_name.is_none(),
            "naming a node here would make placement an assertion again"
        );
    }

    #[test]
    fn a_claim_binds_to_exactly_one_named_volume() {
        let c = claim("ws-alice", "ws-1", "alice", 20, &owner_ref());
        assert_eq!(c.metadata.name.as_deref(), Some("live-ws-1"), "siblings share a namespace");
        let s = c.spec.unwrap();
        // Without volumeName the claim binds to whichever PV of this class fits — which, for
        // per-workspace storage, means somebody else's data.
        assert_eq!(s.volume_name.as_deref(), Some("pv-ws-1"));
        assert_eq!(s.storage_class_name.as_deref(), Some(STORAGE_CLASS));
    }

    #[test]
    fn a_user_pod_cannot_reach_the_api_server_or_escalate() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx());
        let s = p.spec.unwrap();
        assert_eq!(s.automount_service_account_token, Some(false));
        assert_eq!(s.restart_policy.as_deref(), Some("Always"));
        // A key per role, not a shared key with the role as its value: a node can then carry both
        // and a single-node install works.
        assert_eq!(
            s.node_selector.as_ref().unwrap().get("rustic-git.io/session").map(String::as_str),
            Some("true")
        );
        // The label without the toleration schedules nothing.
        assert_eq!(s.tolerations.as_ref().unwrap()[0].key.as_deref(), Some("rustic-git.io/session"));

        let c = &s.containers[0];
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
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
    }

    #[test]
    fn a_namespace_enforces_baseline_and_audits_restricted() {
        let ns = namespace("ws-alice", "alice", "workspace", None);
        let l = ns.metadata.labels.unwrap();
        // baseline blocks hostPath, privileged, hostNetwork/PID/IPC and dangerous capabilities —
        // the actual escape vectors — while leaving root inside the container, which the default
        // image and every common database image need.
        assert_eq!(l.get("pod-security.kubernetes.io/enforce").map(String::as_str), Some("baseline"));
        assert_eq!(l.get("pod-security.kubernetes.io/audit").map(String::as_str), Some("restricted"));
    }

    #[test]
    fn a_workspace_pod_double_mounts_its_volume() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx());
        let s = p.spec.unwrap();
        assert_eq!(s.volumes.as_ref().unwrap().len(), 1, "both mounts name the SAME claim");
        let mounts = s.containers[0].volume_mounts.as_ref().unwrap();
        let ro = mounts.iter().find(|m| m.mount_path == "/usr/share/nginx/html").unwrap();
        assert_eq!(ro.read_only, Some(true), "the web root mount must be read-only");
        assert!(mounts.iter().any(|m| m.mount_path == "/workspace" && m.read_only.is_none()));
    }

    #[test]
    fn every_child_object_cascades_on_delete() {
        // Reclamation via garbage collection rather than cleanup code that can be skipped or crash
        // halfway. If this regresses, deleting a workspace leaks its pod, namespace and PV.
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx());
        assert_eq!(p.metadata.owner_references.unwrap()[0].controller, Some(true));
        assert_eq!(local_pv("ws-1", "alice", 20, &ctx()).metadata.owner_references.unwrap().len(), 1);
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

    #[test]
    fn an_environment_namespace_denies_by_default_and_still_resolves_dns() {
        let pols = default_policies("env-1", "team", &owner_ref());
        let names: Vec<_> = pols.iter().filter_map(|p| p.metadata.name.as_deref()).collect();
        assert_eq!(names, vec!["default-deny", "allow-dns", "allow-same-namespace"]);

        let deny = pols[0].spec.as_ref().unwrap();
        assert_eq!(deny.policy_types.as_ref().unwrap().len(), 2, "deny must cover BOTH directions");
        assert!(deny.ingress.is_none() && deny.egress.is_none(), "a rule here would stop it denying");

        let dns = pols[1].spec.as_ref().unwrap().egress.as_ref().unwrap();
        assert!(dns[0].ports.as_ref().unwrap().iter().any(|p| p.port == Some(IntOrString::Int(53))));
    }

    #[test]
    fn an_attachment_names_one_workspace_not_every_sibling() {
        let ingress = attach_policy("env-1", "ws-alice", "ws-abc", "team", &owner_ref());
        assert_eq!(ingress.metadata.namespace.as_deref(), Some("env-1"));
        let rules = ingress.spec.unwrap().ingress.unwrap();
        let from_list = rules[0].from.as_ref().unwrap();
        // ONE peer, not two: within a peer the selectors AND, across peers they OR. Two peers here
        // would grant the whole namespace OR every pod with that label anywhere.
        assert_eq!(from_list.len(), 1, "two peers would OR and blow the grant wide open");
        let from = &from_list[0];
        let ns_sel = from.namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
        assert_eq!(ns_sel.get("kubernetes.io/metadata.name").map(String::as_str), Some("ws-alice"));
        // Since a user's workspaces SHARE a namespace, the pod selector is what keeps this grant to
        // the one workspace that was attached. Without it every workspace the user owns could reach
        // the environment. (This assertion is the exact inverse of what it was when a namespace
        // held a single workspace — the reasoning flipped with the layout.)
        let pod_sel = from.pod_selector.as_ref().expect("a bare namespace selector over-grants");
        assert_eq!(
            pod_sel.match_labels.as_ref().unwrap().get(WORKSPACE_LABEL).map(String::as_str),
            Some("ws-abc")
        );

        // Egress is denied by default at the workspace end too, so one-sided attachment silently
        // fails — the workspace could not send, whatever the environment allows.
        let egress = attach_egress_policy("ws-abc", "env-1", "alice", &owner_ref());
        assert_eq!(egress.metadata.namespace.as_deref(), Some("ws-abc"));
        assert_eq!(egress.spec.unwrap().policy_types.unwrap(), vec!["Egress".to_string()]);
    }

    #[test]
    fn a_service_gets_a_clusterip_for_each_declared_port() {
        let s = service_clusterip(&svc("data", "/data"), "env-1", "team", &owner_ref());
        let spec = s.spec.unwrap();
        let ports = spec.ports.unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 80);
        assert_eq!(ports[0].target_port, Some(IntOrString::Int(80)));
        // The selector must match the Deployment's template labels or the Service selects nothing
        // and the name resolves to a black hole.
        assert_eq!(spec.selector.unwrap().get(SERVICE_LABEL).map(String::as_str), Some("web"));
    }
}
