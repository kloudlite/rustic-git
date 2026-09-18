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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetiredGeneration {
    pub physical_key: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    Busy,
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
    nonce()
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
    if os.head(&canonical_path(owner, d)).await.is_err() {
        return Ok(None);
    }
    let record = BlobRecord { nonce: nonce(), active: Some(BlobGeneration { physical_key: canonical_path(owner, d).to_string(), pins: Vec::new() }), retired: Vec::new() };
    if create(os, &path, &record).await? {
        return Ok(read(os, &path).await?);
    }
    read(os, &path).await
}

pub async fn resolve(os: &dyn ObjectStore, owner: &str, d: &Digest) -> Result<Option<Path>> {
    Ok(load(os, owner, d).await?.and_then(|l| l.record.active.map(|a| Path::from(a.physical_key))))
}

pub async fn exists(os: &dyn ObjectStore, owner: &str, d: &Digest) -> Result<bool> {
    let Some(path) = resolve(os, owner, d).await? else { return Ok(false) };
    Ok(os.head(&path).await.is_ok())
}

pub fn new_generation(owner: &str, d: &Digest) -> (String, Path) {
    let id = nonce();
    (id.clone(), generation_path(owner, d, &id))
}

pub async fn install(os: &dyn ObjectStore, owner: &str, d: &Digest, physical_key: &str) -> Result<std::result::Result<(), InstallError>> {
    let path = state_path(owner, d);
    for _ in 0..8 {
        let current = load(os, owner, d).await?;
        let next = match current {
            Some(mut loaded) => {
                if loaded.record.active.as_ref().is_some_and(|a| !a.pins.is_empty()) {
                    return Ok(Err(InstallError::Busy));
                }
                if loaded.record.active.as_ref().is_some_and(|a| a.physical_key == physical_key) {
                    return Ok(Ok(()));
                }
                if let Some(old) = loaded.record.active.take() {
                    loaded.record.retired.push(RetiredGeneration { physical_key: old.physical_key });
                }
                loaded.record.active = Some(BlobGeneration { physical_key: physical_key.to_string(), pins: Vec::new() });
                loaded.record.nonce = nonce();
                if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(Ok(())) }
                continue;
            }
            None => BlobRecord { nonce: nonce(), active: Some(BlobGeneration { physical_key: physical_key.to_string(), pins: Vec::new() }), retired: Vec::new() },
        };
        if create(os, &path, &next).await? { return Ok(Ok(())) }
    }
    Err(crate::err("blob state CAS retries exhausted"))
}

pub async fn pin(os: &dyn ObjectStore, owner: &str, d: &Digest, publication: &str) -> Result<bool> {
    let path = state_path(owner, d);
    for _ in 0..8 {
        let Some(mut loaded) = load(os, owner, d).await? else { return Ok(false) };
        let Some(active) = loaded.record.active.as_mut() else { return Ok(false) };
        if !active.pins.iter().any(|p| p == publication) { active.pins.push(publication.to_string()); }
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(true) }
    }
    Err(crate::err("blob pin CAS retries exhausted"))
}

pub async fn unpin(os: &dyn ObjectStore, owner: &str, d: &Digest, publication: &str) -> Result<()> {
    let path = state_path(owner, d);
    for _ in 0..8 {
        let Some(mut loaded) = load(os, owner, d).await? else { return Ok(()) };
        let Some(active) = loaded.record.active.as_mut() else { return Ok(()) };
        active.pins.retain(|p| p != publication);
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(()) }
    }
    Err(crate::err("blob unpin CAS retries exhausted"))
}

pub async fn retire(os: &dyn ObjectStore, owner: &str, d: &Digest) -> Result<Option<String>> {
    let path = state_path(owner, d);
    for _ in 0..8 {
        let Some(mut loaded) = load(os, owner, d).await? else { return Ok(None) };
        let Some(active) = loaded.record.active.take() else { return Ok(None) };
        let physical_key = active.physical_key.clone();
        loaded.record.retired.push(RetiredGeneration { physical_key: physical_key.clone() });
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(Some(physical_key)) }
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

pub async fn retire_if_unpinned(
    os: &dyn ObjectStore,
    owner: &str,
    d: &Digest,
    expected: &UpdateVersion,
) -> Result<Option<String>> {
    let path = state_path(owner, d);
    let Some(mut loaded) = read(os, &path).await? else { return Ok(None) };
    if loaded.version != *expected { return Ok(None) }
    let Some(active) = loaded.record.active.take() else { return Ok(None) };
    if !active.pins.is_empty() { return Ok(None) }
    let physical_key = active.physical_key.clone();
    loaded.record.retired.push(RetiredGeneration { physical_key: physical_key.clone() });
    loaded.record.nonce = nonce();
    if update(os, &path, &loaded.record, expected).await? { Ok(Some(physical_key)) } else { Ok(None) }
}

pub async fn delete_retired(os: &dyn ObjectStore, owner: &str, d: &Digest, physical_key: &str) -> Result<()> {
    match os.delete(&Path::from(physical_key)).await {
        Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
        Err(e) => return Err(e.into()),
    }
    let path = state_path(owner, d);
    for _ in 0..8 {
        let Some(mut loaded) = read(os, &path).await? else { return Ok(()) };
        loaded.record.retired.retain(|r| r.physical_key != physical_key);
        loaded.record.nonce = nonce();
        if update(os, &path, &loaded.record, &loaded.version).await? { return Ok(()) }
    }
    Err(crate::err("retired blob cleanup CAS retries exhausted"))
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
        install(os.as_ref(), "acme", &d, &old_key).await.unwrap().unwrap();
        let (_, new) = new_generation("acme", &d);
        os.put(&new, PutPayload::from("new")).await.unwrap();
        let new_key = new.to_string();
        install(os.as_ref(), "acme", &d, &new_key).await.unwrap().unwrap();
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
        install(os.as_ref(), "acme", &d, &old_key).await.unwrap().unwrap();
        assert!(pin(os.as_ref(), "acme", &d, "manifest").await.unwrap());
        let (_, new) = new_generation("acme", &d);
        os.put(&new, PutPayload::from("new")).await.unwrap();
        let new_key = new.to_string();
        assert_eq!(install(os.as_ref(), "acme", &d, &new_key).await.unwrap(), Err(InstallError::Busy));
        unpin(os.as_ref(), "acme", &d, "manifest").await.unwrap();
        install(os.as_ref(), "acme", &d, &new_key).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_stale_gc_snapshot_cannot_retire_after_pin_and_unpin() {
        let os = Arc::new(InMemory::new());
        let d = digest();
        let (_, generation) = new_generation("acme", &d);
        os.put(&generation, PutPayload::from("blob")).await.unwrap();
        let generation_key = generation.to_string();
        install(os.as_ref(), "acme", &d, &generation_key).await.unwrap().unwrap();
        let snapshot = candidates(os.as_ref(), "acme").await.unwrap().pop().unwrap().2;
        pin(os.as_ref(), "acme", &d, "manifest").await.unwrap();
        unpin(os.as_ref(), "acme", &d, "manifest").await.unwrap();
        assert!(retire_if_unpinned(os.as_ref(), "acme", &d, &snapshot).await.unwrap().is_none());
        assert!(resolve(os.as_ref(), "acme", &d).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_missed_pin_must_be_retried_after_a_generation_is_installed() {
        let os = Arc::new(InMemory::new());
        let d = digest();
        assert!(!pin(os.as_ref(), "acme", &d, "publication").await.unwrap());
        let (_, generation) = new_generation("acme", &d);
        os.put(&generation, PutPayload::from("blob")).await.unwrap();
        install(os.as_ref(), "acme", &d, &generation.to_string()).await.unwrap().unwrap();
        assert!(pin(os.as_ref(), "acme", &d, "publication").await.unwrap());
        assert_eq!(resolve(os.as_ref(), "acme", &d).await.unwrap(), Some(generation));
    }
}
