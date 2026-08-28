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

    /// Send one request and check its status. `what` names the call for the error string and is
    /// the ONLY thing that varies: no `reqwest::Error`, URL or header ever reaches a message, so a
    /// propagated failure cannot leak the bearer token.
    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response, String> {
        let resp = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| format!("registry: {what} request failed"))?;
        if !resp.status().is_success() {
            return Err(format!("registry: {what}: {}", resp.status()));
        }
        Ok(resp)
    }

    fn url(&self, owner: &str, name: &str, tail: &str) -> String {
        format!("{}/vol-agent/{owner}/{name}/{tail}", self.base)
    }

    /// Appends a batch of commit records to `{owner}/{name}`'s history. A no-op on an empty
    /// batch — `push` calling this with nothing unpushed would otherwise be a wasted round trip.
    pub async fn post_commits(&self, owner: &str, name: &str, records: &[CommitRecord]) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        let req = self.client.post(self.url(owner, name, "commits")).json(records);
        self.send(req, "commits").await?;
        Ok(())
    }

    /// Moves `{owner}/{name}`'s ref (fixed name `"main"` — one ref per volume, same as the old
    /// single `Workspace.volume`/`Environment.volume` model) to `commit`.
    pub async fn move_ref(&self, owner: &str, name: &str, ref_name: &str, commit: &str) -> Result<(), String> {
        let req = self
            .client
            .post(self.url(owner, name, "ref"))
            .json(&serde_json::json!({"name": ref_name, "commit": commit}));
        self.send(req, "ref move").await?;
        Ok(())
    }

    /// Every commit record for `{owner}/{name}`, newest first (`history`'s own contract) —
    /// `pull`/`clone_local`/`clone_running` treat `[0]` as the current tip, since a push always moves
    /// the one ref forward and this deployment has no branch/rewind story yet.
    pub async fn get_history(&self, owner: &str, name: &str) -> Result<Vec<CommitRecord>, String> {
        let req = self.client.get(self.url(owner, name, "history"));
        let resp = self.send(req, "history").await?;
        resp.json().await.map_err(|_| "registry: history: bad response body".to_string())
    }
}

/// Fixed ref name every volume's engine ops move — see `RegistryClient::move_ref`'s doc.
pub const MAIN_REF: &str = "main";
