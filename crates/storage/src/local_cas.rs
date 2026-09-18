use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use slatedb::object_store::{
    path::Path, CopyOptions, Error, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, ObjectStoreExt, PutMultipartOptions, PutMode, PutOptions, PutPayload, PutResult, RenameOptions,
    Result, UpdateVersion,
};
use std::{path::PathBuf, sync::Arc};

pub struct LocalCas {
    inner: Arc<slatedb::object_store::local::LocalFileSystem>,
    locks: PathBuf,
}

impl LocalCas {
    fn is_blob_state(location: &Path) -> bool {
        location.as_ref() == "blob-state" || location.as_ref().starts_with("blob-state/")
    }

    fn metadata_mutation(location: &Path) -> Error {
        Error::NotImplemented { operation: format!("conditional mutation of {location}"), implementer: "LocalCas".into() }
    }

    pub fn new(root: &str) -> Result<Self> {
        #[cfg(not(unix))]
        return Err(Error::NotImplemented { operation: "conditional local object-store updates".into(), implementer: "LocalCas".into() });
        #[cfg(unix)]
        let root = {
            let root = PathBuf::from(root);
            std::fs::create_dir_all(&root).map_err(|e| Error::Generic { store: "local-cas", source: Box::new(e) })?;
            std::fs::canonicalize(root).map_err(|e| Error::Generic { store: "local-cas", source: Box::new(e) })?
        };
        #[cfg(unix)]
        let locks = root.with_extension("cas-locks");
        #[cfg(unix)]
        std::fs::create_dir_all(&locks).map_err(|e| Error::Generic { store: "local-cas", source: Box::new(e) })?;
        #[cfg(unix)]
        Ok(Self {
            inner: Arc::new(slatedb::object_store::local::LocalFileSystem::new_with_prefix(root).map_err(|e| Error::Generic { store: "local-cas", source: Box::new(e) })?),
            locks,
        })
    }

    fn lock_path(&self, location: &Path) -> PathBuf {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(location.as_ref().as_bytes());
        self.locks.join(format!("{}.lock", hex::encode(digest)))
    }

    async fn update(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
        expected: Option<UpdateVersion>,
    ) -> Result<PutResult> {
        let inner = self.inner.clone();
        let lock = self.lock_path(location);
        let location = location.clone();
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new().create(true).read(true).write(true).open(lock)
                .map_err(|e| Error::Generic { store: "local-cas", source: Box::new(e) })?;
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if rc != 0 {
                    return Err(Error::Generic { store: "local-cas", source: Box::new(std::io::Error::last_os_error()) });
                }
            }
            handle.block_on(async move {
                if let Some(expected) = expected {
                    let current = inner.get_opts(&location, GetOptions::default().with_head(true)).await.map_err(|e| match e {
                        Error::NotFound { .. } => Error::Precondition { path: location.to_string(), source: Box::new(std::io::Error::new(std::io::ErrorKind::NotFound, "object is absent")) },
                        e => e,
                    })?;
                    if expected.version != current.meta.version || expected.e_tag != current.meta.e_tag {
                        return Err(Error::Precondition { path: location.to_string(), source: Box::new(std::io::Error::new(std::io::ErrorKind::WouldBlock, "object version changed")) });
                    }
                    let mut overwrite = opts;
                    overwrite.mode = PutMode::Overwrite;
                    return inner.put_opts(&location, payload, overwrite).await;
                }
                inner.put_opts(&location, payload, opts).await
            })
        })
        .await
        .map_err(|e| Error::Generic { store: "local-cas", source: Box::new(e) })?
    }
}

impl std::fmt::Debug for LocalCas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("LocalCas").field("locks", &self.locks).finish() }
}

impl std::fmt::Display for LocalCas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.inner.fmt(f) }
}

#[async_trait]
impl ObjectStore for LocalCas {
    async fn put_opts(&self, location: &Path, payload: PutPayload, opts: PutOptions) -> Result<PutResult> {
        match opts.mode.clone() {
            PutMode::Update(_) if !Self::is_blob_state(location) => Err(Self::metadata_mutation(location)),
            PutMode::Update(expected) => self.update(location, payload, opts, Some(expected)).await,
            _ => self.update(location, payload, opts, None).await,
        }
    }

    async fn put_multipart_opts(&self, location: &Path, opts: PutMultipartOptions) -> Result<Box<dyn MultipartUpload>> {
        if Self::is_blob_state(location) { return Err(Self::metadata_mutation(location)); }
        self.inner.put_multipart_opts(location, opts).await
    }
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> { self.inner.get_opts(location, options).await }
    fn delete_stream(&self, mut locations: BoxStream<'static, Result<Path>>) -> BoxStream<'static, Result<Path>> {
        let inner = self.inner.clone();
        futures::stream::unfold((), move |_| {
            let inner = inner.clone();
            async move {
                let next = locations.next().await?;
                let result = match next {
                    Ok(path) if Self::is_blob_state(&path) => Err(Self::metadata_mutation(&path)),
                    Ok(path) => inner.delete(&path).await.map(|_| path),
                    Err(e) => Err(e),
                };
                Some((result, ()))
            }
        }).boxed()
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> { self.inner.list(prefix) }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> { self.inner.list_with_delimiter(prefix).await }
    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        if Self::is_blob_state(from) || Self::is_blob_state(to) { return Err(Self::metadata_mutation(to)); }
        self.inner.copy_opts(from, to, options).await
    }
    async fn rename_opts(&self, from: &Path, to: &Path, options: RenameOptions) -> Result<()> {
        if Self::is_blob_state(from) || Self::is_blob_state(to) { return Err(Self::metadata_mutation(to)); }
        self.inner.rename_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::{ObjectStoreExt, PutPayload};

    #[tokio::test]
    async fn file_store_supports_compare_and_swap_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = Path::from("state/record");
        let first = LocalCas::new(dir.path().to_str().unwrap()).unwrap();
        let created = first.put(&path, PutPayload::from_static(b"one")).await.unwrap();
        let version = UpdateVersion { e_tag: created.e_tag, version: created.version };
        drop(first);
        let reopened = LocalCas::new(dir.path().to_str().unwrap()).unwrap();
        reopened
            .put_opts(&path, PutPayload::from_static(b"two"), PutOptions { mode: PutMode::Update(version.clone()), ..Default::default() })
            .await
            .unwrap();
        let stale = reopened
            .put_opts(&path, PutPayload::from_static(b"three"), PutOptions { mode: PutMode::Update(version), ..Default::default() })
            .await;
        assert!(matches!(stale, Err(Error::Precondition { .. })));
    }

    #[tokio::test]
    async fn metadata_cas_rejects_unconditional_bypass_and_has_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalCas::new(dir.path().to_str().unwrap()).unwrap());
        let path = Path::from("blob-state/acme/sha256/hash");
        let created = store.put(&path, PutPayload::from_static(b"one")).await.unwrap();
        let version = UpdateVersion { e_tag: created.e_tag, version: created.version };
        let a = store.put_opts(&path, PutPayload::from_static(b"two"), PutOptions { mode: PutMode::Update(version.clone()), ..Default::default() });
        let b = store.put_opts(&path, PutPayload::from_static(b"three"), PutOptions { mode: PutMode::Update(version), ..Default::default() });
        let (a, b) = tokio::join!(a, b);
        assert!(a.is_ok() ^ b.is_ok());
        assert!(matches!(store.put_opts(&Path::from("state/record"), PutPayload::from_static(b"x"), PutOptions { mode: PutMode::Update(UpdateVersion { e_tag: None, version: None }), ..Default::default() }).await, Err(Error::NotImplemented { .. })));
        assert!(matches!(store.copy_opts(&path, &Path::from("blob-state/acme/sha256/copy"), CopyOptions::default()).await, Err(Error::NotImplemented { .. })));
    }
}
