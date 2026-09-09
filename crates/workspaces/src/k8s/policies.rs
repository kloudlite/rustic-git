//! Every NetworkPolicy: the default deny with DNS, internet egress that excludes the cluster and
//! the metadata service, the gateway's hole for port 22, the builder gate's two policies, and the
//! pairs that open an attachment or an intercept between a workspace and an environment.

use super::*;


/// Both halves of an intercept grant share this name, one in each namespace, so a release can
/// delete them by name without a lookup.
pub fn intercept_policy_name(ws_id: &str) -> String {
    format!("intercept-{ws_id}")
}


/// Lets the environment's pods reach the intercepting workspace pod — the attach pair's direction
/// reversed, and needed for the same reason: `allow_internet_egress` excludes RFC 1918, so the
/// workspace's pod IP is unreachable from an environment pod by default.
///
/// Namespace and pod selector sit in ONE element of `to`, which ANDs them; as two elements they
/// would OR, opening the whole workspace namespace and every pod anywhere carrying that label.
///
/// The policy's own `podSelector` is empty — every pod in the environment — because any of them
/// may be the one dialling the intercepted service, and they are all this environment's already.
/// The ingress half is the opposite: it names the one workspace pod, since an owner's workspaces
/// share a namespace.
pub fn intercept_egress(env_ns: &str, ws_ns: &str, ws_id: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &intercept_policy_name(ws_id),
        env_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": ws_ns } },
                    "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
                }],
            }],
        }),
    )
}


/// Lets that one workspace pod accept the environment's namespace. Scoped to the pod by the
/// policy's own `podSelector`: an owner's workspaces share a namespace, so a namespace-wide rule
/// would open every workspace they have to this environment.
pub fn intercept_ingress(ws_ns: &str, env_ns: &str, ws_id: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &intercept_policy_name(ws_id),
        ws_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": env_ns } } }],
            }],
        }),
    )
}


/// The specs below are static JSON rather than nested `Some(vec![…])` structs: they never branch,
/// and the shape a reviewer has to check against the Kubernetes docs is the shape they read here.
pub(super) fn policy(name: &str, ns: &str, owner: &str, owner_ref: &OwnerReference, spec: serde_json::Value) -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta(name, Some(ns), owner, "policy", owner_ref),
        spec: Some(serde_json::from_value(spec).expect("static NetworkPolicy spec")),
    }
}


/// The three policies every namespace gets: deny everything, allow DNS out, allow the namespace to
/// talk to itself.
///
/// Generated rather than rendered from YAML so there is exactly one definition of the isolation
/// rule. Order does not matter — NetworkPolicies are additive, and the default-deny is expressed by
/// selecting every pod with no rules rather than by precedence.
pub fn default_policies(ns: &str, owner: &str, owner_ref: &OwnerReference) -> Vec<NetworkPolicy> {
    vec![
        policy(
            "default-deny",
            ns,
            owner,
            owner_ref,
            json!({ "podSelector": {}, "policyTypes": ["Ingress", "Egress"] }),
        ),
        policy(
            "allow-dns",
            ns,
            owner,
            owner_ref,
            // To CoreDNS specifically, by its namespace's well-known label. Without this rule
            // every lookup fails, which is the most common way a default-deny namespace looks
            // like "the network is broken". The namespace label alone admits every kube-system
            // pod on 53 — the agent's peer listener among them — so `k8s-app: kube-dns` (CoreDNS's
            // own label in k3s) narrows it; both selectors must stay in ONE peer, or Kubernetes
            // ORs them back into "all of kube-system OR any k8s-app=kube-dns pod anywhere".
            json!({
                "podSelector": {},
                "policyTypes": ["Egress"],
                "egress": [{
                    "to": [{
                        "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "kube-system" } },
                        "podSelector": { "matchLabels": { "k8s-app": "kube-dns" } },
                    }],
                    "ports": [
                        { "protocol": "UDP", "port": 53 },
                        { "protocol": "TCP", "port": 53 },
                    ],
                }],
            }),
        ),
        allow_internet_egress(ns, owner, owner_ref),
        policy(
            "allow-same-namespace",
            ns,
            owner,
            owner_ref,
            // An environment's services must reach each other — that is what an environment IS.
            json!({
                "podSelector": {},
                "policyTypes": ["Ingress", "Egress"],
                "ingress": [{ "from": [{ "podSelector": {} }] }],
                "egress": [{ "to": [{ "podSelector": {} }] }],
            }),
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
pub(super) const CLUSTER_INTERNALS: [&str; 4] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16"];


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
        json!({
            "podSelector": {},
            "policyTypes": ["Egress"],
            "egress": [{ "to": [{ "ipBlock": { "cidr": "0.0.0.0/0", "except": CLUSTER_INTERNALS } }] }],
        }),
    )
}


/// The namespace `deploy/k3s/gateway.yaml` puts the gateway in. Its own, not `kube-system`: the
/// gateway is the internet-facing process, and the namespace used to be chosen only so this
/// policy's selector could name it — a `namespaceSelector` names any namespace just as well.
pub const GATEWAY_NAMESPACE: &str = "kloudlite-system";


/// The one hole in a workspace namespace's default-deny ingress: port 22, from the gateway pods in
/// `GATEWAY_NAMESPACE` and nothing else.
///
/// Both selectors sit in ONE peer, which is an AND. Written as two peers it would be an OR, and
/// any pod in the cluster — including another tenant's workspace — could reach every sshd by
/// labelling itself `app=kloudlite-gateway`.
pub fn allow_gateway_ingress(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        "allow-gateway-ssh",
        ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": GATEWAY_NAMESPACE } },
                    "podSelector": { "matchLabels": { "app": "kloudlite-gateway" } },
                }],
                "ports": [{ "protocol": "TCP", "port": 22 }],
            }],
        }),
    )
}


/// The port the gate and buildkitd both listen on — one hop from a workspace, through the gate,
/// to the builder's own buildkit service. Task 7's contract for the gate pod's label; the gate is
/// the only thing that may dial a builder's buildkit, so this is the only egress a workspace gets
/// beyond DNS, the gateway and the public internet.
pub(super) const BUILDER_GATE_PORT: i32 = 1234;


/// The one hole a workspace's egress gets to reach its builder: the gate, and nothing past it. A
/// workspace never dials buildkitd directly — `builder_gate_ingress` is what admits the gate to
/// the builder's own namespace, and this is the other half of that one path.
pub fn builder_gate_egress(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        "allow-builder-gate",
        ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": GATEWAY_NAMESPACE } },
                    "podSelector": { "matchLabels": { "app": "kloudlite-builder-gate" } },
                }],
                "ports": [{ "protocol": "TCP", "port": BUILDER_GATE_PORT }],
            }],
        }),
    )
}


/// The one hole in a builder environment's ingress: the gate, and only the gate, reaching its
/// buildkit service pod — never the whole namespace, which would let any other tenant's pod that
/// somehow lands here dial buildkitd directly.
pub fn builder_gate_ingress(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        "allow-builder-gate",
        ns,
        owner,
        owner_ref,
        json!({
            "podSelector": { "matchLabels": { SERVICE_LABEL: "buildkit" } },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": GATEWAY_NAMESPACE } },
                    "podSelector": { "matchLabels": { "app": "kloudlite-builder-gate" } },
                }],
                "ports": [{ "protocol": "TCP", "port": BUILDER_GATE_PORT }],
            }],
        }),
    )
}


/// Both halves of an attachment grant share this name, one in each namespace, so a detach can
/// delete them by name without a lookup.
pub fn attach_policy_name(ws_id: &str) -> String {
    format!("attach-{ws_id}")
}


/// Lets one workspace pod reach the environment's namespace.
///
/// Egress needs its own rule because `allow_internet_egress` deliberately excludes RFC 1918, so
/// the pod network is unreachable by default — the environment's ClusterIP included. Selects the
/// POD by `WORKSPACE_LABEL`, never the namespace: an owner's workspaces share a namespace, so a
/// namespace-wide grant would open every workspace they own to this one environment.
pub fn attach_egress(ws_ns: &str, ws_id: &str, env_ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &attach_policy_name(ws_id),
        ws_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": env_ns } } }],
            }],
        }),
    )
}


/// Lets the environment accept that one workspace pod.
///
/// Namespace and pod selector sit in ONE element of `from`, which ANDs them: as two elements they
/// would OR, admitting every pod in the workspace namespace and every pod anywhere carrying that
/// label — including another owner's workspace that happens to share the same id.
pub fn attach_ingress(env_ns: &str, ws_ns: &str, ws_id: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &attach_policy_name(ws_id),
        env_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": ws_ns } },
                    "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
                }],
            }],
        }),
    )
}
