//! Fsck: rebuild candidate lineage from `layers/*.json` sidecars alone, for when `Snapshot`
//! docs are lost or corrupted. Sidecars chain by `parent_blob`; a chain's tip is any blob no
//! other sidecar names as its parent. `rebuild` never writes refs — a human decides which
//! candidate tip a workspace should actually point at, then calls `adopt`.

use crate::engine::blob::LayerSidecar;
use crate::engine::ops::EngErr;
use crate::model::{LineageEntry, Snapshot};
use crate::store::MetaStore;
use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, path::Path as S3Path};
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct FsckReport {
    /// One entry per candidate tip, each the full root-to-tip lineage for that chain.
    pub chains: Vec<Vec<LineageEntry>>,
    /// The tip blob id of each chain, same order as `chains`.
    pub tips: Vec<String>,
}

fn uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap().trim().into()
}

/// List every `layers/*.json` sidecar, chain them by `parent_blob`, and report one lineage per
/// candidate tip (a blob no sidecar names as its parent). Writes nothing.
pub async fn rebuild(store: &dyn ObjectStore) -> Result<FsckReport, EngErr> {
    let mut sidecars: HashMap<String, LayerSidecar> = HashMap::new();
    let mut list = store.list(Some(&S3Path::from("layers/")));
    while let Some(meta) = list.next().await {
        let meta = meta.map_err(|e| EngErr::other(e.to_string()))?;
        let Some(name) = meta.location.filename() else { continue };
        let Some(blob_id) = name.strip_suffix(".json") else { continue };
        let bytes = store
            .get(&meta.location)
            .await
            .map_err(|e| EngErr::other(e.to_string()))?
            .bytes()
            .await
            .map_err(|e| EngErr::other(e.to_string()))?;
        let sidecar: LayerSidecar = serde_json::from_slice(&bytes).map_err(|e| EngErr::other(e.to_string()))?;
        sidecars.insert(blob_id.to_string(), sidecar);
    }

    let mut is_parent: HashSet<&str> = HashSet::new();
    for s in sidecars.values() {
        if let Some(p) = &s.parent_blob {
            is_parent.insert(p.as_str());
        }
    }
    let mut tips: Vec<String> =
        sidecars.keys().filter(|id| !is_parent.contains(id.as_str())).cloned().collect();
    tips.sort();

    let mut chains = Vec::new();
    for tip in &tips {
        let mut entries = Vec::new();
        let mut cur = Some(tip.clone());
        while let Some(blob_id) = cur {
            let Some(s) = sidecars.get(&blob_id) else { break };
            entries.push(LineageEntry {
                kind: s.kind,
                blob: blob_id.clone(),
                snap: s.snap_uuid.clone(),
                sha256: s.sha256.clone(),
            });
            cur = s.parent_blob.clone();
        }
        entries.reverse();
        chains.push(entries);
    }

    Ok(FsckReport { chains, tips })
}

/// Write `chain` as a new `Snapshot` doc under `ws_id` — the human-approved step after
/// reviewing `rebuild`'s report. Never moves the workspace's ref; the caller does that (or not).
pub async fn adopt(meta: &dyn MetaStore, ws_id: &str, chain: &[LineageEntry]) -> Result<String, EngErr> {
    let id = uuid();
    let snap = Snapshot {
        id: id.clone(),
        workspace_id: ws_id.to_string(),
        lineage: chain.to_vec(),
        created_at: chrono::Utc::now(),
        state: serde_json::Value::Null,
    };
    meta.put_snapshot(&snap).await.map_err(EngErr::store)?;
    Ok(id)
}
