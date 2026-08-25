//! Metadata store abstraction. Task 2 implements this against Cosmos; `MemStore` here is the
//! in-memory reference used by tests and by anything that doesn't need real persistence yet.

use crate::model::{Environment, Region, Snapshot, Workspace};
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
    async fn replace_ws(&self, w: &Workspace, etag: &Etag) -> Result<(), StoreErr>;
    async fn list_ws(&self, owner: &str) -> Result<Vec<Workspace>, StoreErr>;
    async fn put_snapshot(&self, s: &Snapshot) -> Result<(), StoreErr>;
    async fn get_snapshot(&self, ws: &str, id: &str) -> Result<Option<Snapshot>, StoreErr>;
    // environments: same create/get/replace/list shape as workspaces
    async fn create_env(&self, e: &Environment) -> Result<(), StoreErr>;
    async fn get_env(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<Option<(Environment, Etag)>, StoreErr>;
    async fn replace_env(&self, e: &Environment, etag: &Etag) -> Result<(), StoreErr>;
    async fn list_env(&self, owner: &str) -> Result<Vec<Environment>, StoreErr>;
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
    environments: Mutex<HashMap<(String, String), Versioned<Environment>>>,
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

    async fn replace_ws(&self, w: &Workspace, etag: &Etag) -> Result<(), StoreErr> {
        let mut map = self.workspaces.lock().unwrap();
        let key = (w.owner.clone(), w.id.clone());
        let v = map.get_mut(&key).ok_or(StoreErr::NotFound)?;
        if v.etag.to_string() != *etag {
            return Err(StoreErr::CasFailed);
        }
        v.doc = w.clone();
        v.etag += 1;
        Ok(())
    }

    async fn list_ws(&self, owner: &str) -> Result<Vec<Workspace>, StoreErr> {
        Ok(self
            .workspaces
            .lock()
            .unwrap()
            .values()
            .filter(|v| v.doc.owner == owner)
            .map(|v| v.doc.clone())
            .collect())
    }

    async fn put_snapshot(&self, s: &Snapshot) -> Result<(), StoreErr> {
        self.snapshots
            .lock()
            .unwrap()
            .insert((s.workspace_id.clone(), s.id.clone()), s.clone());
        Ok(())
    }

    async fn get_snapshot(&self, ws: &str, id: &str) -> Result<Option<Snapshot>, StoreErr> {
        Ok(self
            .snapshots
            .lock()
            .unwrap()
            .get(&(ws.to_string(), id.to_string()))
            .cloned())
    }

    async fn create_env(&self, e: &Environment) -> Result<(), StoreErr> {
        let mut map = self.environments.lock().unwrap();
        let key = (e.owner.clone(), e.id.clone());
        if map.contains_key(&key) {
            return Err(StoreErr::Conflict);
        }
        map.insert(key, Versioned::new(e.clone()));
        Ok(())
    }

    async fn get_env(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<Option<(Environment, Etag)>, StoreErr> {
        Ok(self
            .environments
            .lock()
            .unwrap()
            .get(&(owner.to_string(), id.to_string()))
            .map(|v| (v.doc.clone(), v.etag.to_string())))
    }

    async fn replace_env(&self, e: &Environment, etag: &Etag) -> Result<(), StoreErr> {
        let mut map = self.environments.lock().unwrap();
        let key = (e.owner.clone(), e.id.clone());
        let v = map.get_mut(&key).ok_or(StoreErr::NotFound)?;
        if v.etag.to_string() != *etag {
            return Err(StoreErr::CasFailed);
        }
        v.doc = e.clone();
        v.etag += 1;
        Ok(())
    }

    async fn list_env(&self, owner: &str) -> Result<Vec<Environment>, StoreErr> {
        Ok(self
            .environments
            .lock()
            .unwrap()
            .values()
            .filter(|v| v.doc.owner == owner)
            .map(|v| v.doc.clone())
            .collect())
    }





}

