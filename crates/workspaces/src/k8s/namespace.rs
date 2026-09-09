//! Per-owner and per-environment namespaces and what caps them: the `LimitRange` (one slot per
//! container), the `ResourceQuota` (the namespace's total), and the RoleBindings that let the api
//! write its pull credential there. The env unit — one service's request and limit — lives here
//! because the LimitRange and the StatefulSets must agree on it.

use super::*;


/// `ws-{id}` / `env-{id}`, labelled for the policies that select it and for Pod Security Admission.
pub fn namespace(name: &str, owner: &str, kind: &str, owner_ref: Option<&OwnerReference>) -> Namespace {
    let mut l = labels(owner, kind);
    // `privileged` because these pods mount host paths: their storage IS the node's filesystem,
    // and `baseline` forbids `hostPath` outright. This is the price of removing the PV layer;
    // `deploy/k3s/workspace-admission.yaml` puts the refused fields back as a
    // ValidatingAdmissionPolicy scoped to namespaces carrying this label, so `hostNetwork`,
    // `hostPID`, `hostIPC`, privileged containers and stray hostPath sources are still refused at
    // admission, not merely by what this code happens to construct. `audit` and `warn` stay at
    // `restricted` so the gap keeps showing up in audit rather than going quiet.
    l.insert("pod-security.kubernetes.io/enforce".into(), "privileged".into());
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


/// The namespace's TOTAL ceiling, as against `limit_range`'s per-container one.
///
/// Enforced by the API server at admission, so it holds for a pod created by any path — a future
/// code path that forgets, a debug pod, an operator with kubectl. That is what makes it the hard
/// stop behind `/v1`'s read-then-write check, which can overshoot by one under concurrency.
///
/// Only cpu and memory: disk is bounded per volume by its own btrfs qgroup, and counts have no
/// Kubernetes expression at all.
///
/// ponytail: the cap is PER NAMESPACE, and a person in several teams has one namespace per team,
/// so the platform-side ceiling repeats per team while `/v1`'s count is the exact per-owner
/// number. Collapse to one namespace per team, or sum across namespaces here, if the platform side
/// ever has to be exact too.
pub fn resource_quota(ns: &str, owner: &str, kind: &str, q: &crate::crd::QuotaSpec) -> ResourceQuota {
    ResourceQuota {
        metadata: ObjectMeta {
            name: Some("owner-quota".to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(owner, kind)),
            // None, for the same reason the namespace and the LimitRange carry none: this is the
            // shared ceiling of everything in here, not a possession of any one object.
            ..Default::default()
        },
        spec: Some(ResourceQuotaSpec {
            hard: Some(BTreeMap::from([
                ("limits.cpu".to_string(), Quantity(q.cpu.to_string())),
                ("limits.memory".to_string(), Quantity(format!("{}Gi", q.memory_gb))),
            ])),
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
    secret_binding(ns, owner, "api-secrets", "kloudlite-api-secrets", api_service_account, api_namespace, owner_ref)
}


/// The agent's OWN per-namespace Secret grant, for the `ws-ssh-{id}` host keys it reads and
/// creates. The alternative was `secrets: get, create` cluster-wide on the agent's ClusterRole —
/// which included `kloudlite-jwt` and the api's credentials in `kube-system`, so one compromised
/// node could read every tenant's signing key. Bound here, in the namespace this same reconciler
/// just made, so the grant exists before the first workspace's `ensure_ssh` needs it
/// (`namespace_ready` gates that). The ClusterRole is in `deploy/k3s/agent-rbac.yaml`; the
/// admission policy beside it pins which roles this binding may name.
pub const AGENT_SERVICE_ACCOUNT: &str = "kloudlite-agent";

pub const AGENT_NAMESPACE: &str = "kube-system";
pub fn agent_secret_binding(ns: &str, owner: &str, owner_ref: &OwnerReference) -> RoleBinding {
    secret_binding(ns, owner, "agent-secrets", "kloudlite-agent-ws-secrets", AGENT_SERVICE_ACCOUNT, AGENT_NAMESPACE, Some(owner_ref))
}


pub(super) fn secret_binding(
    ns: &str,
    owner: &str,
    name: &str,
    role: &str,
    service_account: &str,
    sa_namespace: &str,
    owner_ref: Option<&OwnerReference>,
) -> RoleBinding {
    RoleBinding {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(owner, "workspace")),
            owner_references: owner_ref.map(|r| vec![r.clone()]),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: role.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: service_account.to_string(),
            namespace: Some(sa_namespace.to_string()),
            ..Default::default()
        }]),
    }
}


/// The env unit from the capacity model (`docs/capacity-model.md` — one environment SERVICE, and
/// the sheet's row of that name): 4 GB limit, packed at 1.5x oversubscription, so the
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


/// The namespace `LimitRange` ceiling for an environment: the LARGEST shape any of its services
/// renders with, the env unit when none names its own. `limit_range`'s `max` is enforced at
/// admission, so a service that asks for more than the unit — the builder, at
/// `PodResources::default()` — was refused by the API server before it ever ran, and the
/// StatefulSet retried forever against a ceiling nothing else could raise.
pub fn env_limit_resources(services: &[model::Service]) -> PodResources {
    use crate::quota::{mebibytes, millicores};
    services
        .iter()
        .filter_map(|s| s.resources.clone())
        .chain(std::iter::once(env_unit_resources()))
        .max_by_key(|r| (millicores(&r.cpu_limit), mebibytes(&r.memory_limit)))
        .expect("the unit is always a candidate")
}
