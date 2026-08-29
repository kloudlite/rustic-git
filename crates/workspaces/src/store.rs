//! Metadata store abstraction: cross-cluster `Region` metadata and nothing else — the CRDs are
//! the truth for workspaces and snapshots. `MemStore` is the in-memory reference used by tests
//! and by dev runs without Cosmos.

use crate::model::Region;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, PartialEq, Eq)]
/// One variant on purpose: the two surviving methods are an upsert and a `SELECT * FROM c`, and
/// neither can 404.
pub enum StoreErr {
    Other(String),
}

#[async_trait::async_trait]
pub trait MetaStore: Send + Sync {
    async fn put_region(&self, r: &Region) -> Result<(), StoreErr>;
    async fn regions(&self) -> Result<Vec<Region>, StoreErr>;
}

#[derive(Default)]
pub struct MemStore {
    regions: Mutex<HashMap<String, Region>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl MetaStore for MemStore {
    async fn put_region(&self, r: &Region) -> Result<(), StoreErr> {
        self.regions.lock().unwrap().insert(r.id.clone(), r.clone());
        Ok(())
    }

    async fn regions(&self) -> Result<Vec<Region>, StoreErr> {
        Ok(self.regions.lock().unwrap().values().cloned().collect())
    }
}


/// The store `COSMOS_ENDPOINT`/`COSMOS_KEY`/`COSMOS_DB` name, or `None` when the endpoint is
/// unset. Both `bins/server` and `bins/api` select it the same way and used to spell the same
/// twenty lines; what differs is only what they do with `None` — the server mounts its routes
/// anyway and answers 503, the api falls back to `MemStore` for dev — so that choice, and the
/// warning that explains it, stays with the caller.
pub async fn from_env() -> Result<Option<std::sync::Arc<dyn MetaStore>>, String> {
    let endpoint = match std::env::var("COSMOS_ENDPOINT") {
        Ok(e) if !e.is_empty() => e,
        _ => return Ok(None),
    };
    let key = std::env::var("COSMOS_KEY").map_err(|_| "COSMOS_KEY required with COSMOS_ENDPOINT".to_string())?;
    let db = std::env::var("COSMOS_DB").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "rustic-git".into());
    tracing::info!(db = %db, "workspaces metadata in cosmos db");
    let s = crate::cosmos::CosmosStore::new(&endpoint, &key, &db)
        .await
        .map_err(|e| format!("connecting to cosmos: {e:?}"))?;
    Ok(Some(std::sync::Arc::new(s)))
}
