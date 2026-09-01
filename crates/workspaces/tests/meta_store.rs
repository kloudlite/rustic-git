//! The `MetaStore` contract, run against `MemStore` always and `CosmosStore` when `COSMOS_ENDPOINT`
//! and `COSMOS_KEY` are set (the same variables `bins/api` boots from). `MemStore` stands in for
//! Cosmos in every other test, so this is the one place its parity is asserted rather than assumed.

use rustic_git_workspaces::model::Region;
use rustic_git_workspaces::store::{MemStore, MetaStore};

fn region(id: &str, name: &str) -> Region {
    Region {
        id: id.into(),
        name: name.into(),
        storage_account: "acct".into(),
        blob_container: "blobs".into(),
        status: "active".into(),
    }
}

async fn contract(store: &dyn MetaStore) {
    store.put_region(&region("r1", "one")).await.unwrap();
    store.put_region(&region("r2", "two")).await.unwrap();
    // A second put of the same id is an upsert, not a duplicate.
    store.put_region(&region("r1", "one-renamed")).await.unwrap();
    let mut all = store.regions().await.unwrap();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["r1", "r2"]);
    assert_eq!(all[0].name, "one-renamed", "the later put wins");
    let (got, want) = (&all[1], region("r2", "two"));
    assert!(
        got.name == want.name && got.storage_account == want.storage_account && got.blob_container == want.blob_container
            && got.status == want.status,
        "every field round-trips"
    );
}

#[tokio::test]
async fn mem_store_honours_the_contract() {
    contract(&MemStore::new()).await;
}

#[tokio::test]
async fn cosmos_store_honours_the_contract() {
    let (Ok(endpoint), Ok(key)) = (std::env::var("COSMOS_ENDPOINT"), std::env::var("COSMOS_KEY")) else {
        eprintln!("skip: COSMOS_ENDPOINT/COSMOS_KEY not set");
        return;
    };
    // Its own database, dropped at the end, so a run never sees another run's regions.
    let db = format!("wstest-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis());
    let store = rustic_git_workspaces::cosmos::CosmosStore::new(&endpoint, &key, &db).await.unwrap();
    contract(&store).await;
    store.drop_database().await.unwrap();
}
