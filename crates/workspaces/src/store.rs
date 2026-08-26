//! Metadata store abstraction. Task 2 implements this against Cosmos; `MemStore` here is the
//! in-memory reference used by tests and by anything that doesn't need real persistence yet.

use crate::model::{Region, Snapshot, Workspace};
use std::collections::HashMap;
use std::sync::Mutex;

pub type Etag = String;

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
    async fn create_ws(&self, w: &Workspace) -> Result<(), StoreErr>;
    async fn get_ws(&self, owner: &str, id: &str) -> Result<Option<(Workspace, Etag)>, StoreErr>;
    async fn put_snapshot(&self, s: &Snapshot) -> Result<(), StoreErr>;
}

// A doc plus the etag it was last written with. Cosmos etags are opaque server-assigned
// strings; a bumped counter is the cheapest thing that behaves the same way for tests.
struct Versioned<T> {
    doc: T,
    etag: u64,
}

impl<T> Versioned<T> {
    fn new(doc: T) -> Self {
        Versioned { doc, etag: 1 }
    }
}

#[derive(Default)]
pub struct MemStore {
    regions: Mutex<HashMap<String, Region>>,
    // keyed by (owner, id) since that's the partition + id Cosmos would use.
    workspaces: Mutex<HashMap<(String, String), Versioned<Workspace>>>,
    snapshots: Mutex<HashMap<(String, String), Snapshot>>,
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





    async fn create_ws(&self, w: &Workspace) -> Result<(), StoreErr> {
        let mut map = self.workspaces.lock().unwrap();
        let key = (w.owner.clone(), w.id.clone());
        if map.contains_key(&key) {
            return Err(StoreErr::Conflict);
        }
        map.insert(key, Versioned::new(w.clone()));
        Ok(())
    }

    async fn get_ws(&self, owner: &str, id: &str) -> Result<Option<(Workspace, Etag)>, StoreErr> {
        Ok(self
            .workspaces
            .lock()
            .unwrap()
            .get(&(owner.to_string(), id.to_string()))
            .map(|v| (v.doc.clone(), v.etag.to_string())))
    }



    async fn put_snapshot(&self, s: &Snapshot) -> Result<(), StoreErr> {
        self.snapshots
            .lock()
            .unwrap()
            .insert((s.workspace_id.clone(), s.id.clone()), s.clone());
        Ok(())
    }










}

