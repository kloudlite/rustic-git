//! What an in-force intercept renders: the proxy Pod that stands in for the intercepted service,
//! the workspace-side Service the proxy dials, and the egress grant that lets it.
//!
//! The service's own ClusterIP keeps its selector and now selects THIS pod (`proxy_selector`), so
//! the endpoints stay Kubernetes' own — no hand-written EndpointSlice, no abandoned `Endpoints`
//! object, no mirroring controller to race. The proxy copies bytes to the workspace.
//!
//! The objects sit in two namespaces because an ownerReference may not cross one: the Pod and the
//! egress policy are the Environment's, and the workspace-side target Service is the Workspace's.
//!
//! **The one fact the design rests on:** NetworkPolicy is evaluated after DNAT, on the BACKEND
//! pod's address — so an egress rule whose peer is `namespaceSelector: ws_ns` AND
//! `podSelector: WORKSPACE_LABEL` admits traffic the proxy sends to that Service's ClusterIP, even
//! though no policy may name a ClusterIP.
//!
//! The two halves of the grant are deliberately ASYMMETRIC, and this is the one place a reader will
//! expect symmetry and not find it: the environment side is per SERVICE
//! (`intercept_egress_name(ws_id, service)`), because one policy per workspace cannot express a
//! `podSelector` naming two different proxy pods; the workspace side stays per WORKSPACE
//! (`intercept_policy_name(ws_id)`), one ingress admitting the union of the ports that workspace
//! serves.

use super::*;

pub fn proxy_pod_name(service: &str) -> String {
    format!("intercept-{service}")
}


/// One Service per WORKSPACE, not per service: a workspace serving two of an environment's
/// services has one object, whose ports are the union both proxies dial.
pub fn target_service_name(ws_id: &str) -> String {
    format!("intercept-target-{ws_id}")
}


/// What the ClusterIP selects while the intercept is in force. `SERVICE_LABEL` carries the
/// service name so a second intercept in the same namespace cannot match this pod.
pub fn proxy_selector(owner: &str, service: &str) -> BTreeMap<String, String> {
    let mut l = labels(owner, "intercept");
    l.insert(SERVICE_LABEL.to_string(), service.to_string());
    l
}


pub struct RenderArgs<'a> {
    pub svc: &'a model::Service,
    pub ic: &'a crate::crd::Intercept,
    pub env_id: &'a str,
    pub owner: &'a str,
    /// The Environment — owns the proxy Pod and the egress policy.
    pub env_ref: &'a OwnerReference,
    pub ws_id: &'a str,
    pub ws_ns: &'a str,
    /// The Workspace — owns the target Service, since an ownerReference may not cross namespaces.
    pub ws_ref: &'a OwnerReference,
    /// The workspace-side ports of EVERY service this workspace serves in this environment
    /// (`intercepted_ports`), which is also what the ingress policy is scoped to.
    pub ws_ports: &'a [u16],
    pub image: &'a str,
    pub runtime_class: Option<&'a str>,
}


pub struct ProxyRender {
    pub pod: Pod,
    pub target: CoreService,
    pub egress: NetworkPolicy,
}


pub fn intercept_render(a: RenderArgs<'_>) -> Result<ProxyRender, String> {
    if a.image.is_empty() {
        return Err("no intercept proxy image is configured on this region's agent".into());
    }
    // A portless service has no ClusterIP either (`service_clusterip` returns `None`), so nothing
    // could dial the proxy — and the forwarder refuses an empty `--forward` list, which without
    // this would be a CrashLoopBackOff nobody can read the reason off. `/v1` does not refuse an
    // intercept of one: its port-mapping loop is vacuous when the service declares none.
    if a.svc.ports.is_empty() {
        return Err(format!("{} declares no ports, so there is nothing to intercept", a.svc.name));
    }
    Ok(ProxyRender {
        pod: proxy_pod(&a),
        target: target_service(&a),
        egress: intercept_egress(
            &crate::crd::env_namespace(a.env_id),
            a.ws_ns,
            a.ws_id,
            &a.svc.name,
            a.owner,
            a.env_ref,
        ),
    })
}


/// The stand-in for the intercepted service: it listens on exactly the ports callers dial and
/// forwards each to the workspace's own port for it.
fn proxy_pod(a: &RenderArgs<'_>) -> Pod {
    let mut metadata = meta(
        &proxy_pod_name(&a.svc.name),
        Some(&crate::crd::env_namespace(a.env_id)),
        a.owner,
        "intercept",
        a.env_ref,
    );
    metadata
        .labels
        .get_or_insert_with(BTreeMap::new)
        .insert(SERVICE_LABEL.to_string(), a.svc.name.clone());

    let mut args = vec![
        "--target".to_string(),
        // Fully qualified, trailing dot included: the proxy re-resolves per connection, and an
        // unrooted name walks the pod's whole `ndots: 5` search list first.
        format!("{}.{}.svc.cluster.local.", target_service_name(a.ws_id), a.ws_ns),
    ];
    for p in &a.svc.ports {
        args.push("--forward".to_string());
        args.push(format!("{p}:{}", a.ic.workspace_port(*p)));
    }

    Pod {
        metadata,
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "proxy".to_string(),
                image: Some(a.image.to_string()),
                args: Some(args),
                ports: Some(
                    a.svc
                        .ports
                        .iter()
                        .map(|p| ContainerPort { name: Some(format!("p{p}")), container_port: *p as i32, ..Default::default() })
                        .collect(),
                ),
                // It copies bytes; counted against the namespace ResourceQuota like everything
                // else, with no `Quota` dimension of its own.
                resources: Some(ResourceRequirements {
                    requests: Some(BTreeMap::from([
                        ("cpu".to_string(), Quantity("10m".to_string())),
                        ("memory".to_string(), Quantity("32Mi".to_string())),
                    ])),
                    limits: Some(BTreeMap::from([
                        ("cpu".to_string(), Quantity("200m".to_string())),
                        ("memory".to_string(), Quantity("128Mi".to_string())),
                    ])),
                    ..Default::default()
                }),
                // Readiness is the listener being up — Kubernetes' own check, no health endpoint
                // and no probe port. It is also what keeps the Service's endpoint from appearing
                // before the proxy can accept.
                // `ports` is non-empty — refused above — so the first is the first listener.
                readiness_probe: a.svc.ports.first().map(|p| Probe {
                    tcp_socket: Some(TCPSocketAction { port: IntOrString::Int(*p as i32), host: None }),
                    period_seconds: Some(2),
                    failure_threshold: Some(3),
                    ..Default::default()
                }),
                security_context: Some(SecurityContext {
                    run_as_user: Some(1000),
                    run_as_non_root: Some(true),
                    read_only_root_filesystem: Some(true),
                    ..hardened()
                }),
                ..Default::default()
            }],
            restart_policy: Some("Always".to_string()),
            // The proxy runs under gvisor exactly when the environment's own services do: a pod in
            // the data path must not be the one thing in the namespace outside the sandbox.
            runtime_class_name: a.runtime_class.map(str::to_string),
            // It makes no API call.
            automount_service_account_token: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    }
}


/// What the proxy dials. Its ports are `ws_ports` — the union over every service this workspace
/// serves in this environment — and not this service's alone, because there is ONE such Service per
/// workspace: a second proxy for a sibling service dials the same object and must find its port on
/// it.
fn target_service(a: &RenderArgs<'_>) -> CoreService {
    CoreService {
        metadata: meta(&target_service_name(a.ws_id), Some(a.ws_ns), a.owner, "intercept", a.ws_ref),
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(WORKSPACE_LABEL.to_string(), a.ws_id.to_string())])),
            ports: Some(
                a.ws_ports
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
