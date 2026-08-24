//! Domain models, mirrored 1:1 against the Cosmos JSON in
//! docs/superpowers/specs/2026-08-24-workspaces-environments-design.md §Domain model.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capacity {
    pub cpu: u32,
    pub mem_mb: u64,
    pub disk_gb: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub storage_account: String,
    pub blob_container: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentDoc {
    pub id: String,
    pub region: String,
    pub hostname: String,
    pub pool: String,
    pub capacity: Capacity,
    pub used: Capacity,
    pub heartbeat_at: chrono::DateTime<chrono::Utc>,
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WsState {
    Creating,
    Ready,
    Error,
    Deleted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub region: String,
    pub state: WsState,
    pub placement: Option<String>,
    #[serde(rename = "ref")]
    pub ref_: Option<String>,
    pub quota_gb: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayerKind {
    Block,
    Stream,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineageEntry {
    pub kind: LayerKind,
    pub blob: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snap: Option<String>,
    pub sha256: String,
}

impl LineageEntry {
    /// Local pool string form, matching the POC `Entry` encoding: `s:{blob}:{sha}` for a
    /// stream layer, `b:{blob}:{snap}:{sha}` for a block layer.
    pub fn encode(&self) -> String {
        match self.kind {
            LayerKind::Stream => format!("s:{}:{}", self.blob, self.sha256),
            LayerKind::Block => format!(
                "b:{}:{}:{}",
                self.blob,
                self.snap.as_deref().unwrap_or(""),
                self.sha256
            ),
        }
    }

    pub fn parse(s: &str) -> LineageEntry {
        let p: Vec<&str> = s.split(':').collect();
        match p[0] {
            "b" => LineageEntry {
                kind: LayerKind::Block,
                blob: p[1].into(),
                snap: Some(p[2].into()),
                sha256: p[3].into(),
            },
            _ => LineageEntry {
                kind: LayerKind::Stream,
                blob: p[1].into(),
                snap: None,
                sha256: p[2].into(),
            },
        }
    }

    /// Name of the local RO snapshot this entry materializes: the blob id for a stream
    /// layer, or the contained subvolume name for a block layer (the stream snapshot it
    /// materializes, so streams chain across the block boundary by received-UUID exactly as
    /// they would over the wire).
    pub fn snap_name(&self) -> &str {
        match self.kind {
            LayerKind::Stream => &self.blob,
            LayerKind::Block => self.snap.as_deref().unwrap_or(&self.blob),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub workspace_id: String,
    pub lineage: Vec<LineageEntry>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    pub workspace: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub mounts: Vec<Mount>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvState {
    Creating,
    Running,
    Stopped,
    Error,
    Deleted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Environment {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub region: String,
    pub state: EnvState,
    pub placement: Option<String>,
    pub services: Vec<Service>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    WsCreate,
    WsPush,
    WsFork,
    WsClone,
    WsDelete,
    EnvUp,
    EnvDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Leased,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub region: String,
    pub agent: Option<String>,
    pub kind: JobKind,
    pub payload: serde_json::Value,
    pub state: JobState,
    pub lease_until: Option<chrono::DateTime<chrono::Utc>>,
    pub attempts: u32,
    pub error: Option<String>,
}
