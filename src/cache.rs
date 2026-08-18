//! A response cache the api tier shares. Every entry is keyed by an immutable object id, so a
//! hit is safe to serve from any pod without consulting the node that owns the repo.
//!
//! Every operation fails open: a cache that is down or absent makes requests slower, never wrong.

use redis::aio::ConnectionManagerConfig;
use std::time::Duration;

const KEY_VERSION: &str = "v1";
const GEN_TTL: u64 = 3600;

pub fn key(generation: u64, repo: &str, suffix: &str) -> String {
    format!("{KEY_VERSION}:{generation}:{repo}:{suffix}")
}

pub struct Cache {
    conn: Option<redis::aio::ConnectionManager>,
}

impl Cache {
    pub async fn connect(url: Option<&str>) -> Cache {
        let Some(url) = url else { return Cache { conn: None } };
        // Bounded retry/timeout: an unreachable Redis must fail fast, not retry with the crate's
        // default exponential backoff (6 attempts) and hang callers — a cache that is slow to give
        // up is worse than one that is simply absent.
        let config = ConnectionManagerConfig::new()
            .set_number_of_retries(1)
            .set_connection_timeout(Duration::from_millis(250));
        let conn = async {
            redis::Client::open(url)
                .ok()?
                .get_connection_manager_with_config(config)
                .await
                .ok()
        }
        .await;
        if conn.is_none() {
            eprintln!("cache: {url} unreachable; serving without it"); // ponytail: eprintln
        }
        Cache { conn }
    }

    /// The repo's current generation. A miss means one: a repo that has never been purged.
    async fn generation(&self, repo: &str) -> u64 {
        let Some(mut c) = self.conn.clone() else { return 1 };
        redis::cmd("GET")
            .arg(format!("gen:{repo}"))
            .query_async(&mut c)
            .await
            .unwrap_or(None)
            .unwrap_or(1)
    }

    pub async fn get(&self, repo: &str, suffix: &str) -> Option<Vec<u8>> {
        let mut c = self.conn.clone()?;
        let k = key(self.generation(repo).await, repo, suffix);
        redis::cmd("GET").arg(k).query_async(&mut c).await.ok().flatten()
    }

    pub async fn put(&self, repo: &str, suffix: &str, val: &[u8], ttl_secs: u64) {
        let Some(mut c) = self.conn.clone() else { return };
        let k = key(self.generation(repo).await, repo, suffix);
        let _: Result<(), _> = redis::cmd("SET")
            .arg(k)
            .arg(val)
            .arg("EX")
            .arg(ttl_secs)
            .query_async::<()>(&mut c)
            .await;
    }

    pub async fn drop_refs(&self, repo: &str) {
        let Some(mut c) = self.conn.clone() else { return };
        let k = key(self.generation(repo).await, repo, "refs");
        let _: Result<(), _> = redis::cmd("DEL").arg(k).query_async::<()>(&mut c).await;
    }

    /// Orphans every cached answer for a repo at once. Used when a repo is deleted, or when its
    /// visibility flips — after which no previously cached response may be served to anyone.
    /// No SCAN: the old keys simply become unreachable and age out under `allkeys-lru`.
    pub async fn bump_generation(&self, repo: &str) {
        let Some(mut c) = self.conn.clone() else { return };
        let k = format!("gen:{repo}");
        let _: Result<(), _> = redis::cmd("INCR").arg(&k).query_async::<()>(&mut c).await;
        let _: Result<(), _> = redis::cmd("EXPIRE")
            .arg(&k)
            .arg(GEN_TTL)
            .arg("XX")
            .query_async::<()>(&mut c)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_carry_version_generation_and_repo() {
        assert_eq!(key(7, "alice/web", "tree:abc:src"), "v1:7:alice/web:tree:abc:src");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disabled_cache_answers_without_failing() {
        let c = Cache::connect(None).await;
        assert!(c.get("alice/web", "refs").await.is_none());
        c.put("alice/web", "refs", b"x", 5).await; // must not panic
        c.drop_refs("alice/web").await;
        c.bump_generation("alice/web").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_redis_degrades_to_disabled() {
        // Port 1 refuses instantly; connect must swallow it rather than propagate.
        let c = Cache::connect(Some("redis://127.0.0.1:1")).await;
        assert!(c.get("alice/web", "refs").await.is_none());
    }
}
