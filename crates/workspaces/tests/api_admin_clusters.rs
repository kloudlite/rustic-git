//! `GET /admin/clusters`, `GET /admin/clusters/{region}` and the three node verbs — against a
//! mocked kube API, same harness shape `api_admin_owners.rs` uses.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{admin::router, ApiState};
use rustic_git_workspaces::kube_test::{get, mock_client, patch, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";
const NODES: &str = "/api/v1/nodes";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

async fn admin_server(routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(jwt.clone()).with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn region_obj() -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Region",
           "metadata": {"name": "r1"}, "spec": {"name": "Region one", "status": "active"}})
}

fn node_obj(name: &str, annotations: Value) -> Value {
    json!({"apiVersion": "v1", "kind": "Node",
           "metadata": {"name": name, "labels": {}, "annotations": annotations},
           "status": {"conditions": [{"type": "Ready", "status": "True",
                                      "lastHeartbeatTime": "2026-09-04T00:00:00Z",
                                      "lastTransitionTime": "2026-09-04T00:00:00Z"}]}})
}

fn ws_obj() -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
           "metadata": {"name": "w1"},
           "spec": {"owner": "ann", "team": "", "name": "w1", "region": "r1", "image": "img:1",
                    "desiredState": "running", "packages": [],
                    "resources": {"cpuRequest": "1", "cpuLimit": "2", "memoryRequest": "1Gi", "memoryLimit": "2Gi"},
                    "storage": {"quotaGb": 20}},
           "status": {"phase": "ready", "nodeName": "n1", "podRef": "pod-w1", "volumeRef": "v1"}})
}

fn agent_ds() -> Value {
    json!({"apiVersion": "apps/v1", "kind": "DaemonSet",
           "metadata": {"name": "rustic-git-agent", "namespace": "kube-system"},
           "spec": {"selector": {}, "template": {"metadata": {}, "spec": {"containers": [{"name": "agent", "image": "agent:1"}]}}},
           "status": {"numberReady": 1, "desiredNumberScheduled": 1, "currentNumberScheduled": 1,
                      "numberMisscheduled": 0, "numberUnavailable": 0}})
}

/// The list composes agent readiness, node counts, hosted counts and settings status per region —
/// one row answers "is this region healthy".
#[tokio::test]
async fn clusters_list_composes_agents_nodes_and_hosted_counts() {
    let s = admin_server(vec![
        get(format!("{API}/regions"), list_of("Region", vec![region_obj()])),
        get(format!("{API}/regions/r1"), region_obj()),
        get(NODES, json!({"apiVersion": "v1", "kind": "NodeList", "metadata": {}, "items": [node_obj("n1", json!({}))]})),
        get(format!("{API}/workspaces"), list_of("Workspace", vec![ws_obj()])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get("/apis/apps/v1/namespaces/kube-system/daemonsets/rustic-git-agent", agent_ds()),
        get(format!("{API}/clustersettings/default"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "ClusterSettings", "metadata": {"name": "default"}, "spec": {}})),
    ])
    .await;
    let body = reqwest::Client::new()
        .get(format!("{}/admin/clusters", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap();
    let txt = body.text().await.unwrap();
    let body: Value = serde_json::from_str(&txt).unwrap_or_else(|_| panic!("{txt} calls={:?}", s.rec.calls()));
    let row = &body[0];
    assert_eq!(row["region"], "r1", "{body}");
    assert_eq!(row["status"], "active", "{body}");
    assert_eq!(row["agentsReady"], 1, "{body}");
    assert_eq!(row["agentsDesired"], 1, "{body}");
    assert_eq!(row["nodesReady"], 1, "{body}");
    assert_eq!(row["nodesTotal"], 1, "{body}");
    assert_eq!(row["draining"], 0, "{body}");
    assert_eq!(row["workingCopies"], 1, "{body}");
    assert_eq!(row["settingsStatus"], "present", "{body}");
}

fn node_routes(annotations: Value) -> Vec<Route> {
    vec![
        get(format!("{API}/regions/r1"), region_obj()),
        get(format!("{NODES}/n1"), node_obj("n1", annotations)),
        patch(format!("{NODES}/n1"), node_obj("n1", json!({}))),
    ]
}

async fn post(s: &Server, path: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{path}", s.base))
        .bearer_auth(token(&s.jwt))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Drain sets the label; undrain clears the label AND the status annotation, so a subsequent drain
/// starts fresh rather than showing a stale `drained` stamp from before.
#[tokio::test]
async fn drain_sets_the_label_and_undrain_clears_label_and_status() {
    let s = admin_server(node_routes(json!({}))).await;
    let r = post(&s, "/admin/clusters/r1/nodes/n1/drain", json!({"reason": "retiring the VM"})).await;
    assert_eq!(r.status(), 200);
    let sent = s.rec.sent("PATCH", &format!("{NODES}/n1"));
    assert_eq!(sent[0]["metadata"]["labels"]["rustic-git.io/decommission"], "true", "{:?}", sent);

    let r = post(&s, "/admin/clusters/r1/nodes/n1/undrain", json!({"reason": "changed my mind"})).await;
    assert_eq!(r.status(), 200);
    let sent = s.rec.sent("PATCH", &format!("{NODES}/n1"));
    let undrain = &sent[1];
    assert!(undrain["metadata"]["labels"]["rustic-git.io/decommission"].is_null(), "{undrain}");
    assert!(undrain["metadata"]["annotations"]["rustic-git.io/decommission-status"].is_null(), "{undrain}");
}

/// Decommission refuses a node that has not reached `drained` yet.
#[tokio::test]
async fn decommission_refuses_before_drained() {
    let s = admin_server(node_routes(json!({"rustic-git.io/decommission-status": "draining running=1 owned=2 copies=0 thin=0"}))).await;
    let r = post(&s, "/admin/clusters/r1/nodes/n1/decommission", json!({"reason": "vm going away"})).await;
    assert_eq!(r.status(), 409);
    assert!(s.rec.sent("PATCH", &format!("{NODES}/n1")).is_empty(), "nothing may be written before drained");
}

/// Decommission on a drained node cordons it and never deletes anything.
#[tokio::test]
async fn decommission_cordons_a_drained_node_and_deletes_nothing() {
    let s = admin_server(node_routes(json!({"rustic-git.io/decommission-status": "drained 2026-09-04T00:00:00Z"}))).await;
    let r = post(&s, "/admin/clusters/r1/nodes/n1/decommission", json!({"reason": "vm going away"})).await;
    assert_eq!(r.status(), 200);
    let sent = s.rec.sent("PATCH", &format!("{NODES}/n1"));
    assert_eq!(sent[0]["spec"]["unschedulable"], true, "{:?}", sent);
    assert!(!s.rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", s.rec.calls());
}

/// A missing or empty reason on drain is a 422 — reason is required on every write except approve.
#[tokio::test]
async fn drain_without_a_reason_is_422() {
    let s = admin_server(node_routes(json!({}))).await;
    for body in [json!({}), json!({"reason": "   "})] {
        let r = post(&s, "/admin/clusters/r1/nodes/n1/drain", body).await;
        assert_eq!(r.status(), 422);
    }
    assert!(s.rec.sent("PATCH", &format!("{NODES}/n1")).is_empty());
}

/// The detail attributes a live worktree to the node its VOLUME sits on, and counts the replica
/// rows that node holds — the two numbers that say what a drain of it is waiting for.
#[tokio::test]
async fn cluster_detail_counts_hosted_copies_per_node() {
    let vol = json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume", "metadata": {"name": "v1"},
                     "spec": {"owner": "ann", "team": "", "nodeName": "n1", "region": "r1", "quotaGb": 20, "replicas": 2}});
    let rep = json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica", "metadata": {"name": "v1-n1"},
                     "spec": {"volume": "v1", "node": "n1"}, "status": {"phase": "Synced"}});
    let s = admin_server(vec![
        get(format!("{API}/regions/r1"), region_obj()),
        get(NODES, json!({"apiVersion": "v1", "kind": "NodeList", "metadata": {}, "items": [node_obj("n1", json!({}))]})),
        get(format!("{API}/workspaces"), list_of("Workspace", vec![ws_obj()])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![vol])),
        get(format!("{API}/volumereplicas"), list_of("VolumeReplica", vec![rep])),
        get("/apis/apps/v1/namespaces/kube-system/daemonsets/rustic-git-agent", agent_ds()),
        get(format!("{API}/clustersettings/default"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "ClusterSettings", "metadata": {"name": "default"}, "spec": {}})),
    ])
    .await;
    let txt = reqwest::Client::new()
        .get(format!("{}/admin/clusters/r1", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&txt).unwrap_or_else(|_| panic!("{txt} calls={:?}", s.rec.calls()));
    assert_eq!(body["nodes"][0]["name"], "n1", "{body}");
    assert_eq!(body["nodes"][0]["workingCopies"], 1, "{body}");
    assert_eq!(body["nodes"][0]["replicasHeld"], 1, "{body}");
}
