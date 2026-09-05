//! `/v1/requests` against a mocked API server. The stub `Directory` is the one `api_quota.rs`
//! uses: `karthik` is an admin of team `acme`, `bob` a plain member.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState, Directory, TeamRole};
use kloudlite_workspaces::kube_test::{get, mock_client, post, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite.io/v1alpha1";

struct StubMembership;

#[async_trait::async_trait]
impl Directory for StubMembership {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        if user == "karthik" || user == "bob" { vec!["acme".into()] } else { vec![] }
    }
    async fn team_role(&self, user: &str, team: &str) -> Option<TeamRole> {
        match (user, team) {
            ("karthik", "acme") => Some(TeamRole::Admin),
            ("bob", "acme") => Some(TeamRole::Member),
            _ => None,
        }
    }
    async fn is_live(&self, _jti: &str) -> bool {
        false
    }
    async fn for_owner(&self, _owner: &str) -> Option<kloudlite_workspaces::api::OwnerMaterial> {
        None
    }
    async fn is_team(&self, slug: &str) -> bool {
        slug == "acme"
    }
    async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
        Err("no directory".into())
    }
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

async fn server(routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(jwt.clone()).with_directory(Arc::new(StubMembership)).with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

fn list_of(items: Vec<Value>) -> Value {
    json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "RequestList", "metadata": {}, "items": items})
}

fn stored(id: &str, owner: &str, kind: &str, state: Option<&str>) -> Value {
    let mut o = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Request",
        "metadata": {"name": id, "labels": {"kloudlite.io/owner": owner}},
        "spec": {"owner": owner, "kind": kind, "requestedBy": owner, "reason": "r",
                 "other": {"title": "t", "body": "b"}},
    });
    if let Some(st) = state {
        o["status"] = json!({"state": st});
    }
    o
}

/// The signed-in caller is the author, whatever the body says: `requestedBy` is evidence.
#[tokio::test]
async fn a_create_takes_its_author_from_the_claims() {
    let s = server(vec![
        get(format!("{API}/requests"), list_of(vec![])),
        post(format!("{API}/requests"), stored("req-1", "karthik", "other", None)),
    ])
    .await;
    let client = reqwest::Client::new();
    let r = client
        .post(format!("{}/v1/requests", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"kind": "other", "reason": "r", "other": {"title": "t", "body": "b"},
               "requestedBy": "someone-else"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let sent = s.rec.sent("POST", &format!("{API}/requests")).remove(0);
    assert_eq!(sent["spec"]["requestedBy"], "karthik");
    assert_eq!(sent["spec"]["owner"], "karthik");
    assert_eq!(sent["metadata"]["labels"]["kloudlite.io/owner"], "karthik");
}

/// One pending per owner PER KIND: a pending `other` must not block a `quota`.
#[tokio::test]
async fn one_pending_per_owner_per_kind() {
    let s = server(vec![
        get(format!("{API}/requests"), list_of(vec![stored("req-1", "karthik", "other", Some("pending"))])),
        post(format!("{API}/requests"), stored("req-2", "karthik", "quota", None)),
    ])
    .await;
    let client = reqwest::Client::new();
    let same = client
        .post(format!("{}/v1/requests", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"kind": "other", "reason": "again", "other": {"title": "t", "body": "b"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(same.status(), 409, "a second pending request of the same kind is refused");

    let other_kind = client
        .post(format!("{}/v1/requests", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"kind": "quota", "reason": "room", "quota": {"workspaces": 9}}))
        .send()
        .await
        .unwrap();
    assert_eq!(other_kind.status(), 201, "a different kind is a different queue");
}

/// A malformed request never reaches the cluster: the block has to match the kind.
#[tokio::test]
async fn a_block_that_does_not_match_the_kind_is_refused() {
    let s = server(vec![get(format!("{API}/requests"), list_of(vec![]))]).await;
    let client = reqwest::Client::new();
    let r = client
        .post(format!("{}/v1/requests", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"kind": "quota", "reason": "r", "other": {"title": "t", "body": "b"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    assert!(s.rec.sent("POST", &format!("{API}/requests")).is_empty(), "nothing was written");
}

/// A plain member cannot open a request against the team's ceiling — the same directory rule
/// `/v1/quota-requests` already applies, unchanged by the new kinds.
#[tokio::test]
async fn only_a_team_admin_may_ask_for_a_team() {
    let s = server(vec![get(format!("{API}/requests"), list_of(vec![]))]).await;
    let client = reqwest::Client::new();
    let r = client
        .post(format!("{}/v1/requests", s.base))
        .bearer_auth(token(&s.jwt, "bob"))
        .json(&json!({"owner": "acme", "kind": "quota", "reason": "r", "quota": {"cpu": 9}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

/// `GET /v1/requests/{id}` is the caller's own, and somebody else's is a 404 — never a 403,
/// which would confirm the id exists.
#[tokio::test]
async fn another_owners_request_is_not_found() {
    let s = server(vec![get(format!("{API}/requests/req-9"), stored("req-9", "zoe", "other", Some("pending")))]).await;
    let client = reqwest::Client::new();
    let r = client
        .get(format!("{}/v1/requests/req-9", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
