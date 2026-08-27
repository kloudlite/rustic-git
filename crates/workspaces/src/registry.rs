//! Where a volume's commit history lives: the `vol/{owner}/{name}` registry namespace.
//!
//! One keyspace over from `rustic_git_registry::store`'s image pattern, and deliberately the same
//! shape: a volume gets its own SlateDB, opened through the same storage pool as repos and images
//! (`vol` joins `RESERVED_OWNERS` so no repo or image can collide with it), routed by the same
//! ownership middleware. Single-writer-per-database is what gives `move_ref` its CAS for free —
//! two concurrent pushes racing to move `main` still order against each other because only one
//! node ever holds the database open.
//!
//! Keyspaces: `commit/{id}` -> a `CommitRecord` (immutable once written — a commit is content-
//! addressed by its own id, never mutated), `ref/{name}` -> the commit id it currently names.

use crate::model::LineageEntry;
use rustic_git_core::Result;
use rustic_git_storage::store::Store;
use slatedb::Db;
use std::sync::Arc;

/// A volume commit: the full lineage from base to here (never another record — deleting any
/// commit can never break a descendant), the state captured at commit time, and where the layer
/// blobs it names actually live.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CommitRecord {
    pub id: String,
    /// Free-form: ports, installed packages, whatever the workspace/environment tracks. Absent on
    /// the wire deserializes to `null`, not a missing field error.
    #[serde(default)]
    pub state: serde_json::Value,
    pub lineage: Vec<LineageEntry>,
    /// Where the layer blobs this record names live. Bytes never cross regions; only this label
    /// does.
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// The ownership-map key for a volume. Mirrors `registry::routing_key`: `vol/` is a prefix no
/// repo or image route can produce, and `vol` is a reserved owner name so no repo key begins with
/// it either.
pub fn routing_key(owner: &str, name: &str) -> String {
    format!("vol/{owner}/{name}")
}

pub fn pool_coords(owner: &str, name: &str) -> (&'static str, String) {
    ("vol", format!("{owner}/{name}"))
}

const COMMIT_PREFIX: &str = "commit/";
/// The region that owns this volume, stamped by its first append and never rewritten.
///
/// It exists so the record routes can scope an agent token to the volume it is writing. Every
/// `CommitRecord` already carries a region, but answering "whose volume is this?" from the records
/// would mean reading history on every request; this is one point read.
const REGION_KEY: &str = "meta/region";
const REF_PREFIX: &str = "ref/";
fn commit_key(id: &str) -> String {
    format!("{COMMIT_PREFIX}{id}")
}
fn ref_key(name: &str) -> String {
    format!("{REF_PREFIX}{name}")
}

#[allow(async_fn_in_trait)]
/// `Store`'s volume-registry methods, as an extension trait for the same reason
/// `registry::store::ImageExt` is one: `Store` lives in the `storage` crate, and the orphan rule
/// forbids an inherent impl on a foreign type from here.
pub trait VolExt {
    async fn vol_db(&self, owner: &str, name: &str) -> Result<Arc<Db>>;
    /// Whether this volume's database exists, WITHOUT opening it.
    ///
    /// Opening CREATES: `Db::builder(...).build()` has no create-if-missing switch, so a read path
    /// that opens an unknown name brings a database into being — and a volume that exists on the
    /// object store is a volume the owner-scoped listing shows, forever, with no history behind
    /// it. Every user-facing read guards on this first. Same rule as `image_exists`/`repo_exists`.
    async fn vol_exists(&self, owner: &str, name: &str) -> Result<bool>;
    /// Appends a batch of commit records. Each `put` is independent (no `WriteBatch`): a partial
    /// append leaves every already-written record valid on its own — commits never reference each
    /// other, only their own lineage — so there is nothing for a batch to buy here.
    async fn append_commits(&self, owner: &str, name: &str, records: &[CommitRecord]) -> Result<()>;
    /// Moves `ref_name` to `commit`, refusing an unknown commit id (the caller answers 404/409;
    /// this just reports `false`).
    async fn move_ref(&self, owner: &str, name: &str, ref_name: &str, commit: &str) -> Result<bool>;
    async fn ref_commit(&self, owner: &str, name: &str, ref_name: &str) -> Result<Option<String>>;
    async fn commit(&self, owner: &str, name: &str, id: &str) -> Result<Option<CommitRecord>>;
    /// Every commit record, newest first.
    async fn history(&self, owner: &str, name: &str) -> Result<Vec<CommitRecord>>;
    /// The region that owns this volume, or `None` if nothing has been written to it yet.
    async fn region(&self, owner: &str, name: &str) -> Result<Option<String>>;
}

impl VolExt for Store {
    async fn vol_db(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        let (o, n) = pool_coords(owner, name);
        self.pool.get(o, &n).await
    }

    async fn vol_exists(&self, owner: &str, name: &str) -> Result<bool> {
        let (o, n) = pool_coords(owner, name);
        self.pool.exists(o, &n).await
    }

    async fn append_commits(&self, owner: &str, name: &str, records: &[CommitRecord]) -> Result<()> {
        let db = self.vol_db(owner, name).await?;
        // Claim the volume for its region on the first record ever written, and never rewrite it:
        // the stamp is what later requests are checked against, so a writer that could overwrite it
        // could also hand the volume to itself.
        if let Some(first) = records.first() {
            if !first.region.is_empty() && db.get(REGION_KEY).await?.is_none() {
                db.put(REGION_KEY, first.region.as_bytes().to_vec()).await?;
            }
        }
        for r in records {
            let bytes = serde_json::to_vec(r).map_err(|e| rustic_git_core::err(e.to_string()))?;
            db.put(commit_key(&r.id), bytes).await?;
        }
        Ok(())
    }

    async fn region(&self, owner: &str, name: &str) -> Result<Option<String>> {
        let db = self.vol_db(owner, name).await?;
        Ok(db
            .get(REGION_KEY)
            .await?
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .filter(|s| !s.is_empty()))
    }

    async fn move_ref(&self, owner: &str, name: &str, ref_name: &str, commit: &str) -> Result<bool> {
        let db = self.vol_db(owner, name).await?;
        if db.get(commit_key(commit)).await?.is_none() {
            return Ok(false);
        }
        db.put(ref_key(ref_name), commit.as_bytes().to_vec()).await?;
        Ok(true)
    }

    async fn ref_commit(&self, owner: &str, name: &str, ref_name: &str) -> Result<Option<String>> {
        let db = self.vol_db(owner, name).await?;
        Ok(db.get(ref_key(ref_name)).await?.map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    async fn commit(&self, owner: &str, name: &str, id: &str) -> Result<Option<CommitRecord>> {
        let db = self.vol_db(owner, name).await?;
        let Some(v) = db.get(commit_key(id)).await? else { return Ok(None) };
        Ok(Some(serde_json::from_slice(&v).map_err(|e| rustic_git_core::err(e.to_string()))?))
    }

    async fn history(&self, owner: &str, name: &str) -> Result<Vec<CommitRecord>> {
        let db = self.vol_db(owner, name).await?;
        let mut it = db.scan_prefix(COMMIT_PREFIX, ..).await?;
        let mut out = vec![];
        while let Some(kv) = it.next().await? {
            out.push(serde_json::from_slice::<CommitRecord>(&kv.value).map_err(|e| rustic_git_core::err(e.to_string()))?);
        }
        // `scan_prefix` yields ascending key order, i.e. insertion order by id, not by time — sort
        // by `created_at` and reverse so "newest first" holds even if ids do not sort that way.
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_cannot_collide_with_a_repo_or_an_image() {
        assert_eq!(routing_key("alice", "web"), "vol/alice/web");
        assert_ne!(routing_key("alice", "web"), "alice/web");
        assert_ne!(routing_key("alice", "web"), format!("img/alice/web"));
        let key = routing_key("alice", "web");
        let (o, n) = key.split_once('/').unwrap();
        assert_eq!((o, n), ("vol", "alice/web"));
        assert_eq!(pool_coords("alice", "web"), ("vol", "alice/web".to_string()));
    }
}
