//! The `vol/` registry namespace's agent surface: append commits, move a ref, read history —
//! over HTTP, on the public listener, gated by `RUSTIC_GIT_VOL_AGENT_TOKENS`.
mod common;

use rustic_git_workspaces::model::{LayerKind, LineageEntry};
use rustic_git_workspaces::registry::CommitRecord;

const TOKEN: &str = "test-agent-token";

/// Every test in this file shares one process (integration test binaries run their `#[tokio::test]`
/// fns concurrently), so the env var is set to the same value everywhere rather than per-test —
/// consistent, not racy.
fn with_token() {
    std::env::set_var("RUSTIC_GIT_VOL_AGENT_TOKENS", TOKEN);
}

fn record(id: &str, msg: &str) -> CommitRecord {
    CommitRecord {
        id: id.to_string(),
        state: serde_json::json!({"ports": [3000]}),
        lineage: vec![LineageEntry {
            kind: LayerKind::Stream,
            blob: format!("blob-{id}"),
            snap: None,
            sha256: "a".repeat(64),
            unpushed: false,
        }],
        region: "centralindia".into(),
        message: Some(msg.to_string()),
        created_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn append_move_ref_and_read_history() {
    with_token();
    let (base, _e) = common::serve_public().await;
    let client = reqwest::Client::new();

    // Three commits, spaced so `created_at` orders them unambiguously.
    let mut ids = vec![];
    for i in 0..3 {
        let id = format!("c{i}");
        let r = record(&id, "auto");
        let resp = client
            .post(format!("{base}/vol-agent/alice/web/commits"))
            .bearer_auth(TOKEN)
            .json(&vec![r])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "append {id}: {}", resp.text().await.unwrap());
        ids.push(id);
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let resp = client
        .post(format!("{base}/vol-agent/alice/web/ref"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({"name": "main", "commit": ids[2]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(format!("{base}/vol-agent/alice/web/history"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let history: Vec<CommitRecord> = resp.json().await.unwrap();
    assert_eq!(history.len(), 3);
    // Newest first.
    assert_eq!(history.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["c2", "c1", "c0"]);
}

#[tokio::test]
async fn wrong_or_missing_token_is_unauthorized() {
    with_token();
    let (base, _e) = common::serve_public().await;
    let client = reqwest::Client::new();

    let resp = client.get(format!("{base}/vol-agent/bob/db/history")).send().await.unwrap();
    assert_eq!(resp.status(), 401, "missing token");

    let resp = client
        .get(format!("{base}/vol-agent/bob/db/history"))
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "wrong token");
}

#[tokio::test]
async fn moving_a_ref_to_an_unknown_commit_is_refused() {
    with_token();
    let (base, _e) = common::serve_public().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/vol-agent/carol/api/ref"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({"name": "main", "commit": "does-not-exist"}))
        .send()
        .await
        .unwrap();
    // Documented choice (task-13-brief.md): 404, not 409 — there is no conflicting write to lose
    // to, just a commit id that was never appended.
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn owner_vol_cannot_be_claimed() {
    assert!(!rustic_git_storage::store::valid_owner("vol"));
}

// ── agent work surface: register / work / jobs/{id}/done|failed ────────────────────────────────
// Moved here from `crates/workspaces/tests/api_agent.rs` (Task 14): same assertions, driven
// against the server tier's `/vol-agent/register|work|jobs/*` instead of the old `/v1/agent/*`.

mod jobs {
    use rustic_git_workspaces::api::WS_AGENT_HEADER;
    use rustic_git_workspaces::model::{AgentDoc, Capacity, Job, JobKind, JobState, Region};
    use rustic_git_workspaces::store::MetaStore;
    use serde_json::{json, Value};

    const TOKEN: &str = "region-secret-tok";

    async fn setup() -> (String, std::sync::Arc<rustic_git_workspaces::store::MemStore>) {
        let (base, _e, store) = super::common::serve_public_with_jobs().await;
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
        (base, store)
    }

    async fn register(base: &str, hostname: &str) -> String {
        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/register"))
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
        let (base, store) = setup().await;
        let id = register(&base, "vm-1").await;
        assert!(id.starts_with("agent-"));
        let agents = store.agents_in("centralindia").await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, id);
        assert_eq!(agents[0].status, "alive");
    }

    #[tokio::test]
    async fn wrong_agent_token_is_unauthorized() {
        let (base, _store) = setup().await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/register"))
            .header(WS_AGENT_HEADER, "not-the-token")
            .json(&json!({"region": "centralindia", "hostname": "vm-1", "pool": "/mnt", "capacity": {"cpu":1,"mem_mb":1,"disk_gb":1}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn empty_agent_token_never_matches_empty_header() {
        let (base, store) = setup().await;
        // Simulates a legacy region doc written before `agent_token` existed (serde(default) ⇒
        // empty string) — an empty presented header must not authenticate against it.
        store
            .put_region(&Region {
                id: "legacy".into(),
                name: "Legacy".into(),
                storage_account: "acct".into(),
                blob_container: "wslayers".into(),
                status: "active".into(),
                agent_token: "".into(),
            })
            .await
            .unwrap();

        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/register"))
            .header(WS_AGENT_HEADER, "")
            .json(&json!({"region": "legacy", "hostname": "vm-1", "pool": "/mnt", "capacity": {"cpu":1,"mem_mb":1,"disk_gb":1}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn queued_job_is_leased_exactly_once_across_two_pollers() {
        let (base, store) = setup().await;
        let a1 = register(&base, "vm-1").await;
        let a2 = register(&base, "vm-2").await;
        store.create_job(&job("job-1")).await.unwrap();

        let client = reqwest::Client::new();
        let poll = |agent: String| {
            let client = client.clone();
            let base = base.clone();
            async move {
                client
                    .get(format!("{base}/vol-agent/work?agent={agent}"))
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

        let (leased, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(leased.state, JobState::Leased);
        assert!(leased.agent.as_deref() == Some(a1.as_str()) || leased.agent.as_deref() == Some(a2.as_str()));
    }

    #[tokio::test]
    async fn done_marks_job_done() {
        let (base, store) = setup().await;
        store.create_job(&job("job-1")).await.unwrap();
        let (mut j, etag) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        j.state = JobState::Leased;
        j.agent = Some("agent-x".into());
        store.replace_job(&j, &etag).await.unwrap();

        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/jobs/job-1/done"))
            .header(WS_AGENT_HEADER, TOKEN)
            .json(&json!({"result": {"ok": true}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(got.state, JobState::Done);
    }

    #[tokio::test]
    async fn failed_requeues_until_attempts_exceed_three() {
        let (base, store) = setup().await;
        store.create_job(&job("job-1")).await.unwrap();
        let (mut j, etag) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        j.state = JobState::Leased;
        j.attempts = 3; // this failure will be the 4th attempt
        store.replace_job(&j, &etag).await.unwrap();

        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/jobs/job-1/failed"))
            .header(WS_AGENT_HEADER, TOKEN)
            .json(&json!({"error": "disk full"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(got.attempts, 4);
        assert_eq!(got.state, JobState::Failed);
    }

    #[tokio::test]
    async fn failed_under_the_retry_budget_goes_back_to_queued() {
        let (base, store) = setup().await;
        store.create_job(&job("job-1")).await.unwrap();

        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/jobs/job-1/failed"))
            .header(WS_AGENT_HEADER, TOKEN)
            .json(&json!({"error": "transient"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(got.attempts, 1);
        assert_eq!(got.state, JobState::Queued);
        assert!(got.agent.is_none());
    }

    #[tokio::test]
    async fn missing_store_answers_503_not_404() {
        // No `put_region`/store wiring needed: `common::serve_public` (no jobs backing store)
        // proves the routes stay mounted and answer clearly rather than looking like they don't
        // exist — see `vol_agent::JobsState`'s doc.
        let (base, _e) = super::common::serve_public().await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/register"))
            .header(WS_AGENT_HEADER, TOKEN)
            .json(&json!({"region": "centralindia", "hostname": "vm-1", "pool": "/mnt", "capacity": {"cpu":1,"mem_mb":1,"disk_gb":1}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);
    }

    #[tokio::test]
    async fn sweep_requeues_expired_lease() {
        let store = rustic_git_workspaces::store::MemStore::new();
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
        let store = rustic_git_workspaces::store::MemStore::new();
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
}
