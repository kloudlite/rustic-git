//! Pure builders from the domain types to Kubernetes objects.
//!
//! No client, no I/O, no environment reads — every input arrives as an argument, which is what
//! makes the security-relevant paths here exhaustively testable. The one exception is `client`,
//! which BUILDS the kube client every tier talks through (api and agent alike) and holds no
//! domain type of its own.
//!
//! A workspace is a btrfs subvolume on one node, mounted straight in with `hostPath` — the pods
//! carry their own `nodeSelector` (see `placement`) so the scheduler enforces placement, and the
//! namespace runs `privileged` PSA to admit the mount (see below). Every `hostPath` here is typed
//! (`Directory`/`File`) so a missing path is a mount failure, never a silently-created empty dir.
//! See `namespace` for why the PSA floor is `privileged` now.
//!
//! The module map, one file per object family:
//! - `namespace`: namespaces, LimitRange, ResourceQuota, the api's RoleBindings, the env unit
//! - `secrets`: the user-key and pull Secrets, the sshd host key and config
//! - `workspace`: the workspace Pod and everything mounted into it
//! - `attach`: the per-workspace resolv.conf and the projected authorized_keys file
//! - `environment`: a service's StatefulSet, ClusterIP, and the intercept EndpointSlice
//! - `policies`: every NetworkPolicy
//! - `bench`: the bench Pod, its home-relative folder, and its gateway-only ingress policy
//! - `tests`: one file, since the fixtures are shared
//!

use crate::crd::{PodResources, WorkspaceSpec};
use crate::model;
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EnvVar, HostPathVolumeSource, LimitRange, LimitRangeItem, LimitRangeSpec, Probe, TCPSocketAction,
    KeyToPath, LocalObjectReference, Namespace, ResourceQuota, ResourceQuotaSpec, SeccompProfile, Pod,
    PodSpec, PodTemplateSpec, ResourceRequirements, Secret, SecretVolumeSource,
    SecurityContext, Service as CoreService,
    ServicePort, ServiceSpec, Toleration, Volume, VolumeMount,
};
use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort, EndpointSlice};
use k8s_openapi::api::rbac::v1::{RoleBinding, RoleRef, Subject};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde_json::json;
use std::collections::BTreeMap;

/// The bounded kube client — shared by `bins/api` and `bins/agent`, so it lives here rather than
/// in either.
pub mod client;
mod namespace;
mod secrets;
mod workspace;
mod attach;
mod environment;
mod policies;
mod bench;
pub use namespace::*;
pub use secrets::*;
pub use workspace::*;
pub use attach::*;
pub use environment::*;
pub use policies::*;
pub use bench::*;

#[cfg(test)]
mod tests;


pub const OWNER_LABEL: &str = "kloudlite.io/owner";

pub const KIND_LABEL: &str = "kloudlite.io/kind";
/// The team a workspace was made in, empty for personal. Same rule as the other two: a listing
/// view of `spec.team`, re-stamped by the controller, never authorization.
pub const TEAM_LABEL: &str = "kloudlite.io/team";
/// A view of `Workspace.spec.attachedEnvironment`, same rule as the other three labels: `attach_ws`
/// authorizes through `may_act_on` (team members included), so a plain `owner` selector on
/// `delete_env`'s sweep misses a teammate's attached workspace — this label is what the sweep
/// selects on instead. Stamped by `/v1`'s attach/detach handlers and re-stamped from spec by
/// `heal_labels` on every reconcile; never a decision, only a listing shortcut.
pub const ATTACHED_ENV_LABEL: &str = "kloudlite.io/attached-environment";

pub const SERVICE_LABEL: &str = "kloudlite.io/service";
/// The container's writable layer and logs — NOT the tenant's data, which lives on their btrfs
/// subvolume and is bounded by its own qgroup quota.
///
/// Unbounded, this is a node-wide denial of service available to any tenant: filling the kubelet's
/// disk taints the node `disk-pressure` and stops scheduling for every OTHER tenant on it. That is
/// not theoretical — it happened to this cluster from an ordinary build, and nothing in the
/// workload could have caused the kubelet to evict the offender instead of penalising the node.
/// With a limit the offending pod is evicted and its neighbours are untouched.
pub(super) const EPHEMERAL_REQUEST: &str = "1Gi";
pub(super) const EPHEMERAL_LIMIT: &str = "4Gi";


/// The label naming which workspace a pod belongs to. Load-bearing since workspaces share a
/// namespace: an attachment selects on it, so without it a grant would reach every workspace the
/// user owns.
pub const WORKSPACE_LABEL: &str = "kloudlite.io/workspace";

/// Where `kl ide serve` listens in every workspace pod, on the pod IP: the owner's bench dials it
/// there (`allow_bench_tools`), and `kl-connect ws ide` reaches it over the ssh tunnel.
pub const IDE_PORT: u16 = 7788;


pub struct PodContext<'a> {
    /// The btrfs pool root on the node, e.g. `/wspool-prod`. Every volume builder needs it: a
    /// pod's `hostPath` is computed from it directly now, not resolved through a claim.
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
    /// The tagged image behind `model::DEFAULT_WS_IMAGE`, from the agent's `WS_DEFAULT_IMAGE`.
    pub default_image: &'a str,
    /// The environment's own `EnvironmentSpec::system` (`crd::BUILDER_SYSTEM` for the hidden
    /// per-owner buildkitd environment, `None` for one a person created and for every workspace
    /// pod — a field on the context rather than an extra `service_statefulset` parameter because
    /// every call site already builds one of these per reconcile, and a workspace's is always
    /// `None`.
    pub system: Option<&'a str>,
    /// `WS_REGISTRY_HOST` — the platform registry's external host, learned the same way
    /// `registry::auth::realm()` learns it, because the agent has no route to that api-tier env.
    /// Fed to `login_env` as `KL_REGISTRY_HOST`, the credential helper's `credHelpers` key.
    pub registry_host: &'a str,
}


pub(crate) fn labels(owner: &str, kind: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (OWNER_LABEL.to_string(), owner.to_string()),
        (KIND_LABEL.to_string(), kind.to_string()),
    ])
}


pub(super) fn meta(name: &str, ns: Option<&str>, owner: &str, kind: &str, owner_ref: &OwnerReference) -> ObjectMeta {
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


/// Where sshd reads its config and host key. `/etc/ssh` is not a choice: `sshd` resolves relative
/// paths and its own defaults against it, and a config elsewhere still sends it looking here.
pub const SSHD_DIR: &str = "/etc/ssh";


/// Where sshd expects the owner's public keys. Unlike the git key this one CANNOT move: sshd
/// matches the file's path and mode against what its config declares, and nothing else reads it.
/// Who you are inside a workspace. Not root: sshd refuses root outright (`PermitRootLogin no`),
/// so a leaked key is a shell as an ordinary user, and everything a person writes lands owned
/// by an ordinary user. There is no sudo — root is `kubectl exec`, and installing software is
/// `spec.packages`. The uid is fixed so the home keeps its owner across pod restarts and image
/// changes.
pub const SSH_USER: &str = "kl";
/// Where the workspace's own btrfs volume is mounted, in full, as the home: no shared NFS home,
/// no per-node cache volume, one subvolume that IS `/home/kl` (2026-09-22 ruling: "this will
/// simplify our design"). Source lives at `WORKSPACE_DIR`, everything else the platform mints is
/// a mount over the tree (keys, resolv.conf); everything the person or a tool writes travels with
/// the volume on clone/restore/push.
pub const HOME_DIR: &str = "/home/kl";
/// The source tree inside the volume — `kl` and the tool server root here, `repo`/`branch`
/// seeding clones here.
pub const WORKSPACE_DIR: &str = "/home/kl/workspace";

pub const SSH_UID: i64 = 1000;
// Not under /home/kl: the whole home is the workspace volume now, and nothing else may mount
// at or below it — this is the one thing the platform must project into every pod regardless
// (2026-09-22 ruling). sshd reads `AuthorizedKeysFile` from wherever we tell it, so a sibling
// directory works exactly as well as a path under the home used to.
pub(super) const AUTHORIZED_KEYS_PATH: &str = "/etc/kloudlite/authorized_keys";


pub(super) fn quantities(res: &PodResources) -> ResourceRequirements {
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
