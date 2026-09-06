//! Stage 3 · Pull request: the one journey that leaves this tier entirely.
//!
//! `POST …/merge` answers 202 and nothing else — the merge is a JOB the worker picks up (see
//! `crates/pulls/src/merge_worker.rs`), so "did it merge" cannot be read off the response. It is
//! read off the REFS, which is also what a person watching the page sees change, and that is what
//! `pr.merge.p95` times: the ask, plus the wait for the base branch to move.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;

use super::git::{oid_of, BASE_BRANCH, HEAD_BRANCH};
use super::{api, get, poll_json, post};
use crate::ctx::Ctx;

/// The catalogue's own target for `pr.merge.p95`, used here as the wait: a merge that has not
/// landed inside its target has failed the SLO whether the probe waits longer or not.
const MERGE_CAP: Duration = Duration::from_secs(60);
/// `feed.latency` is bounded at 30 s.
const FEED_CAP: Duration = Duration::from_secs(30);

pub async fn run(c: &mut Ctx) {
    let probe = c.probe_user.clone();
    let Some(name) = c.state.repo.clone() else {
        c.skip("pr.merge.p95", "no repo");
        c.skip("feed.latency", "no repo");
        return;
    };

    // The head oid BEFORE the merge, so the wait below is "the base moved to the change", not
    // "the base is some value" — a repo whose main already equalled the head would otherwise
    // report an instant merge that never happened.
    let refs_url = api(c, &format!("/api/{probe}/{name}/refs"));
    let jwt = c.probe_jwt.clone();
    let target = match get(c, &refs_url, &jwt).await.ok().and_then(|r| oid_of(&r, HEAD_BRANCH)) {
        Some(oid) => oid,
        None => {
            let why = format!("`{HEAD_BRANCH}` was never pushed");
            c.skip("pr.merge.p95", &why);
            c.skip("feed.latency", &why);
            return;
        }
    };

    let number = match open(c, &name).await {
        Ok(n) => n,
        Err(e) => {
            let why = format!("no change to merge: {e:#}");
            c.skip("pr.merge.p95", &why);
            c.skip("feed.latency", &why);
            return;
        }
    };

    let merged = {
        let (name, refs_url, target) = (name.clone(), refs_url.clone(), target.clone());
        c.step("pr.merge.p95", MERGE_CAP + Duration::from_secs(30), move |c| {
            let jwt = c.probe_jwt.clone();
            let url = api(c, &format!("/v1/repos/{probe}/{name}/pulls/{number}/merge?strategy=fast-forward"));
            async move {
                post(c, &url, &jwt, serde_json::Value::Null).await.context("could not ask for the merge")?;
                poll_json(c, &refs_url, &jwt, MERGE_CAP, |refs| {
                    oid_of(refs, BASE_BRANCH).as_deref() == Some(target.as_str())
                })
                .await
            }
            .boxed()
        })
        .await
    };

    if !merged {
        // The feed carries PR events off the Redis stream and nothing else (`feed.rs`, "no
        // fallback here on purpose"), so with no merge there is no event to wait for and the
        // failure has already been counted one line up.
        c.skip("feed.latency", "the merge never landed");
        return;
    }

    c.step("feed.latency", FEED_CAP + Duration::from_secs(10), move |c| {
        let probe = c.probe_user.clone();
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/activity?owner={probe}"));
        let repo = name.clone();
        async move {
            poll_json(c, &url, &jwt, FEED_CAP, |feed| merge_event(feed, &repo)).await
        }
        .boxed()
    })
    .await;
}

/// Open the change. Untimed: it is a precondition, not an SLO — the catalogue has no id for it,
/// and inventing one here would put a number in ClickHouse that `deploy/slo.md` cannot explain.
async fn open(c: &mut Ctx, name: &str) -> Result<i64> {
    let probe = c.probe_user.clone();
    let jwt = c.probe_jwt.clone();
    let url = api(c, &format!("/v1/repos/{probe}/{name}/pulls"));
    let body = serde_json::json!({
        "title": format!("slo probe {}", c.run_id),
        "base": BASE_BRANCH,
        "head": HEAD_BRANCH,
    });
    let out = post(c, &url, &jwt, body).await?;
    out.get("number").and_then(|v| v.as_i64()).ok_or_else(|| anyhow!("the answer carried no number"))
}

/// Whether the feed has the merge of THIS repo yet.
///
/// Matched on `owner/name` as well as the kind: the feed is per-owner and the probe's owner holds
/// one repo per run, but a leftover the sweep has not taken yet would otherwise answer for a merge
/// that happened an hour ago.
fn merge_event(feed: &serde_json::Value, repo: &str) -> bool {
    let events = feed.get("events").and_then(|v| v.as_array()).or_else(|| feed.as_array());
    // Exact, not a suffix: `run-fast-1` is a suffix of `run-fast-11`, and the sweep leaves
    // yesterday's runs listed long enough for that to matter. The feed's `repo` is the BARE name
    // (`feed.rs` writes it without the owner), which the first live run proved.
    events.is_some_and(|rows| {
        rows.iter().any(|e| {
            e.get("kind").and_then(|v| v.as_str()) == Some("pull_merged")
                && e.get("repo").and_then(|v| v.as_str()) == Some(repo)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_repos_merge_counts() {
        let feed = serde_json::json!([
            { "kind": "pull_merged", "repo": "run-fast-1" },
            { "kind": "commit", "repo": "slo-probe/run-fast-2" },
        ]);
        assert!(merge_event(&feed, "run-fast-1"));
        // A commit on another run, and a name this one is only a suffix of, are both wrong.
        assert!(!merge_event(&feed, "run-fast-2"));
        assert!(!merge_event(&feed, "fast-1"));
    }
}
