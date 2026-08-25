//! User-facing `/v1` workspaces/environments/regions routes, in-process against `MemStore`.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::model::{JobKind, JobState};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

struct Server {
    base: String,
    store: Arc<MemStore>,
    jwt: Arc<Jwt>,
}

async fn server(admins: &[&str]) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let state = Arc::new(ApiState::new(
        store.clone() as Arc<dyn MetaStore>,
        jwt.clone(),
        admins.iter().map(|s| s.to_string()).collect::<HashSet<_>>(),
    ));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), store, jwt }
}

fn token(jwt: &Jwt, email: &str) -> String {
    // Unit tests key owners by these email-shaped strings throughout; the username claim
    // (what caller() now resolves) just mirrors them. Owner-name VALIDITY is the e2e/route
    // layer's concern, not MemStore's.
    jwt.mint(email, "Test User", Some(email)).unwrap()
}

async fn region(store: &MemStore, id: &str) {
    store
        .put_region(&rustic_git_workspaces::model::Region {
            id: id.into(),
            name: id.into(),
            storage_account: "acct".into(),
            blob_container: "wslayers".into(),
            status: "active".into(),
            agent_token: format!("tok-{id}"),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn create_workspace_returns_202_with_queued_job() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["state"], "creating");
    assert_eq!(doc["owner"], "karthik@example.com");
    let id = doc["id"].as_str().unwrap().to_string();

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0.kind, JobKind::WsCreate);
    assert_eq!(queued[0].0.state, JobState::Queued);
    assert_eq!(queued[0].0.payload["workspace"], id);
}

#[tokio::test]
async fn clone_copies_ref_from_source() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");
    let client = reqwest::Client::new();

    let mut src = rustic_git_workspaces::model::Workspace {
        id: "ws-src".into(),
        owner: "karthik@example.com".into(),
        name: "src".into(),
        region: "centralindia".into(),
        state: rustic_git_workspaces::model::WsState::Ready,
        image: "nginx:alpine".into(),
        placement: None,
        volume: Some("snap-abc".into()),
        quota_gb: 20,
        live_state: json!({"ports": [3000]}),
    };
    s.store.create_ws(&src).await.unwrap();
    src.volume = Some("snap-abc".into());

    let resp = client
        .post(format!("{}/v1/workspaces/ws-src/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web-clone"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["volume"], "snap-abc");
    assert_eq!(doc["live_state"]["ports"][0], 3000);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let clone_job = queued.iter().find(|(j, _)| j.kind == JobKind::WsClone).unwrap();
    assert_eq!(clone_job.0.payload["src"], "ws-src");
}

/// Mounts name volumes (folders inside an env's own subvolume), never workspaces, so a
/// `WsClone` of a standalone workspace no longer has any env to stop first — `stop_projects`
/// stays empty regardless of what envs exist for the owner. See the "An environment is a
/// composition" decision.
#[tokio::test]
async fn clone_never_stops_envs_since_mounts_no_longer_name_workspaces() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");
    let client = reqwest::Client::new();

    let src = rustic_git_workspaces::model::Workspace {
        id: "ws-src".into(),
        owner: "karthik@example.com".into(),
        name: "src".into(),
        region: "centralindia".into(),
        state: rustic_git_workspaces::model::WsState::Ready,
        image: "nginx:alpine".into(),
        placement: None,
        volume: Some("snap-abc".into()),
        quota_gb: 20,
        live_state: json!({}),
    };
    s.store.create_ws(&src).await.unwrap();

    let env = rustic_git_workspaces::model::Environment {
        id: "env-1".into(),
        owner: "karthik@example.com".into(),
        name: "dev".into(),
        region: "centralindia".into(),
        state: rustic_git_workspaces::model::EnvState::Running,
        placement: None,
        volume: None,
        services: vec![rustic_git_workspaces::model::Service {
            name: "app".into(),
            image: "busybox".into(),
            command: vec![],
            env: Default::default(),
            mounts: vec![rustic_git_workspaces::model::Mount { folder: "data".into(), path: "/ws".into() }],
        }],
    };
    s.store.create_env(&env).await.unwrap();

    let resp = client
        .post(format!("{}/v1/workspaces/ws-src/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web-clone"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let clone_job = queued.iter().find(|(j, _)| j.kind == JobKind::WsClone).unwrap();
    let stop_projects = clone_job.0.payload["stop_projects"].as_array().unwrap();
    assert!(stop_projects.is_empty());
}

#[tokio::test]
async fn clone_with_no_envs_yields_empty_stop_projects() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");
    let client = reqwest::Client::new();

    let src = rustic_git_workspaces::model::Workspace {
        id: "ws-src2".into(),
        owner: "karthik@example.com".into(),
        name: "src2".into(),
        region: "centralindia".into(),
        state: rustic_git_workspaces::model::WsState::Ready,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 20,
        live_state: json!({}),
    };
    s.store.create_ws(&src).await.unwrap();

    let resp = client
        .post(format!("{}/v1/workspaces/ws-src2/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web-clone2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let clone_job = queued.iter().find(|(j, _)| j.kind == JobKind::WsClone).unwrap();
    let stop_projects = clone_job.0.payload["stop_projects"].as_array().unwrap();
    assert!(stop_projects.is_empty());
}

#[tokio::test]
async fn restore_carries_snapshot_id_and_state() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");
    let client = reqwest::Client::new();

    let src = rustic_git_workspaces::model::Workspace {
        id: "ws-src".into(),
        owner: "karthik@example.com".into(),
        name: "src".into(),
        region: "centralindia".into(),
        state: rustic_git_workspaces::model::WsState::Ready,
        image: "nginx:alpine".into(),
        placement: None,
        volume: Some("snap-head".into()),
        quota_gb: 20,
        live_state: json!({"ports": [3000]}),
    };
    s.store.create_ws(&src).await.unwrap();
    s.store
        .put_snapshot(&rustic_git_workspaces::model::Snapshot {
            id: "snap-old".into(),
            workspace_id: "ws-src".into(),
            lineage: vec![],
            created_at: chrono::Utc::now(),
            state: json!({"ports": [8080]}),
        })
        .await
        .unwrap();

    let resp = client
        .post(format!("{}/v1/workspaces/restore", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web-old", "snapshot_id": "snap-old", "src_workspace": "ws-src"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["live_state"]["ports"][0], 8080);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let job = queued.iter().find(|(j, _)| j.kind == JobKind::WsRestore).unwrap();
    assert_eq!(job.0.payload["snapshot_id"], "snap-old");
    assert_eq!(job.0.payload["src_workspace"], "ws-src");
}

#[tokio::test]
async fn missing_token_is_unauthorized() {
    let s = server(&[]).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/workspaces", s.base))
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn wrong_token_is_unauthorized() {
    let s = server(&[]).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth("not-a-real-token")
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn region_create_requires_admin() {
    let s = server(&["admin@example.com"]).await;
    let client = reqwest::Client::new();

    let non_admin = token(&s.jwt, "karthik@example.com");
    let resp = client
        .post(format!("{}/v1/regions", s.base))
        .bearer_auth(&non_admin)
        .json(&json!({"id": "centralindia", "name": "Central India", "storage_account": "a", "blob_container": "b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    let admin = token(&s.jwt, "admin@example.com");
    let resp = client
        .post(format!("{}/v1/regions", s.base))
        .bearer_auth(&admin)
        .json(&json!({"id": "centralindia", "name": "Central India", "storage_account": "a", "blob_container": "b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
}

#[tokio::test]
async fn create_environment_returns_202_with_envup_job() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "services": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["state"], "creating");

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0.kind, JobKind::EnvUp);
    assert_eq!(queued[0].0.payload["environment"], doc["id"]);
}

/// The agent work surface (register/work/jobs/{id}/done|failed) moved to the server tier
/// (`bins/server`'s `/vol-agent/*`, Task 14) — this api router no longer mounts `/v1/agent/*` at
/// all, so a request to the old path is a plain 404, not even reaching an auth check.
#[tokio::test]
async fn agent_routes_are_gone_from_the_api_router() {
    let s = server(&[]).await;
    let resp = reqwest::Client::new().post(format!("{}/v1/agent/register", s.base)).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

// ── commit / push ────────────────────────────────────────────────────────

async fn ws(store: &MemStore, id: &str, owner: &str, region: &str) {
    store
        .create_ws(&rustic_git_workspaces::model::Workspace {
            id: id.into(),
            owner: owner.into(),
            name: id.into(),
            region: region.into(),
            state: rustic_git_workspaces::model::WsState::Ready,
            image: "nginx:alpine".into(),
            placement: None,
            volume: None,
            quota_gb: 20,
            live_state: json!({}),
        })
        .await
        .unwrap();
}

async fn env(store: &MemStore, id: &str, owner: &str, region: &str) {
    store
        .create_env(&rustic_git_workspaces::model::Environment {
            id: id.into(),
            owner: owner.into(),
            name: id.into(),
            region: region.into(),
            state: rustic_git_workspaces::model::EnvState::Running,
            placement: None,
            volume: None,
            services: vec![],
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn commit_creates_a_commit_job_carrying_the_message() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    ws(&s.store, "ws-1", "karthik@example.com", "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/commit", s.base))
        .bearer_auth(&tok)
        .json(&json!({"message": "checkpoint"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let job = queued.iter().find(|(j, _)| j.kind == JobKind::Commit).unwrap();
    assert_eq!(job.0.payload["workspace"], "ws-1");
    assert_eq!(job.0.payload["owner"], "karthik@example.com");
    assert_eq!(job.0.payload["message"], "checkpoint");
}

#[tokio::test]
async fn commit_with_no_body_omits_message() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    ws(&s.store, "ws-1", "karthik@example.com", "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/commit", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let job = queued.iter().find(|(j, _)| j.kind == JobKind::Commit).unwrap();
    assert!(job.0.payload.get("message").is_none());
}

#[tokio::test]
async fn push_creates_a_push_job() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    ws(&s.store, "ws-1", "karthik@example.com", "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let job = queued.iter().find(|(j, _)| j.kind == JobKind::Push).unwrap();
    assert_eq!(job.0.payload["workspace"], "ws-1");
}

#[tokio::test]
async fn env_commit_and_push_target_the_environment() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    env(&s.store, "env-1", "karthik@example.com", "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/environments/env-1/commit", s.base))
        .bearer_auth(&tok)
        .json(&json!({"message": "snap"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let resp = client
        .post(format!("{}/v1/environments/env-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let queued = s.store.queued_jobs("centralindia").await.unwrap();
    let commit_job = queued.iter().find(|(j, _)| j.kind == JobKind::Commit).unwrap();
    assert_eq!(commit_job.0.payload["environment"], "env-1");
    assert_eq!(commit_job.0.payload["message"], "snap");
    let push_job = queued.iter().find(|(j, _)| j.kind == JobKind::Push).unwrap();
    assert_eq!(push_job.0.payload["environment"], "env-1");
}

#[tokio::test]
async fn commit_on_someone_elses_workspace_is_not_found() {
    let s = server(&[]).await;
    region(&s.store, "centralindia").await;
    ws(&s.store, "ws-1", "alice@example.com", "centralindia").await;
    let tok = token(&s.jwt, "karthik@example.com");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/commit", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
