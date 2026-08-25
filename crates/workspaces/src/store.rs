//! Metadata store abstraction. Task 2 implements this against Cosmos; `MemStore` here is the
//! in-memory reference used by tests and by anything that doesn't need real persistence yet.

use crate::model::{AgentDoc, Binding, Environment, Job, JobState, Region, Snapshot, Workspace};
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
    async fn upsert_agent(&self, a: &AgentDoc) -> Result<(), StoreErr>;
    async fn agents_in(&self, region: &str) -> Result<Vec<AgentDoc>, StoreErr>;
    // Shared-node owner binding: create_binding is a one-shot claim (Conflict when the owner
    // already has one — the DB decides uniqueness, not a check-then-insert), get_binding is the
    // scheduler's lookup. No replace/CAS: a binding is never mutated once created (re-homing an
    // owner is a migration, not a scheduler decision — see scheduler.rs).
    async fn get_binding(&self, region: &str, owner: &str) -> Result<Option<Binding>, StoreErr>;
    async fn create_binding(&self, b: &Binding) -> Result<(), StoreErr>;
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
    async fn create_job(&self, j: &Job) -> Result<(), StoreErr>;
    async fn queued_jobs(&self, region: &str) -> Result<Vec<(Job, Etag)>, StoreErr>;
    async fn leased_jobs(&self, region: &str) -> Result<Vec<(Job, Etag)>, StoreErr>;
    async fn get_job(&self, region: &str, id: &str) -> Result<Option<(Job, Etag)>, StoreErr>;
    async fn replace_job(&self, j: &Job, etag: &Etag) -> Result<(), StoreErr>;
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
    agents: Mutex<HashMap<String, Versioned<AgentDoc>>>,
    // keyed by (owner, id) since that's the partition + id Cosmos would use.
    workspaces: Mutex<HashMap<(String, String), Versioned<Workspace>>>,
    snapshots: Mutex<HashMap<(String, String), Snapshot>>,
    environments: Mutex<HashMap<(String, String), Versioned<Environment>>>,
    jobs: Mutex<HashMap<(String, String), Versioned<Job>>>,
    bindings: Mutex<HashMap<(String, String), Binding>>,
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

    async fn upsert_agent(&self, a: &AgentDoc) -> Result<(), StoreErr> {
        let mut map = self.agents.lock().unwrap();
        match map.get_mut(&a.id) {
            Some(v) => {
                v.doc = a.clone();
                v.etag += 1;
            }
            None => {
                map.insert(a.id.clone(), Versioned::new(a.clone()));
            }
        }
        Ok(())
    }

    async fn agents_in(&self, region: &str) -> Result<Vec<AgentDoc>, StoreErr> {
        Ok(self
            .agents
            .lock()
            .unwrap()
            .values()
            .filter(|v| v.doc.region == region)
            .map(|v| v.doc.clone())
            .collect())
    }

    async fn get_binding(&self, region: &str, owner: &str) -> Result<Option<Binding>, StoreErr> {
        Ok(self.bindings.lock().unwrap().get(&(region.to_string(), owner.to_string())).cloned())
    }

    async fn create_binding(&self, b: &Binding) -> Result<(), StoreErr> {
        let mut map = self.bindings.lock().unwrap();
        let key = (b.region.clone(), b.id.clone());
        if map.contains_key(&key) {
            return Err(StoreErr::Conflict);
        }
        map.insert(key, b.clone());
        Ok(())
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

    async fn create_job(&self, j: &Job) -> Result<(), StoreErr> {
        let mut map = self.jobs.lock().unwrap();
        let key = (j.region.clone(), j.id.clone());
        if map.contains_key(&key) {
            return Err(StoreErr::Conflict);
        }
        map.insert(key, Versioned::new(j.clone()));
        Ok(())
    }

    async fn queued_jobs(&self, region: &str) -> Result<Vec<(Job, Etag)>, StoreErr> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .values()
            .filter(|v| v.doc.region == region && v.doc.state == JobState::Queued)
            .map(|v| (v.doc.clone(), v.etag.to_string()))
            .collect())
    }

    async fn leased_jobs(&self, region: &str) -> Result<Vec<(Job, Etag)>, StoreErr> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .values()
            .filter(|v| v.doc.region == region && v.doc.state == JobState::Leased)
            .map(|v| (v.doc.clone(), v.etag.to_string()))
            .collect())
    }

    async fn get_job(&self, region: &str, id: &str) -> Result<Option<(Job, Etag)>, StoreErr> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .get(&(region.to_string(), id.to_string()))
            .map(|v| (v.doc.clone(), v.etag.to_string())))
    }

    async fn replace_job(&self, j: &Job, etag: &Etag) -> Result<(), StoreErr> {
        let mut map = self.jobs.lock().unwrap();
        let key = (j.region.clone(), j.id.clone());
        let v = map.get_mut(&key).ok_or(StoreErr::NotFound)?;
        if v.etag.to_string() != *etag {
            return Err(StoreErr::CasFailed);
        }
        v.doc = j.clone();
        v.etag += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{JobKind, WsState};

    fn ws(owner: &str, id: &str) -> Workspace {
        Workspace {
            id: id.into(),
            owner: owner.into(),
            name: "web".into(),
            region: "centralindia".into(),
            state: WsState::Creating,
            image: "nginx:alpine".into(),
            placement: None,
            volume: None,
            quota_gb: 20,
            live_state: serde_json::Value::Null,
        }
    }

    fn job(region: &str, id: &str) -> Job {
        Job {
            id: id.into(),
            region: region.into(),
            agent: None,
            kind: JobKind::WsCreate,
            payload: serde_json::json!({"workspace": "ws-1"}),
            state: JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        }
    }

    #[tokio::test]
    async fn workspace_round_trip() {
        let store = MemStore::new();
        store.create_ws(&ws("karthik", "ws-1")).await.unwrap();
        let (got, etag) = store.get_ws("karthik", "ws-1").await.unwrap().unwrap();
        assert_eq!(got.id, "ws-1");
        assert_eq!(etag, "1");

        let mut updated = got.clone();
        updated.state = WsState::Ready;
        store.replace_ws(&updated, &etag).await.unwrap();
        let (got2, etag2) = store.get_ws("karthik", "ws-1").await.unwrap().unwrap();
        assert_eq!(got2.state, WsState::Ready);
        assert_eq!(etag2, "2");

        assert_eq!(store.list_ws("karthik").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn snapshot_round_trip() {
        let store = MemStore::new();
        let snap = Snapshot {
            id: "snap-1".into(),
            workspace_id: "ws-1".into(),
            lineage: vec![],
            created_at: chrono::Utc::now(),
            state: serde_json::Value::Null,
        };
        store.put_snapshot(&snap).await.unwrap();
        let got = store.get_snapshot("ws-1", "snap-1").await.unwrap().unwrap();
        assert_eq!(got.id, "snap-1");
    }

    #[tokio::test]
    async fn environment_round_trip() {
        let store = MemStore::new();
        let env = Environment {
            id: "env-1".into(),
            owner: "karthik".into(),
            name: "app-dev".into(),
            region: "centralindia".into(),
            state: crate::model::EnvState::Creating,
            placement: None,
            volume: None,
            services: vec![],
        };
        store.create_env(&env).await.unwrap();
        let (got, etag) = store.get_env("karthik", "env-1").await.unwrap().unwrap();
        assert_eq!(got.name, "app-dev");
        store.replace_env(&got, &etag).await.unwrap();
        assert_eq!(store.list_env("karthik").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn region_and_agent_round_trip() {
        let store = MemStore::new();
        store
            .put_region(&Region {
                id: "centralindia".into(),
                name: "Central India".into(),
                storage_account: "rusticgitkolomi".into(),
                blob_container: "wslayers".into(),
                status: "active".into(),
                agent_token: "tok-1".into(),
            })
            .await
            .unwrap();
        assert_eq!(store.regions().await.unwrap().len(), 1);

        store
            .upsert_agent(&AgentDoc {
                id: "agent-1".into(),
                region: "centralindia".into(),
                hostname: "vm-1".into(),
                pool: "/mnt/wspool".into(),
                capacity: crate::model::Capacity { cpu: 4, mem_mb: 16384, disk_gb: 128 },
                used: crate::model::Capacity { cpu: 0, mem_mb: 0, disk_gb: 0 },
                heartbeat_at: chrono::Utc::now(),
                status: "alive".into(),
            })
            .await
            .unwrap();
        assert_eq!(store.agents_in("centralindia").await.unwrap().len(), 1);
        assert_eq!(store.agents_in("other").await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn cas_first_replace_wins_second_fails() {
        let store = MemStore::new();
        store.create_job(&job("centralindia", "job-1")).await.unwrap();
        let (j1, etag) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        let j2 = j1.clone();

        let mut leased = j1;
        leased.state = JobState::Leased;
        store.replace_job(&leased, &etag).await.unwrap();

        let mut also_leased = j2;
        also_leased.state = JobState::Failed;
        let err = store.replace_job(&also_leased, &etag).await.unwrap_err();
        assert_eq!(err, StoreErr::CasFailed);
    }

    #[tokio::test]
    async fn queued_jobs_filters_by_region_and_state() {
        let store = MemStore::new();
        store.create_job(&job("centralindia", "job-1")).await.unwrap();
        store.create_job(&job("centralindia", "job-2")).await.unwrap();
        store.create_job(&job("other-region", "job-3")).await.unwrap();

        let (mut j2, etag2) = store.get_job("centralindia", "job-2").await.unwrap().unwrap();
        j2.state = JobState::Leased;
        store.replace_job(&j2, &etag2).await.unwrap();

        let queued = store.queued_jobs("centralindia").await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0.id, "job-1");

        let leased = store.leased_jobs("centralindia").await.unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].0.id, "job-2");
    }
}
