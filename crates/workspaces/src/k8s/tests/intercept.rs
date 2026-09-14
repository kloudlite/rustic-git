//! What an in-force intercept renders. Nothing here touches a cluster.

use super::*;
use crate::{crd, k8s};

fn service(name: &str, ports: &[u16]) -> model::Service {
    model::Service {
        name: name.into(),
        image: "nginx".into(),
        command: vec![],
        env: Default::default(),
        mounts: vec![],
        ports: ports.to_vec(),
        resources: None,
    }
}


fn intercept(service: &str, ws: &str, ports: &[(u16, u16)]) -> crd::Intercept {
    crd::Intercept {
        service: service.into(),
        workspace: ws.into(),
        ports: ports.iter().map(|(s, w)| crd::PortMap { service: *s, workspace: *w }).collect(),
    }
}


fn env_ref() -> OwnerReference {
    OwnerReference { kind: "Environment".into(), name: "1".into(), uid: "env-uid".into(), ..owner_ref() }
}


fn ws_ref() -> OwnerReference {
    OwnerReference { kind: "Workspace".into(), name: "ws-1".into(), uid: "ws-uid".into(), ..owner_ref() }
}


fn args<'a>(svc: &'a model::Service, ic: &'a crd::Intercept, env_ref: &'a OwnerReference, ws_ref: &'a OwnerReference, ports: &'a [u16]) -> k8s::RenderArgs<'a> {
    k8s::RenderArgs {
        svc,
        ic,
        env_id: "1",
        owner: "alice",
        env_ref,
        ws_id: "ws-1",
        ws_ns: "ws-alice",
        ws_ref,
        ws_ports: ports,
        image: "ghcr.io/kloudlite/kloudlite-intercept-proxy:test",
        runtime_class: Some("gvisor"),
    }
}


#[test]
fn the_proxy_forwards_one_port_per_declared_service_port_with_the_remap() {
    let svc = service("api", &[8080, 9229]);
    let ic = intercept("api", "ws-1", &[(8080, 3000)]);
    let r = k8s::intercept_render(args(&svc, &ic, &env_ref(), &ws_ref(), &[3000, 9229])).unwrap();
    let c = &r.pod.spec.as_ref().unwrap().containers[0];
    assert_eq!(
        c.args.as_ref().unwrap(),
        &vec![
            "--target".to_string(),
            "intercept-target-ws-1.ws-alice.svc.cluster.local.".to_string(),
            "--forward".to_string(),
            "8080:3000".to_string(),
            // Unmapped ports forward straight through — `workspace_port` is identity for them.
            "--forward".to_string(),
            "9229:9229".to_string(),
        ]
    );
    assert_eq!(
        r.pod.spec.as_ref().unwrap().runtime_class_name.as_deref(),
        Some("gvisor"),
        "a data-path pod must be sandboxed exactly when its neighbours are"
    );
    let sc = c.security_context.as_ref().unwrap();
    assert_eq!(sc.run_as_user, Some(1000));
    assert_eq!(sc.read_only_root_filesystem, Some(true));

    let spec = r.pod.spec.as_ref().unwrap();
    assert_eq!(spec.restart_policy.as_deref(), Some("Always"));
    assert_eq!(spec.automount_service_account_token, Some(false), "it makes no API call");
    let res = c.resources.as_ref().unwrap();
    assert_eq!(res.requests.as_ref().unwrap()["cpu"].0, "10m");
    assert_eq!(res.requests.as_ref().unwrap()["memory"].0, "32Mi");
    assert_eq!(res.limits.as_ref().unwrap()["cpu"].0, "200m");
    assert_eq!(res.limits.as_ref().unwrap()["memory"].0, "128Mi");
    let probe = c.readiness_probe.as_ref().unwrap();
    assert_eq!(probe.tcp_socket.as_ref().unwrap().port, IntOrString::Int(8080), "the first listener being up IS readiness");
    assert_eq!((probe.period_seconds, probe.failure_threshold), (Some(2), Some(3)));
}


/// A portless service has no ClusterIP to dial and gives the forwarder no `--forward` at all: a
/// pod that crash-loops where an error would have said why.
#[test]
fn a_portless_service_is_refused_rather_than_rendered() {
    let svc = service("api", &[]);
    let ic = intercept("api", "ws-1", &[]);
    assert!(k8s::intercept_render(args(&svc, &ic, &env_ref(), &ws_ref(), &[])).is_err());
}


#[test]
fn the_clusterip_keeps_a_selector_and_it_names_the_proxy() {
    let svc = service("api", &[8080]);
    let cs = k8s::service_clusterip(&svc, "1", "alice", &env_ref(), true).unwrap();
    let sel = cs.spec.as_ref().unwrap().selector.as_ref().expect("an intercepted Service must keep a selector — that is the whole fix");
    assert_eq!(sel.get(k8s::KIND_LABEL).map(String::as_str), Some("intercept"));
    assert_eq!(sel.get(k8s::SERVICE_LABEL).map(String::as_str), Some("api"));
    // And the proxy pod carries exactly those labels, or the Service selects nothing.
    let ic = intercept("api", "ws-1", &[]);
    let pod = k8s::intercept_render(args(&svc, &ic, &env_ref(), &ws_ref(), &[8080])).unwrap().pod;
    let l = pod.metadata.labels.as_ref().unwrap();
    assert!(sel.iter().all(|(k, v)| l.get(k) == Some(v)), "the proxy pod must match its own Service's selector");
}


#[test]
fn the_target_service_publishes_the_workspace_side_union() {
    let svc = service("api", &[8080]);
    let ic = intercept("api", "ws-1", &[(8080, 3000)]);
    let r = k8s::intercept_render(args(&svc, &ic, &env_ref(), &ws_ref(), &[3000, 5432])).unwrap();
    assert_eq!(r.target.metadata.namespace.as_deref(), Some("ws-alice"));
    assert_eq!(r.target.metadata.owner_references.as_ref().unwrap()[0].kind, "Workspace", "an ownerReference may not cross namespaces");
    let ports = r.target.spec.as_ref().unwrap().ports.as_ref().unwrap();
    assert_eq!(
        ports.iter().map(|p| (p.name.clone().unwrap(), p.port)).collect::<Vec<_>>(),
        vec![("p3000".to_string(), 3000), ("p5432".to_string(), 5432)]
    );
    assert_eq!(
        r.target.spec.as_ref().unwrap().selector.as_ref().unwrap().get(k8s::WORKSPACE_LABEL).map(String::as_str),
        Some("ws-1")
    );
}


#[test]
fn the_egress_peer_is_the_workspace_pod_and_the_subject_is_the_proxy_alone() {
    let svc = service("api", &[8080]);
    let ic = intercept("api", "ws-1", &[(8080, 3000)]);
    let r = k8s::intercept_render(args(&svc, &ic, &env_ref(), &ws_ref(), &[3000])).unwrap();
    assert_eq!(r.egress.metadata.name.as_deref(), Some("intercept-ws-1-api"), "per service, not per workspace");
    let spec = serde_json::to_value(r.egress.spec.unwrap()).unwrap();
    assert_eq!(spec["podSelector"]["matchLabels"][k8s::SERVICE_LABEL], "api", "every pod in the environment must NOT be the subject any more");
    let to = &spec["egress"][0]["to"];
    assert_eq!(to.as_array().unwrap().len(), 1, "namespace and pod selector must AND, in one element");
    assert_eq!(to[0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"], "ws-alice");
    assert_eq!(to[0]["podSelector"]["matchLabels"][k8s::WORKSPACE_LABEL], "ws-1");
}


#[test]
fn an_unset_image_is_an_error_not_a_pod_that_cannot_pull() {
    let svc = service("api", &[8080]);
    let ic = intercept("api", "ws-1", &[]);
    let (er, wr) = (env_ref(), ws_ref());
    let mut a = args(&svc, &ic, &er, &wr, &[8080]);
    a.image = "";
    assert!(k8s::intercept_render(a).is_err());
}
