//! Cosmos DB implementation of `MetaStore`. Verified against the vendored source of
//! `azure_data_cosmos` 0.30 (~/.cargo/registry/src/*/azure_data_cosmos-0.30.0) rather than
//! guessed: key auth is `CosmosClient::with_key` (needs the `key_auth` feature, off by
//! default), CAS is `ItemOptions::if_match_etag` + the `IF_MATCH` header, and status codes
//! surface via `azure_core::Error::http_status()`.

use crate::model::{AgentDoc, Environment, Job, JobState, Region, Snapshot, Workspace};
use crate::store::{Etag, MetaStore, StoreErr};
use azure_core::credentials::Secret;
use azure_core::http::Etag as CosmosEtag;
use azure_core::http::StatusCode;
use azure_data_cosmos::clients::{ContainerClient, DatabaseClient};
use azure_data_cosmos::models::{ContainerProperties, PartitionKeyDefinition};
use azure_data_cosmos::{CosmosClient, ItemOptions, PartitionKey};
use serde::de::DeserializeOwned;
use serde::Deserialize;

fn map_err(e: azure_core::Error) -> StoreErr {
    match e.http_status() {
        Some(StatusCode::PreconditionFailed) => StoreErr::CasFailed,
        Some(StatusCode::NotFound) => StoreErr::NotFound,
        Some(StatusCode::Conflict) => StoreErr::Conflict,
        _ => StoreErr::Other(e.to_string()),
    }
}

// Cosmos stores `_etag` as a sibling of the document body; our model types don't carry it, so
// reads/queries that need the etag deserialize into this envelope instead.
#[derive(Deserialize)]
struct WithEtag<T> {
    #[serde(flatten)]
    doc: T,
    #[serde(rename = "_etag")]
    etag: String,
}

pub struct CosmosStore {
    db: DatabaseClient,
    regions: ContainerClient,
    agents: ContainerClient,
    workspaces: ContainerClient,
    snapshots: ContainerClient,
    environments: ContainerClient,
    jobs: ContainerClient,
}

impl CosmosStore {
    pub async fn new(endpoint: &str, key: &str, database: &str) -> Result<Self, StoreErr> {
        let client = CosmosClient::with_key(endpoint, Secret::from(key.to_string()), None)
            .map_err(map_err)?;

        match client.create_database(database, None).await {
            Ok(_) => {}
            Err(e) if e.http_status() == Some(StatusCode::Conflict) => {}
            Err(e) => return Err(map_err(e)),
        }
        let db = client.database_client(database);

        for (name, pk) in [
            ("regions", "/id"),
            ("agents", "/region"),
            ("workspaces", "/owner"),
            ("snapshots", "/workspace_id"),
            ("environments", "/owner"),
            ("jobs", "/region"),
        ] {
            create_container_if_not_exists(&db, name, pk).await?;
        }

        Ok(CosmosStore {
            regions: db.container_client("regions"),
            agents: db.container_client("agents"),
            workspaces: db.container_client("workspaces"),
            snapshots: db.container_client("snapshots"),
            environments: db.container_client("environments"),
            jobs: db.container_client("jobs"),
            db,
        })
    }

    /// Drops the underlying database. Used by tests to clean up `wstest-{uuid}` databases.
    pub async fn drop_database(&self) -> Result<(), StoreErr> {
        self.db.delete(None).await.map_err(map_err)?;
        Ok(())
    }
}

async fn create_container_if_not_exists(
    db: &DatabaseClient,
    id: &str,
    partition_key_path: &str,
) -> Result<(), StoreErr> {
    let properties = ContainerProperties {
        id: id.to_string().into(),
        partition_key: PartitionKeyDefinition::from(partition_key_path.to_string()),
        ..Default::default()
    };
    match db.create_container(properties, None).await {
        Ok(_) => Ok(()),
        Err(e) if e.http_status() == Some(StatusCode::Conflict) => Ok(()),
        Err(e) => Err(map_err(e)),
    }
}

async fn read_item<T: DeserializeOwned>(
    container: &ContainerClient,
    partition_key: &str,
    id: &str,
) -> Result<Option<T>, StoreErr> {
    match container
        .read_item::<T>(partition_key.to_string(), id, None)
        .await
    {
        Ok(resp) => Ok(Some(resp.into_model().map_err(map_err)?)),
        Err(e) if e.http_status() == Some(StatusCode::NotFound) => Ok(None),
        Err(e) => Err(map_err(e)),
    }
}

async fn query_items<T: DeserializeOwned + Send + 'static>(
    container: &ContainerClient,
    query: &str,
    partition_key: PartitionKey,
) -> Result<Vec<T>, StoreErr> {
    use futures::TryStreamExt as _;
    let pager = container
        .query_items::<T>(query, partition_key, None)
        .map_err(map_err)?;
    pager.try_collect().await.map_err(map_err)
}

#[async_trait::async_trait]
impl MetaStore for CosmosStore {
    async fn put_region(&self, r: &Region) -> Result<(), StoreErr> {
        self.regions
            .upsert_item(r.id.clone(), r.clone(), None)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn regions(&self) -> Result<Vec<Region>, StoreErr> {
        query_items(&self.regions, "SELECT * FROM c", PartitionKey::EMPTY).await
    }

    async fn upsert_agent(&self, a: &AgentDoc) -> Result<(), StoreErr> {
        self.agents
            .upsert_item(a.region.clone(), a.clone(), None)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn agents_in(&self, region: &str) -> Result<Vec<AgentDoc>, StoreErr> {
        query_items(&self.agents, "SELECT * FROM c", region.to_string().into()).await
    }

    async fn create_ws(&self, w: &Workspace) -> Result<(), StoreErr> {
        self.workspaces
            .create_item(w.owner.clone(), w.clone(), None)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn get_ws(&self, owner: &str, id: &str) -> Result<Option<(Workspace, Etag)>, StoreErr> {
        let got: Option<WithEtag<Workspace>> = read_item(&self.workspaces, owner, id).await?;
        Ok(got.map(|v| (v.doc, v.etag)))
    }

    async fn replace_ws(&self, w: &Workspace, etag: &Etag) -> Result<(), StoreErr> {
        let options = ItemOptions {
            if_match_etag: Some(CosmosEtag::from(etag.as_str())),
            ..Default::default()
        };
        self.workspaces
            .replace_item(w.owner.clone(), &w.id, w.clone(), Some(options))
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn list_ws(&self, owner: &str) -> Result<Vec<Workspace>, StoreErr> {
        query_items(&self.workspaces, "SELECT * FROM c", owner.to_string().into()).await
    }

    async fn put_snapshot(&self, s: &Snapshot) -> Result<(), StoreErr> {
        self.snapshots
            .upsert_item(s.workspace_id.clone(), s.clone(), None)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn get_snapshot(&self, ws: &str, id: &str) -> Result<Option<Snapshot>, StoreErr> {
        read_item(&self.snapshots, ws, id).await
    }

    async fn create_env(&self, e: &Environment) -> Result<(), StoreErr> {
        self.environments
            .create_item(e.owner.clone(), e.clone(), None)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn get_env(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<Option<(Environment, Etag)>, StoreErr> {
        let got: Option<WithEtag<Environment>> = read_item(&self.environments, owner, id).await?;
        Ok(got.map(|v| (v.doc, v.etag)))
    }

    async fn replace_env(&self, e: &Environment, etag: &Etag) -> Result<(), StoreErr> {
        let options = ItemOptions {
            if_match_etag: Some(CosmosEtag::from(etag.as_str())),
            ..Default::default()
        };
        self.environments
            .replace_item(e.owner.clone(), &e.id, e.clone(), Some(options))
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn list_env(&self, owner: &str) -> Result<Vec<Environment>, StoreErr> {
        query_items(&self.environments, "SELECT * FROM c", owner.to_string().into()).await
    }

    async fn create_job(&self, j: &Job) -> Result<(), StoreErr> {
        self.jobs
            .create_item(j.region.clone(), j.clone(), None)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn queued_jobs(&self, region: &str) -> Result<Vec<(Job, Etag)>, StoreErr> {
        let items: Vec<WithEtag<Job>> = query_items(
            &self.jobs,
            "SELECT * FROM c WHERE c.state = 'queued'",
            region.to_string().into(),
        )
        .await?;
        Ok(items.into_iter().map(|v| (v.doc, v.etag)).collect())
    }

    async fn leased_jobs(&self, region: &str) -> Result<Vec<(Job, Etag)>, StoreErr> {
        let items: Vec<WithEtag<Job>> = query_items(
            &self.jobs,
            "SELECT * FROM c WHERE c.state = 'leased'",
            region.to_string().into(),
        )
        .await?;
        Ok(items.into_iter().map(|v| (v.doc, v.etag)).collect())
    }

    async fn get_job(&self, region: &str, id: &str) -> Result<Option<(Job, Etag)>, StoreErr> {
        let got: Option<WithEtag<Job>> = read_item(&self.jobs, region, id).await?;
        Ok(got.map(|v| (v.doc, v.etag)))
    }

    async fn replace_job(&self, j: &Job, etag: &Etag) -> Result<(), StoreErr> {
        let options = ItemOptions {
            if_match_etag: Some(CosmosEtag::from(etag.as_str())),
            ..Default::default()
        };
        self.jobs
            .replace_item(j.region.clone(), &j.id, j.clone(), Some(options))
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

// Keep JobState's serde repr (lowercase) in sync with the literal used in the queued/leased
// queries above; this is a compile-time nudge, not a runtime check.
const _: fn() -> JobState = || JobState::Queued;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Capacity, EnvState, JobKind, WsState};

    fn cosmos_env() -> Option<(String, String)> {
        let endpoint = std::env::var("COSMOS_ENDPOINT").ok()?;
        let key = std::env::var("COSMOS_KEY").ok()?;
        Some((endpoint, key))
    }

    async fn test_store() -> Option<(CosmosStore, String)> {
        let (endpoint, key) = cosmos_env()?;
        let db_name = format!("wstest-{}", uuid_v4());
        let store = CosmosStore::new(&endpoint, &key, &db_name)
            .await
            .expect("create cosmos store");
        Some((store, db_name))
    }

    // Avoids pulling in a `uuid` crate dependency just for test database names.
    fn uuid_v4() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos:x}-{:x}", std::process::id())
    }

    fn ws(owner: &str, id: &str) -> Workspace {
        Workspace {
            id: id.into(),
            owner: owner.into(),
            name: "web".into(),
            region: "centralindia".into(),
            state: WsState::Creating,
            placement: None,
            ref_: None,
            quota_gb: 20,
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
        let Some((store, db)) = test_store().await else {
            println!("skipped: no cosmos env");
            return;
        };
        store.create_ws(&ws("karthik", "ws-1")).await.unwrap();
        let (got, etag) = store.get_ws("karthik", "ws-1").await.unwrap().unwrap();
        assert_eq!(got.id, "ws-1");

        let mut updated = got.clone();
        updated.state = WsState::Ready;
        store.replace_ws(&updated, &etag).await.unwrap();
        let (got2, _etag2) = store.get_ws("karthik", "ws-1").await.unwrap().unwrap();
        assert_eq!(got2.state, WsState::Ready);

        assert_eq!(store.list_ws("karthik").await.unwrap().len(), 1);
        let _ = db;
        store.drop_database().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_round_trip() {
        let Some((store, _db)) = test_store().await else {
            println!("skipped: no cosmos env");
            return;
        };
        let snap = Snapshot {
            id: "snap-1".into(),
            workspace_id: "ws-1".into(),
            lineage: vec![],
            created_at: chrono::Utc::now(),
        };
        store.put_snapshot(&snap).await.unwrap();
        let got = store.get_snapshot("ws-1", "snap-1").await.unwrap().unwrap();
        assert_eq!(got.id, "snap-1");
        store.drop_database().await.unwrap();
    }

    #[tokio::test]
    async fn environment_round_trip() {
        let Some((store, _db)) = test_store().await else {
            println!("skipped: no cosmos env");
            return;
        };
        let env = Environment {
            id: "env-1".into(),
            owner: "karthik".into(),
            name: "app-dev".into(),
            region: "centralindia".into(),
            state: EnvState::Creating,
            placement: None,
            services: vec![],
        };
        store.create_env(&env).await.unwrap();
        let (got, etag) = store.get_env("karthik", "env-1").await.unwrap().unwrap();
        assert_eq!(got.name, "app-dev");
        store.replace_env(&got, &etag).await.unwrap();
        assert_eq!(store.list_env("karthik").await.unwrap().len(), 1);
        store.drop_database().await.unwrap();
    }

    #[tokio::test]
    async fn region_and_agent_round_trip() {
        let Some((store, _db)) = test_store().await else {
            println!("skipped: no cosmos env");
            return;
        };
        store
            .put_region(&Region {
                id: "centralindia".into(),
                name: "Central India".into(),
                storage_account: "rusticgitkolomi".into(),
                blob_container: "wslayers".into(),
                status: "active".into(),
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
                capacity: Capacity { cpu: 4, mem_mb: 16384, disk_gb: 128 },
                used: Capacity { cpu: 0, mem_mb: 0, disk_gb: 0 },
                heartbeat_at: chrono::Utc::now(),
                status: "alive".into(),
            })
            .await
            .unwrap();
        assert_eq!(store.agents_in("centralindia").await.unwrap().len(), 1);
        assert_eq!(store.agents_in("other").await.unwrap().len(), 0);
        store.drop_database().await.unwrap();
    }

    #[tokio::test]
    async fn cas_first_replace_wins_second_fails() {
        let Some((store, _db)) = test_store().await else {
            println!("skipped: no cosmos env");
            return;
        };
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
        store.drop_database().await.unwrap();
    }

    #[tokio::test]
    async fn queued_jobs_filters_by_region_and_state() {
        let Some((store, _db)) = test_store().await else {
            println!("skipped: no cosmos env");
            return;
        };
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
        store.drop_database().await.unwrap();
    }
}
