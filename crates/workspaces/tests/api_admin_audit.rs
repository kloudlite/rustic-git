//! `crate::audit` wired into a live write route, and the `GET /admin/audit` read side —
//! `api::admin::router` against a mocked kube API and an in-memory object store, same harness
//! shape `api_settings.rs` uses for the settings scope.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{admin::router, ApiState};
use rustic_git_workspaces::kube_test::{get, mock_client, patch, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";

fn jwt() -> Arc<Jwt> {
    Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap())
}

fn admin_token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

async fn keys_store() -> Arc<rustic_git_storage::store::Store> {
    let tmp = tempfile::tempdir().unwrap();
    Arc::new(
        rustic_git_storage::store::Store::open(Arc::new(object_store::memory::InMemory::new()), tmp.path().join("cache"), false)
            .await
            .unwrap(),
    )
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
}

async fn admin_server(routes: Vec<Route>, keys: Arc<rustic_git_storage::store::Store>) -> Server {
    let jwt = jwt();
    let mut state = ApiState::new(jwt.clone());
    let (client, _rec) = mock_client(routes);
    state = state.with_kube(client);
    state = state.with_keys(keys);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt }
}

fn req_obj(name: &str, owner: &str, state: Option<&str>) -> Value {
    let mut o = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "QuotaRequest",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"owner": owner, "requested": {"workspaces": 10}, "reason": "more room"}
    });
    if let Some(st) = state {
        o["status"] = json!({"state": st});
    }
    o
}

/// A write route with no reason still lands a row (approve is exempt; deny is not) — the row is
/// what the Audit page and the admin Overview both read, so a route that forgets to call
/// `audit::record` is invisible to both, forever, not just until the next poll.
#[tokio::test]
async fn deny_quota_request_writes_an_audit_row() {
    let routes = vec![
        get(format!("{API}/quotarequests/qr-1"), req_obj("qr-1", "karthik", Some("pending"))),
        patch(
            format!("{API}/quotarequests/qr-1/status"),
            req_obj("qr-1", "karthik", Some("denied")),
        ),
    ];
    let keys = keys_store().await;
    let s = admin_server(routes, keys.clone()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/quota-requests/qr-1/deny", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "over budget"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let resp = reqwest::Client::new()
        .get(format!("{}/admin/audit", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["action"], "deny");
    assert_eq!(rows[0]["target"], "karthik");
    assert_eq!(rows[0]["reason"], "over budget");
    assert_eq!(rows[0]["result"], "ok");
}

/// Filtering by actor, action and target each narrow the result independently.
#[tokio::test]
async fn audit_list_filters_by_actor_action_and_target() {
    let keys = keys_store().await;
    for (actor, action, target) in [("a@x.com", "deny", "acme"), ("b@x.com", "approve", "acme"), ("b@x.com", "roll", "central/worker")] {
        let entry = rustic_git_workspaces::audit::AuditEntry {
            ts: chrono::Utc::now().to_rfc3339(),
            actor: actor.into(),
            action: action.into(),
            target: target.into(),
            reason: None,
            result: "ok".into(),
        };
        rustic_git_workspaces::audit::record(&keys.os, &entry).await.unwrap();
    }
    let s = admin_server(vec![], keys).await;

    let get_rows = |query: String, s: &Server| {
        let base = s.base.clone();
        let token = admin_token(&s.jwt);
        async move {
            let resp = reqwest::Client::new()
                .get(format!("{base}/admin/audit?{query}"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            let body: Value = resp.json().await.unwrap();
            body["rows"].as_array().unwrap().len()
        }
    };

    assert_eq!(get_rows("actor=a@x.com".into(), &s).await, 1);
    assert_eq!(get_rows("action=approve".into(), &s).await, 1);
    assert_eq!(get_rows("target=central/worker".into(), &s).await, 1);
    assert_eq!(get_rows("actor=b@x.com".into(), &s).await, 2);
}
