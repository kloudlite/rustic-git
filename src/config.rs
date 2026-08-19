//! Process bootstrap: environment, object store, and the store itself.
//!
//! In the library rather than in a binary because there is more than one binary.
//! `rustic-git` serves git and `rustic-git-api` serves the read/team API; they are
//! separate processes with separate lifecycles, and both need exactly this.

use crate::store::Store;
use crate::Result;
use std::sync::Arc;

pub fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// Read `[profile]` from an AWS INI file; returns key=value pairs.
pub fn aws_ini(file: &str, profile: &str) -> Vec<(String, String)> {
    let Some(home) = std::env::var_os("HOME") else {
        return vec![];
    };
    let Ok(text) = std::fs::read_to_string(std::path::Path::new(&home).join(".aws").join(file))
    else {
        return vec![];
    };
    let mut out = vec![];
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let name = line.trim_matches(&['[', ']'][..]).trim();
            inside = name == profile || name == format!("profile {profile}");
        } else if inside {
            if let Some((k, v)) = line.split_once('=') {
                out.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

/// If no AWS_ACCESS_KEY_ID in env, export credentials/region from ~/.aws for $AWS_PROFILE (default "default").
pub fn load_aws_profile() {
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
        return;
    }
    let profile = env("AWS_PROFILE", "default");
    let map = [
        ("aws_access_key_id", "AWS_ACCESS_KEY_ID"),
        ("aws_secret_access_key", "AWS_SECRET_ACCESS_KEY"),
        ("aws_session_token", "AWS_SESSION_TOKEN"),
        ("region", "AWS_REGION"),
        ("endpoint_url", "AWS_ENDPOINT"),
    ];
    for (k, v) in aws_ini("credentials", &profile)
        .into_iter()
        .chain(aws_ini("config", &profile))
    {
        if let Some((_, env_key)) = map.iter().find(|(ini, _)| *ini == k) {
            if std::env::var_os(env_key).is_none() {
                std::env::set_var(env_key, v);
            }
        }
    }
    // ponytail: static keys + region only; SSO/assume-role profiles need the AWS SDK credential chain
}

pub fn object_store() -> Result<Arc<dyn slatedb::object_store::ObjectStore>> {
    load_aws_profile();
    let url = std::env::var("RUSTIC_GIT_S3_URL").map_err(|_| {
        crate::err("RUSTIC_GIT_S3_URL required (e.g. s3://bucket, or mem:// for testing)")
    })?;
    let os: Arc<dyn slatedb::object_store::ObjectStore> = if url == "mem://" {
        Arc::new(slatedb::object_store::memory::InMemory::new())
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
        Arc::new(b.build()?)
    } else {
        slatedb::Db::resolve_object_store(&url)?
    };
    Ok(os)
}

pub async fn open_store(background: bool) -> Result<Arc<Store>> {
    let mut store = Store::open(
        object_store()?,
        env("RUSTIC_GIT_CACHE_DIR", "./cache").into(),
        background,
    )
    .await?;
    // Every process that can write refs or flip visibility needs the handle to invalidate through
    // — including the admin CLI, which is where purge-cache and set-visibility run.
    store.cache = Arc::new(
        crate::cache::Cache::connect(std::env::var("RUSTIC_GIT_REDIS_URL").ok().as_deref())
            .await,
    );
    Ok(Arc::new(store))
}

