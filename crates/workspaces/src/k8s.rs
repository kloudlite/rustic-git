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
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EnvVar, LimitRange, LimitRangeItem, LimitRangeSpec,
    LocalObjectReference, LocalVolumeSource, Namespace, SeccompProfile,
    NodeSelectorRequirement, NodeSelectorTerm, PersistentVolume, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, PersistentVolumeSpec, Pod,
    PodSpec, PodTemplateSpec, ResourceRequirements, Secret, SecretVolumeSource,
    SecurityContext, Service as CoreService,
    ServicePort, ServiceSpec, Toleration, Volume, VolumeMount, VolumeNodeAffinity,
    VolumeResourceRequirements,
};
use k8s_openapi::api::rbac::v1::{RoleBinding, RoleRef, Subject};
use k8s_openapi::api::networking::v1::{
    IPBlock, NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer,
    NetworkPolicyPort, NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;

pub const OWNER_LABEL: &str = "rustic-git.io/owner";
pub const KIND_LABEL: &str = "rustic-git.io/kind";
/// The team a workspace was made in, empty for personal. Same rule as the other two: a listing
/// view of `spec.team`, re-stamped by the controller, never authorization.
pub const TEAM_LABEL: &str = "rustic-git.io/team";
pub const SERVICE_LABEL: &str = "rustic-git.io/service";
/// The one StorageClass these PVs bind through. `no-provisioner` + `WaitForFirstConsumer`: nothing
/// is provisioned dynamically, and binding is deferred until a pod exists so the scheduler can
/// consider the PV's node affinity instead of binding first and discovering the conflict after.
pub const STORAGE_CLASS: &str = "rustic-git-local";
/// The container's writable layer and logs — NOT the tenant's data, which lives on their
/// PersistentVolume and is bounded by its own quota.
///
/// Unbounded, this is a node-wide denial of service available to any tenant: filling the kubelet's
/// disk taints the node `disk-pressure` and stops scheduling for every OTHER tenant on it. That is
/// not theoretical — it happened to this cluster from an ordinary build, and nothing in the
/// workload could have caused the kubelet to evict the offender instead of penalising the node.
/// With a limit the offending pod is evicted and its neighbours are untouched.
const EPHEMERAL_REQUEST: &str = "1Gi";
const EPHEMERAL_LIMIT: &str = "4Gi";

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
    /// The sandbox to run TENANT pods under, e.g. `gvisor`. `None` runs them on the host kernel.
    ///
    /// Opt-in, not defaulted, because a `runtimeClassName` naming a runtime the node has not got
    /// makes every pod fail to start — a cluster without gVisor installed must keep working. It is
    /// set from the agent's `WS_RUNTIME_CLASS`, so enabling it is a per-cluster decision made where
    /// the runtime is actually installed.
    ///
    /// Applies to tenant pods only. The controller itself must NOT be sandboxed: it drives btrfs
    /// against the host pool, which is precisely the host access a sandbox exists to remove.
    pub runtime_class: Option<&'a str>,
}

pub(crate) fn labels(owner: &str, kind: &str) -> BTreeMap<String, String> {
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

/// The namespace's ceiling: no container in it may exceed the slot, and one that names no
/// resources at all gets the slot's values rather than none.
///
/// The pod specs this module builds already carry requests and limits, so this is not about them —
/// it is about everything else. A `LimitRange` is enforced by the API SERVER at admission, so it
/// holds for a pod created by any path: a future code path that forgets, a debug pod, an operator
/// with kubectl. Without it "every workspace is an M slot" is a property of one function rather
/// than of the namespace.
///
/// `max` is the slot's LIMIT, not its request: bursting to the limit is the point of the slot, and
/// exceeding it is what must be refused. Capacity is priced on the request (see
/// `PodResources::default`), which `defaultRequest` pins for anything that omits one.
pub fn limit_range(ns: &str, owner: &str, kind: &str, res: &PodResources, owner_ref: Option<&OwnerReference>) -> LimitRange {
    let item = LimitRangeItem {
        type_: "Container".to_string(),
        default: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_limit.clone())),
            ("memory".to_string(), Quantity(res.memory_limit.clone())),
        ])),
        default_request: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_request.clone())),
            ("memory".to_string(), Quantity(res.memory_request.clone())),
        ])),
        max: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_limit.clone())),
            ("memory".to_string(), Quantity(res.memory_limit.clone())),
        ])),
        ..Default::default()
    };
    LimitRange {
        metadata: ObjectMeta {
            name: Some("slot".to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(owner, kind)),
            owner_references: owner_ref.map(|r| vec![r.clone()]),
            ..Default::default()
        },
        spec: Some(LimitRangeSpec { limits: vec![item] }),
    }
}

/// The Secret name a namespace's pods pull private images with.
///
/// Fixed per namespace rather than per pod: a pull credential is scoped to the OWNER, not to one
/// workload, and one Secret per pod would be N copies of the same token to rotate.
pub const PULL_SECRET: &str = "registry-pull";

/// The Secret holding the owner's platform-issued git key, one per workspace namespace.
///
/// Per owner, not per workspace: the key IS the owner's git identity, so a copy per workspace would
/// be N copies of one credential to rotate.
pub const USER_KEY_SECRET: &str = "user-key";

/// Where that key is mounted. Deliberately not `~/.ssh`: workspace images bring their own user and
/// home directory, and `GIT_SSH_COMMAND` points at an absolute path that works whatever they are.
pub const USER_KEY_PATH: &str = "/etc/rustic-git/ssh";

/// The owner's private key as a namespace Secret. Written by the API tier, which holds `secrets`
/// only in namespaces the controller has vouched for — see `api_secret_binding`.
pub fn user_key_secret(owner: &str, namespace: &str, private_openssh: &str, authorized_keys: &str) -> Secret {
    Secret {
        // No ownerReference: the key belongs to the OWNER, not to any one workspace, so deleting
        // the workspace that happened to trigger its creation must not take it with them.
        metadata: ObjectMeta {
            name: Some(USER_KEY_SECRET.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels(owner, "workspace")),
            ..Default::default()
        },
        // Both halves in ONE Secret: the private key the workspace pushes git with, and the
        // public keys sshd lets in. They are rewritten together, so splitting them would only add
        // a second object that can be half-written.
        string_data: Some(BTreeMap::from([
            ("id_ed25519".to_string(), private_openssh.to_string()),
            ("authorized_keys".to_string(), authorized_keys.to_string()),
        ])),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

fn user_key_volume(required: bool) -> Volume {
    Volume {
        name: "user-key".to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(USER_KEY_SECRET.to_string()),
            // 0400. ssh refuses a key any wider than the owner can read.
            default_mode: Some(0o400),
            // The API writes this AFTER the controller has made the namespace, so a workspace can
            // be scheduled before its key exists. Optional means the pod starts anyway and the
            // kubelet fills the mount in when the Secret shows up, instead of the pod sitting
            // Pending until then. A SEEDED workspace cannot tolerate that: the init container
            // clones with this key, and an absent one would start a pod that clones nothing and
            // then reports Ready.
            optional: Some(!required),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Let the API write Secrets in THIS namespace, and nowhere else.
///
/// The API needs to place a short-lived git token for a workspace being seeded from a repository.
/// Granting `secrets: create` cluster-wide to achieve that would hand it every Secret in the
/// cluster, the agent's own credentials included — so the permission is bound per namespace, by the
/// controller, as it creates each workspace namespace.
///
/// The controller can only issue this grant because it holds `bind` on exactly this ClusterRole:
/// Kubernetes otherwise refuses to let a subject hand out permissions it does not itself have, and
/// the alternative (giving the controller cluster-wide secret access so it can delegate a slice of
/// it) is the thing being avoided.
///
/// `owner_ref` is the OwnerBinding that vouched for the namespace, when one did: the grant is
/// per (owner, node) and so shares that lifetime. It is never a Workspace or an Environment — the
/// namespace is shared by every workspace the user owns, so deleting one must not revoke the grant
/// for its siblings.
pub fn api_secret_binding(
    ns: &str,
    owner: &str,
    api_service_account: &str,
    api_namespace: &str,
    owner_ref: Option<&OwnerReference>,
) -> RoleBinding {
    RoleBinding {
        metadata: ObjectMeta {
            name: Some("api-secrets".to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(owner, "workspace")),
            owner_references: owner_ref.map(|r| vec![r.clone()]),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: "rustic-git-api-secrets".to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: api_service_account.to_string(),
            namespace: Some(api_namespace.to_string()),
            ..Default::default()
        }]),
    }
}

/// The PV name for a volume id. Cluster-scoped, so it carries the id rather than living in a
/// namespace that already implies it.
pub fn pv_name(id: &str) -> String {
    format!("pv-{id}")
}

/// A statically provisioned `local` PV over one host path — a workspace's btrfs subvolume, or
/// the shared read-only `/nix` store.
///
/// `Retain`, never `Delete`: the reclaim policy decides what happens to a user's data when their
/// claim goes away, and `Delete` would hand that decision to the kubelet. Reclaiming a subvolume is
/// the controller's job, done deliberately, after the finalizer says the bytes are gone.
pub fn local_pv(
    name: &str,
    host_path: &str,
    access_mode: &str,
    capacity_gb: u64,
    owner: &str,
    ctx: &PodContext,
) -> PersistentVolume {
    PersistentVolume {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(labels(owner, "volume")),
            owner_references: Some(vec![ctx.owner_ref.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeSpec {
            capacity: Some(BTreeMap::from([("storage".to_string(), Quantity(format!("{capacity_gb}Gi")))])),
            access_modes: Some(vec![access_mode.to_string()]),
            persistent_volume_reclaim_policy: Some("Retain".to_string()),
            storage_class_name: Some(STORAGE_CLASS.to_string()),
            local: Some(LocalVolumeSource { path: host_path.to_string(), ..Default::default() }),
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

/// The claim binding a namespace to one PV.
///
/// `volume_name` is set explicitly: without it the claim would bind to whichever PV of this class
/// happens to fit, which for per-workspace storage means someone else's data.
pub fn claim(
    ns: &str,
    name: &str,
    pv: &str,
    access_mode: &str,
    capacity_gb: u64,
    owner: &str,
    owner_ref: &OwnerReference,
) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: meta(name, Some(ns), owner, "volume", owner_ref),
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec![access_mode.to_string()]),
            storage_class_name: Some(STORAGE_CLASS.to_string()),
            volume_name: Some(pv.to_string()),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([("storage".to_string(), Quantity(format!("{capacity_gb}Gi")))])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The host Nix store, exposed to a workspace the same way its subvolume is: a local PV names the
/// host path, the pod names a claim. A local PV binds to exactly one claim, so it is one per
/// workspace even though every one of them points at the same `/nix` — PV objects are cheap and
/// the alternative is a hostPath, which PSA `baseline` forbids for good reason. Capacity is a
/// required field with no meaning for it (shared and read-only), hence the flat 1Gi callers pass.
pub const NIX_ROOT: &str = "/nix";

pub fn nix_pv_name(id: &str) -> String { format!("nix-{id}") }
pub fn nix_claim_name(id: &str) -> String { format!("nix-{id}") }

/// The host path backing a volume's live subvolume.
pub fn live_path(pool: &str, id: &str) -> String { format!("{pool}/vol/{id}/live") }

fn nix_volume(id: &str) -> Volume {
    Volume {
        name: "nix".to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource { claim_name: nix_claim_name(id), read_only: Some(true) }),
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
            ("ephemeral-storage".to_string(), Quantity(EPHEMERAL_REQUEST.to_string())),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_limit.clone())),
            ("memory".to_string(), Quantity(res.memory_limit.clone())),
            ("ephemeral-storage".to_string(), Quantity(EPHEMERAL_LIMIT.to_string())),
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
        // The kernel's default syscall filter. Not required by `baseline` — which is why it was
        // missing — but it is free, needs no change to the image, and is the single largest
        // reduction in kernel attack surface available to a container that must run as root.
        // Both the NSA/CISA hardening guidance and PSA `restricted` ask for it.
        seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".to_string(), localhost_profile: None }),
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

/// The one definition of `GIT_SSH_COMMAND`, shared by the workspace container and the seeder. Two
/// copies of an ssh invocation that must agree is two invocations that will not.
///
/// `IdentitiesOnly` stops ssh offering an agent key first and getting refused for too many
/// attempts; `accept-new` trusts the host on first sight, which is the only workable answer when
/// nothing here has a known_hosts file.
fn git_ssh_command() -> EnvVar {
    EnvVar {
        name: "GIT_SSH_COMMAND".to_string(),
        value: Some(format!(
            "ssh -i {USER_KEY_PATH}/id_ed25519 -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
        )),
        ..Default::default()
    }
}

/// The container that seeds a `gitRepo` workspace, or `None` for any other source.
///
/// It runs INSIDE the workspace, over SSH, as the owner, with the platform key the pod already
/// mounts. That is the whole reason the credential Secret is gone: there is no third party to mint
/// a token for, and the git tier already decides what this key may read.
///
/// `repo` is `owner/name`, never a URL, and the host comes from the agent's env — a caller cannot
/// point this at an arbitrary endpoint, which would be an egress and SSRF primitive available to
/// anyone who can create a workspace. Both halves are validated HERE and not only at the API,
/// because this is the last place before the value becomes an ssh argv: anything that writes a
/// Volume by another path (a restored backup, kubectl) reaches this function and not that handler.
/// `Err` is a permanent failure, never a retry — a bad name never becomes a good one.
///
/// ponytail: `--depth 1` shallow, so `git log` in the workspace shows one commit; deepen on demand
/// if anyone asks for the history they did not ask to clone.
pub fn git_init_container(
    source: &crate::crd::VolumeSource,
    init_image: &str,
    ssh_host: &str,
    ssh_port: &str,
) -> Result<Option<Container>, String> {
    let crate::crd::VolumeSource::GitRepo { repo, branch } = source else { return Ok(None) };
    let ok = repo.split_once('/').is_some_and(|(o, n)| {
        rustic_git_storage::store::valid_owner(o) && rustic_git_storage::store::valid_segment(n)
    });
    if !ok {
        return Err(format!("source repo {repo:?} is not owner/name"));
    }
    // A leading `-` is an option, not a branch: `git clone --branch -upload-pack=…` is arbitrary
    // command execution on this pod. `..` is refused for the same reason `valid_segment` refuses it.
    if branch.is_empty() || branch.starts_with('-') || branch.contains("..") {
        return Err(format!("source branch {branch:?} is not a branch name"));
    }
    let url = if ssh_port.is_empty() {
        format!("ssh://git@{ssh_host}/{repo}.git")
    } else {
        format!("ssh://git@{ssh_host}:{ssh_port}/{repo}.git")
    };
    Ok(Some(Container {
        name: "git-seed".to_string(),
        image: Some(init_image.to_string()),
        // The empty-dir check is what makes this idempotent: a pod restart, a node reboot or a
        // second reconcile must never clone over work the user has done.
        command: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "set -e; [ \"$(ls -A /workspace)\" ] || git clone --depth 1 --single-branch --branch \"$BRANCH\" -- \"$URL\" /workspace"
                .to_string(),
        ]),
        env: Some(vec![
            EnvVar { name: "URL".to_string(), value: Some(url), ..Default::default() },
            EnvVar { name: "BRANCH".to_string(), value: Some(branch.clone()), ..Default::default() },
            git_ssh_command(),
        ]),
        volume_mounts: Some(vec![
            VolumeMount { name: "live".to_string(), mount_path: "/workspace".to_string(), ..Default::default() },
            VolumeMount {
                name: "user-key".to_string(),
                mount_path: USER_KEY_PATH.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        // ponytail: `hardened()` sets no `run_as_user`, so the seed lands as the INIT IMAGE's user
        // (root for `alpine/git`). A workspace image running as a non-root user would find its
        // clone unwritable; the fix then is an explicit `runAsUser` on both containers, from the
        // image's own uid.
        security_context: Some(hardened()),
        ..Default::default()
    }))
}

/// The workspace's one pod.
pub fn workspace_pod(spec: &WorkspaceSpec, id: &str, ctx: &PodContext, init: Option<Container>) -> Pod {
    let _ = ctx.node_name; // placement rides on the PV; kept in context for the PV builder
    let mut pod_spec = PodSpec {
        containers: vec![Container {
            name: "workspace".to_string(),
            image: Some(spec.image.clone()),
            // Only the default image is told what to run: it is a bare alpine whose one job is to
            // stay alive while people exec into it. A user's own image keeps its entrypoint — we
            // cannot know what it expects to run, and overriding it would break every image that
            // starts a daemon.
            command: (spec.image == crate::model::DEFAULT_WS_IMAGE)
                .then(|| vec!["sleep".to_string(), "infinity".to_string()]),
            volume_mounts: Some(vec![
                VolumeMount {
                    name: "live".to_string(),
                    mount_path: "/workspace".to_string(),
                    ..Default::default()
                },
                VolumeMount {
                    name: "user-key".to_string(),
                    mount_path: USER_KEY_PATH.to_string(),
                    read_only: Some(true),
                    ..Default::default()
                },
                // The store, and THIS workspace's profile only. Subpaths of one read-only claim:
                // `/nix` itself holds every other workspace's profile and the daemon socket.
                VolumeMount { name: "nix".to_string(), mount_path: "/nix/store".to_string(), sub_path: Some("store".to_string()), read_only: Some(true), ..Default::default() },
                VolumeMount { name: "nix".to_string(), mount_path: crate::packages::PROFILE_MOUNT.to_string(), sub_path: Some(format!("var/rustic/profiles/{id}")), read_only: Some(true), ..Default::default() },
            ]),
            // So `git` in the workspace uses the platform key without anyone configuring it.
            env: Some(vec![
                git_ssh_command(),
                // ponytail: an image with a non-standard PATH loses it; read it from the image
                // config via the registry if that ever matters.
                EnvVar { name: "PATH".into(), value: Some(crate::packages::path_env(None)), ..Default::default() },
                EnvVar { name: "NIX_PROFILE".into(), value: Some(crate::packages::PROFILE_LINK.into()), ..Default::default() },
                EnvVar { name: "MANPATH".into(), value: Some(format!("{}/share/man:", crate::packages::PROFILE_LINK)), ..Default::default() },
                EnvVar { name: "XDG_DATA_DIRS".into(), value: Some(format!("{}/share:/usr/local/share:/usr/share", crate::packages::PROFILE_LINK)), ..Default::default() },
            ]),
            resources: Some(quantities(&spec.resources)),
            security_context: Some(hardened()),
            ..Default::default()
        }],
        // Required, not optional, for a seeded workspace: the init container cannot clone without
        // the key.
        volumes: Some(vec![claim_volume(id), nix_volume(id), user_key_volume(init.is_some())]),
        init_containers: init.map(|c| vec![c]),
        // Optional by design: the kubelet ignores a named pull secret that does not exist, so a
        // public image keeps working in a namespace that has never been given a credential.
        image_pull_secrets: Some(vec![LocalObjectReference { name: PULL_SECRET.to_string() }]),
        // What `--restart unless-stopped` became: stopping is expressed by deleting the pod, not by
        // a policy the kubelet interprets.
        restart_policy: Some("Always".to_string()),
        runtime_class_name: ctx.runtime_class.map(str::to_string),
        ..Default::default()
    };
    placement(&mut pod_spec, "session");
    let mut m = meta(
        id,
        Some(&crate::crd::ws_namespace(&spec.owner, &spec.team)),
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

/// The env unit from the capacity model: 4 GB limit, packed at 1.5x oversubscription, so the
/// request is 4 GB / 1.5 = 2730Mi. Requesting 512Mi against a 4Gi limit was 8x oversubscription,
/// not 1.5x — five times more services on a node than the model prices, every one of them able to
/// claim memory that is not there.
///
/// CPU stays small deliberately: envs are memory-bound and idle services need almost none, so
/// packing is decided by memory alone.
///
/// One definition, used by both the Deployment and the namespace's `LimitRange`. Two copies of a
/// number that must agree is two numbers that will not.
pub fn env_unit_resources() -> PodResources {
    PodResources {
        cpu_request: "250m".into(),
        cpu_limit: "2".into(),
        memory_request: "2730Mi".into(),
        memory_limit: "4Gi".into(),
    }
}

/// One Deployment per service in an environment.
///
/// **Every mount goes through `validate_mount` here.** An environment has ONE volume, and each
/// declared mount is a folder inside it, expressed as a `subPath` on the shared claim. Kubernetes
/// rejects `..` in a subPath itself, but this does not lean on that: a folder is validated as a
/// single safe segment before it is ever formatted into one.
pub fn service_statefulset(
    svc: &model::Service,
    env_id: &str,
    owner: &str,
    ctx: &PodContext,
) -> Result<StatefulSet, String> {
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
            // Sorted: `env` is a HashMap, and a template whose variable order differs from the
            // last apply is a new revision — a rollout nobody asked for on every reconcile.
            env: Some(
                svc.env
                    .iter()
                    .collect::<std::collections::BTreeMap<_, _>>()
                    .into_iter()
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
            resources: Some(quantities(&env_unit_resources())),
            security_context: Some(hardened()),
            ..Default::default()
        }],
        volumes: Some(vec![claim_volume(env_id)]),
        // An environment's services are the likeliest place a private image appears — they are
        // whatever the user named, not our default.
        image_pull_secrets: Some(vec![LocalObjectReference { name: PULL_SECRET.to_string() }]),
        runtime_class_name: ctx.runtime_class.map(str::to_string),
        ..Default::default()
    };
    placement(&mut pod_spec, "env");

    Ok(StatefulSet {
        metadata: meta(
            &svc.name,
            Some(&crate::crd::env_namespace(env_id)),
            owner,
            "environment",
            &ctx.owner_ref,
        ),
        // A StatefulSet, not a Deployment, and the reason is its one-pod-per-ordinal guarantee:
        // `db-0` is never created until the previous `db-0` is fully gone — on updates AND on
        // node failures — where a Deployment surges a second pod first. Every service mounts the
        // environment's one subvolume, and two mongods on one WiredTiger directory is how a real
        // environment got a torn block. Availability is not what this object is for.
        spec: Some(StatefulSetSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(sel.clone()),
                ..Default::default()
            },
            // The ClusterIP Service of the same name: what makes `db:27017` resolve. Not headless,
            // and nothing here needs the per-ordinal `db-0.db` name.
            service_name: Some(svc.name.clone()),
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
        allow_internet_egress(ns, owner, owner_ref),
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

/// Everything a tenant must NEVER reach on egress, as CIDRs excluded from the public internet.
///
/// `169.254.0.0/16` is the one that matters most: `169.254.169.254` is the cloud instance metadata
/// service, and on Azure it hands out the NODE's managed identity to anything that asks. A tenant
/// that reaches it holds the node's cloud credentials, which is a full escape from the cluster, not
/// merely from the namespace.
///
/// The private ranges cover the pod network (10.42/16), the service network (10.43/16) and the
/// node subnet (10.60/16) without this code having to know them — and blocking all of RFC 1918
/// rather than the three specific ranges means a cluster that renumbers does not silently open a
/// hole. Nothing a dev workspace legitimately fetches lives on a private address.
const CLUSTER_INTERNALS: [&str; 4] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16"];

/// Egress to the public internet, and nothing private.
///
/// A workspace has to reach npm, crates.io, GitHub — a dev environment that cannot fetch a
/// dependency is not one. But "allow egress" written the obvious way (`0.0.0.0/0`) also opens the
/// metadata service and every internal address, which is why this is an allow-list with holes
/// punched OUT rather than a permit-all.
///
/// Additive with the rest: `allow-dns` still permits CoreDNS (inside 10/8, excluded here) and
/// `allow-same-namespace` still permits siblings, because NetworkPolicies union.
pub fn allow_internet_egress(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        "allow-internet-egress",
        ns,
        owner,
        owner_ref,
        NetworkPolicySpec {
            pod_selector: Some(LabelSelector::default()),
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![NetworkPolicyPeer {
                    ip_block: Some(IPBlock {
                        cidr: "0.0.0.0/0".to_string(),
                        except: Some(CLUSTER_INTERNALS.iter().map(|c| c.to_string()).collect()),
                    }),
                    ..Default::default()
                }]),
                ports: None,
            }]),
            ..Default::default()
        },
    )
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
        PodContext { pool: "/mnt/wspool", node_name: "session-0", owner_ref: owner_ref(), runtime_class: Some("gvisor") }
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
            restore: None,
            team: String::new(),
            owner: "alice".into(),
            name: "dev".into(),
            region: "centralindia".into(),
            image: "nginx:alpine".into(),
            storage: Some(crate::crd::WorkspaceStorage { quota_gb: 10, source: None }),
            volume_ref: Some("vol-1".into()),
            node_name: Some("session-0".into()),
            desired_state: DesiredState::Running,
            resources: PodResources::default(),
            packages: vec![],
        }
    }

    #[test]
    fn the_user_key_secret_carries_authorized_keys() {
        let s = user_key_secret("alice", "ws-alice", "PRIVATE", "ssh-ed25519 AAAA alice@laptop");
        let data = s.string_data.unwrap();
        assert_eq!(data["id_ed25519"], "PRIVATE");
        // sshd inside the workspace reads this file; it is the whole of "who may ssh in".
        assert_eq!(data["authorized_keys"], "ssh-ed25519 AAAA alice@laptop");
    }

    #[test]
    fn a_service_is_a_statefulset_with_a_stable_template() {
        let mut s = svc("data", "/data");
        s.env = [("Z", "1"), ("A", "2"), ("M", "3")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let d = service_statefulset(&s, "env-1", "team", &ctx()).unwrap();
        let spec = d.spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(spec.service_name.as_deref(), Some("web"), "the ClusterIP Service of the same name");
        let names: Vec<_> = spec.template.spec.unwrap().containers[0].env.as_ref().unwrap().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["A", "M", "Z"], "a stable template is what keeps the ReplicaSet from changing under a database");
    }

    #[test]
    fn a_service_deployment_refuses_a_mount_that_escapes_the_subvolume() {
        let ctx = ctx();
        let ok = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx).unwrap();
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
                service_statefulset(&svc(bad, "/host"), "env-1", "team", &ctx).is_err(),
                "folder {bad:?} must be refused"
            );
        }
        assert!(service_statefulset(&svc("data", "/data:/etc"), "env-1", "team", &ctx).is_err());
        assert!(service_statefulset(&svc("data", "relative"), "env-1", "team", &ctx).is_err());
    }

    /// Tenants share a node, so they share its kernel. A sandbox runtime puts a userspace kernel
    /// between the tenant and the host one — the only thing here that turns a kernel exploit from
    /// a host compromise into a sandbox escape.
    ///
    /// Opt-in: a `runtimeClassName` naming a runtime the node lacks makes every pod fail to start,
    /// so a cluster without gVisor installed must keep working.
    #[test]
    fn tenant_pods_run_under_the_sandbox_when_one_is_configured() {
        let ctx = ctx(); // runtime_class: Some("gvisor")
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx, None);
        assert_eq!(p.spec.unwrap().runtime_class_name.as_deref(), Some("gvisor"));

        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx).unwrap();
        assert_eq!(
            d.spec.unwrap().template.spec.unwrap().runtime_class_name.as_deref(),
            Some("gvisor"),
            "an environment's services are tenant workloads too"
        );

        // Unset means the host kernel, not a broken pod.
        let bare = PodContext { pool: "/mnt/wspool", node_name: "session-0", owner_ref: owner_ref(), runtime_class: None };
        assert!(workspace_pod(&ws_spec(), "ws-1", &bare, None).spec.unwrap().runtime_class_name.is_none());
    }

    #[test]
    fn no_pod_this_module_builds_uses_a_hostpath() {
        // hostPath is refused by PSA baseline AND restricted, so a single one here would force the
        // whole namespace to `privileged` — the regression this module exists to prevent.
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        for v in p.spec.unwrap().volumes.unwrap() {
            assert!(v.host_path.is_none(), "workspace pod must mount a claim, not a hostPath");
            // The key is a Secret; everything else is the workspace's data, which is a claim.
            assert!(v.persistent_volume_claim.is_some() || v.secret.is_some());
        }
        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx()).unwrap();
        for v in d.spec.unwrap().template.spec.unwrap().volumes.unwrap() {
            assert!(v.host_path.is_none(), "service pod must mount a claim, not a hostPath");
        }
    }

    #[test]
    fn the_volume_pins_the_node_and_never_deletes_the_data() {
        let pv = local_pv(&pv_name("ws-1"), &live_path(ctx().pool, "ws-1"), "ReadWriteOnce", 20, "alice", &ctx());
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

        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        assert!(
            p.spec.unwrap().node_name.is_none(),
            "naming a node here would make placement an assertion again"
        );
    }

    #[test]
    fn a_claim_binds_to_exactly_one_named_volume() {
        let c = claim("ws-alice", &claim_name("ws-1"), &pv_name("ws-1"), "ReadWriteOnce", 20, "alice", &owner_ref());
        assert_eq!(c.metadata.name.as_deref(), Some("live-ws-1"), "siblings share a namespace");
        let s = c.spec.unwrap();
        // Without volumeName the claim binds to whichever PV of this class fits — which, for
        // per-workspace storage, means somebody else's data.
        assert_eq!(s.volume_name.as_deref(), Some("pv-ws-1"));
        assert_eq!(s.storage_class_name.as_deref(), Some(STORAGE_CLASS));
    }

    #[test]
    fn a_user_pod_cannot_reach_the_api_server_or_escalate() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
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
    fn pod_requests_match_the_capacity_model() {
        let r = PodResources::default();
        assert_eq!(r.memory_request, "4Gi", "M workspace guarantee is 4 GB");
        assert_eq!(r.memory_limit, "8Gi", "M workspace limit is 8 GB");
        assert_eq!(r.cpu_request, "2", "2 vCPU guaranteed, and deliberately not oversubscribed");
        assert_eq!(r.cpu_limit, "4");

        // An environment service: 4 GB limit packed at 1.5x oversubscription.
        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx()).unwrap();
        let res = d.spec.unwrap().template.spec.unwrap().containers[0].resources.clone().unwrap();
        let req = res.requests.unwrap();
        let lim = res.limits.unwrap();
        assert_eq!(lim.get("memory").unwrap().0, "4Gi");
        assert_eq!(req.get("memory").unwrap().0, "2730Mi", "4 GB / 1.5x oversubscription");
    }

    /// The slot has to be enforced by the NAMESPACE, not just by the function that builds pods.
    /// A `LimitRange` is applied at admission, so it holds for a pod created by any path — a future
    /// code path that forgets, a debug pod, an operator with kubectl.
    #[test]
    fn the_namespace_refuses_anything_larger_than_its_slot() {
        let lr = limit_range("ws-alice", "alice", "workspace", &PodResources::default(), None);
        let item = &lr.spec.unwrap().limits[0];
        assert_eq!(item.type_, "Container");

        // max is the slot's LIMIT: bursting to it is the point, exceeding it is refused.
        let max = item.max.as_ref().unwrap();
        assert_eq!(max.get("memory").unwrap().0, "8Gi");
        assert_eq!(max.get("cpu").unwrap().0, "4");

        // defaultRequest is what capacity is priced on, for anything that names no request.
        let dr = item.default_request.as_ref().unwrap();
        assert_eq!(dr.get("memory").unwrap().0, "4Gi");
        assert_eq!(dr.get("cpu").unwrap().0, "2");

        // Shared user namespace: no ownerReference, or deleting one workspace drops the ceiling
        // for every sibling.
        assert!(lr.metadata.owner_references.is_none());

        // The environment ceiling matches the unit the Deployment actually requests.
        let env = limit_range("env-1", "team", "environment", &env_unit_resources(), Some(&owner_ref()));
        let env_item = &env.spec.unwrap().limits[0];
        assert_eq!(env_item.max.as_ref().unwrap().get("memory").unwrap().0, "4Gi");
        assert_eq!(env_item.default_request.as_ref().unwrap().get("memory").unwrap().0, "2730Mi");
    }

    /// The API's Secret access must be namespaced, never cluster-wide: a cluster-wide grant would
    /// include every Secret in the cluster, the agent's own credentials among them.
    #[test]
    fn the_api_secret_grant_is_scoped_to_one_namespace() {
        let rb = api_secret_binding("ws-alice", "alice", "rustic-git-api", "kube-system", None);
        assert_eq!(rb.metadata.namespace.as_deref(), Some("ws-alice"), "a RoleBinding, not a ClusterRoleBinding");
        assert_eq!(rb.role_ref.name, "rustic-git-api-secrets");
        assert_eq!(rb.role_ref.kind, "ClusterRole", "the rules are shared; only the scope is per namespace");
        let sub = &rb.subjects.unwrap()[0];
        assert_eq!(sub.name, "rustic-git-api");
        assert_eq!(sub.namespace.as_deref(), Some("kube-system"));
        // Shared user namespace: deleting one workspace must not revoke the grant for its siblings.
        assert!(rb.metadata.owner_references.is_none());
        // The OwnerBinding, and only it, may own the grant: it has the same (owner, node) lifetime.
        let ob = OwnerReference { kind: "OwnerBinding".into(), name: "r1-alice".into(), ..Default::default() };
        let owned = api_secret_binding("ws-alice", "alice", "rustic-git-api", "kube-system", Some(&ob));
        assert_eq!(owned.metadata.owner_references.unwrap()[0].kind, "OwnerBinding");
    }

    /// Three things have to line up for git in a workspace to authenticate, and each fails
    /// silently on its own: the mount, the 0400 mode ssh insists on, and the env var that tells
    /// git which key to use.
    #[test]
    fn a_workspace_pod_carries_the_owners_platform_key() {
        let spec = workspace_pod(&ws_spec(), "ws-1", &ctx(), None).spec.unwrap();
        let v = spec.volumes.unwrap().into_iter().find(|v| v.name == "user-key").expect("volume");
        let sv = v.secret.unwrap();
        assert_eq!(sv.secret_name.as_deref(), Some(USER_KEY_SECRET));
        assert_eq!(sv.default_mode, Some(0o400));
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
    fn tenant_pods_reference_the_namespace_pull_secret() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let refs = p.spec.unwrap().image_pull_secrets.unwrap();
        assert_eq!(refs[0].name, PULL_SECRET);

        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx()).unwrap();
        let refs = d.spec.unwrap().template.spec.unwrap().image_pull_secrets.unwrap();
        assert_eq!(refs[0].name, PULL_SECRET, "an env's services are where private images show up");
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
    fn a_workspace_pod_mounts_the_store_and_only_its_own_profile_read_only() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let c = &p.spec.as_ref().unwrap().containers[0];
        let mounts = c.volume_mounts.as_ref().unwrap();
        let store = mounts.iter().find(|m| m.mount_path == "/nix/store").expect("store mount");
        assert_eq!(store.read_only, Some(true));
        assert_eq!(store.sub_path.as_deref(), Some("store"));
        assert_eq!(store.name, "nix");
        let prof = mounts.iter().find(|m| m.mount_path == "/nix/profile").expect("profile mount");
        assert_eq!(prof.read_only, Some(true));
        assert_eq!(prof.sub_path.as_deref(), Some("var/rustic/profiles/ws-1"));
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
        assert_eq!(nix.persistent_volume_claim.as_ref().unwrap().claim_name, "nix-ws-1");
        assert_eq!(nix.persistent_volume_claim.as_ref().unwrap().read_only, Some(true));
        assert!(vols.iter().all(|v| v.host_path.is_none()), "workspace pod must mount a claim, not a hostPath");
    }

    #[test]
    fn the_nix_pv_is_read_only_and_pinned_to_the_node() {
        let pv = local_pv(&nix_pv_name("ws-1"), NIX_ROOT, "ReadOnlyMany", 1, "acme", &ctx());
        let spec = pv.spec.unwrap();
        assert_eq!(spec.local.as_ref().unwrap().path, "/nix");
        assert_eq!(spec.access_modes.as_deref(), Some(&["ReadOnlyMany".to_string()][..]));
        assert_eq!(spec.persistent_volume_reclaim_policy.as_deref(), Some("Retain"));
        let term = &spec.node_affinity.unwrap().required.unwrap().node_selector_terms[0];
        assert_eq!(term.match_expressions.as_ref().unwrap()[0].values.as_ref().unwrap()[0], ctx().node_name);
        let c = claim("ws-acme", &nix_claim_name("ws-1"), &nix_pv_name("ws-1"), "ReadOnlyMany", 1, "acme", &owner_ref());
        let cs = c.spec.unwrap();
        assert_eq!(cs.volume_name.as_deref(), Some("nix-ws-1"));
        assert_eq!(cs.access_modes.as_deref(), Some(&["ReadOnlyMany".to_string()][..]));
    }

    #[test]
    fn a_workspace_pod_mounts_its_volume_at_workspace_and_only_there() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let s = p.spec.unwrap();
        let claims = s.volumes.as_ref().unwrap().iter().filter(|v| v.name == "live" && v.persistent_volume_claim.is_some());
        assert_eq!(claims.count(), 1);
        let mounts = s.containers[0].volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.iter().filter(|m| m.name == "live").count(), 1, "the nginx web-root mount is gone with nginx");
        assert!(mounts.iter().any(|m| m.mount_path == "/workspace" && m.read_only.is_none()));
    }

    #[test]
    fn only_the_default_image_is_kept_alive_by_sleep() {
        let mut spec = ws_spec();
        spec.image = crate::model::DEFAULT_WS_IMAGE.into();
        let p = workspace_pod(&spec, "ws-1", &ctx(), None);
        assert_eq!(p.spec.unwrap().containers[0].command.as_deref(), Some(&["sleep".to_string(), "infinity".to_string()][..]));
        spec.image = "ghcr.io/acme/dev:1".into();
        let p = workspace_pod(&spec, "ws-1", &ctx(), None);
        assert!(p.spec.unwrap().containers[0].command.is_none(), "a user image keeps its entrypoint");
    }

    #[test]
    fn every_child_object_cascades_on_delete() {
        // Reclamation via garbage collection rather than cleanup code that can be skipped or crash
        // halfway. If this regresses, deleting a workspace leaks its pod, namespace and PV.
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        assert_eq!(p.metadata.owner_references.unwrap()[0].controller, Some(true));
        let pv = local_pv(&pv_name("ws-1"), &live_path(ctx().pool, "ws-1"), "ReadWriteOnce", 20, "alice", &ctx());
        assert_eq!(pv.metadata.owner_references.unwrap().len(), 1);
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
        assert_eq!(names, vec!["default-deny", "allow-dns", "allow-internet-egress", "allow-same-namespace"]);

        let deny = pols[0].spec.as_ref().unwrap();
        assert_eq!(deny.policy_types.as_ref().unwrap().len(), 2, "deny must cover BOTH directions");
        assert!(deny.ingress.is_none() && deny.egress.is_none(), "a rule here would stop it denying");

        let dns = pols[1].spec.as_ref().unwrap().egress.as_ref().unwrap();
        assert!(dns[0].ports.as_ref().unwrap().iter().any(|p| p.port == Some(IntOrString::Int(53))));
    }

    /// A workspace has to reach npm and GitHub, but "allow egress" written the obvious way
    /// (`0.0.0.0/0`) also opens `169.254.169.254` — the cloud metadata service, which on Azure
    /// hands out the NODE's managed identity. That is an escape from the cluster, not the
    /// namespace, so the internet rule must be an allow-list with holes punched out.
    #[test]
    fn internet_egress_never_reaches_the_metadata_service_or_the_cluster() {
        let pols = default_policies("ws-alice", "alice", &owner_ref());
        let net = pols.iter().find(|p| p.metadata.name.as_deref() == Some("allow-internet-egress")).unwrap();
        let rules = net.spec.as_ref().unwrap().egress.as_ref().unwrap();
        let block = rules[0].to.as_ref().unwrap()[0].ip_block.as_ref().unwrap();
        assert_eq!(block.cidr, "0.0.0.0/0");
        let except = block.except.as_ref().unwrap();

        // The metadata service, and every private range the cluster lives on.
        for cidr in ["169.254.0.0/16", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
            assert!(except.contains(&cidr.to_string()), "{cidr} must be excluded from egress");
        }
        // Egress-only: this rule must never become an ingress hole.
        assert_eq!(net.spec.as_ref().unwrap().policy_types.as_ref().unwrap(), &vec!["Egress".to_string()]);
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
