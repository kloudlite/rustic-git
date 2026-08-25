//! Team-owned environments: authorization via a stub `MembershipCheck` (a real `Directory` is
//! mongo-backed and heavy to spin up for a unit test — see `ApiState::membership`'s doc).

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState, MembershipCheck};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

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
    store: Arc<MemStore>,
}

async fn server(with_membership: bool) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(store.clone() as Arc<dyn MetaStore>, jwt.clone(), HashSet::new());
    if with_membership {
        state = state.with_membership(Arc::new(StubMembership));
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, store }
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

#[tokio::test]
async fn member_can_create_and_list_a_team_environment() {
    let s = server(true).await;
    let client = reqwest::Client::new();
    let tok = token(&s.jwt, "karthik");

    let resp = client
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "owner": "acme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["owner"], "acme");
    let id = doc["id"].as_str().unwrap().to_string();

    // Listing without a filter includes the caller's personal envs plus their teams'.
    let list: Vec<Value> =
        client.get(format!("{}/v1/environments", s.base)).bearer_auth(&tok).send().await.unwrap().json().await.unwrap();
    assert!(list.iter().any(|e| e["id"] == id));

    // Filtered by the team explicitly (what the web's /{team}/environments page passes).
    let list: Vec<Value> = client
        .get(format!("{}/v1/environments?owner=acme", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], id);

    // get/start both resolve the env by searching the caller's teams.
    let resp = client.get(format!("{}/v1/environments/{id}", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client.post(format!("{}/v1/environments/{id}/start", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(resp.status(), 202);
}

#[tokio::test]
async fn non_member_cannot_create_or_see_a_team_environment() {
    let s = server(true).await;
    let client = reqwest::Client::new();
    let owner_tok = token(&s.jwt, "karthik");
    let stranger_tok = token(&s.jwt, "mallory");

    // A non-member can't create it in the team's name.
    let resp = client
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&stranger_tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "owner": "acme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Have the real member create one, then confirm the stranger can't reach it.
    let resp = client
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&owner_tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "owner": "acme"}))
        .send()
        .await
        .unwrap();
    let doc: Value = resp.json().await.unwrap();
    let id = doc["id"].as_str().unwrap();

    let resp = client.get(format!("{}/v1/environments/{id}", s.base)).bearer_auth(&stranger_tok).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    let resp = client
        .get(format!("{}/v1/environments?owner=acme", s.base))
        .bearer_auth(&stranger_tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn team_owner_without_a_directory_configured_is_503() {
    let s = server(false).await;
    let client = reqwest::Client::new();
    let tok = token(&s.jwt, "karthik");

    let resp = client
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "owner": "acme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}

/// `clone_env`'s member-authorized read (`find_env`) plus the job it queues — same shape
/// `clone_ws`'s own api tests check, for the team-owned case.
#[tokio::test]
async fn member_can_clone_a_team_environment() {
    let s = server(true).await;
    let client = reqwest::Client::new();
    let tok = token(&s.jwt, "karthik");

    let create = client
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "owner": "acme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 202);
    let src_doc: Value = create.json().await.unwrap();
    let src_id = src_doc["id"].as_str().unwrap().to_string();

    let resp = client
        .post(format!("{}/v1/environments/{src_id}/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev-clone"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let dst_doc: Value = resp.json().await.unwrap();
    assert_eq!(dst_doc["owner"], "acme");
    assert_eq!(dst_doc["state"], "cloning", "a clone's new doc starts Cloning, not Creating");
    let dst_id = dst_doc["id"].as_str().unwrap().to_string();
    assert_ne!(dst_id, src_id);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let clone_job = queued.iter().find(|(j, _)| j.kind == rustic_git_workspaces::model::JobKind::WsClone).unwrap();
    assert_eq!(clone_job.0.payload["environment"], dst_id);
    assert_eq!(clone_job.0.payload["src"], src_id);
    assert_eq!(clone_job.0.payload["owner"], "acme");
    assert_eq!(clone_job.0.payload["stop_project"], format!("env-{src_id}"));
}

#[tokio::test]
async fn personal_workspace_unaffected_by_membership() {
    let s = server(true).await;
    let client = reqwest::Client::new();
    let tok = token(&s.jwt, "karthik");

    let resp = client
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["owner"], "karthik");
}
