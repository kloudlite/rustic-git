//! `cache.nixos.org` as the binary oracle: a lock with a store path is only written once the
//! cache says it holds it, because Hydra never builds a release marked insecure.

use super::*;


pub fn cache_key(attr: &str, version: &VersionReq) -> String {
    format!("pkgs/{SYSTEM}/{attr}/{}", req_str(version))
}

/// One index. `Ok(None)` is "this package/version does not exist" — a 422 for the person.
/// `Err` is "I could not tell you" — a 503, and a reason to ask the next index.
#[async_trait]
pub trait Index: Send + Sync {
    async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String>;
    /// Every known version string for `attr`, newest first (for the 422's "nearest").
    async fn versions(&self, attr: &str) -> Result<Vec<String>, String>;
}

/// Whether the binary cache holds a store path. `has` is one HEAD of the narinfo; an answer
/// other than 200/404 is an outage, not a verdict.
#[async_trait]
pub trait BinaryCache: Send + Sync {
    async fn has(&self, store_path: &str) -> Result<bool, String>;
}


pub struct NixosCache {
    pub client: reqwest::Client,
    /// `https://cache.nixos.org`, no trailing slash.
    pub base: String,
}


#[async_trait]
impl BinaryCache for NixosCache {
    async fn has(&self, store_path: &str) -> Result<bool, String> {
        let hash = store_path
            .rsplit('/')
            .next()
            .and_then(|n| n.split('-').next())
            .filter(|h| h.len() == 32)
            .ok_or_else(|| format!("{store_path} is not a store path"))?;
        let r = self
            .client
            .head(format!("{}/{hash}.narinfo", self.base))
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        match r.status() {
            reqwest::StatusCode::OK => Ok(true),
            reqwest::StatusCode::NOT_FOUND => Ok(false),
            s => Err(format!("the binary cache answered {s}")),
        }
    }
}
