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
