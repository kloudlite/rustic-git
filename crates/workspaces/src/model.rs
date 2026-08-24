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
    /// Per-region shared secret agents present on every `/v1/agent/*` request. `default` so
    /// older region docs (written before this field existed) still deserialize.
    #[serde(default)]
    pub agent_token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentDoc {
    pub id: String,
    pub region: String,
    pub hostname: String,
    pub pool: String,
    pub capacity: Capacity,
    // Self-reported by `rustic-git-agent`'s long-poll (`bins/agent`) via `used_cpu`/
    // `used_mem_mb`/`used_disk_gb` query params on every `GET /v1/agent/work`, written into
    // this doc by that handler — see `api::agent_work`.
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
    /// Pointer to the workspace's storage registry volume (`vol/{owner}/{id}`), written by the
    /// job-done handler once the workspace has first pushed; `None` until then. `ref` is kept as
    /// an alias so docs written before the commit/push split still deserialize.
    #[serde(alias = "ref")]
    pub volume: Option<String>,
    pub quota_gb: u64,
    /// Current live state: exposed ports, installed packages, free-form. Snapshotted into the
    /// `Snapshot` record's `state` at push time. Named `live_state` (not `state`, despite the
    /// design doc's JSON sketch reusing that key) because the field above already owns `state`
    /// for the lifecycle enum.
    #[serde(default)]
    pub live_state: serde_json::Value,
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
    /// LOCAL-ONLY: true while this entry has been committed (RO snapshot taken, lineage
    /// extended) but not yet pushed (blob uploaded, `CommitRecord` registered, ref moved). The
    /// local `.lineage` pool file is the only place this state exists — a `CommitRecord` sent
    /// to the registry never carries it (always false on the wire; `push` clears it locally the
    /// moment the record lands). Defaults to false so old lineage files (written before commit
    /// and push split) and every remote copy still parse.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unpushed: bool,
}

impl LineageEntry {
    /// Local pool string form, matching the POC `Entry` encoding: `s:{blob}:{sha}` for a
    /// stream layer, `b:{blob}:{snap}:{sha}` for a block layer, with a trailing `|u` when the
    /// entry is committed-but-not-pushed (see `unpushed`'s doc). The `|u` is appended, not
    /// woven into the `:`-separated fields, so a parser written before commit/push existed
    /// would simply see it as trailing garbage on `sha256` — there is no such parser left, but
    /// it kept the diff to `parse`/`encode` alone.
    pub fn encode(&self) -> String {
        let body = match self.kind {
            LayerKind::Stream => format!("s:{}:{}", self.blob, self.sha256),
            LayerKind::Block => format!(
                "b:{}:{}:{}",
                self.blob,
                self.snap.as_deref().unwrap_or(""),
                self.sha256
            ),
        };
        if self.unpushed { format!("{body}|u") } else { body }
    }

    pub fn parse(s: &str) -> LineageEntry {
        let (s, unpushed) = match s.strip_suffix("|u") {
            Some(rest) => (rest, true),
            None => (s, false),
        };
        let p: Vec<&str> = s.split(':').collect();
        match p[0] {
            "b" => LineageEntry {
                kind: LayerKind::Block,
                blob: p[1].into(),
                snap: Some(p[2].into()),
                sha256: p[3].into(),
                unpushed,
            },
            _ => LineageEntry {
                kind: LayerKind::Stream,
                blob: p[1].into(),
                snap: None,
                sha256: p[2].into(),
                unpushed,
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
    /// The workspace's `state` at push time, copied verbatim from `Workspace.state`.
    #[serde(default)]
    pub state: serde_json::Value,
}

/// Names a folder inside the env's own subvolume (`live/volumes/{volume}`), never a workspace —
/// see the "An environment is a composition" decision in the design doc. Any non-empty `volume`
/// name is valid; the folder is created on demand by `EnvUp`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    pub volume: String,
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
    /// The env's OWN storage registry volume pointer (`vol/{owner}/{id}`; one btrfs subvolume
    /// for the whole env, every mount a folder inside it) — moved by etag CAS exactly like
    /// `Workspace.volume`.
    #[serde(alias = "ref", default)]
    pub volume: Option<String>,
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
    /// RO snapshot + local lineage append, marked unpushed. Fast, no network. `payload`:
    /// `workspace`, `owner`, optional `message`.
    Commit,
    /// Upload every unpushed layer, POST their `CommitRecord`s, move the registry ref. `payload`:
    /// `workspace`, `owner`.
    Push,
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
