//! The leader lease: `cluster/leader`, one object next to the map it guards, written ONLY with
//! conditional puts. The object store is the arbiter — `PutMode::Create` when nothing is there,
//! `PutMode::Update(version)` over what was just read — so two candidates racing for an expired
//! lease are settled by the store's compare-and-swap, never by ordinal and never by clock. This is
//! the tree's first use of conditional writes, and every one of them lives in this file.
//!
//! Backend support, read from the vendored object_store 0.14.1: `InMemory` and Azure implement
//! both modes; S3 implements both unless `conditional_put` is `Disabled` (the default is
//! `ETagMatch`); `LocalFileSystem` implements `Create` but returns `NotImplemented` for `Update`,
//! which is why a multi-node `file://` fleet is refused at boot (`config::fleet_store_ok`).

use slatedb::object_store::{
    path::Path, Error as StoreError, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload,
    UpdateVersion,
};
use std::time::Duration;

pub const PATH: &str = "cluster/leader";
/// Same as the repo lease TTL: a dead leader is noticed on the clock a dead owner already is.
pub const LEADER_TTL: Duration = Duration::from_secs(10);
/// Three renewals per TTL, like `RENEW_EVERY`: a missed beat or two is not a lost lease.
pub const LEADER_RENEW: Duration = Duration::from_secs(3);

/// `{node}\n{epoch}\n{expires_ms}`. The epoch counts takeovers and rides on every map write as an
/// in-process fencing token (`App::writing_epoch`); `expires_ms` is the holder's own clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub node: String,
    pub epoch: u64,
    pub expires_ms: u64,
}

/// A lease as the store last handed it back, with the version the NEXT conditional write is
/// pinned to. A `Held` that has gone stale is exactly what the store refuses.
#[derive(Debug, Clone)]
pub struct Held {
    pub lease: Lease,
    pub version: UpdateVersion,
}

pub fn is_expired(l: &Lease, now_ms: u64) -> bool {
    now_ms >= l.expires_ms
}

impl Lease {
    fn encode(&self) -> Vec<u8> {
        assert!(!self.node.contains('\n'), "node name must not contain a newline: {}", self.node);
        format!("{}\n{}\n{}", self.node, self.epoch, self.expires_ms).into_bytes()
    }

    fn decode(bytes: &[u8]) -> crate::Result<Lease> {
        let s = std::str::from_utf8(bytes).map_err(|e| crate::err(format!("leader lease: {e}")))?;
        let mut it = s.split('\n');
        let (Some(node), Some(epoch), Some(expires_ms), None) = (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(crate::err(format!("leader lease: malformed: {s:?}")));
        };
        Ok(Lease {
            node: node.to_string(),
            epoch: epoch.parse().map_err(|e| crate::err(format!("leader lease: bad epoch: {e}")))?,
            expires_ms: expires_ms
                .parse()
                .map_err(|e| crate::err(format!("leader lease: bad expires_ms: {e}")))?,
        })
    }
}

pub async fn read(os: &dyn ObjectStore) -> crate::Result<Option<Held>> {
    match os.get(&Path::from(PATH)).await {
        Ok(r) => {
            // Both halves kept: stores differ in which of e_tag/version they condition on.
            let version = UpdateVersion { e_tag: r.meta.e_tag.clone(), version: r.meta.version.clone() };
            let lease = Lease::decode(&r.bytes().await?)?;
            Ok(Some(Held { lease, version }))
        }
        Err(StoreError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn put(os: &dyn ObjectStore, lease: &Lease, mode: PutMode) -> crate::Result<Option<Held>> {
    let opts = PutOptions { mode, ..Default::default() };
    match os.put_opts(&Path::from(PATH), PutPayload::from(lease.encode()), opts).await {
        Ok(r) => Ok(Some(Held { lease: lease.clone(), version: r.into() })),
        // Somebody's put landed between our read and this write. That is the store doing its one
        // job, not an error: the caller reads again and finds the winner.
        Err(StoreError::AlreadyExists { .. } | StoreError::Precondition { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Take the lease. `current` is what `read` returned a moment ago: absent means `Create`, present
/// means `Update` pinned to that version, with the epoch advanced. A LIVE lease held by somebody
/// else is never taken — that is what `LEADER_TTL` means — and the check is here, not in the
/// caller, so no caller can forget it.
pub async fn take(
    os: &dyn ObjectStore,
    node: &str,
    now_ms: u64,
    current: Option<&Held>,
) -> crate::Result<Option<Held>> {
    let (epoch, mode) = match current {
        None => (1, PutMode::Create),
        Some(c) if !is_expired(&c.lease, now_ms) && c.lease.node != node => return Ok(None),
        Some(c) => (c.lease.epoch + 1, PutMode::Update(c.version.clone())),
    };
    let lease = Lease { node: node.to_string(), epoch, expires_ms: now_ms + LEADER_TTL.as_millis() as u64 };
    put(os, &lease, mode).await
}

/// Extend a lease this node holds: same epoch, pinned to the version last read or written, so a
/// renewal that lands after somebody else took the lease is refused by the store rather than
/// trusted by us.
pub async fn renew(os: &dyn ObjectStore, held: &Held, now_ms: u64) -> crate::Result<Option<Held>> {
    let lease = Lease { expires_ms: now_ms + LEADER_TTL.as_millis() as u64, ..held.lease.clone() };
    put(os, &lease, PutMode::Update(held.version.clone())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::memory::InMemory;
    use std::sync::Arc;

    fn mem() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }
    const TTL: u64 = LEADER_TTL.as_millis() as u64;

    #[tokio::test]
    async fn create_wins_once() {
        let os = mem();
        let a = take(os.as_ref(), "rustic-git-srv-0", 1_000, None).await.unwrap().expect("first take wins");
        assert_eq!(a.lease, Lease { node: "rustic-git-srv-0".into(), epoch: 1, expires_ms: 1_000 + TTL });
        // A second candidate that read nothing (it raced the first) is refused by the store, not by us.
        assert!(take(os.as_ref(), "rustic-git-srv-1", 1_000, None).await.unwrap().is_none());
        assert_eq!(read(os.as_ref()).await.unwrap().unwrap().lease.node, "rustic-git-srv-0");
    }

    #[tokio::test]
    async fn a_live_lease_held_by_another_is_never_taken() {
        let os = mem();
        take(os.as_ref(), "rustic-git-srv-0", 1_000, None).await.unwrap().unwrap();
        let cur = read(os.as_ref()).await.unwrap();
        assert!(take(os.as_ref(), "rustic-git-srv-1", 1_000 + TTL - 1, cur.as_ref()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_with_the_next_epoch() {
        let os = mem();
        take(os.as_ref(), "rustic-git-srv-0", 1_000, None).await.unwrap().unwrap();
        let cur = read(os.as_ref()).await.unwrap();
        let b = take(os.as_ref(), "rustic-git-srv-1", 1_000 + TTL, cur.as_ref())
            .await
            .unwrap()
            .expect("expired: up for grabs");
        assert_eq!((b.lease.node.as_str(), b.lease.epoch), ("rustic-git-srv-1", 2));
    }

    /// The holder that missed its own beats finds the lease expired and naming itself. It may take
    /// it back, and the epoch still advances: a takeover is a takeover, whoever wins it.
    #[tokio::test]
    async fn the_holder_retakes_its_own_expired_lease() {
        let os = mem();
        take(os.as_ref(), "rustic-git-srv-0", 1_000, None).await.unwrap().unwrap();
        let cur = read(os.as_ref()).await.unwrap();
        let again = take(os.as_ref(), "rustic-git-srv-0", 1_000 + TTL, cur.as_ref()).await.unwrap().unwrap();
        assert_eq!(again.lease.epoch, 2);
    }

    #[tokio::test]
    async fn renew_with_a_stale_version_fails() {
        let os = mem();
        let a = take(os.as_ref(), "rustic-git-srv-0", 1_000, None).await.unwrap().unwrap();
        let cur = read(os.as_ref()).await.unwrap();
        let b = take(os.as_ref(), "rustic-git-srv-1", 1_000 + TTL, cur.as_ref()).await.unwrap().unwrap();
        assert!(renew(os.as_ref(), &a, 1_000 + TTL + 1).await.unwrap().is_none(), "the old holder's version is stale");
        let b2 = renew(os.as_ref(), &b, 1_000 + TTL + 1).await.unwrap().expect("the holder renews");
        assert_eq!(b2.lease.epoch, 2);
        assert_eq!(b2.lease.expires_ms, 1_000 + 2 * TTL + 1);
        // The renewed version is the one the NEXT renewal must carry; the one before it is stale now.
        assert!(renew(os.as_ref(), &b, 1_000 + TTL + 2).await.unwrap().is_none());
        assert!(renew(os.as_ref(), &b2, 1_000 + TTL + 2).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn concurrent_takers_exactly_one_wins() {
        let os = mem();
        let takers = (0..8).map(|i| {
            let os = os.clone();
            async move { take(os.as_ref(), &format!("rustic-git-srv-{i}"), 1_000, None).await.unwrap() }
        });
        let won: Vec<Held> = futures::future::join_all(takers).await.into_iter().flatten().collect();
        assert_eq!(won.len(), 1, "exactly one Create may land");
        assert_eq!(read(os.as_ref()).await.unwrap().unwrap().lease, won[0].lease);
    }

    #[test]
    fn a_lease_round_trips_and_a_malformed_one_is_refused() {
        let l = Lease { node: "n".into(), epoch: 7, expires_ms: 42 };
        assert_eq!(Lease::decode(&l.encode()).unwrap(), l);
        assert!(Lease::decode(b"n\n7").is_err());
        assert!(Lease::decode(b"n\nx\n42").is_err());
    }
}
