//! An environment's services: one StatefulSet per service with the env unit or its own
//! resources, the ClusterIP Service in front of it, the builder's wider security context, and the
//! EndpointSlice an intercept writes to send the service's traffic to a workspace.

use super::*;


/// The builder environment's context: buildkitd under gvisor, root, with the sandbox's OWN
/// capability list rather than `hardened()`'s. `hardened()` stays untouched above — this is a
/// second, narrower exception, not a widening of it.
///
/// Rootless cannot run under gvisor here: buildkitd's rootless mode needs `newuidmap`/`newgidmap`
/// to build a user namespace, and that needs capabilities `drop: ALL` forbids; buildkitd itself
/// refuses to start as a non-root user without one. So the builder runs as root instead, inside
/// the sandbox, with exactly the capabilities gvisor's kernel emulation needs — NOT the host's
/// list, gvisor intercepts and re-implements what these capabilities gate, so this add list is
/// sized to what its emulation checks for, not to what a bare-metal root would need to do the same
/// work. Found empirically: the 2026-09-09 spike started from `hardened()`'s list and added one
/// capability back at a time, each time until buildkitd's refusal changed to a different one.
pub(super) fn builder_hardened() -> SecurityContext {
    SecurityContext {
        run_as_user: Some(0),
        allow_privilege_escalation: Some(false),
        seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".to_string(), localhost_profile: None }),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: Some(
                [
                    "SYS_ADMIN", "CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "SETUID", "SETGID",
                    "SETPCAP", "SETFCAP", "MKNOD", "SYS_CHROOT", "KILL", "NET_BIND_SERVICE",
                    "NET_RAW", "AUDIT_WRITE",
                ]
                    .iter()
                    .map(|c| c.to_string())
                    .collect(),
            ),
        }),
        privileged: Some(false),
        ..Default::default()
    }
}


/// One StatefulSet per service in an environment.
///
/// **Every mount goes through `validate_mount` here.** An environment has ONE volume, and each
/// declared mount is a folder inside it, expressed as a `subPath` on the shared hostPath. Kubernetes
/// rejects `..` in a subPath itself, but this does not lean on that: a folder is validated as a
/// single safe segment before it is ever formatted into one.
pub fn service_statefulset(
    svc: &model::Service,
    env_id: &str,
    // The VOLUME the worktree lives under — the environment's own for one that owns its volume
    // (the same string as `env_id`), the SOURCE's for a restored/cloned environment, which holds a
    // second worktree of it. Separate parameters because a restored environment must not mount the
    // source's worktree: that is two environments writing one live subvolume.
    volume: &str,
    owner: &str,
    ctx: &PodContext,
) -> Result<StatefulSet, String> {
    // The API checked this at create; re-checked here because this is the last point before the
    // values become object names, and it also covers an Environment written by any other path.
    model::validate_service(svc)?;
    let mut mounts = Vec::new();
    for m in &svc.mounts {
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
            resources: Some(quantities(svc.resources.as_ref().unwrap_or(&env_unit_resources()))),
            security_context: Some(if ctx.system == Some(crate::crd::BUILDER_SYSTEM) { builder_hardened() } else { hardened() }),
            ..Default::default()
        }],
        // Volume root, environment leaf — the same split a workspace clone's mount uses.
        volumes: Some(vec![live_worktree_volume(ctx.pool, volume, env_id)]),
        // An environment's services are the likeliest place a private image appears — they are
        // whatever the user named, not our default.
        image_pull_secrets: Some(vec![LocalObjectReference { name: PULL_SECRET.to_string() }]),
        runtime_class_name: ctx.runtime_class.map(str::to_string),
        ..Default::default()
    };
    placement(&mut pod_spec, ctx.node_name);

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
///
/// `None` for a service with no declared ports: a `Service` with an empty `ports` list is rejected
/// by the API server (`spec.ports: Required value`), so a worker that listens on nothing (e.g.
/// `sleep 1d`) gets a StatefulSet but no ClusterIP — its bare name simply does not resolve, which
/// is correct for something nothing can connect to.
///
/// `intercepted` drops the selector. Kubernetes maintains the endpoints of a Service that HAS one,
/// and a selector can only ever match pods in the Service's own namespace — so it can never name a
/// workspace. Selector-less is the supported way to say "these exact addresses", and
/// `intercept_slice` supplies them; the ClusterIP, the DNS name and the ports callers dial are
/// untouched either way.
pub fn service_clusterip(
    svc: &model::Service,
    env_id: &str,
    owner: &str,
    owner_ref: &OwnerReference,
    intercepted: bool,
) -> Option<CoreService> {
    if svc.ports.is_empty() {
        return None;
    }
    let mut sel = labels(owner, "environment");
    sel.insert(SERVICE_LABEL.to_string(), svc.name.clone());
    Some(CoreService {
        metadata: meta(
            &svc.name,
            Some(&crate::crd::env_namespace(env_id)),
            owner,
            "environment",
            owner_ref,
        ),
        spec: Some(ServiceSpec {
            selector: (!intercepted).then_some(sel),
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
    })
}


/// The endpoints a selector-less intercepted Service is delivered to: the attached workspace's pod,
/// in another namespace, which kube-proxy programs without caring where the address lives.
///
/// The Service's `ports[].port` stays what callers dial and the slice's `ports[].port` is where it
/// lands; the two are matched BY NAME, so these entries must carry `service_clusterip`'s own
/// `p{port}` names or the remap silently does nothing.
///
/// `pod_ip: None` renders the ports with no address rather than nothing at all: an empty
/// `endpoints` list is a Service that refuses connections, which is what a workspace whose pod has
/// gone should do — the alternative, leaving stale endpoints, sends traffic to whoever holds that
/// IP next.
pub fn intercept_slice(
    svc: &model::Service,
    env_id: &str,
    owner: &str,
    owner_ref: &OwnerReference,
    ic: &crate::crd::Intercept,
    pod_ip: Option<&str>,
) -> EndpointSlice {
    let mut meta = meta(
        &format!("{}-intercept", svc.name),
        Some(&crate::crd::env_namespace(env_id)),
        owner,
        "environment",
        owner_ref,
    );
    // How kube-proxy joins a slice to its Service; without it the slice is inert.
    meta.labels
        .get_or_insert_with(BTreeMap::new)
        .insert("kubernetes.io/service-name".to_string(), svc.name.clone());
    EndpointSlice {
        metadata: meta,
        address_type: "IPv4".to_string(),
        endpoints: pod_ip
            .map(|ip| {
                vec![Endpoint {
                    addresses: vec![ip.to_string()],
                    // Stated rather than left to default: an endpoint the controller only writes
                    // once it has seen the pod Ready is ready, and a nil condition is a guess.
                    conditions: Some(EndpointConditions {
                        ready: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]
            })
            .unwrap_or_default(),
        ports: Some(
            svc.ports
                .iter()
                .map(|p| EndpointPort {
                    name: Some(format!("p{p}")),
                    port: Some(ic.workspace_port(*p) as i32),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                })
                .collect(),
        ),
    }
}
