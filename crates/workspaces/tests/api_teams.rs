//! Team-owned environments: authorization via a stub `MembershipCheck` (a real `Directory` is
//! mongo-backed and heavy to spin up for a unit test — see `ApiState::membership`'s doc), against
//! a mocked API server for the objects the handlers write.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState, MembershipCheck};
use rustic_git_workspaces::kube_test::{get, mock_client, post, Recorder, Route};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";
const NODE: &str = "node-a";

/// `karthik` is the only member of team `acme`.
struct StubMembership;

#[async_trait::async_trait]
impl MembershipCheck for StubMembership {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        if user == "karthik" { vec!["acme".into()] } else { vec![] }
    }
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

fn vol_obj(name: &str, owner: &str, node: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner, "rustic-git.io/kind": "environment"}},
        "spec": {"owner": owner, "nodeName": node, "region": "centralindia", "quotaGb": 20}
    })
}

fn env_obj(name: &str, owner: &str, node: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {
            "owner": owner, "name": name, "region": "centralindia", "services": [],
            "storage": {"quotaGb": 20}, "volumeRef": name, "nodeName": node, "desiredState": "running"
        }
    })
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn create_routes() -> Vec<Route> {
    vec![
        get(
            format!("{API}/ownerbindings/centralindia-acme"),
            json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                "metadata": {"name": "centralindia-acme"},
                "spec": {"owner": "acme", "region": "centralindia", "nodeName": NODE}
            }),
        ),
        post(format!("{API}/volumes"), vol_obj("env-new", "acme", NODE)),
        post(format!("{API}/environments"), env_obj("env-new", "acme", NODE)),
    ]
}

async fn server(with_membership: bool, routes: Vec<Route>) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new());
    if with_membership {
        state = state.with_membership(Arc::new(StubMembership));
    }
    let (client, rec) = mock_client(routes);
    state = state.with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

#[tokio::test]
async fn member_can_create_a_team_environment_on_the_teams_node() {
    let s = server(true, create_routes()).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "owner": "acme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["owner"], "acme");

    // Placement is the TEAM's binding, not the member's — a team's data lives on one node.
    assert!(s.rec.calls().contains(&format!("GET {API}/ownerbindings/centralindia-acme")));
    let vol = s.rec.sent("POST", &format!("{API}/volumes")).remove(0);
    assert_eq!(vol["spec"]["owner"], "acme");
    let e = s.rec.sent("POST", &format!("{API}/environments")).remove(0);
    assert_eq!(e["spec"]["owner"], "acme");
    assert_eq!(e["metadata"]["labels"]["rustic-git.io/owner"], "acme");
}

#[tokio::test]
async fn a_team_environment_is_listed_for_its_members() {
    let routes = vec![
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/environments"), list_of("Environment", vec![env_obj("env-1", "acme", NODE)])),
    ];
    let s = server(true, routes).await;
    let tok = token(&s.jwt, "karthik");

    let list: Vec<Value> = reqwest::Client::new()
        .get(format!("{}/v1/environments?owner=acme", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "env-1");
    assert_eq!(list[0]["state"], "creating", "no status yet means creating, never null");
}

#[tokio::test]
async fn non_member_cannot_create_or_see_a_team_environment() {
    let mut routes = create_routes();
    routes.push(get(format!("{API}/environments/env-1"), env_obj("env-1", "acme", NODE)));
    let s = server(true, routes).await;
    let client = reqwest::Client::new();
    let stranger = token(&s.jwt, "mallory");

    // A non-member can't create it in the team's name.
    let resp = client
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&stranger)
        .json(&json!({"name": "app-dev", "region": "centralindia", "owner": "acme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(s.rec.calls().is_empty(), "a refused create writes nothing");

    // And an existing team environment is a 404 to them, never a 403 — they learn nothing.
    let resp = client.get(format!("{}/v1/environments/env-1", s.base)).bearer_auth(&stranger).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    let resp = client
        .get(format!("{}/v1/environments?owner=acme", s.base))
        .bearer_auth(&stranger)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn team_owner_without_a_directory_configured_is_503() {
    let s = server(false, create_routes()).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "owner": "acme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}

/// `clone_env`'s member-authorized read (`find_env`) plus the objects it writes — the copy keeps
/// the team's ownership and lands on the source's node.
#[tokio::test]
async fn member_can_clone_a_team_environment() {
    let routes = vec![
        get(format!("{API}/environments/env-1"), env_obj("env-1", "acme", "node-z")),
        get(format!("{API}/volumes/env-1"), vol_obj("env-1", "acme", "node-z")),
        post(format!("{API}/volumes"), vol_obj("env-new", "acme", "node-z")),
        post(format!("{API}/environments"), env_obj("env-new", "acme", "node-z")),
    ];
    let s = server(true, routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev-clone"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["owner"], "acme");

    let vol = s.rec.sent("POST", &format!("{API}/volumes")).remove(0);
    assert_eq!(vol["spec"]["owner"], "acme");
    assert_eq!(vol["spec"]["nodeName"], "node-z");
    assert_eq!(vol["spec"]["source"]["cloneOf"]["volume"], "env-1");
    assert!(!s.rec.calls().iter().any(|c| c.contains("ownerbindings")), "a clone never re-places");
}

#[tokio::test]
async fn personal_workspace_unaffected_by_membership() {
    let routes = vec![
        get(
            format!("{API}/ownerbindings/centralindia-karthik"),
            json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                "metadata": {"name": "centralindia-karthik"},
                "spec": {"owner": "karthik", "region": "centralindia", "nodeName": NODE}
            }),
        ),
        post(format!("{API}/volumes"), vol_obj("ws-new", "karthik", NODE)),
        post(
            format!("{API}/workspaces"),
            json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
                "metadata": {"name": "ws-new"},
                "spec": {
                    "owner": "karthik", "name": "web", "region": "centralindia", "image": "nginx:alpine",
                    "storage": {"quotaGb": 20}, "volumeRef": "ws-new", "nodeName": NODE, "desiredState": "running"
                }
            }),
        ),
    ];
    let s = server(true, routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["owner"], "karthik");
}
