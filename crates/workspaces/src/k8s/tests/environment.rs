//! An environment's objects: StatefulSets, ClusterIPs, intercept slices, the namespace's
//! LimitRange and ResourceQuota.

use super::*;


/// The env pod gets the same placement fix as the workspace pod: `service_statefulset` has no
/// PV to carry `nodeAffinity` any more either.
#[test]
pub(crate) fn the_service_pod_selects_its_node_by_hostname() {
    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx()).unwrap();
    let s = d.spec.unwrap().template.spec.unwrap();
    let sel = s.node_selector.expect("a node selector");
    assert_eq!(sel.get("kubernetes.io/hostname").map(String::as_str), Some("session-0"));
    assert_eq!(sel.get("kloudlite.io/pool").map(String::as_str), Some("true"));
    assert!(s.node_name.is_none(), "the scheduler still places the pod");
}


/// An environment's worktree is its OWN id under whatever volume it resolved to — volume root,
/// environment leaf. For one that owns its volume the two are the same string, so the path is
/// unchanged from the workspace pod's worktree-path mount.
#[test]
pub(crate) fn the_service_pods_live_mount_is_the_worktree_path() {
    let d = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx()).unwrap();
    let vols = d.spec.unwrap().template.spec.unwrap().volumes.unwrap();
    let live = vols.iter().find(|v| v.name == "live").unwrap();
    assert_eq!(live.host_path.as_ref().unwrap().path, format!("{}/vol/env-1/live/env-1", ctx().pool));
}


/// A RESTORED environment holds a SECOND worktree of the SOURCE's volume: the root comes from
/// the source, the leaf from itself, and its objects live in its own namespace. Mounting
/// `(source, source)` — what this did before the restored-environment fix — pointed two environments at one live
/// subvolume.
#[test]
pub(crate) fn a_restored_environments_live_mount_is_its_own_worktree_of_the_source_volume() {
    let d = service_statefulset(&svc("data", "/data"), "env-restored", "env-src", "team", &ctx()).unwrap();
    assert_eq!(
        d.metadata.namespace.as_deref(),
        Some(crate::crd::env_namespace("env-restored").as_str()),
        "its own namespace, never the source's"
    );
    let vols = d.spec.unwrap().template.spec.unwrap().volumes.unwrap();
    let live = vols.iter().find(|v| v.name == "live").unwrap();
    assert_eq!(live.host_path.as_ref().unwrap().path, format!("{}/vol/env-src/live/env-restored", ctx().pool));
}


/// A service's own `resources` overrides the environment unit; a service with none still gets
/// the unit (`env_unit_resources()`'s 2 vCPU limit), not an empty `ResourceRequirements`.
#[test]
pub(crate) fn a_service_with_its_own_resources_is_rendered_with_them_and_the_unit_otherwise() {
    let mut with_res = svc("data", "/data");
    with_res.resources = Some(PodResources::default());
    let sts = service_statefulset(&with_res, "bld-alice", "bld-alice", "alice", &ctx()).unwrap();
    let c = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(c.resources.as_ref().unwrap().limits.as_ref().unwrap()["cpu"].0, "4");

    let plain = service_statefulset(&svc("data", "/data"), "e", "e", "alice", &ctx()).unwrap();
    let p = &plain.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(p.resources.as_ref().unwrap().limits.as_ref().unwrap()["cpu"].0, "2");
}


/// The namespace ceiling follows the biggest service, so a builder's 4 vCPU pod is admitted
/// where an ordinary environment stays at the unit's 2.
#[test]
pub(crate) fn the_limit_range_ceiling_is_the_largest_service_shape() {
    let mut big = svc("buildkit", "/cache");
    big.resources = Some(PodResources::default());
    assert_eq!(env_limit_resources(&[svc("db", "/data"), big]).cpu_limit, "4");
    assert_eq!(env_limit_resources(&[svc("db", "/data")]).cpu_limit, "2");
    assert_eq!(env_limit_resources(&[]).cpu_limit, "2");
}


/// The spike's ruling, tested: an ordinary service keeps `hardened()`'s narrow list, and a
/// builder-environment service gets root plus gvisor's exact capability set — never the union
/// of the two, and never `hardened()`'s list silently widened.
#[test]
pub(crate) fn a_builder_service_gets_root_and_the_gvisor_capability_list_an_ordinary_one_does_not() {
    let mut builder_ctx = ctx();
    builder_ctx.system = Some(crate::crd::BUILDER_SYSTEM);
    let sts = service_statefulset(&svc("data", "/data"), "bld-alice", "bld-alice", "alice", &builder_ctx).unwrap();
    let sc = sts.spec.unwrap().template.spec.unwrap().containers[0].security_context.clone().unwrap();
    assert_eq!(sc.run_as_user, Some(0));
    let add = sc.capabilities.unwrap().add.unwrap();
    assert_eq!(
        add,
        vec![
            "SYS_ADMIN", "CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "SETUID", "SETGID",
            "SETPCAP", "SETFCAP", "MKNOD", "SYS_CHROOT", "KILL", "NET_BIND_SERVICE", "NET_RAW",
            "AUDIT_WRITE",
        ]
    );

    let ordinary = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "alice", &ctx()).unwrap();
    let sc2 = ordinary.spec.unwrap().template.spec.unwrap().containers[0].security_context.clone().unwrap();
    assert_eq!(sc2.run_as_user, None);
    assert_eq!(
        sc2.capabilities.unwrap().add.unwrap(),
        vec!["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID", "NET_BIND_SERVICE", "SYS_CHROOT"]
    );
}


#[test]
pub(crate) fn a_service_is_a_statefulset_with_a_stable_template() {
    let mut s = svc("data", "/data");
    s.env = [("Z", "1"), ("A", "2"), ("M", "3")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let d = service_statefulset(&s, "env-1", "env-1", "team", &ctx()).unwrap();
    let spec = d.spec.unwrap();
    assert_eq!(spec.replicas, Some(1));
    assert_eq!(spec.service_name.as_deref(), Some("web"), "the ClusterIP Service of the same name");
    let names: Vec<_> = spec.template.spec.unwrap().containers[0].env.as_ref().unwrap().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["A", "M", "Z"], "a stable template is what keeps the ReplicaSet from changing under a database");
}


#[test]
pub(crate) fn a_service_deployment_refuses_a_mount_that_escapes_the_subvolume() {
    let ctx = ctx();
    let ok = service_statefulset(&svc("data", "/data"), "env-1", "env-1", "team", &ctx).unwrap();
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
            service_statefulset(&svc(bad, "/host"), "env-1", "env-1", "team", &ctx).is_err(),
            "folder {bad:?} must be refused"
        );
    }
    assert!(service_statefulset(&svc("data", "/data:/etc"), "env-1", "env-1", "team", &ctx).is_err());
    assert!(service_statefulset(&svc("data", "relative"), "env-1", "env-1", "team", &ctx).is_err());
}


/// The slot has to be enforced by the NAMESPACE, not just by the function that builds pods.
/// A `LimitRange` is applied at admission, so it holds for a pod created by any path — a future
/// code path that forgets, a debug pod, an operator with kubectl.
#[test]
pub(crate) fn the_namespace_refuses_anything_larger_than_its_slot() {
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


#[test]
pub(crate) fn a_resource_quota_caps_the_namespaces_limits() {
    let rq = resource_quota("ws-alice", "alice", "workspace", &crate::crd::default_quota(false));
    let hard = rq.spec.unwrap().hard.unwrap();
    assert_eq!(hard["limits.cpu"].0, "40");
    assert_eq!(hard["limits.memory"].0, "80Gi");
    assert_eq!(rq.metadata.labels.unwrap()["kloudlite.io/owner"], "alice");
    // No ownerReference, the same reason the namespace and the LimitRange have none: the cap
    // is shared by every workspace in here and must not vanish with any one of them.
    assert!(rq.metadata.owner_references.is_none());
}


/// The API's Secret access must be namespaced, never cluster-wide: a cluster-wide grant would
/// include every Secret in the cluster, the agent's own credentials among them.
#[test]
pub(crate) fn the_api_secret_grant_is_scoped_to_one_namespace() {
    let rb = api_secret_binding("ws-alice", "alice", "kloudlite-api", "kube-system", None);
    assert_eq!(rb.metadata.namespace.as_deref(), Some("ws-alice"), "a RoleBinding, not a ClusterRoleBinding");
    assert_eq!(rb.role_ref.name, "kloudlite-api-secrets");
    assert_eq!(rb.role_ref.kind, "ClusterRole", "the rules are shared; only the scope is per namespace");
    let sub = &rb.subjects.unwrap()[0];
    assert_eq!(sub.name, "kloudlite-api");
    assert_eq!(sub.namespace.as_deref(), Some("kube-system"));
    // Shared user namespace: deleting one workspace must not revoke the grant for its siblings.
    assert!(rb.metadata.owner_references.is_none());
    // The OwnerBinding, and only it, may own the grant: it has the same (owner, node) lifetime.
    let ob = OwnerReference { kind: "OwnerBinding".into(), name: "r1-alice".into(), ..Default::default() };
    let owned = api_secret_binding("ws-alice", "alice", "kloudlite-api", "kube-system", Some(&ob));
    assert_eq!(owned.metadata.owner_references.unwrap()[0].kind, "OwnerBinding");
}


#[test]
pub(crate) fn a_namespace_enforces_privileged_and_audits_restricted() {
    let ns = namespace("ws-alice", "alice", "workspace", None);
    let l = ns.metadata.labels.unwrap();
    // privileged is what hostPath mounts require; audit/warn stay at restricted so the gap
    // between what's enforced and what's actually safe keeps showing up.
    assert_eq!(l.get("pod-security.kubernetes.io/enforce").map(String::as_str), Some("privileged"));
    assert_eq!(l.get("pod-security.kubernetes.io/audit").map(String::as_str), Some("restricted"));
}


#[test]
pub(crate) fn a_service_gets_a_clusterip_for_each_declared_port() {
    let s = service_clusterip(&svc("data", "/data"), "env-1", "team", &owner_ref(), false).unwrap();
    let spec = s.spec.unwrap();
    let ports = spec.ports.unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].port, 80);
    assert_eq!(ports[0].target_port, Some(IntOrString::Int(80)));
    // The selector must match the Deployment's template labels or the Service selects nothing
    // and the name resolves to a black hole.
    assert_eq!(spec.selector.unwrap().get(SERVICE_LABEL).map(String::as_str), Some("web"));
}


/// The Service keeps the dialled number and the slice carries the mapped one; they are joined
/// by the `p{port}` name, so a rename here breaks the remap silently.
#[test]
pub(crate) fn the_slice_delivers_a_mapped_port_and_the_service_still_dials_the_declared_one() {
    let s = two_port_svc();
    let ic = intercept(&[(80, 3000)]);
    let sl = intercept_slice(&s, "env-1", "team", &owner_ref(), &ic, Some("10.42.3.231"));
    assert_eq!(sl.metadata.name.as_deref(), Some("web-intercept"));
    assert_eq!(sl.metadata.namespace.as_deref(), Some("env-1"));
    assert_eq!(sl.metadata.labels.as_ref().unwrap()["kubernetes.io/service-name"], "web");
    assert_eq!(sl.address_type, "IPv4");

    let ports = sl.ports.unwrap();
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0].name.as_deref(), Some("p80"));
    assert_eq!(ports[0].port, Some(3000), "the mapped port");
    // Unmapped: answered on its own number.
    assert_eq!(ports[1].name.as_deref(), Some("p5432"));
    assert_eq!(ports[1].port, Some(5432));

    assert_eq!(sl.endpoints[0].addresses, vec!["10.42.3.231".to_string()]);

    let dialled = service_clusterip(&s, "env-1", "team", &owner_ref(), true).unwrap().spec.unwrap();
    let dialled = dialled.ports.unwrap();
    assert_eq!(dialled[0].name.as_deref(), Some("p80"), "the join is by NAME");
    assert_eq!(dialled[0].port, 80, "what callers dial never changes");
}


/// No pod means no address, but the slice still has to exist with its ports: an intercepted
/// Service with no endpoints refuses connections, which is right, while a slice with stale
/// endpoints sends traffic to whoever holds that IP next.
#[test]
pub(crate) fn a_slice_with_no_pod_ip_has_ports_but_no_endpoints() {
    let sl = intercept_slice(&two_port_svc(), "env-1", "team", &owner_ref(), &intercept(&[]), None);
    assert!(sl.endpoints.is_empty());
    assert_eq!(sl.ports.unwrap().len(), 2);
}


/// A Service that keeps its selector has its endpoints overwritten by Kubernetes, and the
/// selector can only match pods in the environment's own namespace — the intercept would
/// silently never take effect.
#[test]
pub(crate) fn an_intercepted_service_has_no_selector() {
    let s = two_port_svc();
    let on = service_clusterip(&s, "env-1", "team", &owner_ref(), true).unwrap();
    assert!(on.spec.unwrap().selector.is_none());
    let off = service_clusterip(&s, "env-1", "team", &owner_ref(), false).unwrap();
    assert!(off.spec.unwrap().selector.is_some());
}


/// Same AND-not-OR rule as the attach pair: two peers would open the whole workspace namespace
/// to the environment, plus any pod anywhere carrying that workspace label.
#[test]
pub(crate) fn the_intercept_policies_name_one_peer_each() {
    let r = owner_ref();
    let eg = intercept_egress("env-abc", "ws-acme", "ws-1", "acme", &r);
    assert_eq!(eg.metadata.name.as_deref(), Some("intercept-ws-1"));
    assert_eq!(eg.metadata.namespace.as_deref(), Some("env-abc"));
    let spec = serde_json::to_value(eg.spec.unwrap()).unwrap();
    let to = spec["egress"][0]["to"].as_array().unwrap();
    assert_eq!(to.len(), 1, "two peers is an OR, not an AND");
    assert_eq!(to[0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"], "ws-acme");
    assert_eq!(to[0]["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
    assert_eq!(spec["policyTypes"], serde_json::json!(["Egress"]));

    let ing = intercept_ingress("ws-acme", "env-abc", "ws-1", "acme", &r);
    assert_eq!(ing.metadata.namespace.as_deref(), Some("ws-acme"));
    let spec = serde_json::to_value(ing.spec.unwrap()).unwrap();
    // Its peer is a whole namespace, so the scoping to this one pod is the top-level selector.
    assert_eq!(spec["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
    assert_eq!(spec["ingress"][0]["from"].as_array().unwrap().len(), 1);
    assert_eq!(spec["policyTypes"], serde_json::json!(["Ingress"]));
}


#[test]
pub(crate) fn a_service_with_no_ports_gets_no_clusterip() {
    let mut s = svc("worker", "/data");
    s.ports.clear();
    // An empty `ports` list on a k8s Service is rejected by the API server, so a portless
    // service must not produce one at all — its StatefulSet still runs, it is just unreachable
    // by name, which is correct for something that listens on nothing.
    assert!(service_clusterip(&s, "env-1", "team", &owner_ref(), false).is_none());
}
