//! `GET /v1/quota` against a mocked API server (`rustic_git_workspaces::kube_test`) with a stub
//! `Directory` for team membership.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState, Directory};
use rustic_git_workspaces::kube_test::{get, mock_client, not_found, Recorder, Route};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";

/// `karthik` is the only member of team `acme`.
struct StubMembership;

#[async_trait::async_trait]
impl Directory for StubMembership {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        if user == "karthik" { vec!["acme".into()] } else { vec![] }
    }
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    #[allow(dead_code)]
    rec: Recorder,
}

async fn server(with_membership: bool, routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(jwt.clone(), HashSet::new());
    if with_membership {
        state = state.with_directory(Arc::new(StubMembership));
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

fn region_route() -> Route {
    get(
        format!("{API}/regions/centralindia"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Region", "metadata": {"name": "centralindia"}, "spec": {"name": "centralindia", "status": "active"}}),
    )
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn ws_obj(id: &str, owner: &str, state: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": id, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"owner": owner, "team": "", "name": id, "region": "centralindia",
                 "image": "img:1", "desiredState": state, "packages": [],
                 "resources": {"cpuRequest": "2", "cpuLimit": "4", "memoryRequest": "4Gi", "memoryLimit": "8Gi"},
                 "storage": {"quotaGb": 20}}
    })
}

fn vol_obj(name: &str, owner: &str, gb: u64) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"owner": owner, "team": "", "nodeName": "node-a", "region": "centralindia", "quotaGb": gb}
    })
}

/// The four listings usage sums, with no `Quota` object anywhere: the compiled-in default table is
/// what an owner with nothing of their own gets, and a cluster with no `default-user` object must
/// not read as unlimited.
#[tokio::test]
async fn quota_reports_the_default_limits_and_the_computed_use() {
    let routes = vec![
        get(format!("{API}/workspaces"), list_of("Workspace", vec![ws_obj("ws-1", "karthik", "running"), ws_obj("ws-2", "karthik", "stopped")])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![vol_obj("ws-1", "karthik", 20), vol_obj("ws-2", "karthik", 30)])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
    ];
    let s = server(true, routes).await;
    let doc: Value = reqwest::Client::new()
        .get(format!("{}/v1/quota", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send().await.unwrap()
        .json().await.unwrap();

    assert_eq!(doc["limit"]["workspaces"], 5, "{doc}");
    assert_eq!(doc["used"]["workspaces"], 2, "a stopped workspace still holds its place");
    // Detached and attached volumes alike.
    assert_eq!(doc["used"]["diskGb"], 50);
    // Only the RUNNING one occupies capacity: 4 cores, 8Gi.
    assert_eq!(doc["used"]["cpu"], 4);
    assert_eq!(doc["used"]["memoryGb"], 8);
}

/// A team's numbers are the team's. The caller is a member, so the read is allowed and the
/// fallback is the TEAM default, not their personal one.
#[tokio::test]
async fn a_member_reads_their_teams_quota_against_the_team_default() {
    let routes = vec![
        get(format!("{API}/workspaces"), list_of("Workspace", vec![])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        not_found(format!("{API}/quotas/acme")),
        not_found(format!("{API}/quotas/default-team")),
    ];
    let s = server(true, routes).await;
    let doc: Value = reqwest::Client::new()
        .get(format!("{}/v1/quota?owner=acme", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(doc["limit"]["workspaces"], 20, "{doc}");
}

/// An owner the caller is neither nor belongs to is a 404, the same answer every other
/// owner-scoped route gives: whether that owner exists is not theirs to learn.
#[tokio::test]
async fn another_owners_quota_is_not_readable() {
    let s = server(true, vec![]).await;
    let code = reqwest::Client::new()
        .get(format!("{}/v1/quota?owner=someoneelse", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send().await.unwrap()
        .status();
    assert_eq!(code, 404);
}

/// The exact sentence from the design doc, on the exact status the web branches on. The routes
/// below all share `guard_alloc`, so one shape check per KIND of allocation is enough; what each
/// case pins is the DIMENSION the handler asks about.
#[tokio::test]
async fn a_create_at_the_workspace_limit_is_refused_with_the_specs_sentence() {
    let mut items = vec![];
    for i in 0..5 {
        items.push(ws_obj(&format!("ws-{i}"), "karthik", "stopped"));
    }
    let routes = vec![
        region_route(),
        get(format!("{API}/workspaces"), list_of("Workspace", items)),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
    ];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "six", "region": "centralindia", "quota_gb": 5}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "workspaces: 5 of 5 in use; request more under Quota");
    // Nothing was written: the refusal happens before the object, or an over-quota create leaves a
    // workspace behind that the person was told they could not have.
    assert!(!s.rec.calls().iter().any(|c| c == &format!("POST {API}/workspaces")), "{:?}", s.rec.calls());
}

/// Disk is its own dimension and it counts DETACHED volumes: 96 of 100 GB used leaves no room for
/// a 5 GB workspace even though the workspace COUNT is fine.
#[tokio::test]
async fn a_create_that_would_cross_the_disk_limit_is_refused_on_disk() {
    let routes = vec![
        region_route(),
        get(format!("{API}/workspaces"), list_of("Workspace", vec![])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![vol_obj("gone-1", "karthik", 96)])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
    ];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "big", "region": "centralindia", "quota_gb": 5}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "diskGb: 96 of 100 in use; request more under Quota");
}

/// A push at the snapshot limit is refused, and the working copy keeps running — the refusal is
/// before the `Snapshot` CR, so there is nothing half-cut to clean up.
#[tokio::test]
async fn a_push_at_the_snapshot_limit_is_refused_and_cuts_nothing() {
    let snaps: Vec<Value> = (0..20)
        .map(|i| json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": format!("snap-{i}"), "labels": {"rustic-git.io/owner": "karthik"}},
            "spec": {"volume": "ws-1", "owner": "karthik", "worktree": "ws-1", "transient": false},
            "status": {"phase": "ready"}
        }))
        .collect();
    let mut ws = ws_obj("ws-1", "karthik", "running");
    ws["status"] = json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"});
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), ws),
        get(format!("{API}/workspaces"), list_of("Workspace", vec![])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", snaps)),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
    ];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "snapshots: 20 of 20 in use; request more under Quota");
    assert!(!s.rec.calls().iter().any(|c| c == &format!("POST {API}/snapshots")), "{:?}", s.rec.calls());
}
