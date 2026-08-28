//! Metadata store abstraction: cross-cluster `Region` metadata and nothing else — the CRDs are
//! the truth for workspaces and snapshots. `MemStore` is the in-memory reference used by tests
//! and by dev runs without Cosmos.

use crate::model::Region;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, PartialEq, Eq)]
pub enum StoreErr {
    CasFailed,
    NotFound,
    Conflict,
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

