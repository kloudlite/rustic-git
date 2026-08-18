//! A response cache the api tier shares. Every entry is keyed by an immutable object id, so a
//! hit is safe to serve from any pod without consulting the node that owns the repo.
//!
//! Every operation fails open: a cache that is down or absent makes requests slower, never wrong.
//!
//! Requires `maxmemory-policy volatile-lru` on the Redis instance, not the more common
//! `allkeys-lru`. Generation keys (`gen:{repo}`) carry no TTL and must never be evicted: if one
//! is reclaimed while entries it guards are still cached, `generation()` falls back to 1 — the
//! pre-purge generation — and every stale entry becomes reachable again, defeating the purge a
//! visibility flip depends on. `volatile-lru` only evicts keys that carry a TTL, so data keys
//! (all `SET ... EX`) are eviction candidates and generation keys are not. Cost: a generation
//! counter lives forever once a repo is purged — one small integer per ever-purged repo,
//! deliberately, since that is cheaper than a stale-serve bug.

use redis::aio::ConnectionManagerConfig;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const KEY_VERSION: &str = "v1";
// ponytail: fixed per-command timeout; make configurable if a deployment needs a different bound
// than the connect timeout below.
const CMD_TIMEOUT: Duration = Duration::from_millis(250);

pub fn key(generation: u64, repo: &str, suffix: &str) -> String {
    format!("{KEY_VERSION}:{generation}:{repo}:{suffix}")
}

/// The in-process backing for `Cache::memory`: the same keys, the same TTLs, no Redis.
type Mem = Mutex<HashMap<String, (Vec<u8>, Instant)>>;

pub struct Cache {
    conn: Option<redis::aio::ConnectionManager>,
    mem: Option<Mem>,
}

fn mem_get(m: &Mem, k: &str) -> Option<Vec<u8>> {
    let mut g = m.lock().unwrap();
    match g.get(k) {
        Some((v, exp)) if *exp > Instant::now() => Some(v.clone()),
        Some(_) => {
            g.remove(k);
            None
        }
        None => None,
    }
}

impl Cache {
    pub async fn connect(url: Option<&str>) -> Cache {
        let Some(url) = url else { return Cache { conn: None, mem: None } };
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
        Cache { conn, mem: None }
    }

    /// A cache that lives in this process. Not for production — nothing is shared between pods —
    /// but it exercises the real key discipline, which a test otherwise cannot reach without a
    /// Redis to talk to.
    // ponytail: entries are evicted lazily, on a `get` of that key only, so this grows without
    // bound if it is ever used outside tests. Give it a size cap or a sweeper before then.
    pub fn memory() -> Cache {
        Cache { conn: None, mem: Some(Mem::default()) }
    }

    /// The repo's current generation. A miss means one: a repo that has never been purged.
    async fn generation(&self, repo: &str) -> u64 {
        if let Some(m) = &self.mem {
            return mem_get(m, &format!("gen:{repo}"))
                .and_then(|v| String::from_utf8(v).ok()?.parse().ok())
                .unwrap_or(1);
        }
        let Some(mut c) = self.conn.clone() else { return 1 };
        run(redis::cmd("GET").arg(format!("gen:{repo}")), &mut c)
            .await
            .unwrap_or(None)
            .unwrap_or(1)
    }

    pub async fn get(&self, repo: &str, suffix: &str) -> Option<Vec<u8>> {
        let k = key(self.generation(repo).await, repo, suffix);
        if let Some(m) = &self.mem {
            return mem_get(m, &k);
        }
        let mut c = self.conn.clone()?;
        run(redis::cmd("GET").arg(k), &mut c).await.ok().flatten()
    }

    pub async fn put(&self, repo: &str, suffix: &str, val: &[u8], ttl_secs: u64) {
        let k = key(self.generation(repo).await, repo, suffix);
        if let Some(m) = &self.mem {
            let exp = Instant::now() + Duration::from_secs(ttl_secs);
            m.lock().unwrap().insert(k, (val.to_vec(), exp));
            return;
        }
        let Some(mut c) = self.conn.clone() else { return };
        let _: Result<(), _> =
            run(redis::cmd("SET").arg(k).arg(val).arg("EX").arg(ttl_secs), &mut c).await;
    }

    pub async fn drop_refs(&self, repo: &str) {
        let k = key(self.generation(repo).await, repo, "refs");
        if let Some(m) = &self.mem {
            m.lock().unwrap().remove(&k);
            return;
        }
        let Some(mut c) = self.conn.clone() else { return };
        let _: Result<(), _> = run(redis::cmd("DEL").arg(k), &mut c).await;
    }

    /// Orphans every cached answer for a repo at once. Used when a repo is deleted, or when its
    /// visibility flips — after which no previously cached response may be served to anyone. No
    /// SCAN: the old keys simply become unreachable and age out under `volatile-lru` (see module
    /// doc). This key itself carries no TTL — it must survive as long as the repo can be purged.
    pub async fn bump_generation(&self, repo: &str) {
        let k = format!("gen:{repo}");
        if let Some(m) = &self.mem {
            let next = self.generation(repo).await + 1;
            // No TTL in Redis; a decade here stands in for "never evicted".
            let exp = Instant::now() + Duration::from_secs(10 * 365 * 24 * 3600);
            m.lock().unwrap().insert(k, (next.to_string().into_bytes(), exp));
            return;
        }
        let Some(mut c) = self.conn.clone() else { return };
        let _: Result<(), _> = run(redis::cmd("INCR").arg(&k), &mut c).await;
    }
}

/// Every command gets the same bound as `connect`: a live-but-black-holed connection must not
/// hang a request path. A timeout is treated exactly like any other command error — fail open.
async fn run<T: redis::FromRedisValue>(
    cmd: &mut redis::Cmd,
    c: &mut redis::aio::ConnectionManager,
) -> redis::RedisResult<T> {
    match tokio::time::timeout(CMD_TIMEOUT, cmd.query_async(c)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into()),
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
