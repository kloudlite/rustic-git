//! `GET /admin/overview` — one round trip composing pending requests, attention items, recent
//! audit and fleet numbers, same harness shape `api_admin_owners.rs`/`api_admin_clusters.rs` use.

use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_workspaces::api::{admin::router, ApiState};
use kloudlite_git_workspaces::kube_test::{get, mock_client, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite-git.io/v1alpha1";

async fn keys_store() -> Arc<kloudlite_git_storage::store::Store> {
    let tmp = tempfile::tempdir().unwrap();
    Arc::new(
        kloudlite_git_storage::store::Store::open(Arc::new(object_store::memory::InMemory::new()), tmp.path().join("cache"), false)
            .await
            .unwrap(),
    )
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
}

async fn admin_server(routes: Vec<Route>, keys: Option<Arc<kloudlite_git_storage::store::Store>>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(jwt.clone());
    let (client, _rec) = mock_client(routes);
    state = state.with_kube(client);
    if let Some(k) = keys {
        state = state.with_keys(k);
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt }
}

fn admin_token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn region_obj(name: &str) -> Value {
    json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Region",
           "metadata": {"name": name}, "spec": {"name": name, "status": "active"}})
}

fn node_obj(name: &str, ready: bool) -> Value {
    let status = if ready { "True" } else { "False" };
    json!({"apiVersion": "v1", "kind": "Node",
           "metadata": {"name": name, "labels": {}, "annotations": {}},
           "status": {"conditions": [{"type": "Ready", "status": status,
                                      "lastHeartbeatTime": "2026-09-04T00:00:00Z",
                                      "lastTransitionTime": "2026-09-04T00:00:00Z"}]}})
}

fn ws_obj(name: &str, owner: &str, region: &str) -> Value {
    json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace",
           "metadata": {"name": name, "labels": {"kloudlite-git.io/owner": owner}},
           "spec": {"owner": owner, "team": "", "name": name, "region": region, "image": "img:1",
                    "desiredState": "running", "packages": [],
                    "resources": {"cpuRequest": "1", "cpuLimit": "2", "memoryRequest": "1Gi", "memoryLimit": "2Gi"},
                    "storage": {"quotaGb": 20}}})
}

fn req_obj(name: &str, owner: &str, created: &str) -> Value {
    json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "QuotaRequest",
           "metadata": {"name": name, "labels": {"kloudlite-git.io/owner": owner}, "creationTimestamp": created},
           "spec": {"owner": owner, "requested": {"workspaces": 10}, "reason": "more room"}})
}

fn daemonset(name: &str, ready: i32, desired: i32) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "DaemonSet",
           "metadata": {"name": name, "namespace": "kube-system"},
           "spec": {"template": {"metadata": {"annotations": {}}, "spec": {"containers": [{"name": "c", "image": "x:1"}]}}},
           "status": {"numberReady": ready, "desiredNumberScheduled": desired}})
}

fn deployment(name: &str, ns: &str, ready: i32, desired: i32) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "Deployment",
           "metadata": {"name": name, "namespace": ns},
           "spec": {"replicas": desired, "template": {"metadata": {"annotations": {}}, "spec": {"containers": [{"name": "c", "image": "x:1"}]}}},
           "status": {"readyReplicas": ready}})
}

/// `kloudlite-git-agent`/`kloudlite-git-gateway` for one region — every per-region workload
/// `list_workloads` walks, so both must be mocked or the whole list (not just this region's row)
/// comes back empty.
fn workload_routes(agent_ready: i32, agent_desired: i32, gateway_ready: i32, gateway_desired: i32) -> Vec<Route> {
    vec![
        get("/apis/apps/v1/namespaces/kube-system/daemonsets/kloudlite-git-agent", daemonset("kloudlite-git-agent", agent_ready, agent_desired)),
        get(
            "/apis/apps/v1/namespaces/kloudlite-git-system/deployments/kloudlite-git-gateway",
            deployment("kloudlite-git-gateway", "kloudlite-git-system", gateway_ready, gateway_desired),
        ),
    ]
}

fn base_routes(regions: Vec<Value>, nodes: Vec<Value>, ws: Vec<Value>, reqs: Vec<Value>) -> Vec<Route> {
    let mut routes = vec![get(format!("{API}/regions"), list_of("Region", regions.clone()))];
    // `client_for_region` re-reads each region by name before trusting it — one mock per region.
    for r in &regions {
        let name = r["metadata"]["name"].as_str().unwrap();
        routes.push(get(format!("{API}/regions/{name}"), r.clone()));
    }
    routes.extend(vec![
        get("/api/v1/nodes".to_string(), list_of("Node", nodes)),
        get(format!("{API}/quotarequests"), list_of("QuotaRequest", reqs)),
        get(format!("{API}/quotas"), list_of("Quota", vec![])),
        get(format!("{API}/workspaces"), list_of("Workspace", ws)),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
    ]);
    routes
}

/// One call composes every card the landing page needs — the spec's "one round trip for the
/// landing page".
#[tokio::test]
async fn overview_composes_pending_attention_audit_and_fleet() {
    let regions = vec![region_obj("r1"), region_obj("r2")];
    let nodes = vec![node_obj("n1", true), node_obj("n2", false)];
    let ws = vec![ws_obj("w1", "ann", "r1"), ws_obj("w2", "bob", "r2")];
    let reqs = vec![req_obj("qr1", "ann", "2026-09-01T00:00:00Z")];
    let routes = base_routes(regions, nodes, ws, reqs);

    let keys = keys_store().await;
    crate::audit_row(&keys, "root", "set-quota", "ann").await;
    let s = admin_server(routes, Some(keys)).await;

    let text = reqwest::Client::new()
        .get(format!("{}/admin/overview", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("not json: {text}"));

    // Pending: the one open request, oldest (only) first.
    let pending = body["pendingRequests"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["owner"], "ann");

    // Attention: the NotReady node is in there.
    let attention = body["attention"].as_array().unwrap();
    assert!(attention.iter().any(|a| a["kind"] == "node" && a["detail"].as_str().unwrap().contains("n2")));

    // Recent audit: the one row written above.
    let audit = body["recentAudit"].as_array().unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["action"], "set-quota");

    // Fleet: two workspaces, one per region, hand-counted against the two `ws_obj` above.
    assert_eq!(body["fleet"]["workspaces"], 2);
    assert_eq!(body["fleet"]["owners"], 2);
    assert_eq!(body["fleet"]["perRegion"]["r1"]["workspaces"], 1);
    assert_eq!(body["fleet"]["perRegion"]["r2"]["workspaces"], 1);
}

/// Every attention kind this handler can produce through a live route (firing signals need a
/// real metrics scrape target this harness has no seam to mock — covered instead by
/// `overview::tests::only_firing_signals_become_attention_items`, a unit test on the pure mapping).
#[tokio::test]
async fn attention_covers_workload_node_region_and_settings_kinds() {
    let regions = vec![region_obj("r1")];
    let nodes = vec![node_obj("n1", false)];
    let ws = vec![ws_obj("w1", "ann", "r1")];
    let mut routes = base_routes(regions, nodes, ws, vec![]);
    // Under-ready agent: fires BOTH `workload` (ready < desired) and `region` (agents_ready == 0)
    // — the same shared object backs `list_workloads`'s row and `agent_counts`'s own read of it.
    routes.extend(workload_routes(0, 1, 1, 1));
    let s = admin_server(routes, None).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{}/admin/overview", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let attention = body["attention"].as_array().unwrap();
    let kind = |k: &str| attention.iter().any(|a| a["kind"] == k);
    assert!(kind("workload"), "{attention:?}");
    assert!(kind("node"), "{attention:?}");
    assert!(kind("region"), "{attention:?}");
    // No `ClusterSettings/default` mocked at all: `settings_status` reads back "absent".
    assert!(kind("settings"), "{attention:?}");
}

/// A sub-source that cannot be read (here: the whole kube API, so pending requests, nodes, the
/// fleet listing and cluster rows all fail) degrades every one of them into `errors` — the page
/// still 200s with its other sections (there are none left to render here, but nothing 5xxs).
#[tokio::test]
async fn a_kube_outage_degrades_every_sub_source_instead_of_5xxing() {
    // No routes at all: every list/get the handler makes 404s, which `kube_err` turns into a
    // real `Response` error each fallible section below must swallow rather than propagate.
    let s = admin_server(vec![], None).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/admin/overview", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["pendingRequests"].as_array().unwrap().len(), 0);
    assert_eq!(body["attention"].as_array().unwrap().len(), 0);
    assert_eq!(body["fleet"]["owners"], 0);
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.iter().any(|e| e.as_str().unwrap().contains("fleet")), "{errors:?}");
}

/// Nothing pending and nothing firing is the documented empty state's data shape — an empty
/// `pendingRequests` and no node/region attention items, fleet numbers still populated.
#[tokio::test]
async fn overview_with_nothing_pending_still_returns_fleet_numbers() {
    let regions = vec![region_obj("r1")];
    let nodes = vec![node_obj("n1", true)];
    let ws = vec![ws_obj("w1", "ann", "r1")];
    let routes = base_routes(regions, nodes, ws, vec![]);
    let s = admin_server(routes, None).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{}/admin/overview", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["pendingRequests"].as_array().unwrap().len(), 0);
    // The one node is Ready and nothing is draining, so nothing NODE-shaped shows up (the region
    // itself still flags zero-agents — no `kloudlite-git-agent` workload is mocked here).
    let attention = body["attention"].as_array().unwrap();
    assert!(!attention.iter().any(|a| a["kind"] == "node"));
    assert_eq!(body["fleet"]["workspaces"], 1);
    assert_eq!(body["fleet"]["owners"], 1);
    // No object store configured: the audit section degrades rather than 5xxing the page.
    assert_eq!(body["recentAudit"].as_array().unwrap().len(), 0);
    assert!(body["errors"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains("audit")));
}

async fn audit_row(keys: &Arc<kloudlite_git_storage::store::Store>, actor: &str, action: &str, target: &str) {
    let entry = kloudlite_git_workspaces::audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        actor: actor.to_string(),
        action: action.to_string(),
        target: target.to_string(),
        reason: None,
        result: "ok".into(),
    };
    kloudlite_git_workspaces::audit::record(&keys.os, &entry).await.unwrap();
}
