//! Process bootstrap: environment, object store, and the store itself.
//!
//! In the library rather than in a binary because there is more than one binary.
//! `rustic-git` serves git and `rustic-git-api` serves the read/team API; they are
//! separate processes with separate lifecycles, and both need exactly this.

use crate::store::Store;
use crate::Result;
use std::sync::Arc;

/// Choose the TLS backend, once per process.
///
/// Both `ring` and `aws-lc-rs` end up in the dependency graph (reqwest pulls one,
/// redis's TLS the other), and rustls 0.23 refuses to guess between them — it
/// panics on the FIRST handshake, which is startup for anything that talks to
/// object storage, Redis or Cosmos. The provider itself is not load-bearing; only
/// that exactly one is installed.
///
/// It lives here, in the bootstrap both binaries call, rather than in a `main`.
/// When the api server became its own binary it inherited every other startup
/// step and silently lost this one, and the pod crash-looped on a panic that
/// looks nothing like its cause.
pub fn install_crypto_provider() {
    // A second install is a no-op, not a failure.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// The object store, plus the same store seen through `MultipartStore` where the backend has one.
///
/// Built as the concrete type first so both views point at ONE client: `Arc<dyn ObjectStore>` is
/// not downcastable, so a second view has to be cloned off the concrete value here or not exist.
/// `LocalFileSystem` has no `MultipartStore` impl, which is why the second half is an `Option` and
/// why every consumer needs a path that works without it.
pub type StoreViews = (
    Arc<dyn slatedb::object_store::ObjectStore>,
    Option<Arc<dyn slatedb::object_store::multipart::MultipartStore>>,
);

/// Two ways to run: `AWS_*` in the environment (`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`,
/// `AWS_REGION`, `AWS_ENDPOINT`) with an `s3://bucket` URL, or `RUSTIC_GIT_S3_URL=file://./dir`
/// (or `mem://`) with no credentials at all. `~/.aws` profiles are NOT read — export the env or
/// use a file/mem URL.
pub fn object_store_views() -> Result<StoreViews> {
    let url = std::env::var("RUSTIC_GIT_S3_URL").map_err(|_| {
        crate::err(
            "RUSTIC_GIT_S3_URL required (e.g. s3://bucket; mem:// or file://./dir for testing)",
        )
    })?;
    use slatedb::object_store::multipart::MultipartStore;
    let mut mp: Option<Arc<dyn MultipartStore>> = None;
    let os: Arc<dyn slatedb::object_store::ObjectStore> = if url == "mem://" {
        let m = Arc::new(slatedb::object_store::memory::InMemory::new());
        mp = Some(m.clone());
        m
    } else if let Some(dir) = url.strip_prefix("file://") {
        // A directory on local disk, persisted across processes — unlike `mem://`, a second
        // process (an `admin` command against a running `serve`) sees what the first wrote.
        // `slatedb::Db::resolve_object_store` rejects this URL shape (it requires an empty
        // leftover path after the scheme), so it is built directly instead.
        std::fs::create_dir_all(dir)?;
        Arc::new(slatedb::object_store::local::LocalFileSystem::new_with_prefix(dir)?)
    } else if let Some(bucket) = url.strip_prefix("s3://") {
        // Built by hand rather than via resolve_object_store so the request timeout can be
        // raised: repack uploads a whole repository in one PUT, and object_store's 180s default
        // aborts that on a slow or distant link.
        use slatedb::object_store::{aws::AmazonS3Builder, ClientOptions};
        let timeout = env("RUSTIC_GIT_S3_TIMEOUT_SECS", "900")
            .parse()
            .map_err(|_| crate::err("RUSTIC_GIT_S3_TIMEOUT_SECS must be a number"))?;
        let mut b = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .with_client_options(
                ClientOptions::new().with_timeout(std::time::Duration::from_secs(timeout)),
            );
        if let Ok(ep) = std::env::var("AWS_ENDPOINT") {
            b = b.with_endpoint(ep).with_virtual_hosted_style_request(false);
        }
        let s = Arc::new(b.build()?);
        mp = Some(s.clone());
        s
    } else {
        slatedb::Db::resolve_object_store(&url)?
    };
    Ok((os, mp))
}

/// The object store alone, for the callers that never upload a blob in chunks.
pub fn object_store() -> Result<Arc<dyn slatedb::object_store::ObjectStore>> {
    Ok(object_store_views()?.0)
}

pub async fn open_store(background: bool) -> Result<Arc<Store>> {
    // Before the first TLS handshake, which the object store is about to make.
    install_crypto_provider();
    let (os, mp) = object_store_views()?;
    let mut store =
        Store::open(os, env("RUSTIC_GIT_CACHE_DIR", "./.local/cache").into(), background).await?;
    store.mp = mp;
    // Every process that can write refs or flip visibility needs the handle to invalidate through
    // — including the admin CLI, which is where purge-cache and set-visibility run.
    store.cache = Arc::new(
        crate::cache::Cache::connect(std::env::var("RUSTIC_GIT_REDIS_URL").ok().as_deref())
            .await,
    );
    Ok(Arc::new(store))
}

