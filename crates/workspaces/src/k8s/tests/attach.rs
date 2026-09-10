//! The attachment path: the rendered resolv.conf, its mount, and the policy pair.

use super::*;


/// Unattached: the workspace's own namespace leads, and everything the agent's file said about
/// nameserver, ndots and the node's suffix is carried through untouched.
#[test]
pub(crate) fn an_unattached_resolv_conf_is_what_kubelet_would_have_written() {
    let got = resolv_conf(AGENT_RESOLV, "ws-acme", None);
    assert_eq!(
        got,
        "search ws-acme.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
    );
}


/// Attached: the environment's namespace goes FIRST, so a name the environment defines wins
/// over one in the workspace's own namespace.
#[test]
pub(crate) fn an_attached_resolv_conf_searches_the_environment_first() {
    let got = resolv_conf(AGENT_RESOLV, "ws-acme", Some("env-abc"));
    assert_eq!(
        got,
        "search env-abc.svc.cluster.local ws-acme.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
    );
}


/// A cluster started with a non-default `--cluster-domain` must not get `cluster.local`
/// search entries — the domain is derived from the template, never assumed.
#[test]
pub(crate) fn a_non_default_cluster_domain_is_derived_from_the_template() {
    let template = "search kube-system.svc.cluster.internal svc.cluster.internal cluster.internal node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n";
    let got = resolv_conf(template, "ws-acme", Some("env-abc"));
    assert_eq!(
        got,
        "search env-abc.svc.cluster.internal ws-acme.svc.cluster.internal svc.cluster.internal cluster.internal node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
    );
}


/// A template with no search line at all still yields a usable file rather than a malformed one.
#[test]
pub(crate) fn a_template_without_a_search_line_gains_one() {
    let got = resolv_conf("nameserver 10.43.0.10\n", "ws-acme", Some("env-abc"));
    assert_eq!(
        got,
        "search env-abc.svc.cluster.local ws-acme.svc.cluster.local svc.cluster.local cluster.local\nnameserver 10.43.0.10\n"
    );
}


/// Per NAMESPACE, like the home claim: a local PV binds to one claim, but one claim serves
/// every pod in the namespace. The per-workspace part is the subPath, not the object.
#[test]
pub(crate) fn the_attach_paths_are_per_workspace_under_the_pool() {
    assert_eq!(attach_root("/pool"), "/pool/attach");
    assert_eq!(attach_file("/pool", "ws-1"), "/pool/attach/ws-1/resolv.conf");
}


/// The mount is what makes attachment live: the agent rewrites the host file and the running
/// pod sees it. Read-only so the person in the workspace cannot point their own DNS elsewhere.
#[test]
pub(crate) fn a_workspace_pod_mounts_its_own_resolv_conf() {
    let spec = ws_spec();
    let pod = workspace_pod(&spec, "ws-1", "ws-1", &ctx(), None).unwrap();
    let podspec = pod.spec.unwrap();
    let vol = podspec.volumes.unwrap().into_iter().find(|v| v.name == "attach").expect("attach volume");
    let h = vol.host_path.unwrap();
    assert_eq!(h.path, attach_file(ctx().pool, "ws-1"));
    assert_eq!(h.type_.as_deref(), Some("File"));
    let mount = podspec.containers[0]
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.mount_path == "/etc/resolv.conf")
        .expect("resolv.conf mount");
    assert!(mount.sub_path.is_none(), "the volume IS the file now");
    assert_eq!(mount.read_only, Some(true));
}


/// The grant selects the POD, never the namespace: an owner's workspaces share a namespace, so
/// a namespace-wide rule would open every workspace they have to the environment.
#[test]
pub(crate) fn the_attachment_egress_selects_one_workspace_pod() {
    let p = attach_egress("ws-acme", "ws-1", "env-abc", "acme", &owner_ref());
    assert_eq!(p.metadata.name.as_deref(), Some("attach-ws-1"));
    assert_eq!(p.metadata.namespace.as_deref(), Some("ws-acme"));
    let spec = serde_json::to_value(p.spec.unwrap()).unwrap();
    assert_eq!(spec["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
    assert_eq!(spec["policyTypes"], serde_json::json!(["Egress"]));
    assert_eq!(
        spec["egress"][0]["to"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
        "env-abc"
    );
}


/// The environment side names both the namespace and the pod: a namespace selector alone would
/// admit every workspace of every owner who happens to share that namespace.
#[test]
pub(crate) fn the_attachment_ingress_names_the_namespace_and_the_pod() {
    let p = attach_ingress("env-abc", "ws-acme", "ws-1", "acme", &owner_ref());
    assert_eq!(p.metadata.namespace.as_deref(), Some("env-abc"));
    let spec = serde_json::to_value(p.spec.unwrap()).unwrap();
    let from = &spec["ingress"][0]["from"][0];
    assert_eq!(from["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"], "ws-acme");
    assert_eq!(from["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
    assert_eq!(spec["policyTypes"], serde_json::json!(["Ingress"]));
}
