//! Process bootstrap: environment, object store, and the store itself.
//!
//! In the library rather than in a binary because there is more than one binary.
//! `kloudlite-git` serves git and `kloudlite-git-api` serves the read/team API; they are
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
/// `AWS_REGION`, `AWS_ENDPOINT`) with an `s3://bucket` URL, or `KLOUDLITE_GIT_S3_URL=file://./dir`
/// (or `mem://`) with no credentials at all. `~/.aws` profiles are NOT read — export the env or
/// use a file/mem URL.
pub fn object_store_views() -> Result<StoreViews> {
    let url = std::env::var("KLOUDLITE_GIT_S3_URL").map_err(|_| {
        crate::err(
            "KLOUDLITE_GIT_S3_URL required (e.g. s3://bucket; mem:// or file://./dir for testing)",
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
        let timeout = env("KLOUDLITE_GIT_S3_TIMEOUT_SECS", "900")
            .parse()
            .map_err(|_| crate::err("KLOUDLITE_GIT_S3_TIMEOUT_SECS must be a number"))?;
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
    } else if let Some(container) = url.strip_prefix("az://") {
        // `resolve_object_store` would build the very same client from the same `AZURE_STORAGE_*`
        // env, but hands it back as `Arc<dyn ObjectStore>`, which cannot be re-viewed as
        // `MultipartStore` — and this is the production backend, so without the concrete value
        // here every registry PATCH took the O(N·K) re-stream fallback.
        use slatedb::object_store::azure::MicrosoftAzureBuilder;
        let s = Arc::new(
            MicrosoftAzureBuilder::from_env()
                .with_container_name(container)
                .build()?,
        );
        mp = Some(s.clone());
        s
    } else {
        slatedb::Db::resolve_object_store(&url)?
    };
    if mp.is_none() {
        tracing::warn!(%url, "object store has no MultipartStore; chunked uploads take the slow path");
    }
    Ok((os, mp))
}

/// A fleet's leader lease is a conditional put, and `LocalFileSystem` has no `PutMode::Update`
/// (object_store 0.14.1 `local.rs`: `NotImplemented`). A multi-node `file://` deployment would
/// take the lease once and then never renew or fence it — refused here, where the URL is
/// parsed, rather than discovered as an election that silently stops.
///
/// S3 carries the same risk without a URL to spot it by: `conditional_put` defaults to
/// `ETagMatch`, but an endpoint configured with it Disabled makes every conditional put an
/// unconditional one — two candidates both "win" the lease and both open the map's writer. The
/// URL is all this function is given (SlateDB resolves the store itself, and plumbing an
/// `AmazonS3Builder` through here just to read one flag would cost more than it catches), so it is
/// named in the error rather than checked: an S3-compatible endpoint used for a fleet MUST have
/// conditional puts enabled.
pub fn fleet_store_ok(url: &str) -> Result<()> {
    if url.starts_with("file://") {
        return Err(crate::err(
            "KLOUDLITE_GIT_S3_URL=file:// cannot host a fleet: the leader lease needs conditional \
             updates, which LocalFileSystem lacks; use s3:// / az:// \
             — and on s3://, an endpoint with conditional_put disabled is just as unfenced, \
             because the lease's compare-and-swap silently becomes an overwrite",
        ));
    }
    // `InMemory` is per-process. In a real multi-pod fleet every pod takes epoch 1 of its own
    // lease, every pod is leader and every pod opens every database — the same two-writer bug the
    // `file://` refusal above exists for, with no URL scheme to give it away. The in-process test
    // fleet is the legitimate case, and it says so.
    // Set only by this module's own unit test and by an in-process test fleet. Nothing in
    // `deploy/` sets it, and nothing should: a real fleet on `mem://` is the two-writer bug.
    if url == "mem://" && std::env::var("KLOUDLITE_GIT_ALLOW_MEM_FLEET").is_err() {
        return Err(crate::err(
            "KLOUDLITE_GIT_S3_URL=mem:// cannot host a fleet: InMemory is per-process, so every pod \
             would be its own leader and open every database; set KLOUDLITE_GIT_ALLOW_MEM_FLEET=1 \
             only for an in-process test fleet",
        ));
    }
    Ok(())
}

/// One GET of `cluster/settings`, `None` for both "never written" and "unreachable" — the caller
/// (`refresh_central_beat`, or a binary's own boot-time load) treats both the same way: keep
/// whatever `LiveSettings` already holds.
pub async fn get_central(os: &Arc<dyn slatedb::object_store::ObjectStore>) -> Option<Vec<u8>> {
    use slatedb::object_store::{path::Path as OsPath, ObjectStoreExt};
    let key = OsPath::from(kloudlite_git_core::settings::CENTRAL_SETTINGS_KEY);
    match os.get(&key).await {
        Ok(r) => r.bytes().await.ok().map(|b| b.to_vec()),
        Err(_) => None,
    }
}

/// A `CentralFetch` closure over a concrete object store — the one thing `refresh_central_beat`
/// needs and `crates/core` cannot build itself (no object-store dependency there).
pub fn central_fetch(os: Arc<dyn slatedb::object_store::ObjectStore>) -> kloudlite_git_core::settings::CentralFetch {
    std::sync::Arc::new(move || {
        let os = os.clone();
        Box::pin(async move { get_central(&os).await })
    })
}

pub async fn open_store(background: bool) -> Result<Arc<Store>> {
    // Before the first TLS handshake, which the object store is about to make.
    install_crypto_provider();
    let (os, mp) = object_store_views()?;
    let mut store =
        Store::open(os, env("KLOUDLITE_GIT_CACHE_DIR", "./.local/cache").into(), background).await?;
    store.mp = mp;
    // Every process that can write refs or flip visibility needs the handle to invalidate through
    // — including the admin CLI, which is where purge-cache and set-visibility run.
    store.cache = Arc::new(
        crate::cache::Cache::connect(std::env::var("KLOUDLITE_GIT_REDIS_URL").ok().as_deref())
            .await,
    );
    Ok(Arc::new(store))
}


#[cfg(test)]
mod tests {
    // cargo runs a crate's tests in parallel threads, and every test below mutates process-wide
    // env vars — unserialized, two of them interleave and each sees the other's values. One lock
    // per crate, held for the body of each test, the same way `bins/kl/src/config.rs` does it.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn azure_url_gets_a_multipart_view() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Edition 2021: `set_var` is a safe fn.
        std::env::set_var("KLOUDLITE_GIT_S3_URL", "az://c");
        std::env::set_var("AZURE_STORAGE_ACCOUNT_NAME", "acct");
        std::env::set_var("AZURE_STORAGE_ACCOUNT_KEY", "a2V5"); // any valid base64
        let (_, mp) = super::object_store_views().unwrap();
        assert!(mp.is_some(), "az:// must be built concretely so the registry fast path exists");
    }

    #[test]
    fn a_file_store_cannot_host_a_fleet() {
        assert!(super::fleet_store_ok("file://./x").is_err());
        for ok in ["s3://bucket", "az://container"] {
            super::fleet_store_ok(ok).unwrap();
        }
    }

    /// `InMemory` is per-process: every pod would take epoch 1 of its own lease, every pod would
    /// be leader, and every pod would open every database — the exact two-writer bug the guard
    /// exists to prevent. Allowed only when something says out loud that it is a test fleet.
    #[test]
    fn mem_is_not_a_fleet_store_unless_opted_into() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("KLOUDLITE_GIT_ALLOW_MEM_FLEET");
        assert!(super::fleet_store_ok("mem://").is_err());
        std::env::set_var("KLOUDLITE_GIT_ALLOW_MEM_FLEET", "1");
        assert!(super::fleet_store_ok("mem://").is_ok());
        std::env::remove_var("KLOUDLITE_GIT_ALLOW_MEM_FLEET");
    }
}
