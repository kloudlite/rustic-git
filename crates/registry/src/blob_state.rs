use crate::{store::Digest, Result};
use slatedb::object_store::{path::Path, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};

const STATE_PREFIX: &str = "blob-state";
const GENERATION_PREFIX: &str = "blob-generations";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobGeneration {
    pub physical_key: String,
    #[serde(default)]
    pub pins: Vec<String>,
    #[serde(default)]
    pub installed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetiredGeneration {
    pub physical_key: String,
    /// When this generation was retired, millis — `0` (the serde default) means "written before
    /// this field existed, age unknown". `sweep_owner`'s `sweep_retired` treats that as "stamp it
    /// this round, delete nothing" (keep-bias), never as 1970 the way the old `installed_at`
    /// fallback did for the same shape of gap.
    #[serde(default)]
    pub retired_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobRecord {
    pub nonce: String,
    pub active: Option<BlobGeneration>,
    #[serde(default)]
    pub retired: Vec<RetiredGeneration>,
}

#[derive(Debug, Clone)]
struct Loaded {
    record: BlobRecord,
    version: UpdateVersion,
}

pub fn state_path(owner: &str, d: &Digest) -> Path {
    Path::from(format!("{STATE_PREFIX}/{owner}/{}/{}", d.algo, d.hex))
}

pub fn generation_path(owner: &str, d: &Digest, generation: &str) -> Path {
    Path::from(format!("{GENERATION_PREFIX}/{owner}/{}/{}/{}", d.algo, d.hex, generation))
}

pub fn canonical_path(owner: &str, d: &Digest) -> Path {
    crate::store::blob_path(owner, d)
}

pub fn owner_prefix(owner: &str) -> Path {
    Path::from(format!("{STATE_PREFIX}/{owner}/"))
}

fn nonce() -> String {
    crate::hex(&rand::random::<[u8; 16]>())
}

pub fn publication_id() -> String {
    format!("{}@{}", nonce(), chrono::Utc::now().timestamp_millis())
}

/// The time travels IN the publication string, not as a separate field on the pin (`pins` stays
/// `Vec<String>` — a record the fleet already wrote must keep parsing). `pin`/`unpin` match this
/// exact string, so nothing about how a pin is stored or released changes; only the sweep, through
/// `pin_is_live`, reads the suffix.
pub fn manifest_publication(owner: &str, name: &str, d: &Digest) -> String {
    format!("manifest/{owner}/{name}/{d}#{}@{}", nonce(), chrono::Utc::now().timestamp_millis())
}

/// Is this pin still young enough to protect its blob? `rsplit_once('@')` rather than `split`,
/// because the publication itself may contain `@`-free segments before the time — only the LAST
/// `@` is the time's own separator. A pin with no parsable suffix at all is one an older build
/// wrote (before this field existed) and is always live: uncertainty here means keep, the same
/// rule the sweep applies everywhere else in this crate.
pub fn pin_is_live(pin: &str, cutoff_millis: i64) -> bool {
    pin.rsplit_once('@').and_then(|(_, t)| t.parse::<i64>().ok()).is_none_or(|t| t >= cutoff_millis)
}

/// Whether ANY pin on this generation still protects it — what the sweep asks instead of
/// `pins.is_empty()`, so a pin from a push that died mid-flight (leaked, pre-R-2) does not block
/// collection forever once it ages past the grace window.
pub fn has_live_pin(generation: &BlobGeneration, cutoff_millis: i64) -> bool {
    generation.pins.iter().any(|p| pin_is_live(p, cutoff_millis))
}

/// `5..25ms * (attempt+1)`: short enough that 8 attempts still finish well inside the CAS retry
/// budget, long enough (and jittered, not fixed) that 8 tasks retrying a lockstep CAS against one
/// record stop re-colliding in the same tick — the fix for `blob_state::pin`'s exhaustion under
/// concurrent pushes sharing a base layer (R-2 / review Rust Important).
async fn backoff(attempt: u32) {
    let ms = 5 + rand::random::<u64>() % (20 * (attempt as u64 + 1));
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

fn encode(record: &BlobRecord) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(record)?)
}

async fn create(os: &dyn ObjectStore, path: &Path, record: &BlobRecord) -> Result<bool> {
    match os
        .put_opts(path, PutPayload::from(encode(record)?), PutOptions { mode: PutMode::Create, ..Default::default() })
        .await
    {
        Ok(_) => Ok(true),
        Err(slatedb::object_store::Error::AlreadyExists { .. } | slatedb::object_store::Error::Precondition { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn update(os: &dyn ObjectStore, path: &Path, record: &BlobRecord, current: &UpdateVersion) -> Result<bool> {
    match os
        .put_opts(
            path,
            PutPayload::from(encode(record)?),
            PutOptions { mode: PutMode::Update(current.clone()), ..Default::default() },
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(slatedb::object_store::Error::Precondition { .. } | slatedb::object_store::Error::AlreadyExists { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn read(os: &dyn ObjectStore, path: &Path) -> Result<Option<Loaded>> {
    match os.get(path).await {
        Ok(r) => {
            let version = UpdateVersion { e_tag: r.meta.e_tag.clone(), version: r.meta.version.clone() };
            Ok(Some(Loaded { record: serde_json::from_slice(&r.bytes().await?)?, version }))
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn load(os: &dyn ObjectStore, owner: &str, d: &Digest) -> Result<Option<Loaded>> {
    let path = state_path(owner, d);
    if let Some(found) = read(os, &path).await? {
        return Ok(Some(found));
    }
    let installed_at = match os.head(&canonical_path(owner, d)).await {
        Ok(meta) => meta.last_modified.timestamp_millis(),
        Err(slatedb::object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let record = BlobRecord { nonce: nonce(), active: Some(BlobGeneration { physical_key: canonical_path(owner, d).to_string(), pins: Vec::new(), installed_at }), retired: Vec::new() };
    if create(os, &path, &record).await? {
        return read(os, &path).await;
    }
    read(os, &path).await
}

pub async fn resolve(os: &dyn ObjectStore, owner: &str, d: &Digest) -> Result<Option<Path>> {
    Ok(load(os, owner, d).await?.and_then(|l| l.record.active.map(|a| Path::from(a.physical_key))))
}

pub async fn exists(os: &dyn ObjectStore, owner: &str, d: &Digest) -> Result<bool> {
    let Some(path) = resolve(os, owner, d).await? else { return Ok(false) };
    match os.head(&path).await {
        Ok(_) => Ok(true),
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn new_generation(owner: &str, d: &Digest) -> (String, Path) {
    let id = nonce();
    (id.clone(), generation_path(owner, d, &id))
}

pub async fn install(os: &dyn ObjectStore, owner: &str, d: &Digest, physical_key: &str) -> Result<()> {
    let path = state_path(owner, d);
    for attempt in 0..8 {
        let current = load(os, owner, d).await?;
        let next = match current {
            Some(mut loaded) => {
                if loaded.record.active.as_ref().is_some_and(|a| a.physical_key == physical_key) {
                    return Ok(());
                }
                if let Some(old) = loaded.record.active.take() {
                    loaded.record.retired.push(RetiredGeneration { physical_key: old.physical_key, retired_at: chrono::Utc::now().timestamp_millis() });
                    loaded.record.active = Some(BlobGeneration { physical_key: physical_key.to_string(), pins: old.pins, installed_at: chrono::Utc::now().timestamp_millis() });
                } else {
                    loaded.record.active = Some(BlobGeneration { physical_key: physical_key.to_string(), pins: Vec::new(), installed_at: chrono::Utc::now().timestamp_millis() });
                }
                loaded.record.nonce = nonce();
                if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(()) }
                backoff(attempt).await;
                continue;
            }
            None => BlobRecord { nonce: nonce(), active: Some(BlobGeneration { physical_key: physical_key.to_string(), pins: Vec::new(), installed_at: chrono::Utc::now().timestamp_millis() }), retired: Vec::new() },
        };
        if create(os, &path, &next).await? { return Ok(()) }
        backoff(attempt).await;
    }
    Err(crate::err("blob state CAS retries exhausted"))
}

pub async fn pin(os: &dyn ObjectStore, owner: &str, d: &Digest, publication: &str) -> Result<bool> {
    let path = state_path(owner, d);
    for attempt in 0..8 {
        let Some(mut loaded) = load(os, owner, d).await? else { return Ok(false) };
        let Some(active) = loaded.record.active.as_mut() else { return Ok(false) };
        if !active.pins.iter().any(|p| p == publication) { active.pins.push(publication.to_string()); }
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(true) }
        backoff(attempt).await;
    }
    Err(crate::err("blob pin CAS retries exhausted"))
}

pub async fn unpin(os: &dyn ObjectStore, owner: &str, d: &Digest, publication: &str) -> Result<()> {
    let path = state_path(owner, d);
    for attempt in 0..8 {
        let Some(mut loaded) = load(os, owner, d).await? else { return Ok(()) };
        let Some(active) = loaded.record.active.as_mut() else { return Ok(()) };
        active.pins.retain(|p| p != publication);
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(()) }
        backoff(attempt).await;
    }
    Err(crate::err("blob unpin CAS retries exhausted"))
}

pub async fn retire(os: &dyn ObjectStore, owner: &str, d: &Digest) -> Result<Option<String>> {
    let path = state_path(owner, d);
    for attempt in 0..8 {
        let Some(mut loaded) = load(os, owner, d).await? else { return Ok(None) };
        let Some(active) = loaded.record.active.take() else { return Ok(None) };
        let physical_key = active.physical_key.clone();
        loaded.record.retired.push(RetiredGeneration { physical_key: physical_key.clone(), retired_at: chrono::Utc::now().timestamp_millis() });
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(Some(physical_key)) }
        backoff(attempt).await;
    }
    Err(crate::err("blob retire CAS retries exhausted"))
}

pub async fn candidates(os: &dyn ObjectStore, owner: &str) -> Result<Vec<(Digest, BlobRecord, UpdateVersion)>> {
    let mut out = Vec::new();
    let mut listing = os.list(Some(&owner_prefix(owner)));
    while let Some(meta) = futures::StreamExt::next(&mut listing).await {
        let meta = meta?;
        let location = meta.location.to_string();
        let Some(rest) = location.strip_prefix(&format!("{STATE_PREFIX}/{owner}/")) else { continue };
        let mut parts = rest.split('/');
        let (Some(algo), Some(hex), None) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some(d) = Digest::parse(&format!("{algo}:{hex}")) else { continue };
        let Some(loaded) = read(os, &meta.location).await? else { continue };
        out.push((d, loaded.record, loaded.version));
    }
    Ok(out)
}

/// `cutoff_millis`: a pin the sweep already decided is expired (`has_live_pin` false) must not
/// keep protecting the generation here — this is the same cutoff `sweep_owner` computed, passed
/// through rather than recomputed, so the two readings of "is this pin still live" cannot disagree
/// on the clock.
pub async fn retire_if_unpinned(
    os: &dyn ObjectStore,
    owner: &str,
    d: &Digest,
    expected: &UpdateVersion,
    cutoff_millis: i64,
) -> Result<Option<String>> {
    let path = state_path(owner, d);
    let Some(mut loaded) = read(os, &path).await? else { return Ok(None) };
    if loaded.version != *expected { return Ok(None) }
    let Some(active) = loaded.record.active.take() else { return Ok(None) };
    if has_live_pin(&active, cutoff_millis) { return Ok(None) }
    let physical_key = active.physical_key.clone();
    loaded.record.retired.push(RetiredGeneration { physical_key: physical_key.clone(), retired_at: chrono::Utc::now().timestamp_millis() });
    loaded.record.nonce = nonce();
    if update(os, &path, &loaded.record, expected).await? { Ok(Some(physical_key)) } else { Ok(None) }
}

pub async fn delete_retired(os: &dyn ObjectStore, owner: &str, d: &Digest, physical_key: &str) -> Result<()> {
    match os.delete(&Path::from(physical_key)).await {
        Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
        Err(e) => return Err(e.into()),
    }
    let path = state_path(owner, d);
    for attempt in 0..8 {
        let Some(mut loaded) = read(os, &path).await? else { return Ok(()) };
        loaded.record.retired.retain(|r| r.physical_key != physical_key);
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(()) }
        backoff(attempt).await;
    }
    Err(crate::err("retired blob cleanup CAS retries exhausted"))
}

/// Stamps a `retired_at` on a generation an OLDER build retired before this field existed
/// (`retired_at == 0`) — the sweep's own keep-bias: it does not delete an entry whose age it
/// cannot judge, but it also must not leave that entry unjudgeable forever, so this gives it a
/// clock the NEXT sweep can act on. Same CAS-retry-with-backoff shape as `delete_retired`, but no
/// object-store delete: the bytes are untouched.
pub async fn stamp_retired(os: &dyn ObjectStore, owner: &str, d: &Digest, physical_key: &str, retired_at: i64) -> Result<()> {
    let path = state_path(owner, d);
    for attempt in 0..8 {
        let Some(mut loaded) = read(os, &path).await? else { return Ok(()) };
        let Some(entry) = loaded.record.retired.iter_mut().find(|r| r.physical_key == physical_key) else { return Ok(()) };
        if entry.retired_at != 0 {
            // Already stamped by a racing sweep (or this one, retried after an update that
            // actually landed but answered as a version conflict) — nothing to do.
            return Ok(());
        }
        entry.retired_at = retired_at;
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(()) }
        backoff(attempt).await;
    }
    Err(crate::err("retired blob stamp CAS retries exhausted"))
}

pub async fn delete_active(os: &dyn ObjectStore, owner: &str, d: &Digest) -> Result<Option<String>> {
    let physical_key = retire(os, owner, d).await?;
    if let Some(key) = &physical_key {
        delete_retired(os, owner, d, key).await?;
    }
    Ok(physical_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::{memory::InMemory, ObjectStoreExt};
    use std::sync::Arc;

    fn digest() -> Digest {
        Digest::of(b"blob")
    }

    #[tokio::test]
    async fn legacy_objects_are_adopted_once() {
        let os = Arc::new(InMemory::new());
        let d = digest();
        os.put(&canonical_path("acme", &d), PutPayload::from("legacy")).await.unwrap();
        let path = resolve(os.as_ref(), "acme", &d).await.unwrap().unwrap();
        assert_eq!(path, canonical_path("acme", &d));
    }

    #[tokio::test]
    async fn reupload_uses_a_new_generation_and_preserves_it_after_old_cleanup() {
        let os = Arc::new(InMemory::new());
        let d = digest();
        let (_, old) = new_generation("acme", &d);
        os.put(&old, PutPayload::from("old")).await.unwrap();
        let old_key = old.to_string();
        install(os.as_ref(), "acme", &d, &old_key).await.unwrap();
        let (_, new) = new_generation("acme", &d);
        os.put(&new, PutPayload::from("new")).await.unwrap();
        let new_key = new.to_string();
        install(os.as_ref(), "acme", &d, &new_key).await.unwrap();
        delete_retired(os.as_ref(), "acme", &d, &old_key).await.unwrap();
        assert_eq!(resolve(os.as_ref(), "acme", &d).await.unwrap(), Some(new));
    }

    #[tokio::test]
    async fn pins_fence_generation_replacement_until_release() {
        let os = Arc::new(InMemory::new());
        let d = digest();
        let (_, old) = new_generation("acme", &d);
        os.put(&old, PutPayload::from("old")).await.unwrap();
        let old_key = old.to_string();
        install(os.as_ref(), "acme", &d, &old_key).await.unwrap();
        assert!(pin(os.as_ref(), "acme", &d, "manifest").await.unwrap());
        let (_, new) = new_generation("acme", &d);
        os.put(&new, PutPayload::from("new")).await.unwrap();
        let new_key = new.to_string();
        install(os.as_ref(), "acme", &d, &new_key).await.unwrap();
        assert_eq!(resolve(os.as_ref(), "acme", &d).await.unwrap(), Some(new.clone()));
        unpin(os.as_ref(), "acme", &d, "manifest").await.unwrap();
        assert!(candidates(os.as_ref(), "acme").await.unwrap().into_iter().all(|(_, record, _)| record.active.as_ref().is_none_or(|active| active.pins.is_empty())));
    }

    #[tokio::test]
    async fn a_stale_gc_snapshot_cannot_retire_after_pin_and_unpin() {
        let os = Arc::new(InMemory::new());
        let d = digest();
        let (_, generation) = new_generation("acme", &d);
        os.put(&generation, PutPayload::from("blob")).await.unwrap();
        let generation_key = generation.to_string();
        install(os.as_ref(), "acme", &d, &generation_key).await.unwrap();
        let snapshot = candidates(os.as_ref(), "acme").await.unwrap().pop().unwrap().2;
        pin(os.as_ref(), "acme", &d, "manifest").await.unwrap();
        unpin(os.as_ref(), "acme", &d, "manifest").await.unwrap();
        assert!(retire_if_unpinned(os.as_ref(), "acme", &d, &snapshot, i64::MAX).await.unwrap().is_none());
        assert!(resolve(os.as_ref(), "acme", &d).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_missed_pin_must_be_retried_after_a_generation_is_installed() {
        let os = Arc::new(InMemory::new());
        let d = digest();
        assert!(!pin(os.as_ref(), "acme", &d, "publication").await.unwrap());
        let (_, generation) = new_generation("acme", &d);
        os.put(&generation, PutPayload::from("blob")).await.unwrap();
        install(os.as_ref(), "acme", &d, generation.as_ref()).await.unwrap();
        assert!(pin(os.as_ref(), "acme", &d, "publication").await.unwrap());
        assert_eq!(resolve(os.as_ref(), "acme", &d).await.unwrap(), Some(generation));
    }

    /// R-2 T1: eight pushes sharing a base layer each pin it concurrently, 8-wide CAS contention on
    /// one record — before the fix (no backoff between retries) this fails intermittently with
    /// "blob pin CAS retries exhausted" because all 8 tasks retry in lockstep and keep re-colliding.
    /// Looped 20 times so the flake is deterministic pre-fix rather than a coin flip that might pass
    /// this run; the loop stays after the fix as the ongoing proof the jitter actually spreads
    /// retries out.
    // multi_thread: a current-thread runtime cooperatively serializes the 8 spawned tasks at
    // their own `.await` points, so the CAS collision this test exists to provoke barely happens
    // — real OS-thread concurrency is what actually races 8 loads against one record.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn eight_concurrent_pins_of_one_blob_all_succeed() {
        for _ in 0..20 {
            let os = Arc::new(InMemory::new());
            let d = digest();
            let (_, generation) = new_generation("acme", &d);
            os.put(&generation, PutPayload::from("blob")).await.unwrap();
            install(os.as_ref(), "acme", &d, generation.as_ref()).await.unwrap();

            let pins: Vec<_> = (0..8)
                .map(|i| {
                    let os = os.clone();
                    let d = d.clone();
                    tokio::spawn(async move { pin(os.as_ref(), "acme", &d, &format!("pub-{i}")).await })
                })
                .collect();
            for p in pins {
                assert!(p.await.unwrap().unwrap(), "a concurrent pin failed: CAS retries exhausted");
            }
            let record = candidates(os.as_ref(), "acme").await.unwrap().into_iter().find(|(dig, _, _)| *dig == d).unwrap().1;
            assert_eq!(record.active.as_ref().unwrap().pins.len(), 8, "not all 8 pins landed");

            let unpins: Vec<_> = (0..8)
                .map(|i| {
                    let os = os.clone();
                    let d = d.clone();
                    tokio::spawn(async move { unpin(os.as_ref(), "acme", &d, &format!("pub-{i}")).await })
                })
                .collect();
            for u in unpins {
                u.await.unwrap().unwrap();
            }
            let record = candidates(os.as_ref(), "acme").await.unwrap().into_iter().find(|(dig, _, _)| *dig == d).unwrap().1;
            assert!(record.active.as_ref().unwrap().pins.is_empty(), "not all 8 unpins landed");
        }
    }

    /// R-2 T2: `pin_is_live` — a pin older than the cutoff no longer protects; one at or after it
    /// does; a pin with no parsable `@time` (an older build's format) is always live — keep-bias.
    #[test]
    fn a_pin_older_than_the_cutoff_does_not_protect() {
        assert!(!pin_is_live("x#n@1000", 2000));
        assert!(pin_is_live("x#n@3000", 2000));
        assert!(pin_is_live("old-format-no-time", 2000));
    }
}
