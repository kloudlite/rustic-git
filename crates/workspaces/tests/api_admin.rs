//! `api::admin::router` in isolation: every request answers 401/403 without the claim, and every
//! `/v1` path 404s here — the two routers must never both answer the same URL.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{admin::router, ApiState, Directory, TeamRole};
use kloudlite_workspaces::kube_test::{get, mock_client, not_found, post, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite.io/v1alpha1";

/// `karthik` is a member of team `acme`, exercised by the approve-a-team-request case moved here
/// from `api_quota.rs`.
struct StubMembership;

#[async_trait::async_trait]
impl Directory for StubMembership {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        if user == "karthik" { vec!["acme".into()] } else { vec![] }
    }

    async fn team_role(&self, _user: &str, _team: &str) -> Option<TeamRole> {
        None
    }

    async fn is_team(&self, slug: &str) -> bool {
        slug == "acme"
    }
    async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
        Err("no directory".into())
    }

    // This stub exercises team membership and admin routing only; CLI tokens and ssh keys are
    // not part of its case, and an unwired revocation list must refuse rather than admit.
    async fn is_live(&self, _jti: &str) -> bool {
        false
    }

    // No keys in this case: `None` is "the lookup failed", which is what an unwired directory is.
    async fn for_owner(&self, _owner: &str) -> Option<kloudlite_workspaces::api::OwnerMaterial> {
        None
    }
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
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

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

/// A superadmin token, minted the way the api tier mints one at sign-in.
fn admin_token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn req_obj(name: &str, owner: &str, state: Option<&str>) -> Value {
    let mut o = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "QuotaRequest",
        "metadata": {"name": name, "labels": {"kloudlite.io/owner": owner}},
        "spec": {"owner": owner, "requested": {"workspaces": 10}, "reason": "more room"}
    });
    if let Some(st) = state {
        o["status"] = json!({"state": st});
    }
    o
}

/// The one property this whole task exists to guarantee: a `/v1`-shaped path finds nothing on the
/// admin router, so a routing bug cannot make an admin process answer an ordinary user's request
/// with an ordinary user's authorization.
#[tokio::test]
async fn the_admin_router_has_never_heard_of_v1() {
    let s = admin_server(vec![]).await;
    for path in ["/v1/workspaces", "/v1/quota", "/v1/regions", "/v1/quota-requests"] {
        let code = reqwest::Client::new()
            .get(format!("{}{path}", s.base))
            .bearer_auth(admin_token(&s.jwt))
            .send().await.unwrap()
            .status();
        assert_eq!(code, 404, "{path}");
    }
}

/// No token, and a token with no claim, are both refused before any handler runs — the recorder
/// has zero calls either way, which is the "before routing" half of the spec sentence.
#[tokio::test]
async fn every_admin_path_refuses_without_the_claim() {
    let s = admin_server(vec![]).await;
    let code = reqwest::Client::new()
        .get(format!("{}/admin/quota-requests", s.base))
        .send().await.unwrap()
        .status();
    assert_eq!(code, 401);

    let code = reqwest::Client::new()
        .get(format!("{}/admin/quota-requests", s.base))
        .bearer_auth(token(&s.jwt, "karthik")) // an ordinary session, no claim
        .send().await.unwrap()
        .status();
    assert_eq!(code, 403);
    assert!(s.rec.calls().is_empty(), "no handler must run before the claim check: {:?}", s.rec.calls());
}

#[tokio::test]
async fn a_superadmin_may_register_a_region_on_the_admin_host() {
    let routes = vec![Route {
        method: "PATCH",
        path: format!("{API}/regions/us"),
        status: 200,
        body: json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Region",
                      "metadata": {"name": "us"}, "spec": {"name": "US", "status": "active"}}),
    }];
    let s = admin_server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/regions", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"id": "us", "name": "US", "note": "new region"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
}

/// Approving writes the Quota FIRST and only then marks the request: a request marked approved
/// whose quota never landed is the one ordering that leaves a person told yes and still refused.
/// Moved from `api_quota.rs` (Task 5b): same assertions, `/admin` base and router.
#[tokio::test]
async fn approving_writes_the_quota_then_marks_the_request() {
    let routes = vec![
        get(format!("{API}/quotarequests/qr-1"), req_obj("qr-1", "karthik", Some("pending"))),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
        post(format!("{API}/quotas"), json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "karthik"},
            "spec": {"workspaces": 10, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 8, "memoryGb": 32}
        })),
        Route { method: "PATCH", path: format!("{API}/quotarequests/qr-1/status"), status: 200, body: req_obj("qr-1", "karthik", Some("approved")) },
    ];
    let s = admin_server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/quota-requests/qr-1/approve", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "ok"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let written = s.rec.sent("POST", &format!("{API}/quotas")).remove(0);
    // Only the dimension the request named moved; the other five stay at the default they had.
    assert_eq!(written["spec"]["workspaces"], 10);
    assert_eq!(written["spec"]["environments"], 2);
    let calls = s.rec.calls();
    let quota_at = calls.iter().position(|c| c == &format!("POST {API}/quotas")).expect("quota written");
    let mark_at = calls.iter().position(|c| c.contains("quotarequests/qr-1/status")).expect("request marked");
    assert!(quota_at < mark_at, "the quota must land before the request is marked: {calls:?}");
}

/// The other side of the same seed: a request whose owner IS a team (`is_team`, not a guess from
/// who is approving — the admin is never the requester) starts from the team defaults.
#[tokio::test]
async fn approving_a_team_request_seeds_the_team_defaults() {
    let routes = vec![
        get(format!("{API}/quotarequests/qr-2"), req_obj("qr-2", "acme", Some("pending"))),
        not_found(format!("{API}/quotas/acme")),
        not_found(format!("{API}/quotas/default-team")),
        post(format!("{API}/quotas"), json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "acme"},
            "spec": {"workspaces": 10, "environments": 8, "snapshots": 80, "diskGb": 400, "cpu": 32, "memoryGb": 128}
        })),
        Route { method: "PATCH", path: format!("{API}/quotarequests/qr-2/status"), status: 200, body: req_obj("qr-2", "acme", Some("approved")) },
    ];
    let s = admin_server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/quota-requests/qr-2/approve", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "ok"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let written = s.rec.sent("POST", &format!("{API}/quotas")).remove(0);
    // Only the dimension the request named moved; the rest stay at the TEAM default, not the
    // person one — the bug this test guards against is seeding a team's ceiling too low.
    assert_eq!(written["spec"]["workspaces"], 10);
    assert_eq!(written["spec"]["environments"], 8);
}

/// Deciding is the claim's, not the owner's: the person who asked cannot approve their own — and
/// on this router that refusal happens in `refuse_without_claim`, before `approve_quota_request`
/// ever runs.
#[tokio::test]
async fn an_owner_may_not_approve_their_own_request() {
    let s = admin_server(vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/quota-requests/qr-1/approve", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(resp.text().await.unwrap(), "admin only");
}

/// A request that is already decided is not re-decidable: the record of who said what stands.
#[tokio::test]
async fn a_decided_request_cannot_be_decided_again() {
    let routes = vec![get(format!("{API}/quotarequests/qr-1"), req_obj("qr-1", "karthik", Some("denied")))];
    let s = admin_server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/quota-requests/qr-1/deny", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "no"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "that request has already been decided");
}

/// A superadmin listing `?owner=karthik` on `/admin/workspaces` gets karthik's objects — the
/// audit-relevant property Task 9 exists to add: cross-owner reads answer with the QUERIED
/// owner's rows, not the caller's own.
#[tokio::test]
async fn a_superadmin_listing_by_owner_gets_that_owners_workspaces() {
    let ws = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-1", "labels": {"kloudlite.io/owner": "karthik"}},
        "spec": {"owner": "karthik", "team": "", "name": "ws-1", "region": "centralindia",
                 "image": "img:1", "desiredState": "running", "packages": [],
                 "resources": {"cpuRequest": "2", "cpuLimit": "4", "memoryRequest": "4Gi", "memoryLimit": "8Gi"},
                 "storage": {"quotaGb": 20}}
    });
    let routes = vec![
        get(format!("{API}/workspaces"), list_of("Workspace", vec![ws])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
    ];
    let s = admin_server(routes).await;
    let body: Value = reqwest::Client::new()
        .get(format!("{}/admin/workspaces?owner=karthik", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap()
        .json().await.unwrap();
    let rows = body.as_array().expect("a list");
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["owner"], "karthik");
    assert_eq!(rows[0]["id"], "ws-1");
}

/// `NodeDoc` must serialize camelCase like every other doc in this router — the web reads
/// `decommissionStatus`, and a snake_case field is a silently-blank column, not a compile error.
#[tokio::test]
async fn admin_nodes_reports_decommission_status_camel_case() {
    let node = json!({
        "apiVersion": "v1", "kind": "Node",
        "metadata": {
            "name": "node-1",
            "labels": {"kloudlite.io/decommission": "true"},
            "annotations": {"kloudlite.io/decommission-status": "drained 2026-09-04T00:00:00Z"}
        },
        "status": {"conditions": [{"type": "Ready", "status": "True"}]}
    });
    let routes = vec![get("/api/v1/nodes".to_string(), list_of("Node", vec![node]))];
    let s = admin_server(routes).await;
    let body: Value = reqwest::Client::new()
        .get(format!("{}/admin/nodes", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap()
        .json().await.unwrap();
    let rows = body.as_array().expect("a list");
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["decommissionStatus"], "drained 2026-09-04T00:00:00Z", "{body}");
    assert!(rows[0].get("decommission_status").is_none(), "must not also emit snake_case: {body}");
}

/// `?owner=` and `?state=` each narrow the queue independently, and combine.
#[tokio::test]
async fn list_all_quota_requests_filters_by_owner_and_state() {
    let routes = vec![get(
        format!("{API}/quotarequests"),
        list_of(
            "QuotaRequest",
            vec![
                req_obj("qr-1", "karthik", Some("pending")),
                req_obj("qr-2", "karthik", Some("approved")),
                req_obj("qr-3", "acme", Some("pending")),
            ],
        ),
    )];
    let s = admin_server(routes).await;
    let get_req = |qs: &str| {
        let url = format!("{}/admin/quota-requests{qs}", s.base);
        reqwest::Client::new().get(url).bearer_auth(admin_token(&s.jwt)).send()
    };

    let body: Value = get_req("?owner=karthik").await.unwrap().json().await.unwrap();
    let ids: Vec<&str> = body.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["qr-1", "qr-2"], "{body}"); // no creationTimestamp on either fixture: stable order

    let body: Value = get_req("?state=pending").await.unwrap().json().await.unwrap();
    let ids: Vec<&str> = body.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["qr-1", "qr-3"], "{body}");

    let body: Value = get_req("?owner=karthik&state=pending").await.unwrap().json().await.unwrap();
    let ids: Vec<&str> = body.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["qr-1"], "{body}");
}

/// Approving with an edited `requested` body grants what was submitted, not what was originally
/// asked — the "approve with edits" decision from the spec's Decisions section.
#[tokio::test]
async fn approve_with_an_edited_body_grants_the_edited_values() {
    let routes = vec![
        get(format!("{API}/quotarequests/qr-1"), req_obj("qr-1", "karthik", Some("pending"))),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
        post(format!("{API}/quotas"), json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "karthik"},
            "spec": {"workspaces": 6, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 8, "memoryGb": 32}
        })),
        Route { method: "PATCH", path: format!("{API}/quotarequests/qr-1/status"), status: 200, body: req_obj("qr-1", "karthik", Some("approved")) },
    ];
    let s = admin_server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/quota-requests/qr-1/approve", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "granting less", "requested": {"workspaces": 6}}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    // The request asked for 10; the edited approval must land 6, not the original ask.
    let written = s.rec.sent("POST", &format!("{API}/quotas")).remove(0);
    assert_eq!(written["spec"]["workspaces"], 6, "{written}");

    let marked = s.rec.sent("PATCH", &format!("{API}/quotarequests/qr-1/status")).remove(0);
    assert_eq!(marked["status"]["state"], "approved", "{marked}");
}

/// Approving with no body (today's shape) is unchanged — the edit is optional, not mandatory.
#[tokio::test]
async fn approve_with_no_body_still_grants_exactly_what_was_asked() {
    let routes = vec![
        get(format!("{API}/quotarequests/qr-1"), req_obj("qr-1", "karthik", Some("pending"))),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
        post(format!("{API}/quotas"), json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "karthik"},
            "spec": {"workspaces": 10, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 8, "memoryGb": 32}
        })),
        Route { method: "PATCH", path: format!("{API}/quotarequests/qr-1/status"), status: 200, body: req_obj("qr-1", "karthik", Some("approved")) },
    ];
    let s = admin_server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/quota-requests/qr-1/approve", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let written = s.rec.sent("POST", &format!("{API}/quotas")).remove(0);
    assert_eq!(written["spec"]["workspaces"], 10, "{written}");
}
