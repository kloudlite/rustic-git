//! Cosmos DB implementation of `MetaStore`. Verified against the vendored source of
//! `azure_data_cosmos` 0.30 (~/.cargo/registry/src/*/azure_data_cosmos-0.30.0) rather than
//! guessed: key auth is `CosmosClient::with_key` (needs the `key_auth` feature, off by
//! default), CAS is `ItemOptions::if_match_etag` + the `IF_MATCH` header, and status codes
//! surface via `azure_core::Error::http_status()`.

use crate::model::{Region, Snapshot, Workspace};
use crate::store::{Etag, MetaStore, StoreErr};
use azure_core::credentials::Secret;
use azure_core::http::StatusCode;
use azure_data_cosmos::clients::{ContainerClient, DatabaseClient};
use azure_data_cosmos::models::{ContainerProperties, PartitionKeyDefinition};
use azure_data_cosmos::{CosmosClient, PartitionKey};
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
    workspaces: ContainerClient,
    snapshots: ContainerClient,
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
            ("workspaces", "/owner"),
            ("snapshots", "/workspace_id"),
        ] {
            create_container_if_not_exists(&db, name, pk).await?;
        }

        Ok(CosmosStore {
            regions: db.container_client("regions"),
            workspaces: db.container_client("workspaces"),
            snapshots: db.container_client("snapshots"),
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



    async fn put_snapshot(&self, s: &Snapshot) -> Result<(), StoreErr> {
        self.snapshots
            .upsert_item(s.workspace_id.clone(), s.clone(), None)
            .await
            .map_err(map_err)?;
        Ok(())
    }










}

