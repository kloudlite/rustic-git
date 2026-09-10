//! Every NetworkPolicy, asserted by what it lets through and what it does not.

use super::*;


/// The gate is the only thing that may reach a builder, and a workspace reaching the gate is
/// the only new hole this task opens — one peer, both selectors, same AND trap as
/// `allow_gateway_ingress`.
#[test]
pub(crate) fn only_the_builder_gate_may_be_reached_on_egress() {
    let p = builder_gate_egress("ws-alice", "alice", &owner_ref());
    assert_eq!(p.metadata.name.as_deref(), Some("allow-builder-gate"));
    assert_eq!(p.metadata.namespace.as_deref(), Some("ws-alice"));
    assert_eq!(p.metadata.owner_references.unwrap()[0].controller, Some(true));
    let spec = p.spec.unwrap();
    assert_eq!(spec.policy_types.as_ref().unwrap(), &vec!["Egress".to_string()], "never an ingress hole here");
    let pod_sel = spec.pod_selector.unwrap();
    assert!(pod_sel.match_labels.is_none() && pod_sel.match_expressions.is_none(), "every pod in the namespace");
    let rule = &spec.egress.as_ref().unwrap()[0];
    let to = rule.to.as_ref().unwrap();
    assert_eq!(to.len(), 1, "one peer: namespace AND pod, not namespace OR pod");
    let ns = to[0].namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(ns["kubernetes.io/metadata.name"], "kloudlite-system");
    let pod = to[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(pod["app"], "kloudlite-builder-gate");
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(1234)));
}


/// Only the gate may dial buildkit — never any other pod, in this namespace or any other.
#[test]
pub(crate) fn only_the_builder_gate_may_reach_buildkit() {
    let p = builder_gate_ingress("env-bld-alice", "alice", &owner_ref());
    assert_eq!(p.metadata.name.as_deref(), Some("allow-builder-gate"));
    assert_eq!(p.metadata.namespace.as_deref(), Some("env-bld-alice"));
    assert_eq!(p.metadata.owner_references.unwrap()[0].controller, Some(true));
    let spec = p.spec.unwrap();
    assert_eq!(spec.policy_types.as_ref().unwrap(), &vec!["Ingress".to_string()], "never an egress hole here");
    let sel = spec.pod_selector.unwrap().match_labels.clone().unwrap();
    assert_eq!(sel[SERVICE_LABEL], "buildkit", "only the buildkit service pod, not the whole namespace");
    let rule = &spec.ingress.as_ref().unwrap()[0];
    let from = rule.from.as_ref().unwrap();
    assert_eq!(from.len(), 1, "one peer: namespace AND pod, not namespace OR pod");
    let ns = from[0].namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(ns["kubernetes.io/metadata.name"], "kloudlite-system");
    let pod = from[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(pod["app"], "kloudlite-builder-gate");
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(1234)));
}


/// Port 22 is open to exactly one peer. Without the namespace half every tenant's own pods
/// could label themselves `app=kloudlite-gateway` and reach each other's sshd.
#[test]
pub(crate) fn only_the_gateway_may_reach_port_22() {
    let p = allow_gateway_ingress("ws-alice", "alice", &owner_ref());
    assert_eq!(p.metadata.name.as_deref(), Some("allow-gateway-ssh"));
    let spec = p.spec.unwrap();
    assert_eq!(spec.policy_types.as_ref().unwrap(), &vec!["Ingress".to_string()], "never an egress hole");
    let rule = &spec.ingress.as_ref().unwrap()[0];
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(22)));
    let from = rule.from.as_ref().unwrap();
    assert_eq!(from.len(), 1, "one peer: namespace AND pod, not namespace OR pod");
    let ns = from[0].namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(ns["kubernetes.io/metadata.name"], GATEWAY_NAMESPACE);
    assert_eq!(GATEWAY_NAMESPACE, "kloudlite-system", "deploy/k3s/gateway.yaml puts the gateway here; keep them equal");
    let pod = from[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
    assert_eq!(pod["app"], "kloudlite-gateway");
}


/// `allow-dns` reached every pod in kube-system on 53 — the agent's own DaemonSet included —
/// where only CoreDNS was ever meant. One peer, both selectors: the two-peer form would mean
/// "all of kube-system OR every k8s-app=kube-dns pod anywhere", which is wider than what it
/// replaces (see `attach_egress`'s comment on the same trap).
#[test]
pub(crate) fn allow_dns_reaches_coredns_only() {
    let p = default_policies("ws-alice", "alice", &owner_ref())
        .into_iter()
        .find(|p| p.metadata.name.as_deref() == Some("allow-dns"))
        .expect("allow-dns");
    let to = &p.spec.as_ref().unwrap().egress.as_ref().unwrap()[0].to.as_ref().unwrap();
    assert_eq!(to.len(), 1, "one peer, or the selectors are an OR");
    let peer = &to[0];
    assert_eq!(
        peer.namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap()["kubernetes.io/metadata.name"],
        "kube-system",
        "the namespace selector must survive alongside the pod selector, not be replaced by it"
    );
    assert_eq!(
        peer.pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap()["k8s-app"],
        "kube-dns"
    );
    // Both selectors in one peer are ANDed by Kubernetes: a pod in kube-system without
    // k8s-app=kube-dns (the agent, say) must not match. Two peers would OR them instead and
    // let exactly this pod through — the regression this test exists to catch.
    let ns_labels = std::collections::BTreeMap::from([("kubernetes.io/metadata.name".to_string(), "kube-system".to_string())]);
    let other_pod_labels = std::collections::BTreeMap::from([("app".to_string(), "kloudlite-agent".to_string())]);
    let matches = |sel: &k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector, labels: &std::collections::BTreeMap<String, String>| {
        sel.match_labels.as_ref().unwrap().iter().all(|(k, v)| labels.get(k) == Some(v))
    };
    assert!(matches(peer.namespace_selector.as_ref().unwrap(), &ns_labels), "kube-system namespace must match");
    assert!(!matches(peer.pod_selector.as_ref().unwrap(), &other_pod_labels), "a non-CoreDNS kube-system pod must not match");
}


#[test]
pub(crate) fn an_environment_namespace_denies_by_default_and_still_resolves_dns() {
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
pub(crate) fn internet_egress_never_reaches_the_metadata_service_or_the_cluster() {
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


/// ONE peer, always. `namespaceSelector` and `podSelector` in one element of `from`/`to` is an
/// AND; split across two elements it is an OR, and every sshd in the cluster becomes reachable
/// by any pod that labels itself correctly. The functions say so; this is what holds them to it.
#[test]
pub(crate) fn every_grant_ands_its_namespace_and_pod_selectors_in_one_peer() {
    let r = owner_ref();
    let cases: Vec<(&str, NetworkPolicy, &str)> = vec![
        ("attach_ingress", attach_ingress("env-1", "ws-alice", "ws-1", "alice", &r), "ingress"),
        ("allow_gateway_ingress", allow_gateway_ingress("ws-alice", "alice", &r), "ingress"),
        ("attach_egress", attach_egress("ws-alice", "ws-1", "env-1", "alice", &r), "egress"),
    ];
    for (name, pol, dir) in cases {
        let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
        let rules = spec[dir].as_array().unwrap_or_else(|| panic!("{name}: no {dir}"));
        assert_eq!(rules.len(), 1, "{name}: one rule");
        let peers = rules[0][if dir == "ingress" { "from" } else { "to" }].as_array().unwrap();
        assert_eq!(peers.len(), 1, "{name}: two peers is an OR, not an AND: {peers:?}");
        // And the selectors that must be there ARE there — a single peer with only a
        // namespaceSelector would pass the count above while opening the whole namespace.
        if name != "attach_egress" {
            assert!(peers[0].get("podSelector").is_some(), "{name}: no podSelector");
        } else {
            // Its peer has no podSelector (it targets a whole namespace), so the scoping to
            // this one workspace pod is the policy's own top-level podSelector instead.
            assert_eq!(spec["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
        }
        assert!(peers[0].get("namespaceSelector").is_some(), "{name}: no namespaceSelector");
    }
}


/// The gateway hole is port 22 and nothing else, from the gateway namespace and nothing else.
#[test]
pub(crate) fn the_gateway_hole_is_one_port_from_one_place() {
    let pol = allow_gateway_ingress("ws-alice", "alice", &owner_ref());
    let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
    let rule = &spec["ingress"][0];
    assert_eq!(rule["ports"], serde_json::json!([{"protocol": "TCP", "port": 22}]));
    assert_eq!(
        rule["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
        GATEWAY_NAMESPACE
    );
    assert_eq!(rule["from"][0]["podSelector"]["matchLabels"]["app"], "kloudlite-gateway");
}


/// `169.254.0.0/16` is the one that matters: on Azure `169.254.169.254` hands out the NODE's
/// managed identity to anything that asks, which is a full escape from the cluster. RFC 1918
/// covers pod, service and node networks without this code knowing their numbers.
#[test]
pub(crate) fn internet_egress_excludes_the_metadata_service_and_all_of_rfc_1918() {
    let pol = allow_internet_egress("ws-alice", "alice", &owner_ref());
    let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
    let block = &spec["egress"][0]["to"][0]["ipBlock"];
    assert_eq!(block["cidr"], "0.0.0.0/0");
    let except: Vec<String> =
        block["except"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    for want in ["169.254.0.0/16", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
        assert!(except.contains(&want.to_string()), "{want} is not excluded: {except:?}");
    }
    assert_eq!(spec["egress"].as_array().unwrap().len(), 1, "one rule; a second would union it open");
}
