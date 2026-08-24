//! Thin reqwest client for the agent-facing volume registry routes served by
//! `bins/server/src/vol_agent.rs`: `POST {owner}/{name}/commits`, `POST .../ref`,
//! `GET .../history`. The bearer token is the same shared-secret `RUSTIC_GIT_VOL_AGENT_TOKENS`
//! the agent already carries for `register`/`work`/`jobs/*`.
//!
//! Same discipline as `crates/pulls/src/merge_worker.rs`'s `local()`/`networked()` split: the
//! token lives only in the `Authorization` header, and every error here is a fixed string (or a
//! bare HTTP status) — never a formatted `reqwest::Error`, request URL, or header — so a
//! propagated push/pull failure can never leak it into a log or an API response.

use crate::registry::CommitRecord;
use std::time::Duration;

pub struct RegistryClient {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl RegistryClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> RegistryClient {
        RegistryClient {
            base: base.into(),
            token: token.into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("build reqwest client"),
        }
    }

    /// Appends a batch of commit records to `{owner}/{name}`'s history. A no-op on an empty
    /// batch — `push` calling this with nothing unpushed would otherwise be a wasted round trip.
    pub async fn post_commits(&self, owner: &str, name: &str, records: &[CommitRecord]) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        let resp = self
            .client
            .post(format!("{}/vol-agent/{owner}/{name}/commits", self.base))
            .bearer_auth(&self.token)
            .json(records)
            .send()
            .await
            .map_err(|_| "registry: commits request failed".to_string())?;
        if !resp.status().is_success() {
            return Err(format!("registry: commits: {}", resp.status()));
        }
        Ok(())
    }

    /// Moves `{owner}/{name}`'s ref (fixed name `"main"` — one ref per volume, same as the old
    /// single `Workspace.ref_`/`Environment.ref_` model) to `commit`.
    pub async fn move_ref(&self, owner: &str, name: &str, ref_name: &str, commit: &str) -> Result<(), String> {
        let resp = self
            .client
            .post(format!("{}/vol-agent/{owner}/{name}/ref", self.base))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({"name": ref_name, "commit": commit}))
            .send()
            .await
            .map_err(|_| "registry: ref move request failed".to_string())?;
        if !resp.status().is_success() {
            return Err(format!("registry: ref move: {}", resp.status()));
        }
        Ok(())
    }

    /// Every commit record for `{owner}/{name}`, newest first (`history`'s own contract) —
    /// `pull`/`fork`/`clone_running` treat `[0]` as the current tip, since a push always moves
    /// the one ref forward and this deployment has no branch/rewind story yet.
    pub async fn get_history(&self, owner: &str, name: &str) -> Result<Vec<CommitRecord>, String> {
        let resp = self
            .client
            .get(format!("{}/vol-agent/{owner}/{name}/history", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| "registry: history request failed".to_string())?;
        if !resp.status().is_success() {
            return Err(format!("registry: history: {}", resp.status()));
        }
        resp.json().await.map_err(|_| "registry: history: bad response body".to_string())
    }
}

/// Fixed ref name every volume's engine ops move — see `RegistryClient::move_ref`'s doc.
pub const MAIN_REF: &str = "main";
