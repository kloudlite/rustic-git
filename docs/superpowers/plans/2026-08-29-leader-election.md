# Leader election Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Any `kloudlite-git-srv` pod may write the ownership map: the writer is whoever holds a lease at `cluster/leader`, taken and renewed with conditional puts, so a dead leader is replaced in ~15 s with no operator and no dedicated pod.

**Architecture:** One new module, `ownership::lease` (`take`/`renew`/`read` over `Arc<dyn ObjectStore>` with `PutMode::Create`/`Update(version)`), and one election loop on `App` (`election_tick`, every `LEADER_RENEW`) that promotes the pod to the map's writer (`OwnershipStore::promote`) when it wins and demotes it (`OwnershipStore::demote`) when it loses, is refused a renewal, or hits a SlateDB fence on a map write. `is_leader()` becomes "I hold an epoch", every map write checks that epoch under `leader_lock`, followers re-read the lease when the node they asked answers 421 or is unreachable, and `/healthz` gates on "a live leader exists". The name-based leader (`leader_of`, `servers()`, `with_topology`, `KLOUDLITE_GIT_LEADER`, the `kloudlite-git-leader` StatefulSet and its PDB) is deleted.

**Tech Stack:** Rust; `object_store` 0.14.1 (via `slatedb::object_store`) conditional puts; SlateDB 0.15 writer fencing; tokio; the existing fleet harness in `tests/routing.rs`; kubectl manifests in `deploy/`.

**Spec:** `docs/superpowers/specs/2026-08-29-leader-election-design.md`

## Global Constraints

- **Single writer for the ownership map, always.** SlateDB's writer fence (`Closed(Fenced)`, `pool::is_fenced`) is the backstop; the lease epoch check in `App::writing_epoch` is in-process and runs under `leader_lock`, the same lock every grant holds, so a demotion can never interleave with a read-decide-write.
- **`App::route`'s invariant is untouched:** a node never serves on a failed claim unless it already held the repo and the repo is warm. No line of `route()` changes except one comment.
- **Followers keep `DbReader::open(FollowLatest)`**, opened lazily as today (`open_reader`); a change of writer is only a newer manifest to follow.
- **The checkpoint beat runs on whoever writes.** `OwnershipStore::checkpoint` stays a no-op on a reader; the beat in `lanes.rs` keeps running everywhere. The prune beat is gated per beat on `is_leader()`.
- **`file://` cannot host a fleet.** `LocalFileSystem` has `PutMode::Create` but returns `NotImplemented` for `PutMode::Update` (object_store 0.14.1 `src/local.rs:399`), so `config::fleet_store_ok` refuses `KLOUDLITE_GIT_S3_URL=file://…` when `KLOUDLITE_GIT_PEER_SVC` is set. Solo mode (no peer Service) never touches the lease.
- **No new crates.** `object_store` 0.14.1 has everything: `PutMode`, `PutOptions`, `UpdateVersion`, `Error::{AlreadyExists, Precondition, NotFound}`. Conditional-put support per backend, read from the vendored source: `InMemory` Create + Update (e_tag); `LocalFileSystem` Create only; `AmazonS3` Create + Update when `conditional_put != Disabled` (default `ETagMatch`); `MicrosoftAzure` Create (`If-None-Match: *`) + Update (`If-Match`). Production is `az://`; tests are `mem://`.
- **Lease constants fixed by the spec:** object `cluster/leader`, body `{node}\n{epoch}\n{expires_ms}`, `LEADER_TTL = 10 s`, `LEADER_RENEW = 3 s`. Ties are broken by the store, never by ordinal.
- **`claim_gate`, `recovery_asked`, `Patience`, `CLAIM_ATTEMPTS`/`CLAIM_BACKOFF`, `FORCE_MIN_AGE` unchanged.**
- **Clippy clean:** `cargo clippy --workspace --all-targets -- -D warnings` introduces no NEW warnings in files you touch (CI gates `cargo clippy --workspace -- -D warnings`). `cargo test --workspace --locked` green after every task.
- **Comments explain WHY, never what**; match the density of `bins/server/src/router/route.rs`. Keep every `// ponytail:` marker you edit near.
- **Commit subjects are imperative sentence case with no tool attribution.** No `Co-Authored-By`, no "Generated with".
- **Every deploy manifest parses** (`ruby -ryaml -e 'YAML.load_stream(File.read(ARGV[0])) {}' <file>`), `bash -n` on every script.
- **Migration order** — roll `kloudlite-git-srv` to the new build BEFORE deleting the leader StatefulSet — is written into `deploy/RECOVERY.md` in Task 8 (the last task) and in that task's commit body.
- **Two things deliberately NOT done** (each is a one-commit follow-up, named so nobody hunts for them): the leader does not resign its lease at SIGTERM (a graceful roll therefore waits out `LEADER_TTL` like a crash does — add `Update(version)` with `expires_ms = now` in the SIGTERM path when roll timing matters); and the `draining` announcement (`own_draining`, `set_draining`) keeps being written though nothing reads it once `least_loaded` is gone — delete the protocol in its own commit.

---

## File map

| File | Responsibility in this plan |
|---|---|
| `crates/storage/src/ownership/lease.rs` (new) | `Lease`, `Held`, `read`/`take`/`renew`, `LEADER_TTL`, `LEADER_RENEW`, `PATH`; unit tests on `InMemory`. |
| `crates/storage/src/config.rs` | `fleet_store_ok(url)`. |
| `crates/storage/src/ownership/mod.rs` | `OwnershipStore` becomes a struct with a runtime `Role` (`Solo`/`Reader`/`Writer`), `open(os)`, `solo()`, `promote`, `demote`, `is_writer`, `object_store`, `is_solo`; `leader_of`, `servers`, `least_loaded` deleted. |
| `crates/storage/src/ownership/tests.rs` | Tests for promote/demote/fence; deleted tests for the deleted functions. |
| `crates/app/src/lib.rs` | `election_tick`, `promote`, `demote`, `refresh_leader`, `leader_live`, `leader_epoch`, `set_leader`; `grant_*` epoch check and fence→demote; `ask_leader_with` re-reads on 421/connect failure; `with_topology`, `leader_name`, `server_prefix`, `replicas`, `LEADER_SILENCE`, `mark_leader_seen`, `leader_reachable` deleted. |
| `bins/server/src/router/route.rs` | `/healthz` on `leader_live`; `/own/*` answer 421 when the grant left the node demoted. |
| `bins/server/src/lanes.rs` | Election beat; prune gated on `is_leader()`. |
| `bins/server/src/main.rs` | `OwnershipStore::open(os)`, `fleet_store_ok`, first tick at boot; `KLOUDLITE_GIT_LEADER`/`SERVER_PREFIX`/`REPLICAS` gone. |
| `tests/routing.rs`, `tests/common/mod.rs`, `tests/ownership.rs`, `crates/workspaces/tests/engine_ops.rs` | Harness elects; three new fleet tests. |
| `deploy/kloudlite-git.yaml`, `deploy/kloudlite-git-leader.yaml` (deleted), `deploy/roll.sh`, `deploy/pin.sh`, `deploy/RECOVERY.md`, `deploy/alerts.md`, `deploy/BACKUPS.md`, `deploy/k3s/README.md`, `CLAUDE.md`, `README.md` | Deploy and docs. |

---

### Task 1: `ownership::lease` — the conditional-put helper, and the `file://` refusal

**Files:**
- Create: `crates/storage/src/ownership/lease.rs`
- Modify: `crates/storage/src/ownership/mod.rs` (add `pub mod lease;` next to `mod tests;`)
- Modify: `crates/storage/src/config.rs` (add `fleet_store_ok` + test)

**Interfaces:**
- Consumes: `slatedb::object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion, Error, path::Path}`; `crate::{Result, err}`.
- Produces:
  - `pub const PATH: &str = "cluster/leader"`, `pub const LEADER_TTL: Duration`, `pub const LEADER_RENEW: Duration`
  - `pub struct Lease { pub node: String, pub epoch: u64, pub expires_ms: u64 }` (`Debug, Clone, PartialEq, Eq`)
  - `pub struct Held { pub lease: Lease, pub version: UpdateVersion }` (`Debug, Clone`)
  - `pub fn is_expired(l: &Lease, now_ms: u64) -> bool`
  - `pub async fn read(os: &dyn ObjectStore) -> Result<Option<Held>>`
  - `pub async fn take(os: &dyn ObjectStore, node: &str, now_ms: u64, current: Option<&Held>) -> Result<Option<Held>>` — `Ok(None)` = lost the race or the lease is live and somebody else's
  - `pub async fn renew(os: &dyn ObjectStore, held: &Held, now_ms: u64) -> Result<Option<Held>>` — `Ok(None)` = the store refused (stale version)
  - `pub fn config::fleet_store_ok(url: &str) -> Result<()>`

- [ ] **Step 1: Write the failing tests**

Create `crates/storage/src/ownership/lease.rs` with only the test module for now (the items it names do not exist yet):

```rust
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
        let a = take(os.as_ref(), "kloudlite-git-srv-0", 1_000, None).await.unwrap().expect("first take wins");
        assert_eq!(a.lease, Lease { node: "kloudlite-git-srv-0".into(), epoch: 1, expires_ms: 1_000 + TTL });
        // A second candidate that read nothing (it raced the first) is refused by the store, not by us.
        assert!(take(os.as_ref(), "kloudlite-git-srv-1", 1_000, None).await.unwrap().is_none());
        assert_eq!(read(os.as_ref()).await.unwrap().unwrap().lease.node, "kloudlite-git-srv-0");
    }

    #[tokio::test]
    async fn a_live_lease_held_by_another_is_never_taken() {
        let os = mem();
        take(os.as_ref(), "kloudlite-git-srv-0", 1_000, None).await.unwrap().unwrap();
        let cur = read(os.as_ref()).await.unwrap();
        assert!(take(os.as_ref(), "kloudlite-git-srv-1", 1_000 + TTL - 1, cur.as_ref()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_with_the_next_epoch() {
        let os = mem();
        take(os.as_ref(), "kloudlite-git-srv-0", 1_000, None).await.unwrap().unwrap();
        let cur = read(os.as_ref()).await.unwrap();
        let b = take(os.as_ref(), "kloudlite-git-srv-1", 1_000 + TTL, cur.as_ref())
            .await
            .unwrap()
            .expect("expired: up for grabs");
        assert_eq!((b.lease.node.as_str(), b.lease.epoch), ("kloudlite-git-srv-1", 2));
    }

    /// The holder that missed its own beats finds the lease expired and naming itself. It may take
    /// it back, and the epoch still advances: a takeover is a takeover, whoever wins it.
    #[tokio::test]
    async fn the_holder_retakes_its_own_expired_lease() {
        let os = mem();
        take(os.as_ref(), "kloudlite-git-srv-0", 1_000, None).await.unwrap().unwrap();
        let cur = read(os.as_ref()).await.unwrap();
        let again = take(os.as_ref(), "kloudlite-git-srv-0", 1_000 + TTL, cur.as_ref()).await.unwrap().unwrap();
        assert_eq!(again.lease.epoch, 2);
    }

    #[tokio::test]
    async fn renew_with_a_stale_version_fails() {
        let os = mem();
        let a = take(os.as_ref(), "kloudlite-git-srv-0", 1_000, None).await.unwrap().unwrap();
        let cur = read(os.as_ref()).await.unwrap();
        let b = take(os.as_ref(), "kloudlite-git-srv-1", 1_000 + TTL, cur.as_ref()).await.unwrap().unwrap();
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
            async move { take(os.as_ref(), &format!("kloudlite-git-srv-{i}"), 1_000, None).await.unwrap() }
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
```

Add `pub mod lease;` to `crates/storage/src/ownership/mod.rs` directly above `#[cfg(test)] mod tests;`.

Append to the `tests` module in `crates/storage/src/config.rs`:

```rust
    #[test]
    fn a_file_store_cannot_host_a_fleet() {
        assert!(super::fleet_store_ok("file://./x").is_err());
        for ok in ["mem://", "s3://bucket", "az://container"] {
            super::fleet_store_ok(ok).unwrap();
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kloudlite-git-storage lease 2>&1 | tail -20`
Expected: compile error — `cannot find function `take``, `cannot find type `Lease``.

Run: `cargo test -p kloudlite-git-storage a_file_store_cannot_host_a_fleet 2>&1 | tail -5`
Expected: compile error — `cannot find function `fleet_store_ok``.

- [ ] **Step 3: Write the lease module**

Put this above the test module in `crates/storage/src/ownership/lease.rs`:

```rust
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
```

Add to `crates/storage/src/config.rs`, above `pub async fn open_store`:

```rust
/// A fleet's leader lease is a conditional put, and `LocalFileSystem` has no `PutMode::Update`
/// (object_store 0.14.1 `local.rs`: `NotImplemented`). A multi-node `file://` deployment would
/// take the lease once and then never renew or fence it — refused here, where the URL is
/// parsed, rather than discovered as an election that silently stops.
pub fn fleet_store_ok(url: &str) -> Result<()> {
    if url.starts_with("file://") {
        return Err(crate::err(
            "KLOUDLITE_GIT_S3_URL=file:// cannot host a fleet: the leader lease needs conditional \
             updates, which LocalFileSystem lacks; use mem:// for a local fleet, or s3:// / az://",
        ));
    }
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-storage lease 2>&1 | tail -15`
Expected: `test result: ok. 7 passed` (the seven tests in `ownership::lease::tests`).

Run: `cargo test -p kloudlite-git-storage a_file_store_cannot_host_a_fleet`
Expected: `1 passed`.

Run: `cargo clippy -p kloudlite-git-storage --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/storage/src/ownership/lease.rs crates/storage/src/ownership/mod.rs crates/storage/src/config.rs
git commit -m "Add the leader lease helper over conditional puts and refuse file:// fleets"
```

---

### Task 2: `OwnershipStore::promote` / `demote` — the writer as a runtime role

**Files:**
- Modify: `crates/storage/src/ownership/mod.rs` (the `OwnershipStore` enum and its `impl`, lines 157–420)
- Modify: `crates/storage/src/ownership/tests.rs` (call sites; two new tests)
- Modify: `tests/ownership.rs`, `tests/common/mod.rs:55-59`, `tests/routing.rs:40`, `crates/app/src/lib.rs:679`, `crates/workspaces/tests/engine_ops.rs:43`, `bins/server/src/main.rs:69-70` (call sites only)

**Interfaces:**
- Consumes: `leader_settings` (unchanged), `PATH`, `tokio_util::sync::CancellationToken` (tokio-util is already a storage dependency; `sync` needs no feature).
- Produces:
  - `pub struct OwnershipStore` (no longer an enum — no caller matches on it; `main.rs` constructs `Solo` and is updated here)
  - `pub fn OwnershipStore::solo() -> OwnershipStore`
  - `pub fn OwnershipStore::open(os: Arc<dyn ObjectStore>) -> OwnershipStore` — synchronous now; starts as a follower
  - `pub fn is_solo(&self) -> bool`, `pub fn object_store(&self) -> Option<Arc<dyn ObjectStore>>`
  - `pub async fn is_writer(&self) -> bool`
  - `pub async fn promote(&self) -> Result<()>` — idempotent; opens the writer OUTSIDE the role lock, then swaps
  - `pub async fn demote(&self)` — idempotent; closes the writer, reopens the lazy reader
  - `get/put/put_many/delete/close/set_draining/draining/all/checkpoint` keep their signatures

- [ ] **Step 1: Write the failing tests**

Append to `crates/storage/src/ownership/tests.rs`:

```rust
/// The role changes under a running node: follower → writer → follower, and the map is readable
/// through every state. `promote`/`demote` are idempotent because the election loop calls them
/// on every tick it believes something changed, not only on the tick it actually did.
#[tokio::test]
async fn promote_then_demote_reopens_as_a_reader() {
    use slatedb::object_store::{memory::InMemory, ObjectStore};
    use std::sync::Arc;
    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let s = OwnershipStore::open(os);
    assert!(!s.is_writer().await);
    assert!(s.put("alice/web", &entry("n", 1)).await.is_err(), "a follower never writes");

    s.promote().await.unwrap();
    s.promote().await.unwrap();
    assert!(s.is_writer().await);
    s.put("alice/web", &entry("n", 1)).await.unwrap();

    s.demote().await;
    s.demote().await;
    assert!(!s.is_writer().await);
    assert!(s.put("alice/web", &entry("n", 2)).await.is_err());
    let mut seen = None;
    for _ in 0..40 {
        seen = s.get("alice/web").await.unwrap();
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(seen, Some(entry("n", 1)), "the reopened reader must catch up to what the writer left");
}

/// The storage-level backstop the election leans on: a second writer on the same map fences the
/// first, and the first's next write says so (`pool::is_fenced`) rather than landing. Without
/// this property a stale leader that has not noticed losing the lease could keep granting.
#[tokio::test]
async fn a_second_writer_fences_the_first() {
    use slatedb::object_store::{memory::InMemory, ObjectStore};
    use std::sync::Arc;
    let os: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let a = OwnershipStore::open(os.clone());
    a.promote().await.unwrap();
    a.put("alice/web", &entry("n", 1)).await.unwrap();
    let b = OwnershipStore::open(os);
    b.promote().await.unwrap();
    let e = a.put("alice/web", &entry("n", 2)).await.expect_err("the fenced writer must not succeed");
    assert!(crate::pool::is_fenced(&e), "not reported as a fence: {e}");
    b.put("alice/web", &entry("n", 3)).await.unwrap();
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kloudlite-git-storage promote_then_demote 2>&1 | tail -5`
Expected: compile error — `no function or associated item named `open` ... takes 2 arguments` / `no method named `promote``.

- [ ] **Step 3: Rewrite `OwnershipStore`**

In `crates/storage/src/ownership/mod.rs`, replace the `pub enum OwnershipStore { ... }` definition and the whole `impl OwnershipStore` (keep `leader_settings` exactly as it is, and keep every doc comment on `checkpoint` and the WAL-GC explanation above `open` — move that explanation onto `promote`) with:

```rust
/// The ownership map: one SlateDB database, written by whoever holds the leader lease and read
/// (via a `FollowLatest` reader) by everyone else. The role changes at runtime — `promote` when
/// this node wins the lease, `demote` when it loses it — behind one lock, so a route decision in
/// flight always reads through SOME handle.
pub struct OwnershipStore {
    /// `None` is single-node: nothing to coordinate, so there is no database. The map is always
    /// empty, which makes every repo unowned, which makes this node claim it and own it. No
    /// object-store traffic, no lease, no renewal.
    os: Option<std::sync::Arc<dyn slatedb::object_store::ObjectStore>>,
    role: tokio::sync::RwLock<Role>,
    /// Serialises promote against demote: two overlapping promotes would open two writers and
    /// the second would fence the first for nothing.
    swap: tokio::sync::Mutex<()>,
}

enum Role {
    Solo,
    /// Follower. The reader is acquired lazily (`open_reader`): only a writer's `Db::builder`
    /// creates the database, so on a fresh cluster every node starts before the map exists.
    /// Until the reader opens, the map reads as empty — exactly like `Solo` — which means
    /// "nothing is known to be owned" and sends every request down the claim path.
    Reader {
        slot: std::sync::Arc<tokio::sync::RwLock<Option<std::sync::Arc<slatedb::DbReader>>>>,
        /// Stops the lazy opener when this role is retired, so a promote is not followed by a
        /// reader landing in a slot nothing reads any more.
        opener: tokio_util::sync::CancellationToken,
    },
    Writer(std::sync::Arc<slatedb::Db>),
}

fn open_reader(os: std::sync::Arc<dyn slatedb::object_store::ObjectStore>) -> Role {
    let slot = std::sync::Arc::new(tokio::sync::RwLock::new(None));
    let opener = tokio_util::sync::CancellationToken::new();
    let (cell, cancel) = (slot.clone(), opener.clone());
    tokio::spawn(async move {
        let mut logged = false;
        loop {
            let open = slatedb::DbReader::open(
                PATH,
                os.clone(),
                slatedb::DbReaderMode::FollowLatest,
                slatedb::config::DbReaderOptions {
                    manifest_poll_interval: std::time::Duration::from_millis(200),
                    ..Default::default()
                },
            );
            let r = tokio::select! {
                _ = cancel.cancelled() => return,
                r = open => r,
            };
            // Cancelled while the open was in flight: close what just opened rather than park it.
            if cancel.is_cancelled() {
                if let Ok(r) = r {
                    let _ = r.close().await;
                }
                return;
            }
            match r {
                Ok(r) => {
                    *cell.write().await = Some(std::sync::Arc::new(r));
                    if logged {
                        tracing::info!("ownership map opened");
                    }
                    return;
                }
                Err(e) => {
                    // First failure only: the writer may not have created the map yet, and one
                    // line a second forever is noise, not signal.
                    if !logged {
                        tracing::warn!(error = %e, "ownership map not readable yet; retrying");
                        logged = true;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });
    Role::Reader { slot, opener }
}

impl Role {
    async fn retire(self) {
        match self {
            Role::Solo => {}
            Role::Reader { slot, opener } => {
                opener.cancel();
                if let Some(r) = slot.write().await.take() {
                    if let Err(e) = r.close().await {
                        tracing::warn!(error = %e, "closing the ownership reader");
                    }
                }
            }
            // A fenced writer's close reports the fence again; there is nothing left to do about it.
            Role::Writer(db) => {
                if let Err(e) = db.close().await {
                    tracing::warn!(error = %e, "closing the ownership writer");
                }
            }
        }
    }
}

impl OwnershipStore {
    pub fn solo() -> OwnershipStore {
        OwnershipStore { os: None, role: tokio::sync::RwLock::new(Role::Solo), swap: tokio::sync::Mutex::new(()) }
    }

    /// Fleet: starts as a follower. `promote` makes it the writer when this node wins the lease.
    pub fn open(os: std::sync::Arc<dyn slatedb::object_store::ObjectStore>) -> OwnershipStore {
        OwnershipStore {
            role: tokio::sync::RwLock::new(open_reader(os.clone())),
            os: Some(os),
            swap: tokio::sync::Mutex::new(()),
        }
    }

    pub fn is_solo(&self) -> bool {
        self.os.is_none()
    }

    /// The store the leader lease lives in — `None` in solo mode, where there is no election.
    pub fn object_store(&self) -> Option<std::sync::Arc<dyn slatedb::object_store::ObjectStore>> {
        self.os.clone()
    }

    pub async fn is_writer(&self) -> bool {
        matches!(*self.role.read().await, Role::Writer(_))
    }

    /// Become the writer. Compaction ON and every collector at its default — see `leader_settings`
    /// for why that is safe for a `FollowLatest` follower. Opening fences any previous writer of
    /// this map, which is the storage-level half of the election: a stale leader that has not
    /// noticed losing the lease cannot write.
    ///
    /// (The WAL-GC paragraphs that used to sit on `open` go here, verbatim: WAL GC enabled, why
    /// `checkpoint` is the missing half, the 18,521- and 23,083-object incidents.)
    pub async fn promote(&self) -> crate::Result<()> {
        let Some(os) = &self.os else { return Ok(()) };
        let _g = self.swap.lock().await;
        if self.is_writer().await {
            return Ok(());
        }
        // Opened OUTSIDE the role lock: this replays the map's WAL, and a route decision must keep
        // reading through the follower handle meanwhile rather than queue behind the replay.
        let db = slatedb::Db::builder(PATH, os.clone())
            .with_settings(leader_settings(
                std::time::Duration::from_secs(300),
                std::time::Duration::from_secs(300),
            ))
            .build()
            .await?;
        // Said out loud because a writer that quietly came up as anything else is the difference
        // between a WAL that gets reclaimed and one that grows forever.
        tracing::info!(path = %PATH, "ownership: opened as WRITER (leader)");
        let old = std::mem::replace(&mut *self.role.write().await, Role::Writer(std::sync::Arc::new(db)));
        old.retire().await;
        Ok(())
    }

    /// Stop being the writer: close it and follow the map again. Never fails — a fenced handle's
    /// close errors, and that is exactly the case this is called for.
    pub async fn demote(&self) {
        let Some(os) = &self.os else { return };
        let _g = self.swap.lock().await;
        if !self.is_writer().await {
            return;
        }
        let old = std::mem::replace(&mut *self.role.write().await, open_reader(os.clone()));
        old.retire().await;
        tracing::info!(path = %PATH, "ownership: reopened as reader");
    }

    /// (`checkpoint` unchanged, except its match: `if let Role::Writer(db) = &*self.role.read().await`.)

    pub async fn get(&self, repo: &str) -> crate::Result<Option<Entry>> {
        let role = self.role.read().await;
        let bytes = match &*role {
            Role::Writer(db) => db.get(key(repo)).await?,
            Role::Reader { slot, .. } => match slot.read().await.clone() {
                Some(r) => r.get(key(repo)).await?,
                // No reader yet: same answer as `Solo`, and safe for the same reason.
                None => return Ok(None),
            },
            Role::Solo => return Ok(None),
        };
        bytes.as_deref().map(Entry::decode).transpose()
    }

    /// Writer only. A follower writing is a bug, not a fallback — this errors rather than
    /// silently opening a writer or dropping the write.
    pub async fn put(&self, repo: &str, e: &Entry) -> crate::Result<()> {
        match &*self.role.read().await {
            Role::Writer(db) => {
                db.put(key(repo), e.encode()).await?;
                Ok(())
            }
            Role::Reader { .. } => Err(crate::err("ownership: put on a follower")),
            Role::Solo => Ok(()),
        }
    }
    // put_many, delete, close, set_draining, draining, all: the same mechanical change — the
    // `match self { OwnershipStore::Writer { db, .. } => ..., OwnershipStore::Reader(slot) => ...,
    // OwnershipStore::Solo => ... }` becomes `let role = self.role.read().await; match &*role {
    // Role::Writer(db) => ..., Role::Reader { slot, .. } => ..., Role::Solo => ... }`, with the
    // guard `role` held to the end of the function in `draining` and `all` because the scan
    // iterator borrows it. Bodies unchanged.
}
```

Do the mechanical change for `put_many`, `delete`, `close`, `set_draining`, `draining`, `all` and `checkpoint` as the comment says — the six bodies and their doc comments stay word for word.

- [ ] **Step 4: Update every constructor call site**

- `crates/storage/src/ownership/tests.rs`: `OwnershipStore::open(os, true).await.unwrap()` (three sites: `checkpointing_an_untouched_map_returns`, `checkpointing_after_a_write_returns`, `a_renew_beat_is_one_durable_write`) become
  ```rust
  let store = OwnershipStore::open(os);
  store.promote().await.unwrap();
  ```
  In `a_renew_beat_is_one_durable_write` the `Counting` store needs an explicit coercion first: `let os: std::sync::Arc<dyn slatedb::object_store::ObjectStore> = counting.clone();`.
- `tests/ownership.rs`: every `OwnershipStore::open(os, true).await.unwrap()` → `{ let s = OwnershipStore::open(os); s.promote().await.unwrap(); s }`; every `OwnershipStore::open(os, false).await.unwrap()` → `OwnershipStore::open(os)`. Update the file's header comment: "a writer (the lease holder) writes `cluster/ownership`".
- `tests/common/mod.rs` `app()`: 
  ```rust
  let ownership = kloudlite_git_storage::ownership::OwnershipStore::open(store.os.clone());
  ownership.promote().await.unwrap();
  ```
- `tests/routing.rs:40`: 
  ```rust
  let ownership = OwnershipStore::open(os);
  if name == LEADER {
      ownership.promote().await.unwrap();
  }
  ```
  (temporary — Task 6 replaces this with the election).
- `crates/app/src/lib.rs:679` (`test_app`): `let ownership = OwnershipStore::open(os); ownership.promote().await.unwrap();`.
- `crates/workspaces/tests/engine_ops.rs:43`: same two lines.
- `bins/server/src/main.rs`: `kloudlite_git_server::ownership::OwnershipStore::Solo` → `kloudlite_git_server::ownership::OwnershipStore::solo()`; and lines 69–70 become
  ```rust
  let store = kloudlite_git_server::ownership::OwnershipStore::open(store.os.clone());
  if me == leader {
      store.promote().await?;
  }
  ```
  (temporary — Task 3 replaces the name check with the election).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --workspace --locked 2>&1 | grep -E '^test result|FAILED|panicked' | sort | uniq -c`
Expected: every line `test result: ok.`; in particular `promote_then_demote_reopens_as_a_reader`, `a_second_writer_fences_the_first`, and every test in `tests/ownership.rs` and `tests/routing.rs` pass.

If `a_second_writer_fences_the_first` sees the second `a.put` succeed: the fence surfaces on the writer's next flush, and `put` awaits one (`flush_interval` 10 ms). Compare with `crates/storage/src/pool/mod.rs` around line 480 (`a stray opener ... is_fenced`), which proves the same property for repo databases — the settings differ only in GC knobs, so the assertion must hold here too; if it does not, the bug is in the new `promote`, not the test.

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E 'ownership/mod.rs|ownership/tests.rs' ; echo "exit $?"` — expect no lines.

- [ ] **Step 6: Commit**

```bash
git add crates/storage/src/ownership/mod.rs crates/storage/src/ownership/tests.rs tests/ownership.rs tests/common/mod.rs tests/routing.rs crates/app/src/lib.rs crates/workspaces/tests/engine_ops.rs bins/server/src/main.rs
git commit -m "Make the ownership map's writer a runtime role with promote and demote"
```

---

### Task 3: `App` — the election loop, the epoch, and grants that refuse without it

**Files:**
- Modify: `crates/app/src/lib.rs` (struct, `new`, `leader`, `is_leader`, `grant_*`, `prune_once`, tests)
- Modify: `bins/server/src/main.rs` (drop `with_topology`, `replicas`; the name check from Task 2)
- Modify: `tests/common/mod.rs`, `tests/routing.rs`, `crates/workspaces/tests/engine_ops.rs` (`App::new` loses `replicas`)

**Interfaces:**
- Consumes: `ownership::lease::{read, take, renew, is_expired, Lease, Held, LEADER_TTL}`; `OwnershipStore::{object_store, is_solo, promote, demote}`; `pool::is_fenced`.
- Produces (all on `App`):
  - `pub fn new(store, ownership, self_name, addr_of, peer_secret) -> App` — `replicas` gone
  - `pub fn leader(&self) -> Option<String>`, `pub fn set_leader(&self, node: Option<&str>)`
  - `pub fn is_leader(&self) -> bool` (= `leader_epoch() != 0`), `pub fn leader_epoch(&self) -> u64`
  - `pub fn leader_live(&self) -> bool`
  - `pub async fn election_tick(&self) -> Result<()>`
  - `pub async fn demote(&self, why: &str)`
  - `grant_claim/grant_renew/grant_release/prune_once` return `Err("not the leader")` when `leader_epoch() == 0`, and demote on a fenced map write
  - Deleted: `leader_name`, `server_prefix`, `replicas`, `with_topology`, `LEADER_SILENCE`, `mark_leader_seen`, `leader_reachable`, `leader_seen_ms` (the `/healthz` and `ask_leader_with` callers are fixed in Task 4 — until then keep a one-line `pub fn leader_reachable(&self) -> bool { self.leader_live() }` shim, deleted in Task 4)

- [ ] **Step 1: Write the failing tests**

Replace the `mod tests` at the bottom of `crates/app/src/lib.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ownership::lease::{self, LEADER_TTL};
    use slatedb::object_store::{memory::InMemory, path::Path, ObjectStore, ObjectStoreExt, PutPayload};

    fn mem() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    /// A fleet node over a shared object store. Nothing is ticked here: each test decides when.
    async fn fleet_app(os: &Arc<dyn ObjectStore>, name: &str) -> App {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        // Leaked so the App can outlive this helper's tempdir binding without the test wiring a
        // Node like tests/routing.rs does.
        std::mem::forget(tmp);
        App::new(
            store,
            Arc::new(OwnershipStore::open(os.clone())),
            name.into(),
            Arc::new(|_: &str| "127.0.0.1:1".into()),
            "test-secret".into(),
        )
    }

    /// Write the lease object outright — what another node's put looks like from here.
    async fn plant(os: &Arc<dyn ObjectStore>, node: &str, epoch: u64, expires_ms: u64) {
        os.put(&Path::from(lease::PATH), PutPayload::from(format!("{node}\n{epoch}\n{expires_ms}").into_bytes()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_lone_node_takes_the_lease_and_leads() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-git-srv-0").await;
        assert!(!a.is_leader() && a.leader().is_none() && !a.leader_live());
        a.election_tick().await.unwrap();
        assert!(a.is_leader());
        assert_eq!(a.leader_epoch(), 1);
        assert_eq!(a.leader().as_deref(), Some("kloudlite-git-srv-0"));
        assert!(a.leader_live());
        assert!(a.ownership.is_writer().await);
        // A second tick renews rather than re-takes: same epoch, still the writer.
        a.election_tick().await.unwrap();
        assert_eq!(a.leader_epoch(), 1);
    }

    #[tokio::test]
    async fn a_second_node_follows_the_holder() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-git-srv-0").await;
        a.election_tick().await.unwrap();
        let b = fleet_app(&os, "kloudlite-git-srv-1").await;
        b.election_tick().await.unwrap();
        assert!(!b.is_leader());
        assert_eq!(b.leader().as_deref(), Some("kloudlite-git-srv-0"));
        assert!(b.leader_live());
        assert!(!b.ownership.is_writer().await);
    }

    #[tokio::test]
    async fn a_lease_taken_by_another_node_demotes() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-git-srv-0").await;
        a.election_tick().await.unwrap();
        plant(&os, "kloudlite-git-srv-1", 2, a.now_ms() + 5_000).await;
        a.election_tick().await.unwrap();
        assert!(!a.is_leader(), "somebody else holds a live lease at a newer epoch");
        assert_eq!(a.leader().as_deref(), Some("kloudlite-git-srv-1"));
        assert!(!a.ownership.is_writer().await);
        let e = a.grant_claim("alice/web", "kloudlite-git-srv-2", false).await.expect_err("demoted: must not grant");
        assert!(e.to_string().contains("not the leader"), "{e}");
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_with_the_next_epoch() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-git-srv-0").await;
        plant(&os, "kloudlite-git-srv-9", 5, a.now_ms() - 1).await;
        a.election_tick().await.unwrap();
        assert!(a.is_leader());
        assert_eq!(a.leader_epoch(), 6);
    }

    /// A pod that restarts keeps its name, and within one TTL the lease still names it. It resumes
    /// that lease rather than waiting for it to lapse — a restart must not cost ten seconds of
    /// "not the leader" answered to itself.
    #[tokio::test]
    async fn a_restarted_holder_resumes_its_own_live_lease() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-git-srv-0").await;
        plant(&os, "kloudlite-git-srv-0", 3, a.now_ms() + 5_000).await;
        a.election_tick().await.unwrap();
        assert!(a.is_leader());
        assert_eq!(a.leader_epoch(), 3);
        assert!(a.ownership.is_writer().await);
    }

    /// The storage-level fence, turned into a demotion: a stray writer on the map (another node
    /// that won the lease and opened it) makes this node's next map write fail, and that failure
    /// must strip its leadership rather than be reported as one bad grant.
    #[tokio::test]
    async fn a_fenced_map_write_demotes() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-git-srv-0").await;
        a.election_tick().await.unwrap();
        let stray = OwnershipStore::open(os.clone());
        stray.promote().await.unwrap();
        assert!(a.grant_claim("alice/web", "kloudlite-git-srv-1", false).await.is_err());
        assert!(!a.is_leader(), "a fenced writer is not the leader");
        assert!(!a.ownership.is_writer().await);
    }

    #[tokio::test]
    async fn grants_refuse_without_the_lease() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-git-srv-0").await;
        for r in [
            a.grant_claim("alice/web", "kloudlite-git-srv-1", false).await.map(|_| ()),
            a.grant_renew("kloudlite-git-srv-1", &["alice/web".into()]).await.map(|_| ()),
            a.grant_release("alice/web", "kloudlite-git-srv-1").await,
            a.prune_once().await,
        ] {
            let e = r.expect_err("no lease, no writes");
            assert!(e.to_string().contains("not the leader"), "{e}");
        }
    }

    /// What `/healthz` proves: a live leader exists — this node, or a lease read within
    /// `LEADER_TTL` that has not expired. The leader is always live to itself.
    #[tokio::test]
    async fn leader_live_follows_the_lease() {
        let os = mem();
        let a = fleet_app(&os, "kloudlite-git-srv-0").await;
        a.election_tick().await.unwrap();
        let b = fleet_app(&os, "kloudlite-git-srv-1").await;
        assert!(!b.leader_live(), "no lease read yet: a rolled pod must not take traffic");
        b.election_tick().await.unwrap();
        assert!(b.leader_live());
        b.advance_clock(LEADER_TTL + std::time::Duration::from_millis(1));
        assert!(!b.leader_live(), "the lease lapsed and nobody took it: un-ready");
        a.advance_clock(LEADER_TTL * 10);
        assert!(a.leader_live(), "the holder is live to itself until it is demoted");
    }

    /// Solo: one node, no lease, no store traffic. It leads by construction.
    #[tokio::test]
    async fn a_solo_node_leads_without_a_lease() {
        let os = mem();
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap());
        std::mem::forget(tmp);
        let a = App::new(store, Arc::new(OwnershipStore::solo()), "kloudlite-git-0".into(), Arc::new(|_: &str| "127.0.0.1:1".into()), "s".into());
        assert!(a.is_leader() && a.leader_live());
        a.election_tick().await.unwrap();
        assert!(lease::read(os.as_ref()).await.unwrap().is_none(), "solo never writes a lease");
    }

    /// A cold claim waits out a leader roll (~30 s of retries). With the gate full, one more
    /// fails at once instead of pinning another task for that long — the fast 503.
    #[tokio::test]
    async fn a_claim_past_the_gate_fails_fast() {
        let os = mem();
        let follower = fleet_app(&os, "kloudlite-git-srv-1").await; // nobody leads; the addr is a refused port
        let _held = follower.claim_gate.acquire_many(MAX_WAITING_CLAIMS as u32).await.unwrap();
        let t = std::time::Instant::now();
        let err = follower.claim("alice/cold").await.expect_err("must not be granted");
        assert!(err.to_string().contains("too many claims"), "{err}");
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "must not enter the retry loop");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kloudlite-git-app 2>&1 | grep -E '^error' | head -5`
Expected: `no method named `election_tick``, `this function takes 6 arguments but 5 were supplied`.

- [ ] **Step 3: Rewrite the `App` struct and constructor**

In `crates/app/src/lib.rs`:

Replace the `use` line `use ownership::{Entry, Grant, OwnershipStore, Route};` with

```rust
use ownership::lease::{self, Held, Lease, LEADER_TTL};
use ownership::{Entry, Grant, OwnershipStore, Route};
```

Delete the struct fields `leader_name`, `server_prefix`, `replicas`, `leader_seen_ms` and their doc comments, and add after `self_name`:

```rust
    /// Who holds the leader lease, as this node last read it. `None` until the first read, and
    /// again after a read finds the lease absent or expired — an unknown leader is found by
    /// re-reading the lease, never guessed from a name.
    leader: std::sync::Mutex<Option<String>>,
    /// The epoch of the lease THIS node holds; zero when it holds none. `is_leader()` is exactly
    /// `!= 0`, and every map write checks it under `leader_lock` — a leader mid-demotion stops
    /// granting in-process, before SlateDB's fence has to say so.
    leader_epoch: std::sync::atomic::AtomicU64,
    /// `now_ms()` when a LIVE lease was last read (any holder), and when that lease expires.
    /// `/healthz` reads both: readiness means "a leader exists", not "I am one".
    lease_seen_ms: std::sync::atomic::AtomicU64,
    lease_expires_ms: std::sync::atomic::AtomicU64,
```

Delete `pub const LEADER_SILENCE` and its comment. Update the `leader_lock` doc to name the fifth path: "(grant_claim, grant_renew, grant_release, prune_once — and `demote`, so no grant is mid-write when the writer goes)".

Replace `new` with:

```rust
    pub fn new(
        store: Arc<store::Store>,
        ownership: Arc<OwnershipStore>,
        self_name: String,
        addr_of: AddrOf,
        peer_secret: String,
    ) -> Self {
        let jwt_secret = std::env::var("KLOUDLITE_GIT_JWT_SECRET").unwrap_or_else(|_| {
            use rand::Rng;
            rand::thread_rng()
                .sample_iter(rand::distributions::Alphanumeric)
                .take(48)
                .map(char::from)
                .collect()
        });
        // Solo: one node and no lease. It leads by construction — epoch 1, itself — so every
        // claim is local and nothing here ever reads the store.
        let (leader, epoch) = if ownership.is_solo() { (Some(self_name.clone()), 1) } else { (None, 0) };
        App {
            store,
            ownership,
            self_name,
            leader: std::sync::Mutex::new(leader),
            leader_epoch: std::sync::atomic::AtomicU64::new(epoch),
            lease_seen_ms: std::sync::atomic::AtomicU64::new(0),
            lease_expires_ms: std::sync::atomic::AtomicU64::new(0),
            addr_of,
            forwarder: Arc::new(proxy::Forwarder::new(peer_secret)),
            recovery_asked: Default::default(),
            skew_ms: std::sync::atomic::AtomicU64::new(0),
            jwt: Arc::new(jwt::Jwt::new(&jwt_secret).expect("jwt secret")),
            leader_lock: tokio::sync::Mutex::new(()),
            claim_gate: tokio::sync::Semaphore::new(MAX_WAITING_CLAIMS),
            dir: pulls::Source::Absent,
        }
    }
```

- [ ] **Step 4: Replace `leader`/`with_topology`/`is_leader`/`mark_leader_seen`/`leader_reachable` with the election**

Delete `fn leader(&self) -> &str`, `with_topology`, `is_leader`, `mark_leader_seen`, `leader_reachable` and their comments. In their place (after `owner`):

```rust
    pub fn leader(&self) -> Option<String> {
        self.leader.lock().unwrap().clone()
    }

    pub fn set_leader(&self, node: Option<&str>) {
        *self.leader.lock().unwrap() = node.map(str::to_string);
    }

    /// Leading means holding an epoch. Nothing here is derived from a name: two nodes cannot
    /// both hold one, because the store hands the lease to exactly one put.
    pub fn is_leader(&self) -> bool {
        self.leader_epoch() != 0
    }

    pub fn leader_epoch(&self) -> u64 {
        self.leader_epoch.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A live leader exists: this node, or a lease read within `LEADER_TTL` that has not expired.
    /// What `/healthz` gates readiness on — a node that knows nobody who can grant cannot take a
    /// cold repo, and must not take traffic. Cached reads; the probe costs nothing.
    pub fn leader_live(&self) -> bool {
        if self.is_leader() {
            return true;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let now = self.now_ms();
        now.saturating_sub(self.lease_seen_ms.load(Relaxed)) < LEADER_TTL.as_millis() as u64
            && now < self.lease_expires_ms.load(Relaxed)
    }

    // Task 4 deletes this shim; it exists so `/healthz` and `ask_leader_with` compile until then.
    pub fn leader_reachable(&self) -> bool {
        self.leader_live()
    }

    fn note_live(&self, l: &Lease) {
        use std::sync::atomic::Ordering::Relaxed;
        self.set_leader(Some(&l.node));
        self.lease_seen_ms.store(self.now_ms(), Relaxed);
        self.lease_expires_ms.store(l.expires_ms, Relaxed);
    }

    /// One beat of the election, run every `LEADER_RENEW` on every fleet node (and once at boot).
    ///
    /// Read the lease. If it names me and is live, renew it — the store refuses a renewal pinned
    /// to a version somebody else has since overwritten, which is how "my renewal raced an expiry"
    /// resolves: by the store's answer, not by our clock. If it names somebody else and is live,
    /// follow them, and stop leading if we thought we did. If it is absent or expired, try to take
    /// it with the next epoch; exactly one candidate's put lands, and the rest read the winner on
    /// their next tick. Solo mode has no store to read and returns at once.
    pub async fn election_tick(&self) -> Result<()> {
        let Some(os) = self.ownership.object_store() else { return Ok(()) };
        let now = self.now_ms();
        let cur = lease::read(os.as_ref()).await?;
        match cur {
            Some(c) if c.lease.node == self.self_name && !lease::is_expired(&c.lease, now) => {
                match lease::renew(os.as_ref(), &c, now).await? {
                    // `promote` is idempotent: on the beat after winning this only refreshes the
                    // expiry we cache; after a restart within one TTL it resumes our own lease.
                    Some(h) => self.promote(h).await?,
                    None => self.demote("renewal refused by the store").await,
                }
            }
            Some(c) if !lease::is_expired(&c.lease, now) => {
                if self.is_leader() {
                    self.demote(&format!("{} holds the lease at epoch {}", c.lease.node, c.lease.epoch)).await;
                }
                self.note_live(&c.lease);
            }
            c => {
                if let Some(h) = lease::take(os.as_ref(), &self.self_name, now, c.as_ref()).await? {
                    self.promote(h).await?;
                }
            }
        }
        // One gauge per pod; the alert is `sum(...) != 1`.
        metrics::gauge!("ownership_is_leader").set(if self.is_leader() { 1.0 } else { 0.0 });
        Ok(())
    }

    /// Hold the lease `h` names: open the writer FIRST, then publish the epoch. A grant that sees
    /// the epoch must find a writer behind it. Opening fences any previous writer of the map, so a
    /// stale leader that has not yet noticed losing the lease cannot write.
    async fn promote(&self, h: Held) -> Result<()> {
        // A lease we cannot use lapses on its own TTL and somebody else takes it; leading with a
        // reader would grant nothing anyway.
        self.ownership.promote().await?;
        let fresh = self.leader_epoch() != h.lease.epoch;
        self.leader_epoch.store(h.lease.epoch, std::sync::atomic::Ordering::Relaxed);
        self.note_live(&h.lease);
        if fresh {
            tracing::info!(epoch = h.lease.epoch, "lease: leading");
        }
        Ok(())
    }

    /// Stop leading: epoch to zero under `leader_lock` — so no grant is mid-write when the writer
    /// goes — then close the writer and follow the map again. Called for a refused renewal, a
    /// lease read that names somebody else, and a fenced map write.
    pub async fn demote(&self, why: &str) {
        let _g = self.leader_lock.lock().await;
        self.demote_locked(why).await;
    }

    /// `demote` for a caller already holding `leader_lock` (the grants).
    async fn demote_locked(&self, why: &str) {
        if !self.is_leader() {
            return;
        }
        tracing::warn!(epoch = self.leader_epoch(), why, "lease: demoting");
        self.leader_epoch.store(0, std::sync::atomic::Ordering::Relaxed);
        self.set_leader(None);
        self.ownership.demote().await;
        metrics::counter!("ownership_demotions_total").increment(1);
    }

    /// The epoch a map write is made under. Zero — not leading, or demoted since the handler
    /// checked — refuses: the in-process half of the fence, ahead of SlateDB's.
    fn writing_epoch(&self) -> Result<u64> {
        match self.leader_epoch() {
            0 => Err(err("not the leader")),
            e => Ok(e),
        }
    }

    /// A map operation's result, with a fence turned into a demotion. Caller holds `leader_lock`.
    async fn fenced_check<T>(&self, r: Result<T>) -> Result<T> {
        if let Err(e) = &r {
            if pool::is_fenced(e) {
                self.demote_locked("map write fenced").await;
            }
        }
        r
    }
```

- [ ] **Step 5: Put the epoch on every map write**

Replace `grant_claim`, `grant_renew`, `grant_release` and `prune_once` bodies:

```rust
    /// Leader only: drop entries whose lease lapsed without a release — the node holding them died
    /// or was partitioned away. Keeps the map bounded by what is actually open.
    pub async fn prune_once(&self) -> Result<()> {
        let _g = self.leader_lock.lock().await;
        self.writing_epoch()?;
        let now = self.now_ms();
        let all = self.fenced_check(self.ownership.all().await).await?;
        // The writer is the only one that can sweep, so its count is the one honest size of the map.
        metrics::gauge!("ownership_map_size").set(all.len() as f64);
        for (repo, e) in all {
            if ownership::is_expired(&e, now) {
                self.fenced_check(self.ownership.delete(&repo).await).await?;
            }
        }
        Ok(())
    }

    // ---- The leader's side of the three messages. Only ever reached on the lease holder. ----

    pub async fn grant_claim(&self, repo: &str, asker: &str, force: bool) -> Result<Grant> {
        // Serialize every read-modify-write on the map: concurrent claims/renews/prunes on the
        // same repo could otherwise both read a stale map and both write, granting one repo to
        // two nodes — which fences the loser's live database. One process, one lock: cheap and
        // total. `demote` takes the same lock, so an epoch seen here is still held at the write.
        let _g = self.leader_lock.lock().await;
        let epoch = self.writing_epoch()?;
        let now = self.now_ms();
        let cur = self.fenced_check(self.ownership.get(repo).await).await?;
        let g = if force {
            ownership::decide_force_claim(cur.as_ref(), asker, now)
        } else {
            ownership::decide_claim(cur.as_ref(), asker, now)
        };
        if let Grant::Granted(e) = &g {
            // A grant over a live entry naming another node is a MOVE (a roll, a drain, a
            // force-claim), which is the event worth graphing against 421s and fences.
            let result = match &cur {
                Some(c) if c.node != e.node => "moved",
                _ => "granted",
            };
            metrics::counter!("ownership_claims_total", "result" => result).increment(1);
            self.fenced_check(self.ownership.put(repo, e).await).await?;
            tracing::debug!(repo = %repo, node = %e.node, epoch, "ownership: granted");
        } else {
            metrics::counter!("ownership_claims_total", "result" => "heldby").increment(1);
        }
        Ok(g)
    }

    pub async fn grant_renew(&self, asker: &str, repos: &[String]) -> Result<Vec<String>> {
        // One lock, N local reads, ONE durable write. (Comment from before, unchanged.)
        let _g = self.leader_lock.lock().await;
        self.writing_epoch()?;
        let now = self.now_ms();
        let mut lost = Vec::new();
        let mut renewed = Vec::new();
        for repo in repos {
            let cur = self.fenced_check(self.ownership.get(repo).await).await?;
            match ownership::decide_renew(cur.as_ref(), asker, now) {
                Some(e) => renewed.push((repo.clone(), e)),
                None => lost.push(repo.clone()),
            }
        }
        self.fenced_check(self.ownership.put_many(&renewed).await).await?;
        Ok(lost)
    }

    pub async fn grant_release(&self, repo: &str, asker: &str) -> Result<()> {
        let _g = self.leader_lock.lock().await;
        self.writing_epoch()?;
        let cur = self.fenced_check(self.ownership.get(repo).await).await?;
        if ownership::may_release(cur.as_ref(), asker) {
            self.fenced_check(self.ownership.delete(repo).await).await?;
        }
        Ok(())
    }
```

The old `grant_claim` handed the leader's own claims to `least_loaded` ("Pod zero stores the lease; it does not hold repositories"). That carve-out is gone: the asker is granted what it asked for, whoever it is. Delete the `let asker = if asker == self.leader() { ... }` block entirely.

In `ask_leader_with`, for now only: `let leader = self.leader();` → `let leader = self.leader().ok_or_else(|| err("no live leader known"))?;` and delete the `self.mark_leader_seen();` line (Task 4 rewrites the whole function). In `route()`, change the comment sentence "During a roll pod zero updates last, which ages out every entry" to "During a leader failover every entry ages out unrenewed".

- [ ] **Step 6: Fix the `App::new` callers and `main.rs`**

- `tests/common/mod.rs` `app()`: drop the trailing `1,` argument and its comment; replace the `ownership.promote().await.unwrap();` line from Task 2 with nothing, and after `App::new(...)` do
  ```rust
      let app = kloudlite_git_app::App::new(store, Arc::new(ownership), "kloudlite-git-0".into(), Arc::new(|_| "127.0.0.1:1".to_string()), "test-peer-secret".into());
      // One beat: with nobody else on this store the node takes the lease and every claim is local.
      app.election_tick().await.unwrap();
      assert!(app.is_leader());
      Arc::new(app)
  ```
  (update the doc comment: "this node takes the lease on its first beat, so it is the leader and every claim is decided locally").
- `tests/routing.rs` `node()`: drop `fleet.len() as u32,`; keep the Task 2 `if name == LEADER { promote }` for now (Task 6 replaces it with a tick).
- `crates/workspaces/tests/engine_ops.rs`: drop the `1,` argument; replace the promote line with `app.election_tick().await.unwrap();` after construction (before `Arc::new` if it is wrapped — construct, tick, then wrap).
- `bins/server/src/main.rs`: delete the `server_prefix` block (lines 81–93) and the `replicas` block (lines 94–107); `App::new(store.clone(), Arc::new(ownership), me, addr_of, peer_secret).with_directory(dir)` with no `.with_topology(...)`; keep the Task 2 `if me == leader { store.promote().await?; }` and `leader_for_app` for one more task, but since `App` no longer knows the leader's name, add right after `let app = Arc::new(...)`:
  ```rust
      if !svc.is_empty() {
          // One beat before anything asks: a fresh fleet has no leader until somebody takes the
          // lease, and the first claim should not wait a tick for it. Not fatal — the loop retries
          // and /healthz stays un-ready until a lease is read.
          if let Err(e) = app.election_tick().await {
              tracing::warn!(error = %e, "first election tick");
          }
      }
  ```
  and delete the `if me == leader { store.promote() }` from Task 2 along with `leader`/`leader_for_app` and the `KLOUDLITE_GIT_LEADER` comment block (lines 58–68). The tuple becomes `(me, peer_secret, ownership)`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-app 2>&1 | tail -15`
Expected: `test result: ok. 10 passed`.

Run: `cargo test --workspace --locked 2>&1 | grep -E '^test result|FAILED|panicked' | sort | uniq -c`
Expected: all `ok`. `tests/routing.rs` still passes: `LEADER` is promoted by name in the harness and every follower's first `ask` finds `leader()` set — wait, it does not: followers have `leader == None` until a tick. So in `node()` also add `app.election_tick().await.unwrap();` after `App::new` (before the `if name == LEADER` promote is no longer needed — delete it: the first node's tick takes the lease). This is the Task 6 harness change arriving early; Task 6 then only adds tests and comments.

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E 'crates/app|bins/server/src/main.rs|tests/' ; echo "exit $?"` — expect no lines.

- [ ] **Step 8: Commit**

```bash
git add crates/app/src/lib.rs bins/server/src/main.rs tests/common/mod.rs tests/routing.rs crates/workspaces/tests/engine_ops.rs
git commit -m "Elect the ownership map's writer by lease and fence every map write on its epoch"
```

---

### Task 4: Followers — re-read the lease on 421 or connect failure; `/healthz` on a live leader

**Files:**
- Modify: `crates/app/src/lib.rs` (`ask_leader_with`, new `refresh_leader`; delete the `leader_reachable` shim)
- Modify: `bins/server/src/router/route.rs:11-29` (`healthz`), `:43-46` (doc), `:47-122` (`/own/*` error path), `:133-145` (`leader_only`)

**Interfaces:**
- Consumes: `App::{leader, set_leader, note_live, leader_live, is_leader, ownership.object_store}`, `lease::read`.
- Produces: `async fn App::refresh_leader(&self) -> Option<String>` (private); `/healthz` 503 body `"no live leader"`; `/own/*` answer 421 whenever the node is not the leader AFTER the grant ran, not only before.

- [ ] **Step 1: Write the failing test**

Add to `crates/app/src/lib.rs` `mod tests`:

```rust
    /// A connect failure re-reads the lease before the next attempt: the name this node had was a
    /// tick old, and a failover has to finish inside the asker's patience, not the loop's cadence.
    /// Every address here is a refused port, so the ask never succeeds — what is asserted is what
    /// the node BELIEVES afterwards.
    #[tokio::test]
    async fn a_failed_ask_re_reads_the_lease() {
        let os = mem();
        let b = fleet_app(&os, "kloudlite-git-srv-1").await;
        b.set_leader(Some("ghost"));
        assert!(b.claim_to_recover("alice/web").await.is_err()); // two quick tries, 250 ms apart
        assert_eq!(b.leader(), None, "the lease is absent: nobody leads, and 'ghost' is forgotten");

        plant(&os, "kloudlite-git-srv-0", 4, b.now_ms() + 5_000).await;
        assert!(b.claim_to_recover("alice/web").await.is_err());
        assert_eq!(b.leader().as_deref(), Some("kloudlite-git-srv-0"), "re-read on the failed connect");
        assert!(b.leader_live());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kloudlite-git-app a_failed_ask_re_reads_the_lease 2>&1 | tail -5`
Expected: FAIL — the first `claim_to_recover` returns `Err("no live leader known")`? No: `leader` is `Some("ghost")`, so it tries and fails, and afterwards `b.leader()` is still `Some("ghost")` — assertion `assert_eq!(b.leader(), None)` fails.

- [ ] **Step 3: Rewrite `ask_leader_with` and add `refresh_leader`**

Replace `ask_leader_with` in `crates/app/src/lib.rs` (keep the long comment block on patience and budgets above the `attempts` match — it is still true) with:

```rust
    async fn ask_leader_with(&self, what: &str, body: String, patience: Patience) -> Result<String> {
        // (the existing comment on claim/renew/release/recover patience stays here)
        let attempts = match patience {
            Patience::Claim => proxy::CLAIM_ATTEMPTS,
            Patience::Recover => proxy::RECOVER_ATTEMPTS,
            Patience::Release => proxy::RELEASE_ATTEMPTS,
            Patience::None => 1,
        };
        // Only the patient path is gated: it is the one that can hold a task for the length of a
        // leader failover. The permit lives for the whole retry loop.
        let _permit = match patience {
            Patience::Claim => Some(
                self.claim_gate
                    .try_acquire()
                    .map_err(|_| err("too many claims already waiting on the leader; retry"))?,
            ),
            _ => None,
        };
        let mut leader = self.leader();
        let mut last = err("the leader was unreachable");
        for attempt in 0..attempts {
            if attempt > 0 {
                let backoff = match patience {
                    Patience::Claim => proxy::CLAIM_BACKOFF,
                    Patience::Recover => proxy::RECOVER_BACKOFF,
                    _ => proxy::RELEASE_BACKOFF,
                };
                tokio::time::sleep(backoff).await;
            }
            // No name, or a name that just failed us: the lease is the authority, and the loop's
            // last read may be a tick old. Re-read it here so a failover completes inside THIS
            // request's patience rather than waiting for the next beat.
            let name = match leader.clone() {
                Some(n) => n,
                None => match self.refresh_leader().await {
                    Some(n) => {
                        leader = Some(n.clone());
                        n
                    }
                    None => {
                        last = err("no live leader");
                        continue;
                    }
                },
            };
            let addr = (self.addr_of)(&name);
            let res = self
                .forwarder
                .client
                .post(format!("http://{addr}/own/{what}"))
                .header(proxy::PEER_HEADER, &self.forwarder.secret)
                .timeout(proxy::LEADER_TIMEOUT)
                .body(body.clone())
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => return Ok(r.text().await?),
                // The node we asked is not the leader — it never was, or it was just fenced and
                // demoted. Our name is stale; drop it so the next attempt re-reads.
                Ok(r) if r.status() == reqwest::StatusCode::MISDIRECTED_REQUEST => {
                    last = err(format!("own/{what}: {name} is not the leader"));
                    leader = None;
                }
                // Any other answer is about the request, not about who leads: retrying cannot change it.
                Ok(r) => return Err(err(format!("own/{what}: leader answered {}", r.status()))),
                Err(e) => {
                    last = e.into();
                    leader = None;
                }
            }
        }
        Err(last)
    }

    /// Re-read who leads. `None` means the lease is absent or expired — nobody can grant right
    /// now — and forgets the name we had. A store error says nothing about who leads, so it keeps
    /// what we had rather than forgetting a leader that is probably fine.
    async fn refresh_leader(&self) -> Option<String> {
        let os = self.ownership.object_store()?;
        match lease::read(os.as_ref()).await {
            Ok(Some(h)) if !lease::is_expired(&h.lease, self.now_ms()) => {
                self.note_live(&h.lease);
                Some(h.lease.node)
            }
            Ok(_) => {
                self.set_leader(None);
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "reading the leader lease");
                self.leader()
            }
        }
    }
```

`reqwest` is already a dependency of `kloudlite-git-app`. Delete the `leader_reachable` shim from Task 3.

- [ ] **Step 4: `/healthz` and the `/own/*` handlers**

In `bins/server/src/router/route.rs`:

```rust
/// Liveness/readiness. 503 when the object store has stopped answering, or when no live leader
/// exists — this node holds the lease, or it read a lease within `LEADER_TTL` that has not
/// expired. Readiness gates the public Service, and a node that knows nobody who can grant cannot
/// claim, so it would take traffic and 5xx it. Both are cached bits written by their own beats —
/// the probe costs nothing. Peer DNS is NOT gated on this (`publishNotReadyAddresses`), so
/// forwarding between nodes keeps working through a failover.
/// Same handler on both listeners: nothing in-repo probes the peer one.
pub(crate) async fn healthz(State(app): State<Arc<App>>) -> Response {
    if !app.store.healthy() {
        return (StatusCode::SERVICE_UNAVAILABLE, "object store unreachable").into_response();
    }
    if !app.leader_live() {
        return (StatusCode::SERVICE_UNAVAILABLE, "no live leader").into_response();
    }
    (
        StatusCode::OK,
        format!("ok ({} warm)", app.store.pool.warm_count()),
    )
        .into_response()
}
```

Replace the paragraph "**A follower answers 421 to all three.** ... quietly relaying would hide that." in the `/own/*` doc with:

```
/// **A node that does not hold the lease answers 421 to all four** — before the grant (it never
/// led) and after it (the grant hit SlateDB's fence and demoted it: the successor's writer is
/// already open). Either way the caller's lease read is stale; it re-reads `cluster/leader` and
/// asks again. Nothing is relayed: the lease is the only authority, and a relay would hide a
/// caller that reads it wrong.
```

Add below `two_lines`:

```rust
/// A grant's failure, on the wire. A grant that FENCED this node (`App::fenced_check`) has left it
/// demoted, and the honest answer is then 421 — "not the leader" — so the asker re-reads the
/// lease at once instead of reporting one bad grant and waiting a beat.
fn own_err(app: &App, e: kloudlite_git_core::Error) -> Response {
    if !app.is_leader() {
        return leader_only(app).expect("a demoted node is not the leader");
    }
    internal(e)
}
```

and in `own_claim`, `own_renew`, `own_release`, `own_draining` replace `Err(e) => internal(e),` with `Err(e) => own_err(&app, e),`. In `leader_only`, change the body string `"not the leader; ask pod zero"` to `"not the leader; read cluster/leader"`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-app 2>&1 | tail -5`
Expected: `11 passed`.

Run: `cargo test --test routing a_follower_refuses_to_decide_ownership the_peer_listener_serves_healthz an_unhealthy_node 2>&1 | tail -5`
Expected: all pass (if `a_follower_refuses_to_decide_ownership` compares the 421 body text, update it to `"read cluster/leader"`).

Run: `cargo test --workspace --locked 2>&1 | grep -E '^test result|FAILED' | sort | uniq -c` — all `ok`.

- [ ] **Step 6: Commit**

```bash
git add crates/app/src/lib.rs bins/server/src/router/route.rs
git commit -m "Re-read the leader lease on a misdirected or failed ask and gate readiness on a live leader"
```

---

### Task 5: Delete the name-based leader; wire the beats and the boot check

**Files:**
- Modify: `crates/storage/src/ownership/mod.rs` (delete `leader_of`, `servers`, `least_loaded` and their comments)
- Modify: `crates/storage/src/ownership/tests.rs` (delete `leader_of_picks_ordinal_zero`, `leader_of_rejects_names_without_an_ordinal`, `servers_exclude_the_leader`, `least_loaded_picks_the_emptiest_and_ignores_lapsed_entries`, `least_loaded_skips_a_draining_node_even_though_it_looks_emptiest`, `a_split_leader_leaves_every_server_ordinal_serving`)
- Modify: `tests/routing.rs` (delete `the_leader_does_not_grant_to_a_node_that_is_shutting_down`, lines 1352–1397)
- Modify: `bins/server/src/lanes.rs:7-83`
- Modify: `bins/server/src/main.rs` (fleet branch: `fleet_store_ok`)
- Modify: `tests/ws_e2e.sh:207`, `crates/core/src/err.rs:15` (comments naming `KLOUDLITE_GIT_LEADER`/`KLOUDLITE_GIT_REPLICAS`)

**Interfaces:**
- Consumes: `App::{election_tick, is_leader, prune_once, renew_once}`, `ownership::lease::LEADER_RENEW`, `config::fleet_store_ok`.
- Produces: `spawn_lease_tasks` runs an election task; nothing else exported changes.

- [ ] **Step 1: Write the failing check**

There is no unit under test here beyond deletion; the check is the grep. Run it now and expect hits:

Run: `grep -rn -e 'leader_of' -e 'fn servers' -e 'least_loaded' -e 'with_topology' -e 'KLOUDLITE_GIT_LEADER' -e 'KLOUDLITE_GIT_SERVER_PREFIX' -e 'KLOUDLITE_GIT_REPLICAS' crates bins tests --include='*.rs' --include='*.sh' | wc -l`
Expected: a number greater than 0.

- [ ] **Step 2: Delete the functions and their tests**

In `crates/storage/src/ownership/mod.rs` delete `pub fn leader_of` (and its doc), `pub fn servers` (and its long doc), `pub fn least_loaded` (and its doc). In `crates/storage/src/ownership/tests.rs` delete the six tests listed above. In `tests/routing.rs` delete `the_leader_does_not_grant_to_a_node_that_is_shutting_down` with its doc comment, and in the doc of `a_forward_to_a_departed_owner_recovers` drop the sentence "A holds another repo so that the leader's least-loaded pick lands on B, which makes the recovered route Local rather than another hop: both outcomes must work." (A still holds `alice/other`; leave the code.)

- [ ] **Step 3: The beats**

Replace the start of `spawn_lease_tasks` in `bins/server/src/lanes.rs` (through the renew task) and the prune task at its end:

```rust
/// Election, renewal, and pruning on the lease holder — the background halves of the lifecycle
/// invariant. The work itself lives on `App`; these are only the clocks.
pub fn spawn_lease_tasks(app: Arc<App>) {
    use crate::ownership::lease::LEADER_RENEW;
    use crate::ownership::{LEASE_TTL, RENEW_EVERY};
    const CHECKPOINT_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
    const CHECKPOINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    // The election beat, alone in its task: the holder renews the leader lease, everyone else
    // reads who holds it and takes it when it lapses. Nothing below may delay it — a beat that
    // slips past LEADER_TTL is a leader that loses the lease while healthy.
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LEADER_RENEW).await;
            if let Err(e) = a.election_tick().await {
                metrics::counter!("ownership_election_failures_total").increment(1);
                tracing::warn!(error = %e, "election tick");
            }
        }
    });

    // Renewal runs ALONE. (existing comment, unchanged) ...
    let a = app.clone();
    tokio::spawn(async move {
        // (existing body, unchanged — `checkpoint` is a no-op on a follower, so this beat is
        // already "on whoever writes" without being started or stopped)
    });

    // (the five `lane(...)` calls, unchanged)

    // Prune on whoever holds the lease. Gated per beat rather than started on promotion: a beat
    // that checks `is_leader()` is the same behaviour with nothing to start, stop, or leak.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LEASE_TTL).await;
            if !app.is_leader() {
                continue;
            }
            if let Err(e) = app.prune_once().await {
                tracing::warn!(error = %e, "pruning ownership");
            }
        }
    });
}
```

Delete the old `if !app.is_leader() { return; }` guard and the prune task it guarded. Update the module doc's first line to "lease election and renewal/checkpointing".

- [ ] **Step 4: The boot check and stray comments**

In `bins/server/src/main.rs` fleet branch, before `let me = need("KLOUDLITE_GIT_SELF")?;`:

```rust
        // The lease that elects the map's writer is a conditional put; a backend without them
        // cannot fence a stale leader, so it is refused here rather than found out in a failover.
        kloudlite_git_server::config::fleet_store_ok(&env("KLOUDLITE_GIT_S3_URL", ""))?;
```

Update the `serve()` doc comment ("Nothing is elected here" is false now): "Which node serves a repo is the ownership map's decision; which node WRITES the map is elected by lease (`App::election_tick`)." In `tests/ws_e2e.sh:207` change `(no KLOUDLITE_GIT_PEER_SVC/KLOUDLITE_GIT_LEADER)` to `(no KLOUDLITE_GIT_PEER_SVC)`. In `crates/core/src/err.rs:15` change the example `KLOUDLITE_GIT_REPLICAS` check to `the KLOUDLITE_GIT_S3_URL=file:// fleet check`.

- [ ] **Step 5: Verify**

Run: `grep -rn -e 'leader_of' -e 'fn servers' -e 'least_loaded' -e 'with_topology' -e 'KLOUDLITE_GIT_LEADER' -e 'KLOUDLITE_GIT_SERVER_PREFIX' -e 'KLOUDLITE_GIT_REPLICAS' -e 'LEADER_SILENCE' -e 'leader_reachable' crates bins tests --include='*.rs' --include='*.sh' | wc -l`
Expected: `0`.

Run: `cargo test --workspace --locked 2>&1 | grep -E '^test result|FAILED' | sort | uniq -c` — all `ok`.
Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3` — no new warnings in touched files (`unused import` for `LEASE_TTL`/`RENEW_EVERY` would be one; both are still used).

- [ ] **Step 6: Commit**

```bash
git add crates/storage/src/ownership/mod.rs crates/storage/src/ownership/tests.rs tests/routing.rs bins/server/src/lanes.rs bins/server/src/main.rs tests/ws_e2e.sh crates/core/src/err.rs
git commit -m "Delete the name-based leader and run the election beat on every node"
```

---

### Task 6: The fleet harness elects, and three failover tests

**Files:**
- Modify: `tests/routing.rs` (`LEADER` doc, `node()`, `fleet()` doc; three new tests)

**Interfaces:**
- Consumes: `App::{election_tick, is_leader, leader, leader_epoch, leader_live, advance_clock, grant_claim, claim, owner}`, `OwnershipStore::is_writer`, `lease::LEADER_TTL`.
- Produces: nothing exported; the harness contract "the node started first leads" is stated on `LEADER`.

- [ ] **Step 1: Restate the harness**

Replace the `LEADER` doc comment and the constant:

```rust
/// The node every fleet starts FIRST, which is why it holds the lease: `node()` runs one election
/// beat before serving, and the first beat on an empty store wins. Nothing else is special about it.
const LEADER: &str = "kloudlite-git-0";
```

In `node()` the doc becomes "**Start `LEADER` first** — the first node to tick takes the lease, and every later one reads who holds it." Confirm the body has (from Task 3) exactly:

```rust
    let ownership = OwnershipStore::open(os);
    ...
    let app = Arc::new(App::new(store.clone(), Arc::new(ownership), name.into(), Arc::new(move |n: &str| { ... }), SECRET.into()));
    // One election beat before serving, and no loop: renewal cadence is lanes.rs's, not what these
    // tests prove. A test that needs a failover advances a follower's clock past LEADER_TTL and
    // ticks it by hand — deterministic, and ten seconds faster than waiting.
    app.election_tick().await.unwrap();
```

Update `fleet()`'s doc: "A fleet of `n` nodes, `kloudlite-git-0` first — start it first, and it leads."

- [ ] **Step 2: Write the tests (they fail against nothing new — they document the harness — but write them before touching anything else and run them)**

Append to `tests/routing.rs`:

```rust
/// Three nodes, one store: exactly one takes the lease, and every node reads the same holder.
#[tokio::test(flavor = "multi_thread")]
async fn a_fleet_elects_exactly_one_leader() {
    let e = common::env().await;
    let f = fleet(3);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-git-1", &f).await;
    let c = node(e.store.os.clone(), "kloudlite-git-2", &f).await;
    let leaders: Vec<String> =
        [&a, &b, &c].iter().filter(|n| n.app.is_leader()).map(|n| n.app.self_name.clone()).collect();
    assert_eq!(leaders, vec![LEADER.to_string()], "started first, took the lease first");
    for n in [&a, &b, &c] {
        assert_eq!(n.app.leader().as_deref(), Some(LEADER));
        assert!(n.app.leader_live());
    }
    assert!(a.app.ownership.is_writer().await);
    assert!(!b.app.ownership.is_writer().await && !c.app.ownership.is_writer().await);
    // Another beat on a follower changes nothing: the lease is live and not its own.
    b.app.election_tick().await.unwrap();
    assert!(!b.app.is_leader());
    // Nor does a renewal on the holder change the epoch.
    a.app.election_tick().await.unwrap();
    assert_eq!(a.app.leader_epoch(), 1);
}

/// The leader dies: its lease stops renewing and lapses, a peer takes it with the next epoch, and
/// a claim that first asks the dead leader by its stale name still succeeds inside the claim
/// budget. The dead leader's own late write is refused: SlateDB fenced its writer the moment the
/// successor opened the map, the fence demoted it, and its `/own/*` answers 421 from then on.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_leader_is_replaced_and_its_late_write_is_refused() {
    use kloudlite_git_storage::ownership::lease::LEADER_TTL;
    use kloudlite_git_storage::ownership::Grant;
    let e = common::env().await;
    let f = fleet(3);
    let zero = node(e.store.os.clone(), LEADER, &f).await;
    let one = node(e.store.os.clone(), "kloudlite-git-1", &f).await;
    let two = node(e.store.os.clone(), "kloudlite-git-2", &f).await;
    e.store.create_repo("alice", "web").await.unwrap();
    assert!(zero.app.is_leader() && !one.app.is_leader() && !two.app.is_leader());

    // Zero "dies": it never ticks again, so its lease lapses. Seen from ONE's clock it already has.
    one.app.advance_clock(LEADER_TTL + std::time::Duration::from_millis(1));
    one.app.election_tick().await.unwrap();
    assert!(one.app.is_leader(), "an expired lease is taken by the next node to look");
    assert_eq!(one.app.leader_epoch(), 2);

    // TWO still names zero, and zero still believes it leads — nothing has told it otherwise. Its
    // grant hits the fence, it demotes, and it answers 421; TWO re-reads the lease and lands on ONE.
    assert_eq!(two.app.leader().as_deref(), Some(LEADER));
    match two.app.claim("alice/web").await.unwrap() {
        Grant::Granted(en) => assert_eq!(en.node, "kloudlite-git-2"),
        g => panic!("expected a grant, got {g:?}"),
    }
    assert_eq!(two.app.leader().as_deref(), Some("kloudlite-git-1"), "the stale name was replaced by the lease");
    assert_eq!(one.app.owner("alice/web").await.unwrap().unwrap().node, "kloudlite-git-2");
    assert!(!zero.app.is_leader(), "the fenced grant demoted it");
    assert!(!zero.app.ownership.is_writer().await);

    // And a write from the old leader after that is refused in-process, before any storage.
    let late = zero.app.grant_claim("alice/late", "kloudlite-git-2", false).await;
    assert!(late.as_ref().is_err_and(|e| e.to_string().contains("not the leader")), "{late:?}");
}

/// `/healthz` follows the lease: ready while a live leader exists, un-ready while nobody holds a
/// live lease, ready again once somebody does — and "somebody" may be this node.
#[tokio::test(flavor = "multi_thread")]
async fn healthz_is_unready_only_while_no_leader_lives() {
    use kloudlite_git_storage::ownership::lease::LEADER_TTL;
    let e = common::env().await;
    let f = fleet(2);
    let _zero = node(e.store.os.clone(), LEADER, &f).await;
    let one = node(e.store.os.clone(), "kloudlite-git-1", &f).await;
    let c = client().await;
    let healthz = |n: &Node| {
        let url = format!("http://{}/healthz", n.peer);
        let c = c.clone();
        async move { c.get(url).header(kloudlite_git_core::peer::PEER_HEADER, SECRET).send().await.unwrap().status() }
    };
    assert_eq!(healthz(&one).await, 200, "a live leader exists");

    // Zero stops renewing (it never ticks in this harness); on ONE's clock the lease has lapsed.
    one.app.advance_clock(LEADER_TTL + std::time::Duration::from_millis(1));
    assert_eq!(healthz(&one).await, 503, "no live leader: a node that cannot claim must not take traffic");

    one.app.election_tick().await.unwrap();
    assert!(one.app.is_leader());
    assert_eq!(healthz(&one).await, 200);
}
```

- [ ] **Step 3: Run them**

Run: `cargo test --test routing a_fleet_elects a_dead_leader healthz_is_unready 2>&1 | tail -8`
Expected: `3 passed`. If `a_dead_leader...` fails at `two.app.claim` with `own/claim: leader answered 500 Internal Server Error`, the fence did not surface on zero's `get`/`put` — check that Task 4's `own_err` is wired into `own_claim` and that Task 3 wraps the `get` in `fenced_check`.

Run: `cargo test --test routing 2>&1 | tail -3` — the whole file green.

- [ ] **Step 4: Commit**

```bash
git add tests/routing.rs
git commit -m "Prove a fleet elects one leader and replaces a dead one within the claim budget"
```

---

### Task 7: README — the ownership map has an elected writer

**Files:**
- Modify: `README.md:36-38` (diagram), `:85-86` (components table), `:134-135` (source-of-truth rule)

**Interfaces:** none (docs).

- [ ] **Step 1: Edit**

Diagram: replace

```
    LEAD[kloudlite-git-leader-0<br/>StatefulSet, ownership map writer]
    SRV[kloudlite-git-srv-0..2<br/>StatefulSet, holds repo/image/vol DBs]
    SRV <--> LEAD
```

with

```
    SRV[kloudlite-git-srv-0..2<br/>StatefulSet, holds repo/image/vol DBs;<br/>one of them holds the leader lease and writes the ownership map]
```

Components table: in the "Server tier" row change `AKS, StatefulSets `kloudlite-git-leader` (1) and `kloudlite-git-srv` (3);` to `AKS, StatefulSet `kloudlite-git-srv` (3);` and append to its "Owns" cell `; the ownership map, on whichever pod holds the lease at `cluster/leader``. Delete the "Leader" row.

Source-of-truth rule: replace the two-line bullet beginning `- **The leader is the only writer of the ownership map**` with

```
- **The ownership map has one writer, elected.** The pod holding the lease at `cluster/leader`
  (conditional puts in the object store — `crates/storage/src/ownership/lease.rs`, TTL 10 s,
  renewed every 3 s) opens `cluster/ownership` as the writer; the lease epoch is checked on every
  map write and SlateDB's writer fence is the backstop. Any `kloudlite-git-srv` pod may lead; a dead
  leader is replaced within about 15 s with no operator.
```

- [ ] **Step 2: Verify**

Run: `grep -n 'kloudlite-git-leader\|KLOUDLITE_GIT_LEADER' README.md | wc -l` — expect `0`.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "Describe the elected ownership-map writer in the README"
```

---

### Task 8: Deploy — one StatefulSet, one apply; RECOVERY, CLAUDE.md, alerts; the migration

**Files:**
- Delete: `deploy/kloudlite-git-leader.yaml`
- Modify: `deploy/kloudlite-git.yaml` (header, Namespace, StatefulSet comment, env, probe comments, Service comments), `deploy/roll.sh`, `deploy/pin.sh`, `deploy/RECOVERY.md`, `deploy/alerts.md:13`, `deploy/BACKUPS.md` (consumer columns), `deploy/k3s/README.md:131`, `CLAUDE.md` ("The one invariant" and "Deploying" paragraphs)

**Interfaces:** none (manifests and docs). Every manifest must parse; both scripts must pass `bash -n`.

- [ ] **Step 1: Move the Namespace and delete the leader file**

`git rm deploy/kloudlite-git-leader.yaml`. Replace the first two comment lines of `deploy/kloudlite-git.yaml` with

```yaml
# Everything on AKS. The ownership map's writer is elected by lease (`ownership::lease`), so there
# is no leader StatefulSet, no order between the objects here, and `deploy/roll.sh` is one apply.
apiVersion: v1
kind: Namespace
metadata:
  name: kloudlite-git
---
```

(the Namespace object exactly as it was at the top of the deleted file), keeping the "Stable identity per pod" paragraph that follows.

- [ ] **Step 2: The StatefulSet**

Replace the `# THE SERVERS. ...` comment block (through `# both StatefulSets without addr_of having to learn about any of this.`) with:

```yaml
  # Every ordinal holds repositories, and every ordinal is a candidate for the ownership map's
  # writer: whichever pod holds the lease at `cluster/leader` opens the map, the rest follow it
  # read-only. There is no preferred ordinal — the object store's conditional put decides.
```

In `env:` delete the comment block `# WHO WRITES THE MAP. ... whichever one was legitimately serving.`, the `KLOUDLITE_GIT_LEADER` and `KLOUDLITE_GIT_SERVER_PREFIX` entries, the comment block `# How many serving pods the leader may hand a repo to ... holds no repositories.` and the `KLOUDLITE_GIT_REPLICAS` entry; change the `KLOUDLITE_GIT_SELF` trailing comment to `# the stable pod name, which the lease records as the holder`. In the startup-probe comment, change "the leader replays the ownership map's WAL before it binds :8080" to "a pod that wins the lease replays the ownership map's WAL before it can grant" and "a healthy leader now starts in about 30 seconds" to "a healthy pod now takes the writer in about 30 seconds". On the headless Service, change "left the leader unreachable for ~20s after a restart" to "left a restarted pod unreachable to its peers for ~20s"; on `kloudlite-git-http` and `kloudlite-git-lb`, replace the "Servers only: the leader holds no repositories … depend on that." comments with `# Every pod serves; the role selector is kept so a future non-serving role can opt out.`.

- [ ] **Step 3: Scripts**

`deploy/roll.sh`:

```bash
#!/usr/bin/env bash
# Roll the AKS side. One apply: the ownership map's writer is elected by lease
# (`ownership::lease`), so there is no leader pod to roll first and no order between the
# StatefulSet and the Deployments — a srv pod that goes down mid-roll takes its lease with it,
# and a peer holds the writer inside LEADER_TTL plus one tick. The rollout waits say when it is done.
set -euo pipefail
cd "$(dirname "$0")"
kubectl apply -f kloudlite-git.yaml -f kloudlite-git-web.yaml
kubectl -n kloudlite-git rollout status statefulset/kloudlite-git-srv --timeout=900s
for d in kloudlite-git-api kloudlite-git-worker kloudlite-git-web; do
  kubectl -n kloudlite-git rollout status "deployment/$d" --timeout=300s
done
echo "AKS rolled. The k3s side is separate: kubectl apply -f deploy/k3s/agent-daemonset.yaml -f deploy/k3s/gateway.yaml with that cluster's kubeconfig."
```

`deploy/pin.sh`: in the contract comment change `That is <sha>: kloudlite-git-leader.yaml, kloudlite-git.yaml (srv, api, worker), k3s/agent-daemonset.yaml, k3s/gateway.yaml.` to `That is <sha>: kloudlite-git.yaml (srv, api, worker), k3s/agent-daemonset.yaml, k3s/gateway.yaml.`; in the `perl -pi -e` line drop `kloudlite-git-leader.yaml`; in the final heredoc change `# AKS: leader, wait, then the rest` to `# AKS: one apply, then the rollout waits`.

- [ ] **Step 4: RECOVERY.md, alerts, backups, k3s README**

`deploy/RECOVERY.md`: in the Secrets table replace every `leader, srv` with `srv`. In A.3 replace the `deploy/roll.sh` line with `deploy/roll.sh          # one apply; the srv StatefulSet elects its own map writer` and the three verify lines about the leader with:

```sh
kubectl -n kloudlite-git logs -l role=server --tail=500 | grep -E 'lease: leading|newer DB client'
#   want exactly ONE pod logging "lease: leading" (and "opened as WRITER" beside it), and NO
#   "newer DB client" on a settled fleet — that line means a demoted leader wrote after it lost
#   the lease, which the epoch check is supposed to stop first
kubectl -n kloudlite-git get endpoints kloudlite-git-lb -o jsonpath='{range .subsets[*].addresses[*]}{.targetRef.name}{"\n"}{end}'
#   every srv pod
```

In A.5 change `kubectl -n kloudlite-git logs kloudlite-git-leader-0 | grep -iE 'checkpoint|timed out'` to `kubectl -n kloudlite-git logs -l role=server | grep -iE 'checkpoint|timed out'` (only the lease holder logs it). Add a new section before "## What is still manual after all of this":

```markdown
## Migrating from the named leader (one-time)

Before the election build, `kloudlite-git-leader-0` held the map's writer by name. The election build
takes it by lease, and the two overlap safely in exactly one order:

1. `kubectl apply -f deploy/kloudlite-git.yaml` with the new pins, and wait:
   `kubectl -n kloudlite-git rollout status statefulset/kloudlite-git-srv`. The old leader keeps running.
   The first new srv pod takes `cluster/leader` and opens the writer, which FENCES the old leader;
   from then on its `/own/*` handlers fail, so the old-build srv pods still waiting to roll cannot
   claim or renew until their turn — the same window a leader roll used to cost, once.
2. Only then: `kubectl -n kloudlite-git delete statefulset/kloudlite-git-leader pdb/kloudlite-git-leader`.
   Deleting it first would leave every old-build pod with nobody to ask for the whole roll.
3. Verify as in A.3: exactly one `lease: leading`, no `newer DB client` afterwards.

The Namespace object moved from the deleted `kloudlite-git-leader.yaml` into `kloudlite-git.yaml`;
`kubectl apply` never prunes, so nothing about the namespace changes.
```

`deploy/alerts.md:13`: replace the `LeaderUnreachable` row with

```
| **NoLeader** | `sum(ownership_is_leader) != 1` for 2m | Zero: nobody holds the lease, so no claim in the fleet succeeds; two: the epoch check failed and the fence is all that stands between two writers. `ownership_demotions_total` rising with it says which pod keeps losing the lease. |
```

`deploy/BACKUPS.md`: replace every `leader, srv` / `leader and srv` / `roll leader+srv` with `srv` / `srv` / `roll srv` (`grep -n leader deploy/BACKUPS.md` lists the five lines).

`deploy/k3s/README.md:131`: `#    deploy/roll.sh applies the leader first, then this).` → `#    deploy/roll.sh applies it in one go).`

- [ ] **Step 5: CLAUDE.md**

In "## The one invariant everything hangs off" replace the sentences from `Pod `kloudlite-git-leader-0` is the leader by *name*` through `those live on `kloudlite-git-srv-{0..N}`.` with:

```
The ownership map has one writer, **elected**: every `kloudlite-git-srv` pod runs `App::election_tick`
every 3 s, and the pod holding the lease at `cluster/leader` (`crates/storage/src/ownership/lease.rs`
— conditional puts only, TTL 10 s) opens the map as WRITER (`OwnershipStore::promote`). The lease
epoch is checked under `leader_lock` on every map write, a fenced write demotes, and SlateDB's own
writer fence is the backstop; followers re-read the lease when the node they asked answers 421 or is
unreachable. There is no leader pod, no `KLOUDLITE_GIT_LEADER`, and no preferred ordinal; a dead leader
is replaced in ~15 s. A multi-node `file://` store is refused at boot (`LocalFileSystem` has no
conditional update).
```

In "## Deploying" replace `→ commit → `deploy/roll.sh` (leader first, wait, then srv/api/worker and web; the k3s side is applied by hand per `deploy/k3s/README.md`). Never `kubectl apply` the leader and srv files in one command: the leader's restart overlapping the first srv ordinal's re-claim is the window in which claims fail, which is why the leader lives in its own file. The StatefulSet roll moves DB ownership between nodes;` with `→ commit → `deploy/roll.sh` (one apply, then the rollout waits; the k3s side is applied by hand per `deploy/k3s/README.md`). The StatefulSet roll moves DB ownership between nodes, and the map's writer moves with the lease when the holder rolls (≤ one TTL plus one tick);`.

- [ ] **Step 6: Verify**

Run: `for f in deploy/kloudlite-git.yaml deploy/kloudlite-git-web.yaml; do ruby -ryaml -e 'YAML.load_stream(File.read(ARGV[0])) { |d| puts d["kind"] }' "$f" | sort | uniq -c; done`
Expected: every kind listed, `Namespace` once, no `kloudlite-git-leader` anywhere; no parse error.
Run: `bash -n deploy/roll.sh deploy/pin.sh && echo ok` — `ok`.
Run: `grep -rn 'kloudlite-git-leader\|KLOUDLITE_GIT_LEADER\|KLOUDLITE_GIT_REPLICAS\|KLOUDLITE_GIT_SERVER_PREFIX' deploy CLAUDE.md README.md | grep -v 'RECOVERY.md'` — expect `0` lines (RECOVERY.md keeps the name in the migration section on purpose).
Run: `ls deploy/kloudlite-git-leader.yaml 2>&1` — `No such file`.
Run: `cargo test --workspace --locked 2>&1 | grep -E '^test result|FAILED' | sort | uniq -c` — all `ok` (nothing in `crates/` changed; this is the final gate).

- [ ] **Step 7: Commit**

```bash
git add -A deploy CLAUDE.md
git commit -m "Deploy one StatefulSet with an elected map writer and delete the leader pod" -m "Migration order for the running fleet, as written into deploy/RECOVERY.md: roll kloudlite-git-srv to this build FIRST (the first new pod takes the lease and fences the old leader), wait for the rollout, and only then delete statefulset/kloudlite-git-leader and pdb/kloudlite-git-leader."
```

---

## Self-review

**Spec coverage.** The lease (object, body, conditional puts, constants, helper) — Task 1. Election loop steps 1–3, ties by the store — Task 3 `election_tick`. `KLOUDLITE_GIT_LEADER`/`leader_of` deleted, `App::leader()` from the lease, `set_leader` — Tasks 3 and 5. Promotion opens the writer and fences, demotion closes and reopens as reader, both beats on the writer — Tasks 2, 3, 5. Two demotion triggers plus the fence path through one function (`demote_locked`) — Task 3. Epoch on every map write, in-process check, entries unchanged — Task 3 (`writing_epoch`, `fenced_check`). Followers: 421 / connect failure re-read, the holder never forwards to itself (`claim_inner`'s `is_leader()` branch is untouched) — Task 4. `/healthz` on a live leader — Task 4. Leader serves like every other node; `servers()`, `leader_of`, `with_topology`, prefix split deleted — Tasks 3 and 5. Deploy: leader yaml and PDB deleted, srv PDB kept, `roll.sh` one apply, headless Service keeps `publishNotReadyAddresses`, env and `pin.sh` — Task 8. Migration order in RECOVERY.md and the last commit — Task 8. Failure modes: leader dies (Task 6 test), lost store access (refused renewal → demote, Task 3), two believe they lead (Task 2 fence test + Task 3 `a_fenced_map_write_demotes` + Task 6), `file://` refused (Task 1), clock skew (bounded by TTL; `advance_clock` in Task 6 exercises exactly the "reader takes early, fenced by SlateDB" case). Tests listed in the spec: lease unit tests incl. concurrent takers (Task 1), promote/demote/fence/stale-epoch in `crates/app` (Task 3 — the "fake lease" is the in-memory object store driven through the real helper, which is stronger than a fake), the three `tests/routing.rs` cases (Task 6), manifests parse and `bash -n` (Task 8). One deviation, stated in Global Constraints: no lease resignation at SIGTERM.

**Placeholder scan.** No "TBD", "similar to", or "add error handling". Two places say "unchanged"/"mechanical change" for bodies that are copied verbatim from the current file (`put_many`/`delete`/`close`/`set_draining`/`draining`/`all`/`checkpoint` in Task 2; the renew task and the five `lane()` calls in Task 5) — each names the exact edit and the line to keep.

**Type consistency.** `lease::take(os: &dyn ObjectStore, node: &str, now_ms: u64, current: Option<&Held>) -> Result<Option<Held>>` and `renew(os, held: &Held, now_ms)` are used with those shapes in Task 3 (`take(os.as_ref(), &self.self_name, now, c.as_ref())`, `renew(os.as_ref(), &c, now)`); `Held.version: UpdateVersion` is public because Task 3 clones `Held` whole. `OwnershipStore::open(os)` is synchronous everywhere (Tasks 2–6); `promote() -> Result<()>` and `demote()` (no result) match their callers in `App::promote`/`demote_locked`. `App::new` has five parameters in Tasks 3–6 and in `main.rs`. `leader() -> Option<String>` is compared with `.as_deref()` in every test. `fenced_check` wraps `get`, `put`, `put_many`, `delete`, `all`, and Task 6's failover test depends on the `get` wrap. `own_err` (Task 4) is what turns a fenced grant into the 421 that Task 6 asserts via `two.app.leader()` changing.
