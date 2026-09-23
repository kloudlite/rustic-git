//! Every NetworkPolicy: the default deny with DNS, internet egress that excludes the cluster and
//! the metadata service, the gateway's hole for port 22, the builder gate's two policies, and the
//! pairs that open an attachment or an intercept between a workspace and an environment, and the
//! bench's hole to its workspaces' tool port, and tenants' one hole to the node collector's OTLP port.

use super::*;


/// The WORKSPACE side of an intercept grant: one ingress per workspace, admitting the union of the
/// ports it serves, so a release can delete it by name without a lookup.
///
/// The two halves no longer share a name — see `intercept_egress_name` for the environment side and
/// the asymmetry that forces it.
pub fn intercept_policy_name(ws_id: &str) -> String {
    format!("intercept-{ws_id}")
}


/// The ENVIRONMENT side, per SERVICE rather than per workspace: the policy's own `podSelector` now
/// names one proxy pod, and one object cannot name two of them — a workspace serving two of an
/// environment's services needs two grants.
pub fn intercept_egress_name(ws_id: &str, service: &str) -> String {
    format!("intercept-{ws_id}-{service}")
}


/// Lets the intercept's proxy pod reach the intercepting workspace pod — the attach pair's
/// direction reversed, and needed for the same reason: `allow_internet_egress` excludes RFC 1918,
/// so the workspace's pod IP is unreachable from an environment pod by default.
///
/// Namespace and pod selector sit in ONE element of `to`, which ANDs them; as two elements they
/// would OR, opening the whole workspace namespace and every pod anywhere carrying that label.
///
/// Strictly tighter than before: only the proxy may reach the workspace, where every pod in the
/// environment could dial it directly. The ingress half stays scoped to the one workspace pod,
/// since an owner's workspaces share a namespace.
pub fn intercept_egress(env_ns: &str, ws_ns: &str, ws_id: &str, service: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &intercept_egress_name(ws_id, service),
        env_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": { "matchLabels": proxy_selector(owner, service) },
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
///
/// Scoped to `ports`, the workspace-side ports of the intercept: the tool server listens on the pod
/// IP with no auth of its own, so every port would hand the environment an `exec`. Empty admits
/// nothing, never everything — an ingress rule without `ports` means all of them.
pub fn intercept_ingress(ws_ns: &str, env_ns: &str, ws_id: &str, ports: &[u16], owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    let ingress: Vec<serde_json::Value> = if ports.is_empty() {
        vec![]
    } else {
        vec![json!({
            "from": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": env_ns } } }],
            "ports": ports.iter().map(|p| json!({ "protocol": "TCP", "port": p })).collect::<Vec<_>>(),
        })]
    };
    policy(
        &intercept_policy_name(ws_id),
        ws_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
            "policyTypes": ["Ingress"],
            "ingress": ingress,
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


/// The node-local collector's Service (`deploy/k3s/otel-agent.yaml`, `internalTrafficPolicy: Local`),
/// handed to the tool server and the bench as `KLOUDLITE_OTLP_URL`. Plain HTTP: no TLS in `kl`.
pub const OTLP_URL: &str = "http://kloudlite-otel-agent-otlp.kube-system.svc:4318";

/// The policies every namespace gets: deny everything, allow DNS out, internet egress, OTLP to the
/// node collector, and the namespace talking to itself.
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
            "allow-otlp",
            ns,
            owner,
            owner_ref,
            // The tool server and the bench export spans to the collector on their own node;
            // `allow-internet-egress` excludes the cluster, so without this every export drops
            // and the bench -> tool server hop never reaches HyperDX. Only the collector's pods
            // (namespace AND pod in ONE peer — two peers would OR into all of kube-system) and
            // only OTLP/HTTP: nothing in a tenant pod speaks gRPC to it. No CIDR.
            // Environment namespaces get it too: harmless, since the only peer is the collector.
            json!({
                "podSelector": {},
                "policyTypes": ["Egress"],
                "egress": [{
                    "to": [{
                        "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "kube-system" } },
                        "podSelector": { "matchLabels": { "app": "kloudlite-otel-agent" } },
                    }],
                    "ports": [{ "protocol": "TCP", "port": 4318 }],
                }],
            }),
        ),
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


/// Workspace pods accept IDE_PORT from bench pods in their own namespace: a person's bench runs
/// their workspace sessions. `allow-same-namespace` admits this today; the grant is named so the
/// bench keeps its path if that is ever narrowed.
///
/// A bench pod carries `WORKSPACE_LABEL` too (its id), so the target excludes `kind=bench`: a
/// bench serves no tools. No `namespaceSelector` in the peer — this namespace only, which is the
/// owner (or the owner in one team), never another person's bench.
// ponytail: AKS runs no network policy engine; the fence holds on the k3s regions where benches run
pub fn allow_bench_tools(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        "allow-bench-tools",
        ns,
        owner,
        owner_ref,
        json!({
            // The TOOL server's pod only — a bench serves no tools. The SHELL port below is on
            // this pod too, and on the bench's own pod; the bench pod's own shell is reached
            // through `allow_gateway_bench`'s peer, which already admits the gateway.
            "podSelector": { "matchExpressions": [
                { "key": WORKSPACE_LABEL, "operator": "Exists" },
                { "key": KIND_LABEL, "operator": "NotIn", "values": ["bench"] },
            ] },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{ "podSelector": { "matchLabels": { KIND_LABEL: "bench" } } }],
                // `SHELL_PORT` beside `IDE_PORT` (spec §2.3): the desktop reaches a workspace's
                // terminal by splicing through the person's own bench, so the same peer that may
                // call the tool server may dial ttyd — and nothing else may dial either.
                "ports": [
                    { "protocol": "TCP", "port": IDE_PORT },
                    { "protocol": "TCP", "port": SHELL_PORT },
                ],
            }],
        }),
    )
}


/// The port `kloudlite-kompress` listens on (`deploy/k3s/kompress.yaml`) — one process, no
/// egress need of its own, and this is the only hole a bench's egress gets to reach it.
const KOMPRESS_PORT: i32 = 8787;


/// A bench's one egress hole to the region's Kompress service, the exact shape of
/// `builder_gate_egress`: `kloudlite-system`, a label, one port. Empty `kompressUrl` means no
/// service is deployed, but the policy is unconditional — an unreachable ClusterIP costs nothing
/// and this stays simple instead of threading the setting into `binding.rs`'s ensure call.
// ponytail: AKS runs no network policy engine; the fence holds on the k3s regions where benches run
pub fn allow_bench_kompress(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        "allow-bench-kompress",
        ns,
        owner,
        owner_ref,
        json!({
            "podSelector": { "matchLabels": { KIND_LABEL: "bench" } },
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": GATEWAY_NAMESPACE } },
                    "podSelector": { "matchLabels": { "app": "kloudlite-kompress" } },
                }],
                "ports": [{ "protocol": "TCP", "port": KOMPRESS_PORT }],
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


/// The name of a space's egress half in its own namespace. One per space, not per pod: the
/// choice is the whole space's, so every pod in the namespace follows it.
pub const SPACE_EGRESS_POLICY: &str = "space-env";


/// The name of a space's ingress half in the environment's namespace, keyed by the space so two
/// spaces pointing at one environment never share (or delete) each other's grant.
pub fn space_ingress_name(space_ns: &str) -> String {
    format!("space-{space_ns}")
}


/// Lets every pod of a person's space reach the environment's namespace. Namespace-wide on
/// purpose (the per-pod `attach_egress` stopped at one workspace): the choice belongs to the space,
/// and a pod kind added later follows it with no wiring of its own. Egress needs its own rule
/// because `allow_internet_egress` excludes RFC 1918.
pub fn space_egress(space_ns: &str, env_ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        SPACE_EGRESS_POLICY,
        space_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Egress"],
            "egress": [{ "to": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": env_ns } } }] }],
        }),
    )
}


/// Lets the environment accept every pod of that one space. The peer is the space NAMESPACE alone,
/// which is exactly the space: a `ws_namespace` holds one person in one team and nobody else.
pub fn space_ingress(env_ns: &str, space_ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &space_ingress_name(space_ns),
        env_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Ingress"],
            "ingress": [{ "from": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": space_ns } } }] }],
        }),
    )
}
