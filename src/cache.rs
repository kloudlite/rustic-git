//! A response cache the api tier shares. Every entry is keyed by an immutable object id, so a
//! hit is safe to serve from any pod without consulting the node that owns the repo.
//!
//! Every read and write fails open: a cache that is down or absent makes requests slower, never
//! wrong. `bump_generation` is the deliberate exception — see its doc comment.
//!
//! Requires `maxmemory-policy volatile-lru` on the Redis instance, not the more common
//! `allkeys-lru`. Generation keys (`gen:{repo}`) carry no TTL and must never be evicted: if one
//! is reclaimed while entries it guards are still cached, `generation()` reads a real miss —
//! indistinguishable from "never purged" — and every stale entry becomes reachable again,
//! defeating the purge a visibility flip depends on. `volatile-lru` only evicts keys that carry a
//! TTL, so data keys (all `SET ... EX`) are eviction candidates and generation keys are not. Cost:
//! a generation counter lives forever once a repo is purged — one small integer per ever-purged
//! repo, deliberately, since that is cheaper than a stale-serve bug.
//!
//! `generation()` does NOT fail open: a backend *error* reading `gen:{repo}` (as opposed to a
//! real miss) returns `None`, and `get`/`put`/`drop_refs` treat that as "cache disabled for this
//! call" rather than substituting a generation — otherwise a transient Redis blip would make a
//! purged repo's pre-purge entries reachable again.

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

/// In-memory stand-in for a single Redis stream (see `xadd`): entry ids in append order, each
/// carrying its field/value pairs, trimmed with the same MAXLEN-from-the-front semantics XADD
/// gives `MAXLEN ~`. No consumer groups here — the events module's own tests only need to see
/// what was published, not to exercise group delivery.
type MemStream = Mutex<Vec<(String, Vec<(String, String)>)>>;

pub struct Cache {
    conn: Option<redis::aio::ConnectionManager>,
    mem: Option<Mem>,
    mem_stream: Option<MemStream>,
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
        let Some(url) = url else { return Cache { conn: None, mem: None, mem_stream: None } };
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
            // No `url` dep in this crate; drop credentials by keeping only the part after the
            // last '@' (redis://:password@host -> host), never log the raw URL.
            let host = url.rsplit('@').next().unwrap_or("redis");
            eprintln!("cache: {host} unreachable; serving without it"); // ponytail: eprintln
        }
        Cache { conn, mem: None, mem_stream: None }
    }

    /// A cache that lives in this process. Not for production — nothing is shared between pods —
    /// but it exercises the real key discipline, which a test otherwise cannot reach without a
    /// Redis to talk to.
    // Expired entries are swept on insert once the map passes a size no test reaches (see
    // `put_key`), so this no longer grows without bound if it is ever used outside tests.
    pub fn memory() -> Cache {
        Cache { conn: None, mem: Some(Mem::default()), mem_stream: Some(MemStream::default()) }
    }

    /// The repo's current generation, or `None` when it cannot be read. A miss means ZERO, not
    /// one, and the distinction is the whole mechanism: `INCR` on a missing key yields 1, so if a
    /// miss also read as 1 the very first purge would move the generation from 1 to 1 and orphan
    /// nothing. A repo that has never been purged sits at generation 0; its first purge moves it
    /// to 1. A backend *error* (as opposed to a real miss) is `None`, never a substituted
    /// generation — callers must skip the cache for this request, or a purged repo's pre-purge
    /// entries become reachable again during a transient failure.
    pub async fn generation(&self, repo: &str) -> Option<u64> {
        if let Some(m) = &self.mem {
            return Some(
                mem_get(m, &format!("gen:{repo}"))
                    .and_then(|v| String::from_utf8(v).ok()?.parse().ok())
                    .unwrap_or(0),
            );
        }
        let mut c = self.conn.clone()?;
        // `Ok(None)` is a real miss -> generation 0. `Err` is a backend failure -> None (skip
        // cache), never defaulted to 0.
        match run::<Option<u64>>(redis::cmd("GET").arg(format!("gen:{repo}")), &mut c).await {
            Ok(v) => Some(v.unwrap_or(0)),
            Err(_) => None,
        }
    }

    pub async fn get(&self, repo: &str, suffix: &str) -> Option<Vec<u8>> {
        let gen = self.generation(repo).await?; // None => cannot key it safely; treat as a miss
        let k = key(gen, repo, suffix);
        if let Some(m) = &self.mem {
            return mem_get(m, &k);
        }
        let mut c = self.conn.clone()?;
        run(redis::cmd("GET").arg(k), &mut c).await.ok().flatten()
    }

    pub async fn put(&self, repo: &str, suffix: &str, val: &[u8], ttl_secs: u64) {
        let Some(gen) = self.generation(repo).await else { return }; // cannot key it safely; skip
        let k = key(gen, repo, suffix);
        self.put_key(k, val, ttl_secs).await;
    }

    /// `put` under a generation read EARLIER — before a read-through miss went upstream. The key
    /// is built from `generation`, never from a fresh read, so a purge that lands mid-flight cannot
    /// be defeated: the write goes to the old generation, which the bump already made unreachable,
    /// and ages out under its TTL. No check, no atomicity needed — losing the race fails safe.
    pub async fn put_at(&self, generation: u64, repo: &str, suffix: &str, val: &[u8], ttl_secs: u64) {
        self.put_key(key(generation, repo, suffix), val, ttl_secs).await;
    }

    async fn put_key(&self, k: String, val: &[u8], ttl_secs: u64) {
        if let Some(m) = &self.mem {
            let exp = Instant::now() + Duration::from_secs(ttl_secs);
            let mut g = m.lock().unwrap();
            // Entries otherwise only expire when that exact key is read again, so keys written and
            // never re-read stay forever — and unlike Redis, nothing else evicts them. Drop the
            // expired ones on insert once the map is larger than any test needs; an entry past its
            // expiry is dead by definition, so this can never evict a live value.
            const SWEEP_AT: usize = 1024;
            if g.len() >= SWEEP_AT {
                let now = Instant::now();
                g.retain(|_, (_, exp)| *exp > now);
            }
            g.insert(k, (val.to_vec(), exp));
            return;
        }
        let Some(mut c) = self.conn.clone() else { return };
        let _: Result<(), _> =
            run(redis::cmd("SET").arg(k).arg(val).arg("EX").arg(ttl_secs), &mut c).await;
    }

    /// Deliberately fire-and-forget, unlike `bump_generation`: a missed drop self-heals when the
    /// 5s `refs` TTL expires, and this sits on the push path — failing a push over a cache blip
    /// would cost more than five seconds of stale refs. Invalidation that a security guarantee
    /// depends on is the other case, and reports its failure.
    pub async fn drop_refs(&self, repo: &str) {
        let Some(gen) = self.generation(repo).await else { return }; // cannot key it; nothing to drop
        let k = key(gen, repo, "refs");
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
    ///
    /// The one operation that does NOT fail open. Reads may degrade to a slower request; a purge
    /// that quietly fails leaves a repo the operator just made private still cached — and served
    /// anonymously — for up to the body TTL. A cache that is *disabled* (`conn: None, mem: None`)
    /// is not a failure: there is nothing cached, so the purge is a correct no-op.
    pub async fn bump_generation(&self, repo: &str) -> crate::Result<()> {
        let k = format!("gen:{repo}");
        if let Some(m) = &self.mem {
            // INCR semantics, deliberately: a missing key becomes 1, exactly as Redis does. Going
            // through `generation()` instead would add 1 to whatever default that returns, which
            // made this double advance the generation in a case where Redis would not — hiding a
            // real bug where the first purge of a repo orphaned nothing.
            let cur: u64 = mem_get(m, &k)
                .and_then(|v| String::from_utf8(v).ok()?.parse().ok())
                .unwrap_or(0);
            let next = cur + 1;
            // No TTL in Redis; a decade here stands in for "never evicted".
            let exp = Instant::now() + Duration::from_secs(10 * 365 * 24 * 3600);
            m.lock().unwrap().insert(k, (next.to_string().into_bytes(), exp));
            return Ok(());
        }
        let Some(mut c) = self.conn.clone() else { return Ok(()) };
        run::<()>(redis::cmd("INCR").arg(&k), &mut c)
            .await
            .map_err(|e| crate::err(format!("cache purge failed: {e}")))
    }

    /// `XADD {stream} MAXLEN ~ {maxlen} * {field} {value} …`. Fire-and-forget like `drop_refs`:
    /// the stream is a nudge (see `crate::events`), never the record, so a lost publish is not a
    /// lost event — it just costs the consumer a poll cycle. A disabled cache (`conn: None,
    /// mem: None`) is a silent no-op, same as every other cache miss path.
    pub async fn xadd(&self, stream: &str, maxlen: usize, fields: &[(String, String)]) {
        if let Some(m) = &self.mem_stream {
            // `~` (approximate trim) has no meaning in-process; trim exactly, which is a superset
            // of what the real MAXLEN ~ guarantees and therefore never masks a bug the real one
            // would hide.
            let mut g = m.lock().unwrap();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let id = format!("{now_ms}-0");
            g.push((id, fields.to_vec()));
            let len = g.len();
            if len > maxlen {
                g.drain(0..len - maxlen);
            }
            return;
        }
        if let Some(mut c) = self.conn.clone() {
            let mut cmd = redis::cmd("XADD");
            cmd.arg(stream).arg("MAXLEN").arg("~").arg(maxlen).arg("*");
            for (k, v) in fields {
                cmd.arg(k).arg(v);
            }
            // ponytail: eprintln — same fire-and-forget discipline as `drop_refs`; a lost nudge
            // self-heals via each consumer's fallback scan (see `crate::events` module doc).
            if let Err(e) = run::<()>(&mut cmd, &mut c).await {
                eprintln!("cache: xadd {stream} failed: {e}");
            }
        }
    }

    /// Test-only read-back for `Cache::memory()`: what `xadd` has appended so far, in order.
    /// There is no Redis equivalent on purpose — a real stream is read via `XREADGROUP` by a
    /// consumer, never snapshotted whole by the producer; this exists only so a test can assert
    /// what a handler published without standing up a consumer group.
    #[cfg(test)]
    pub(crate) fn mem_stream_snapshot(&self) -> Vec<(String, Vec<(String, String)>)> {
        self.mem_stream.as_ref().map(|m| m.lock().unwrap().clone()).unwrap_or_default()
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

    /// The first purge of a repo must orphan its entries. It did not: `generation()` read a
    /// missing key as 1 and `INCR` also produces 1, so the first bump moved 1 -> 1 and every
    /// cached answer stayed reachable. Only the second purge onwards worked.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_purge_orphans_what_was_cached() {
        let c = Cache::memory();
        c.put("alice/web", "tree:abc", b"body", 60).await;
        assert_eq!(c.get("alice/web", "tree:abc").await.as_deref(), Some(&b"body"[..]));
        c.bump_generation("alice/web").await.unwrap();
        assert!(
            c.get("alice/web", "tree:abc").await.is_none(),
            "the first purge must make the entry unreachable"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disabled_cache_answers_without_failing() {
        let c = Cache::connect(None).await;
        assert!(c.get("alice/web", "refs").await.is_none());
        c.put("alice/web", "refs", b"x", 5).await; // must not panic
        c.drop_refs("alice/web").await;
        // A disabled cache is not a failed purge: nothing is cached, so there is nothing to orphan.
        c.bump_generation("alice/web").await.unwrap();
    }

    /// Catches: a purge against an unreachable Redis reporting success, which is how a repo stays
    /// publicly cached after being made private.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_against_an_unreachable_redis_reports_failure() {
        // A refused port degrades to disabled, which is a correct no-op — so this needs a server
        // that connects and then refuses every command, the shape a real broken Redis has.
        let c = broken_cache_for_test(&["INCR"]).await;
        assert!(c.bump_generation("alice/web").await.is_err());
    }

    /// Catches: a `GET gen:{repo}` that errors (not merely misses) making `generation` answer 0,
    /// which reopens a purged repo's pre-purge entries during a Redis blip. `generation` must
    /// answer `None` instead, and `get`/`put` must treat that as "skip the cache", not "gen 0".
    #[tokio::test(flavor = "multi_thread")]
    async fn generation_error_disables_cache_not_defaults_to_zero() {
        let c = broken_cache_for_test(&["GET"]).await;
        assert_eq!(c.generation("alice/repo").await, None);
        // put would write under gen 0 if it fails open; instead it must be a no-op...
        c.put("alice/repo", "refs", b"stale", 60).await;
        // ...and get must not return that entry.
        assert_eq!(c.get("alice/repo", "refs").await, None);
    }

    /// A stub Redis that connects successfully (so `conn` is `Some`) but errors on every command
    /// whose name is in `error_on` and answers `+OK` to everything else — the shape a real broken
    /// Redis has, as opposed to a refused port, which degrades to `conn: None`.
    async fn broken_cache_for_test(error_on: &'static [&'static str]) -> Cache {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]).to_uppercase();
                        // Requests arrive pipelined, and redis-rs expects one reply per command;
                        // commands are RESP arrays, so count the `*` at each command boundary.
                        let cmds = req.matches("\r\n*").count() + 1;
                        let reply: Vec<u8> = if error_on.iter().any(|cmd| req.contains(cmd)) {
                            b"-ERR nope\r\n".repeat(cmds)
                        } else {
                            b"+OK\r\n".repeat(cmds)
                        };
                        if s.write_all(&reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let c = Cache::connect(Some(&format!("redis://{addr}"))).await;
        assert!(c.conn.is_some(), "the stub must connect, or this tests nothing");
        c
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_redis_degrades_to_disabled() {
        // Port 1 refuses instantly; connect must swallow it rather than propagate.
        let c = Cache::connect(Some("redis://127.0.0.1:1")).await;
        assert!(c.get("alice/web", "refs").await.is_none());
    }
}
