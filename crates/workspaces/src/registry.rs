//! Where a volume's snapshot history lives: the `vol/{owner}/{name}` registry namespace.
//!
//! One keyspace over from `rustic_git_registry::store`'s image pattern, and deliberately the same
//! shape: a volume gets its own SlateDB, opened through the same storage pool as repos and images
//! (`vol` joins `RESERVED_OWNERS` so no repo or image can collide with it), routed by the same
//! ownership middleware. Read-only in PRODUCTION — nothing writes a `SnapshotRecord` any more, the
//! write side (`vol_agent.rs`) went with the durable-snapshots cutover (see
//! `docs/superpowers/specs/2026-09-03-durable-snapshots-design.md`) — but `append_snapshots` stays
//! because `tests/browse_http.rs` (root `rustic-git-tests`, exercising `browse_api::volumes` —
//! FROZEN, keep-until-drained) has no other way to seed a pre-cutover row for the frozen read side
//! to serve.
//!
//! Keyspace: `commit/{id}` (on-disk prefix unchanged — real production rows already use it) -> a
//! `SnapshotRecord` (immutable once written — a snapshot is content-addressed by its own id, never
//! mutated).

use rustic_git_core::Result;
use rustic_git_storage::store::Store;
use slatedb::object_store::ObjectStoreExt;
use slatedb::Db;
use std::sync::Arc;

/// A volume snapshot: the full lineage from base to here (never another record — deleting any
/// snapshot can never break a descendant), the state captured at snapshot time, and where the
/// layer blobs it names actually live.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SnapshotRecord {
    pub id: String,
    /// Free-form: ports, installed packages, whatever the workspace/environment tracks. Absent on
    /// the wire deserializes to `null`, not a missing field error.
    #[serde(default)]
    pub state: serde_json::Value,
    /// Opaque now that the object-store lineage encoding (`LineageEntry`) is gone: nothing writes
    /// a new `SnapshotRecord` any more (the write side, `vol_agent.rs`, is deleted — see
    /// `docs/superpowers/specs/2026-09-03-durable-snapshots-design.md`), so
    /// this only ever carries an OLD record back out to a browse-API caller verbatim, never
    /// interpreted server-side. `serde_json::Value` round-trips whatever shape is already on disk.
    pub lineage: serde_json::Value,
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

/// The per-volume listing marker, `index/vol/{owner}/{name}`, touched by `append_snapshots`. Its
/// mtime is what `GET /api/{owner}/volumes` reports as `latest_ms`.
pub fn volume_marker(owner: &str, name: &str) -> slatedb::object_store::path::Path {
    slatedb::object_store::path::Path::from(format!("{}{name}", volume_marker_prefix(owner)))
}

/// The per-volume listing marker prefix, `index/vol/{owner}/`: one LIST in place of every SST and
/// WAL object of every volume's database under the owner.
pub fn volume_marker_prefix(owner: &str) -> String {
    format!("index/vol/{owner}/")
}

// on-disk prefix stays "commit/": real production rows already use it, and this is read-only.
const SNAPSHOT_PREFIX: &str = "commit/";
fn snapshot_key(id: &str) -> String {
    format!("{SNAPSHOT_PREFIX}{id}")
}

#[allow(async_fn_in_trait)]
/// `Store`'s volume-registry methods, as an extension trait for the same reason
/// `registry::store::ImageExt` is one: `Store` lives in the `storage` crate, and the orphan rule
/// forbids an inherent impl on a foreign type from here.
///
/// Read-only in production: nothing writes a `SnapshotRecord` any more (the write side,
/// `vol_agent.rs`, was deleted — durable-snapshots cutover) — `append_snapshots` only still exists
/// to seed the frozen `browse_api::volumes` read side in tests, see the module doc.
pub trait VolExt {
    async fn vol_db(&self, owner: &str, name: &str) -> Result<Arc<Db>>;
    /// Whether this volume's database exists, WITHOUT opening it.
    ///
    /// Opening CREATES: `Db::builder(...).build()` has no create-if-missing switch, so a read path
    /// that opens an unknown name brings a database into being — and a volume that exists on the
    /// object store is a volume the owner-scoped listing shows, forever, with no history behind
    /// it. Every user-facing read guards on this first. Same rule as `image_exists`/`repo_exists`.
    async fn vol_exists(&self, owner: &str, name: &str) -> Result<bool>;
    /// Appends a batch of snapshot records. Each `put` is independent (no `WriteBatch`): a partial
    /// append leaves every already-written record valid on its own — snapshots never reference
    /// each other, only their own lineage — so there is nothing for a batch to buy here.
    async fn append_snapshots(&self, owner: &str, name: &str, records: &[SnapshotRecord]) -> Result<()>;
    /// Every snapshot record, newest first.
    async fn history(&self, owner: &str, name: &str) -> Result<Vec<SnapshotRecord>>;
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

    async fn append_snapshots(&self, owner: &str, name: &str, records: &[SnapshotRecord]) -> Result<()> {
        let db = self.vol_db(owner, name).await?;
        for r in records {
            let bytes = serde_json::to_vec(r).map_err(|e| rustic_git_core::err(e.to_string()))?;
            db.put(snapshot_key(&r.id), bytes).await?;
        }
        // The listing marker, AFTER the records: `browse_api::volumes` reads its mtime as "last
        // pushed" without opening the database, which it may not do. A view for a listing, never
        // authorization — the same rule as every other `index/` key.
        self.os.put(&volume_marker(owner, name), slatedb::object_store::PutPayload::from_static(b"")).await?;
        Ok(())
    }

    async fn history(&self, owner: &str, name: &str) -> Result<Vec<SnapshotRecord>> {
        let db = self.vol_db(owner, name).await?;
        let mut it = db.scan_prefix(SNAPSHOT_PREFIX, ..).await?;
        let mut out = vec![];
        while let Some(kv) = it.next().await? {
            out.push(serde_json::from_slice::<SnapshotRecord>(&kv.value).map_err(|e| rustic_git_core::err(e.to_string()))?);
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
        assert_ne!(routing_key("alice", "web"), "img/alice/web");
        let key = routing_key("alice", "web");
        let (o, n) = key.split_once('/').unwrap();
        assert_eq!((o, n), ("vol", "alice/web"));
        assert_eq!(pool_coords("alice", "web"), ("vol", "alice/web".to_string()));
    }
}
