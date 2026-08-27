//! The api tier's client for the server tier's volume BROWSE routes
//! (`bins/server/src/browse_api/volumes.rs`): `GET /api/{owner}/volumes` and
//! `GET /api/{owner}/{name}/volumehistory`.
//!
//! Separate from `registry_client` on purpose, even though both reach the same process. That one
//! speaks the agent surface and proves itself with a region's agent token; this one speaks the
//! browse surface on the PEER listener and proves itself with the peer secret, naming the owner it
//! has already verified in `OWNER_HEADER`. Sharing a struct would mean one object holding two
//! unrelated credentials, and the peer secret is the stronger of the two.
//!
//! Same secret discipline as `registry_client`: the secret lives only in a header, and no error
//! here carries a `reqwest::Error`, a URL or a header — only a fixed string.

use crate::registry::CommitRecord;
use rustic_git_core::peer::{OWNER_HEADER, PEER_HEADER};
use std::time::Duration;

/// One volume as the server tier's listing knows it. Deliberately thin: that route reads the
/// object store alone and may never open a volume's database, so this is everything a listing can
/// say without one.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct VolumeRow {
    pub name: String,
    /// Epoch millis of the last write to the volume's database — approximate, see the handler.
    #[serde(default)]
    pub latest_ms: Option<i64>,
}

pub struct Upstream {
    base: String,
    secret: String,
    client: reqwest::Client,
}

impl Upstream {
    pub fn new(base: impl Into<String>, secret: impl Into<String>) -> Upstream {
        Upstream {
            base: base.into().trim_end_matches('/').to_string(),
            secret: secret.into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("build reqwest client"),
        }
    }

    /// `as_owner` is who this request acts as, and the server tier trusts it because the peer
    /// secret vouches for it — so it must only ever be an owner this tier has ALREADY authorized
    /// the caller for (themselves, or a team membership it checked). Passing an unverified value
    /// here would hand the caller someone else's data.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        as_owner: &str,
        path: &str,
    ) -> Result<Option<T>, String> {
        let resp = self
            .client
            .get(format!("{}{path}", self.base))
            .header(PEER_HEADER, &self.secret)
            .header(OWNER_HEADER, as_owner)
            .send()
            .await
            .map_err(|_| "upstream: request failed".to_string())?;
        // The browse tier answers 404 for "not yours" as well as "not there" — deliberately
        // indistinguishable, and this tier must keep them that way.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("upstream: status {}", resp.status().as_u16()));
        }
        resp.json::<T>().await.map(Some).map_err(|_| "upstream: bad response body".to_string())
    }

    /// Every volume `owner` has ever pushed. `None` when the owner is not visible to this caller.
    pub async fn volumes(&self, as_owner: &str, owner: &str) -> Result<Option<Vec<VolumeRow>>, String> {
        self.get_json(as_owner, &format!("/api/{owner}/volumes")).await
    }

    /// One volume's snapshots, newest first.
    pub async fn history(
        &self,
        as_owner: &str,
        owner: &str,
        name: &str,
    ) -> Result<Option<Vec<CommitRecord>>, String> {
        self.get_json(as_owner, &format!("/api/{owner}/{name}/volumehistory")).await
    }
}

/// The provenance a push writes into `CommitRecord.state`: what the volume belonged to at the time.
/// Absent on records written before this existed, and on anything backfilled — readers fall back to
/// the volume id, which is what the page showed for everything before.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl Provenance {
    pub fn of(state: &serde_json::Value) -> Provenance {
        serde_json::from_value(state.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::Provenance;

    /// The free-form `state` slot carries other things too (ports, packages); provenance reads
    /// past them, and a record with none at all must not error.
    #[test]
    fn provenance_reads_past_unrelated_state_and_tolerates_none() {
        let p = Provenance::of(&serde_json::json!({"kind": "workspace", "name": "api-scratch", "ports": [3000]}));
        assert_eq!(p.kind.as_deref(), Some("workspace"));
        assert_eq!(p.name.as_deref(), Some("api-scratch"));

        let empty = Provenance::of(&serde_json::Value::Null);
        assert!(empty.kind.is_none() && empty.name.is_none());

        let unrelated = Provenance::of(&serde_json::json!({"ports": [3000]}));
        assert!(unrelated.kind.is_none() && unrelated.name.is_none());
    }
}
