//! `OwnerBinding`: one namespace per team in use, and the owner's ResourceQuota re-stamped on
//! every pass.

use super::*;

pub(crate) fn binding_status() -> String {
    format!("/apis/kloudlite.io/v1alpha1/ownerbindings/{}/status", crd::binding_name("r1", "alice"))
}

pub(crate) fn ws_in_team(team: &str, node: &str) -> serde_json::Value {
    let mut o = ws_json(serde_json::json!({"phase": "ready", "nodeName": node}));
    o["spec"]["team"] = serde_json::json!(team);
    o
}

/// No `Quota` object for this owner/team and none for its kind's default either: `quota::effective`
/// falls all the way through to the compiled-in table, which is what most binding tests want —
/// they are not testing quota sizing, and this is the fallback that exercises without needing one.
pub(crate) fn quota_fallback_routes(name: &str, team: bool) -> Vec<Route> {
    vec![
        kloudlite_workspaces::kube_test::not_found(format!("/apis/kloudlite.io/v1alpha1/quotas/{name}")),
        kloudlite_workspaces::kube_test::not_found(format!(
            "/apis/kloudlite.io/v1alpha1/quotas/{}",
            if team { "default-team" } else { "default-user" }
        )),
    ]
}

/// Every object the binding ensures in one namespace, answered with itself.
pub(crate) fn ns_routes(ns: &str) -> Vec<Route> {
    let ok = |path: String, api: &str, kind: &str| Route {
        method: "PATCH",
        path,
        status: 200,
        body: serde_json::json!({"apiVersion": api, "kind": kind, "metadata": {"name": "x"}}),
    };
    let mut r = vec![
        ok(format!("/api/v1/namespaces/{ns}"), "v1", "Namespace"),
        ok(format!("/api/v1/namespaces/{ns}/limitranges/slot"), "v1", "LimitRange"),
        ok(format!("/api/v1/namespaces/{ns}/resourcequotas/owner-quota"), "v1", "ResourceQuota"),
        ok(
            format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings/api-secrets"),
            "rbac.authorization.k8s.io/v1",
            "RoleBinding",
        ),
        // The agent's own per-namespace host-key grant, in place of `secrets` cluster-wide.
        ok(
            format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings/agent-secrets"),
            "rbac.authorization.k8s.io/v1",
            "RoleBinding",
        ),
    ];
    for p in ["default-deny", "allow-dns", "allow-same-namespace", "allow-internet-egress", "allow-gateway-ssh", "allow-builder-gate"] {
        r.push(ok(
            format!("/apis/networking.k8s.io/v1/namespaces/{ns}/networkpolicies/{p}"),
            "networking.k8s.io/v1",
            "NetworkPolicy",
        ));
    }
    r
}

pub(crate) fn binding_json() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
        "metadata": {"name": crd::binding_name("r1", "alice"), "uid": "ob-uid-1", "generation": 1},
        "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"}
    })
}

/// The per-owner shared objects have exactly ONE owner now. They used to be re-ensured by the
/// workspace reconciler and the environment reconciler on every pass, which is two writers for one
/// object and a namespace deleted by whichever ran last.
#[tokio::test]
async fn a_binding_ensures_one_namespace_per_team_in_use_and_reports_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        // A team workspace here, and one on ANOTHER node: the second must not make this node build
        // a namespace it does not host.
        "items": [
            ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"})),
            ws_in_team("acme", "node-a"),
            ws_in_team("elsewhere", "node-b"),
        ]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
        ]
        .into_iter()
        .chain(quota_fallback_routes("alice", false))
        .chain(quota_fallback_routes("acme", true))
        .chain(ns_routes("ws-alice"))
        .chain(ns_routes(&crd::ws_namespace("alice", "acme")))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    kloudlite_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == "PATCH /api/v1/namespaces/ws-alice"), "{:?}", rec.calls());
    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice");
    assert!(
        sent[0]["metadata"].get("ownerReferences").is_none(),
        "a namespace shared by every workspace this user owns must never be GC'd with one binding: {}", sent[0]
    );
    let limit = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/limitranges/slot");
    assert!(limit[0]["metadata"].get("ownerReferences").is_none(), "a quota ceiling must not vanish with a binding rewrite");
    // Everything else the binding vouches for IS owned by it, so a re-homed owner does not strand
    // a grant on the old node.
    let rb = rec.sent("PATCH", "/apis/rbac.authorization.k8s.io/v1/namespaces/ws-alice/rolebindings/api-secrets");
    assert_eq!(rb[0]["metadata"]["ownerReferences"][0]["kind"], "OwnerBinding", "{}", rb[0]);
    let acme = crd::ws_namespace("alice", "acme");
    assert!(rec.calls().iter().any(|c| *c == format!("PATCH /api/v1/namespaces/{acme}")), "{:?}", rec.calls());
    let stranded = crd::ws_namespace("alice", "elsewhere");
    assert!(
        !rec.calls().iter().any(|c| *c == format!("PATCH /api/v1/namespaces/{stranded}")),
        "a workspace on another node must not make namespaces here: {:?}", rec.calls()
    );
    let st = rec.sent("PATCH", &binding_status());
    assert_eq!(st.len(), 1);
    assert!(
        st[0]["status"]["conditions"].as_array().unwrap().iter()
            .any(|c| c["type"] == "NamespaceReady" && c["status"] == "True"),
        "{}", st[0]
    );
}

/// The hot loop this design has to not have: `crd::condition` stamps `lastTransitionTime` with
/// `now`, so a status write on every pass is new bytes, which fires this controller's own watch,
/// which writes again — forever, on an object nothing asked to change.
#[tokio::test]
async fn a_second_reconcile_of_a_ready_binding_writes_no_status() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list)]
            .into_iter()
                .chain(quota_fallback_routes("alice", false))
                .chain(ns_routes("ws-alice"))
            .collect(),
    );
    // What the FIRST reconcile left behind, with an older `lastTransitionTime` than `now`.
    let mut b = binding_json();
    b["status"] = serde_json::json!({
        "observedGeneration": 1,
        "conditions": [{"type": "NamespaceReady", "status": "True", "reason": "Converged",
                        "message": "namespaces exist on this node", "observedGeneration": 1,
                        "lastTransitionTime": "2020-01-01T00:00:00Z"}],
    });
    let b: crd::OwnerBinding = serde_json::from_value(b).unwrap();

    kloudlite_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    assert!(
        rec.sent("PATCH", &binding_status()).is_empty(),
        "a status re-stamped with `now` is not a change: {:?}", rec.calls()
    );
}

/// The owner's ceiling is projected into their namespace on every binding pass, so a raise takes
/// effect without a roll and a namespace made before quotas existed gets one on its next reconcile.
#[tokio::test]
async fn a_binding_pass_writes_the_owners_resource_quota() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let quota = kloudlite_workspaces::kube_test::get(
        "/apis/kloudlite.io/v1alpha1/quotas/alice",
        serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "alice"},
            "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 12, "memoryGb": 48}
        }),
    );
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            quota,
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    kloudlite_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/resourcequotas/owner-quota");
    assert!(!sent.is_empty(), "{:?}", rec.calls());
    assert_eq!(sent[0]["spec"]["hard"]["limits.cpu"], "12");
    assert_eq!(sent[0]["spec"]["hard"]["limits.memory"], "48Gi");
}

/// `run.rs` wakes every `OwnerBinding` on any `Quota` write (`all_in_store`, same pattern as the
/// Node watch) rather than trying to map a `Quota`'s name back to the hashed binding name it does
/// not appear in — this is the reconcile that wake reaches: a raised `Quota` re-read on the very
/// next binding pass, with no roll and no wait for an unrelated event to happen to touch it.
#[tokio::test]
async fn a_quota_change_re_stamps_the_resource_quota_on_the_next_binding_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let raised_quota = kloudlite_workspaces::kube_test::get(
        "/apis/kloudlite.io/v1alpha1/quotas/alice",
        serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "alice"},
            "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 16, "memoryGb": 64}
        }),
    );
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_workspaces::kube_test::get("/apis/kloudlite.io/v1alpha1/workspaces", ws_list),
            raised_quota,
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    // The event this test proves the reconcile side of: `run.rs`'s Quota watch requeues this same
    // binding, and the requeued pass is exactly another `apply_binding` call — nothing about the
    // binding itself changed, only the `Quota` object the mock now answers with the raised numbers.
    kloudlite_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/resourcequotas/owner-quota");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["spec"]["hard"]["limits.cpu"], "16", "the raised number, not the old one: {sent:?}");
    assert_eq!(sent[0]["spec"]["hard"]["limits.memory"], "64Gi");
}

pub(crate) fn home_vol_json(quota: u64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "home-alice", "uid": "home-uid-1", "generation": 1,
                     "ownerReferences": [{"apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerBinding",
                                          "name": crd::binding_name("r1", "alice"), "uid": "ob-uid-1",
                                          "controller": true, "blockOwnerDeletion": true}]},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": quota},
        "status": {"phase": "ready", "subvolumePresent": true},
    })
}
