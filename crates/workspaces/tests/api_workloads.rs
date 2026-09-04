//! `GET /admin/workloads` and `POST /admin/workloads/{scope}/{name}/roll` against a mocked API
//! server — the recorder is what proves "409, nothing written" is enforced by `roll_readers`
//! itself (zero patch calls on a conflict), not merely documented.

use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_workspaces::api::{admin::router, ApiState};
use kloudlite_git_workspaces::kube_test::{get, mock_client, patch, Recorder, Route};
use serde_json::json;
use std::sync::Arc;

const APPS: &str = "/apis/apps/v1";

fn deployment(name: &str, ready: i32, desired: i32) -> serde_json::Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name, "namespace": "kloudlite-git"},
        "spec": {"replicas": desired, "template": {"metadata": {"annotations": {}}, "spec": {"containers": [{"name": "c", "image": "ghcr.io/x:1"}]}}},
        "status": {"readyReplicas": ready},
    })
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

async fn admin_server(routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(jwt.clone());
    let (client, rec) = mock_client(routes);
    state = state.with_aks(client.clone());
    state = state.with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn admin_token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

/// Ready == desired: the roll patches the annotation and answers 200.
#[tokio::test]
async fn roll_patches_when_settled() {
    let s = admin_server(vec![
        get(format!("{APPS}/namespaces/kloudlite-git/deployments/kloudlite-git-api"), deployment("kloudlite-git-api", 2, 2)),
        patch(format!("{APPS}/namespaces/kloudlite-git/deployments/kloudlite-git-api"), deployment("kloudlite-git-api", 2, 2)),
    ])
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/admin/workloads/central/kloudlite-git-api/roll", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"reason": "rotate secret"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);

    let sent = s.rec.sent("PATCH", &format!("{APPS}/namespaces/kloudlite-git/deployments/kloudlite-git-api"));
    assert_eq!(sent.len(), 1);
    let ann = &sent[0]["spec"]["template"]["metadata"]["annotations"];
    assert!(ann["kloudlite-git.io/restarted-at"].is_string());
    assert_eq!(ann["kloudlite-git.io/roll-reason"], "rotate secret");
}

/// Ready < desired: 409, and the recorder shows the patch was never sent — the atomic half of
/// the "409, nothing written" promise.
#[tokio::test]
async fn roll_conflicts_mid_rollout() {
    let s = admin_server(vec![get(
        format!("{APPS}/namespaces/kloudlite-git/deployments/kloudlite-git-api"),
        deployment("kloudlite-git-api", 1, 2),
    )])
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/admin/workloads/central/kloudlite-git-api/roll", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"reason": "rotate secret"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ready"], 1);
    assert_eq!(body["desired"], 2);
    assert!(s.rec.sent("PATCH", &format!("{APPS}/namespaces/kloudlite-git/deployments/kloudlite-git-api")).is_empty());
}

/// A name outside `KNOWN` for the scope is a 404, never a passthrough to the cluster.
#[tokio::test]
async fn roll_unknown_name_is_404() {
    let s = admin_server(vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/workloads/central/not-a-real-workload/roll", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"reason": "x"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 404);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("PATCH")));
}

/// A region scope that names no `crd::Region` at all is a 404 too, before `client_for` ever
/// resolves to `kube(s)` — this is the review finding on Task 5: with only one `kube::Client`
/// wired, a bogus region used to fall through to the real cluster's client unchecked.
#[tokio::test]
async fn roll_unknown_region_is_404() {
    let s = admin_server(vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/workloads/no-such-region/kloudlite-git-agent/roll", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"reason": "x"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 404);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("PATCH")));
}

/// An empty reason is refused before anything is read — the one validation the manual route
/// adds beyond what a settings-triggered roll needs.
#[tokio::test]
async fn roll_empty_reason_is_400() {
    let s = admin_server(vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/workloads/central/kloudlite-git-api/roll", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"reason": "   "}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 400);
    assert!(s.rec.calls().is_empty(), "no read before the reason is validated: {:?}", s.rec.calls());
}

/// `GET /admin/workloads`' central half: one row per `KNOWN_CENTRAL` name, image/ready/desired
/// read off the fetched object, `last_roll` absent until a roll has happened.
#[tokio::test]
async fn list_workloads_shape() {
    let names = ["kloudlite-git-srv", "kloudlite-git-api", "kloudlite-git-worker", "kloudlite-git-web", "kloudlite-git-admin"];
    let mut routes = vec![get(
        "/apis/kloudlite-git.io/v1alpha1/regions",
        json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "RegionList", "metadata": {}, "items": []}),
    )];
    routes.push(get(
        format!("{APPS}/namespaces/kloudlite-git/statefulsets/kloudlite-git-srv"),
        json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": {"name": "kloudlite-git-srv", "namespace": "kloudlite-git"},
            "spec": {"replicas": 3, "template": {"metadata": {"annotations": {}}, "spec": {"containers": [{"name": "c", "image": "ghcr.io/x:2"}]}}},
            "status": {"readyReplicas": 3},
        }),
    ));
    for name in &names[1..] {
        routes.push(get(format!("{APPS}/namespaces/kloudlite-git/deployments/{name}"), deployment(name, 1, 1)));
    }
    let s = admin_server(routes).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/admin/workloads", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let rows: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(rows.len(), names.len());
    let srv = rows.iter().find(|r| r["name"] == "kloudlite-git-srv").unwrap();
    assert_eq!(srv["ready"], 3);
    assert_eq!(srv["desired"], 3);
    assert_eq!(srv["rolloutState"], "Stable");
    assert!(srv["lastRoll"].is_null());
    // `Scope` serializes as a plain string, not serde's default externally-tagged shape.
    assert_eq!(srv["scope"], "central");
}

/// `Scope::Region` serializes as the bare region id — `"centralindia"`, never
/// `{"Region":"centralindia"}` or `"region/centralindia"` (that's `Display`, used only in logs).
#[tokio::test]
async fn list_workloads_region_scope_is_plain_string() {
    let mut routes = vec![get(
        "/apis/kloudlite-git.io/v1alpha1/regions",
        json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "RegionList", "metadata": {}, "items": [
            {"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Region", "metadata": {"name": "centralindia"}, "spec": {"name": "centralindia", "status": "active"}},
        ]}),
    )];
    // `admin_server` wires both `aks` and `kube` to the same mock client, so `list_workloads`'s
    // central half still runs and needs its own routes even though this test's assertions are
    // about the region half.
    routes.push(get(
        format!("{APPS}/namespaces/kloudlite-git/statefulsets/kloudlite-git-srv"),
        json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": {"name": "kloudlite-git-srv", "namespace": "kloudlite-git"},
            "spec": {"replicas": 1, "template": {"metadata": {"annotations": {}}, "spec": {"containers": [{"name": "c", "image": "ghcr.io/x:1"}]}}},
            "status": {"readyReplicas": 1},
        }),
    ));
    for name in ["kloudlite-git-api", "kloudlite-git-worker", "kloudlite-git-web", "kloudlite-git-admin"] {
        routes.push(get(format!("{APPS}/namespaces/kloudlite-git/deployments/{name}"), deployment(name, 1, 1)));
    }
    routes.extend([
        get(
            "/apis/apps/v1/namespaces/kube-system/daemonsets/kloudlite-git-agent",
            json!({
                "apiVersion": "apps/v1", "kind": "DaemonSet",
                "metadata": {"name": "kloudlite-git-agent", "namespace": "kube-system"},
                "spec": {"template": {"metadata": {"annotations": {}}, "spec": {"containers": [{"name": "c", "image": "ghcr.io/x:1"}]}}},
                "status": {"numberReady": 1, "desiredNumberScheduled": 1},
            }),
        ),
        get(
            format!("{APPS}/namespaces/kloudlite-git-system/deployments/kloudlite-git-gateway"),
            deployment("kloudlite-git-gateway", 1, 1),
        ),
    ]);
    let s = admin_server(routes).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/admin/workloads", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let rows: Vec<serde_json::Value> = resp.json().await.unwrap();
    let agent = rows.iter().find(|r| r["name"] == "kloudlite-git-agent").unwrap();
    assert_eq!(agent["scope"], "centralindia");
}
