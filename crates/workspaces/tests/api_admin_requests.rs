//! `GET /admin/requests` — one queue over `Request` and the legacy `QuotaRequest` CRD, so a
//! console never has to know whether the migration to the generic CRD has run.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{admin::router, ApiState, Directory, TeamRole};
use rustic_git_workspaces::kube_test::{get as route_get, mock_client, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";

struct StubMembership;

#[async_trait::async_trait]
impl Directory for StubMembership {
    async fn teams_for(&self, _user: &str) -> Vec<String> {
        vec![]
    }

    async fn team_role(&self, _user: &str, _team: &str) -> Option<TeamRole> {
        None
    }

    async fn is_team(&self, _slug: &str) -> bool {
        false
    }

    async fn is_live(&self, _jti: &str) -> bool {
        false
    }

    async fn for_owner(&self, _owner: &str) -> Option<rustic_git_workspaces::api::OwnerMaterial> {
        None
    }
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    #[allow(dead_code)]
    rec: Recorder,
}

async fn admin_server(routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(jwt.clone());
    state = state.with_directory(Arc::new(StubMembership));
    let (client, rec) = mock_client(routes);
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

async fn get(url: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new().get(url).bearer_auth(token).send().await.unwrap()
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

/// The queue is one list over two CRDs while the legacy objects still exist: a console must not
/// have to know that a migration is half-done.
#[tokio::test]
async fn the_queue_unions_legacy_quota_requests() {
    let s = admin_server(vec![
        route_get(
            format!("{API}/requests"),
            list_of(
                "Request",
                vec![json!({
                    "metadata": {"name": "req-1", "creationTimestamp": "2026-09-04T10:00:00Z"},
                    "spec": {"owner": "acme", "kind": "access", "requestedBy": "meera", "reason": "r",
                             "access": {"team": "acme", "role": "admin"}},
                    "status": {"state": "pending"}
                })],
            ),
        ),
        route_get(
            format!("{API}/quotarequests"),
            list_of(
                "QuotaRequest",
                vec![json!({
                    "metadata": {"name": "qr-9", "creationTimestamp": "2026-09-03T10:00:00Z"},
                    "spec": {"owner": "zoe", "requested": {"cpu": 12}, "reason": "old"},
                    "status": {"state": "pending"}
                })],
            ),
        ),
    ])
    .await;
    let r = get(&format!("{}/admin/requests", s.base), &admin_token(&s.jwt)).await;
    assert_eq!(r.status(), 200);
    let rows: Vec<Value> = r.json().await.unwrap();
    assert_eq!(rows.len(), 2);
    // Newest first, across both sources.
    assert_eq!(rows[0]["id"], "req-1");
    assert_eq!(rows[0]["kind"], "access");
    // A legacy row wears the same doc shape, kind quota, with its `requested` moved into `quota`.
    assert_eq!(rows[1]["id"], "qr-9");
    assert_eq!(rows[1]["kind"], "quota");
    assert_eq!(rows[1]["quota"]["cpu"], 12);
}

/// `?kind=` narrows to one queue; a legacy row is a quota row, so it drops out of every other.
#[tokio::test]
async fn the_kind_filter_narrows_both_sources() {
    let s = admin_server(vec![
        route_get(
            format!("{API}/requests"),
            list_of(
                "Request",
                vec![json!({
                    "metadata": {"name": "req-1", "creationTimestamp": "2026-09-04T10:00:00Z"},
                    "spec": {"owner": "acme", "kind": "access", "requestedBy": "meera", "reason": "r",
                             "access": {"team": "acme", "role": "admin"}},
                    "status": {"state": "pending"}
                })],
            ),
        ),
        route_get(format!("{API}/quotarequests"), list_of("QuotaRequest", vec![])),
    ])
    .await;
    let r = get(&format!("{}/admin/requests?kind=access", s.base), &admin_token(&s.jwt)).await;
    let rows: Vec<Value> = r.json().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "req-1");
}
