//! Agent-facing `/v1/agent/*` routes, in-process against `MemStore`. Mirrors `api_user.rs`'s
//! server-setup shape.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState, WS_AGENT_HEADER};
use rustic_git_workspaces::model::{AgentDoc, Capacity, Job, JobKind, JobState, Region};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

struct Server {
    base: String,
    store: Arc<MemStore>,
}

const TOKEN: &str = "region-secret-tok";

async fn server() -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(store.clone() as Arc<dyn MetaStore>, jwt, HashSet::new());
    // Short poll window so the leasing/204-timeout tests don't take real minutes.
    state.agent_poll_window = Duration::from_millis(600);
    state.agent_poll_interval = Duration::from_millis(50);
    let state = Arc::new(state);

    store
        .put_region(&Region {
            id: "centralindia".into(),
            name: "Central India".into(),
            storage_account: "acct".into(),
            blob_container: "wslayers".into(),
            status: "active".into(),
            agent_token: TOKEN.into(),
        })
        .await
        .unwrap();

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), store }
}

async fn register(base: &str, hostname: &str) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/agent/register"))
        .header(WS_AGENT_HEADER, TOKEN)
        .json(&json!({
            "region": "centralindia",
            "hostname": hostname,
            "pool": "/mnt/wspool",
            "capacity": {"cpu": 4, "mem_mb": 16384, "disk_gb": 128},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    body["id"].as_str().unwrap().to_string()
}

fn job(id: &str) -> Job {
    Job {
        id: id.into(),
        region: "centralindia".into(),
        agent: None,
        kind: JobKind::WsCreate,
        payload: json!({"workspace": "ws-1"}),
        state: JobState::Queued,
        lease_until: None,
        attempts: 0,
        error: None,
    }
}

#[tokio::test]
async fn register_returns_id_and_alive_agent_doc() {
    let s = server().await;
    let id = register(&s.base, "vm-1").await;
    assert!(id.starts_with("agent-"));
    let agents = s.store.agents_in("centralindia").await.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].id, id);
    assert_eq!(agents[0].status, "alive");
}

#[tokio::test]
async fn wrong_agent_token_is_unauthorized() {
    let s = server().await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/agent/register", s.base))
        .header(WS_AGENT_HEADER, "not-the-token")
        .json(&json!({"region": "centralindia", "hostname": "vm-1", "pool": "/mnt", "capacity": {"cpu":1,"mem_mb":1,"disk_gb":1}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn queued_job_is_leased_exactly_once_across_two_pollers() {
    let s = server().await;
    let a1 = register(&s.base, "vm-1").await;
    let a2 = register(&s.base, "vm-2").await;
    s.store.create_job(&job("job-1")).await.unwrap();

    let client = reqwest::Client::new();
    let poll = |agent: String| {
        let client = client.clone();
        let base = s.base.clone();
        async move {
            client
                .get(format!("{base}/v1/agent/work?agent={agent}"))
                .header(WS_AGENT_HEADER, TOKEN)
                .send()
                .await
                .unwrap()
        }
    };

    let (r1, r2) = tokio::join!(poll(a1.clone()), poll(a2.clone()));
    let statuses: Vec<_> = [r1.status(), r2.status()].into_iter().collect();
    assert_eq!(statuses.iter().filter(|s| **s == 200).count(), 1, "exactly one poller gets the job");
    assert_eq!(statuses.iter().filter(|s| **s == 204).count(), 1, "the other times out");

    let (leased, _) = s.store.get_job("centralindia", "job-1").await.unwrap().unwrap();
    assert_eq!(leased.state, JobState::Leased);
    assert!(leased.agent.as_deref() == Some(a1.as_str()) || leased.agent.as_deref() == Some(a2.as_str()));
}

#[tokio::test]
async fn done_marks_job_done() {
    let s = server().await;
    s.store.create_job(&job("job-1")).await.unwrap();
    let (mut j, etag) = s.store.get_job("centralindia", "job-1").await.unwrap().unwrap();
    j.state = JobState::Leased;
    j.agent = Some("agent-x".into());
    s.store.replace_job(&j, &etag).await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/agent/jobs/job-1/done", s.base))
        .header(WS_AGENT_HEADER, TOKEN)
        .json(&json!({"result": {"ok": true}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (got, _) = s.store.get_job("centralindia", "job-1").await.unwrap().unwrap();
    assert_eq!(got.state, JobState::Done);
}

#[tokio::test]
async fn failed_requeues_until_attempts_exceed_three() {
    let s = server().await;
    s.store.create_job(&job("job-1")).await.unwrap();
    let (mut j, etag) = s.store.get_job("centralindia", "job-1").await.unwrap().unwrap();
    j.state = JobState::Leased;
    j.attempts = 3; // this failure will be the 4th attempt
    s.store.replace_job(&j, &etag).await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/agent/jobs/job-1/failed", s.base))
        .header(WS_AGENT_HEADER, TOKEN)
        .json(&json!({"error": "disk full"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (got, _) = s.store.get_job("centralindia", "job-1").await.unwrap().unwrap();
    assert_eq!(got.attempts, 4);
    assert_eq!(got.state, JobState::Failed);
}

#[tokio::test]
async fn failed_under_the_retry_budget_goes_back_to_queued() {
    let s = server().await;
    s.store.create_job(&job("job-1")).await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/agent/jobs/job-1/failed", s.base))
        .header(WS_AGENT_HEADER, TOKEN)
        .json(&json!({"error": "transient"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (got, _) = s.store.get_job("centralindia", "job-1").await.unwrap().unwrap();
    assert_eq!(got.attempts, 1);
    assert_eq!(got.state, JobState::Queued);
    assert!(got.agent.is_none());
}

#[tokio::test]
async fn sweep_requeues_expired_lease() {
    let store = MemStore::new();
    let mut j = job("job-1");
    j.state = JobState::Leased;
    j.agent = Some("agent-x".into());
    j.lease_until = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    store.create_job(&j).await.unwrap();
    store
        .upsert_agent(&AgentDoc {
            id: "agent-x".into(),
            region: "centralindia".into(),
            hostname: "vm-1".into(),
            pool: "/mnt".into(),
            capacity: Capacity { cpu: 4, mem_mb: 1, disk_gb: 1 },
            used: Capacity { cpu: 0, mem_mb: 0, disk_gb: 0 },
            heartbeat_at: chrono::Utc::now(),
            status: "alive".into(),
        })
        .await
        .unwrap();

    rustic_git_workspaces::lease::sweep(&store, "centralindia").await.unwrap();

    let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
    assert_eq!(got.state, JobState::Queued);
    assert_eq!(got.attempts, 1);
    assert!(got.agent.is_none());
}

#[tokio::test]
async fn sweep_requeues_jobs_of_a_dead_agent() {
    let store = MemStore::new();
    let mut j = job("job-1");
    j.state = JobState::Leased;
    j.agent = Some("agent-dead".into());
    j.lease_until = Some(chrono::Utc::now() + chrono::Duration::seconds(60)); // lease not expired
    store.create_job(&j).await.unwrap();
    store
        .upsert_agent(&AgentDoc {
            id: "agent-dead".into(),
            region: "centralindia".into(),
            hostname: "vm-1".into(),
            pool: "/mnt".into(),
            capacity: Capacity { cpu: 4, mem_mb: 1, disk_gb: 1 },
            used: Capacity { cpu: 0, mem_mb: 0, disk_gb: 0 },
            heartbeat_at: chrono::Utc::now() - chrono::Duration::minutes(5),
            status: "alive".into(),
        })
        .await
        .unwrap();

    rustic_git_workspaces::lease::sweep(&store, "centralindia").await.unwrap();

    let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
    assert_eq!(got.state, JobState::Queued);
}
