//! A service that left `spec.services` leaves the cluster with it.
//!
//! `PATCH /v1/environments/{id}` can shorten the list; nothing else removes what an earlier spec
//! rendered, because the ownerReference collects a service's objects only when the whole
//! Environment goes.

use super::*;


fn sts(name: &str, ours: bool) -> serde_json::Value {
    let labels = if ours {
        serde_json::json!({"kloudlite.io/owner": "acme", "kloudlite.io/kind": "environment"})
    } else {
        serde_json::json!({})
    };
    serde_json::json!({
        "apiVersion": "apps/v1", "kind": "StatefulSet",
        "metadata": {"name": name, "namespace": "env-1", "labels": labels},
        "status": {"readyReplicas": 1},
    })
}

/// `web` is in spec and stays; `cache` left it and goes — the ClusterIP first, so a failure between
/// the two leaves the StatefulSet for the next pass to find. A StatefulSet that is not this
/// controller's rendering of a service (no labels of ours) is never touched.
#[tokio::test]
async fn a_service_that_left_the_spec_takes_its_statefulset_and_clusterip_with_it() {
    let tmp = env_tmp();
    let listing = sts_list(vec![sts("web", true), sts("cache", true), sts("someone-elses", false)]);
    // The shared fixture lists `web` alone; this test's whole subject is what ELSE the namespace
    // holds, so its listing replaces that one rather than queueing behind it.
    let mut routes: Vec<Route> = intercept_routes(vec![])
        .into_iter()
        .filter(|r| !(r.method == "GET" && r.path == "/apis/apps/v1/namespaces/env-1/statefulsets"))
        .collect();
    routes.extend([
        kloudlite_workspaces::kube_test::get("/apis/apps/v1/namespaces/env-1/statefulsets", listing),
        Route { method: "DELETE", path: "/apis/apps/v1/namespaces/env-1/statefulsets/cache".into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
        Route { method: "DELETE", path: "/api/v1/namespaces/env-1/services/cache".into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let (ctx, rec) = intercept_ctx(tmp.path(), routes);
    // As the pass that rendered `cache` left the cache: without the forgetting below, the next
    // definition that brings the name back renders nothing, because this process believes it
    // applied them already.
    for key in ["StatefulSet/env-1/cache", "Service/env-1/cache"] {
        ctx.applied.lock().unwrap().insert(key.to_string(), (0, std::time::Instant::now()));
    }

    kloudlite_agent::controller::apply_environment(&intercept_env(serde_json::json!([]), None), &ctx).await.unwrap();

    let calls = rec.calls();
    let svc = calls.iter().position(|c| c == "DELETE /api/v1/namespaces/env-1/services/cache").unwrap_or_else(|| panic!("the ClusterIP survives: {calls:?}"));
    let set = calls.iter().position(|c| c == "DELETE /apis/apps/v1/namespaces/env-1/statefulsets/cache").unwrap_or_else(|| panic!("the StatefulSet survives: {calls:?}"));
    assert!(svc < set, "the ClusterIP goes first: {calls:?}");
    for kept in ["web", "someone-elses"] {
        assert!(
            !calls.iter().any(|c| c.starts_with(&format!("DELETE /apis/apps/v1/namespaces/env-1/statefulsets/{kept}"))),
            "{kept} was pruned: {calls:?}"
        );
    }
    let applied = ctx.applied.lock().unwrap();
    assert!(!applied.keys().any(|k| k.ends_with("/cache")), "deleted without forget_applied: {:?}", applied.keys().collect::<Vec<_>>());
}
