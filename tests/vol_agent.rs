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

/// The audit's finding: any registered region's agent token authorized writes to ANY volume, so a
/// leaked token from one region could rewrite another region's commit history and move its `main`
/// ref. A token is now scoped to volumes of its own region.
#[tokio::test]
async fn a_token_from_another_region_cannot_write_this_volume() {
    // Deliberately does NOT touch RUSTIC_GIT_VOL_AGENT_TOKENS. Tests in this binary share one
    // process and that variable (see `with_token`), so mutating it here would race them. It is not
    // needed: break-glass is only ever set to TOKEN, and the tokens presented below are different
    // strings, so the fleet-wide path cannot match and cannot mask the scoping being tested.
    let (base, _e) = common::serve_public_with_regions(&[("region-a", "tok-a"), ("region-b", "tok-b")]).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/vol-agent/alice/web/commits");

    let mut first = record("c1", "from region a");
    first.region = "region-a".into();
    let r = client.post(&url).bearer_auth("tok-a").json(&vec![first]).send().await.unwrap();
    assert_eq!(r.status(), 200, "the first writer claims the volume for its region");

    // Same volume, a different region's token.
    let mut intruder = record("c2", "from region b");
    intruder.region = "region-b".into();
    let r = client.post(&url).bearer_auth("tok-b").json(&vec![intruder]).send().await.unwrap();
    assert_eq!(r.status(), 401, "region-b must not write a region-a volume");

    // And moving the ref is refused by the same rule — the ref move is the damaging half.
    let r = client
        .post(format!("{base}/vol-agent/alice/web/ref"))
        .bearer_auth("tok-b")
        .json(&serde_json::json!({"name": "main", "commit": "c1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "region-b must not move a region-a volume's ref");

    // The owning region still works.
    let mut second = record("c3", "still region a");
    second.region = "region-a".into();
    let r = client.post(&url).bearer_auth("tok-a").json(&vec![second]).send().await.unwrap();
    assert_eq!(r.status(), 200, "the owning region must keep working");
}

/// The other half of region scoping: an unstamped volume is claimed by its first record's
/// `region`, so a region-A token that could write a record labelled region-B would stamp the
/// volume as B and lock A out of it. A record's region must be the region the token is for.
#[tokio::test]
async fn a_record_for_another_region_than_the_token_s_is_refused() {
    let (base, _e) = common::serve_public_with_regions(&[("region-a", "tok-a"), ("region-b", "tok-b")]).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/vol-agent/dave/web/commits");

    let mut mislabelled = record("c1", "stamp it as b");
    mislabelled.region = "region-b".into();
    let r = client.post(&url).bearer_auth("tok-a").json(&vec![mislabelled]).send().await.unwrap();
    assert_eq!(r.status(), 400, "{}", r.text().await.unwrap());

    // Nothing was stamped: region-b's own token can still claim the volume.
    let mut own = record("c2", "region b's own");
    own.region = "region-b".into();
    let r = client.post(&url).bearer_auth("tok-b").json(&vec![own]).send().await.unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}
