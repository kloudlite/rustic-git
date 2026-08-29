//! Cosmos DB implementation of `MetaStore`. Verified against the vendored source of
//! `azure_data_cosmos` 0.30 (~/.cargo/registry/src/*/azure_data_cosmos-0.30.0) rather than
//! guessed: key auth is `CosmosClient::with_key` (needs the `key_auth` feature, off by
//! default), CAS is `ItemOptions::if_match_etag` + the `IF_MATCH` header, and status codes
//! surface via `azure_core::Error::http_status()`.

use crate::model::Region;
use crate::store::{MetaStore, StoreErr};
use azure_core::credentials::Secret;
use azure_core::http::StatusCode;
use azure_data_cosmos::clients::{ContainerClient, DatabaseClient};
use azure_data_cosmos::models::{ContainerProperties, PartitionKeyDefinition};
use azure_data_cosmos::{CosmosClient, PartitionKey};

fn map_err(e: azure_core::Error) -> StoreErr {
    StoreErr::Other(e.to_string())
}

pub struct CosmosStore {
    db: DatabaseClient,
    regions: ContainerClient,
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

        create_container_if_not_exists(&db, "regions", "/id").await?;

        Ok(CosmosStore { regions: db.container_client("regions"), db })
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
        use futures::TryStreamExt as _;
        self.regions
            .query_items::<Region>("SELECT * FROM c", PartitionKey::EMPTY, None)
            .map_err(map_err)?
            .try_collect()
            .await
            .map_err(map_err)
    }
}

