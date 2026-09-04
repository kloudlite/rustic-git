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

mod streams;

const KEY_VERSION: &str = "v1";
// ponytail: fixed per-command timeout; make configurable if a deployment needs a different bound
// than the connect timeout below.
const CMD_TIMEOUT: Duration = Duration::from_millis(250);
/// Bound for background stream maintenance (XAUTOCLAIM). Generous on purpose: it scans a consumer
/// group's pending list on the worker's own clock and nobody is blocked on the result.
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(3);

/// Generation and entry in one server round trip. The entry's key depends on the generation's
/// value, so a pipeline cannot express this and two sequential GETs were two RTTs on every read.
/// A missing generation is `'0'` here for the same reason `generation()` says zero — see its doc.
static GET_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        "local g = redis.call('GET', 'gen:' .. ARGV[1]) or '0'\n\
         return redis.call('GET', ARGV[2] .. ':' .. g .. ':' .. ARGV[1] .. ':' .. ARGV[3])",
    )
});

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
    /// Whether this cache has a live Redis connection — as opposed to the in-memory fallback or
    /// no cache at all. Used at worker startup: a stream-driven lane with no Redis is degraded
    /// (see `worker.rs`), and that is worth a loud one-line warning, not a silent default.
    pub fn connected(&self) -> bool {
        self.conn.is_some()
    }

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
            tracing::warn!(host = %host, "cache.unavailable");
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
        if let Some(m) = &self.mem {
            let gen = self.generation(repo).await?;
            return mem_get(m, &key(gen, repo, suffix));
        }
        let mut c = self.conn.clone()?;
        // A script error (the generation unreadable, a timeout) is a miss, exactly as a failed
        // `generation()` read is: never a guessed generation.
        // Bound first: `arg` returns a borrow of the invocation, and a chained temporary would
        // be dropped before the future that borrows it is awaited.
        let mut call = GET_SCRIPT.prepare_invoke();
        call.arg(repo).arg(KEY_VERSION).arg(suffix);
        let fut = call.invoke_async::<Option<Vec<u8>>>(&mut c);
        let r = tokio::time::timeout(CMD_TIMEOUT, fut).await;
        // Failing open is right — a miss is always safe — but silently, it is invisible: a Redis
        // that answers PING while refusing EVAL (no scripting, a proxy that drops it, a version
        // without it) turns the cache off fleet-wide and the only symptom is latency. Once per
        // process, so a broken backend cannot become the log.
        if !matches!(r, Ok(Ok(_))) {
            static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!("cache.script.failed");
            }
        }
        r.ok()?.ok().flatten()
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
}

/// Every command gets the same bound as `connect`: a live-but-black-holed connection must not
/// hang a request path. A timeout is treated exactly like any other command error — fail open.
async fn run<T: redis::FromRedisValue>(
    cmd: &mut redis::Cmd,
    c: &mut redis::aio::ConnectionManager,
) -> redis::RedisResult<T> {
    run_within(CMD_TIMEOUT, cmd, c).await
}

/// `run` with an explicit bound, for commands that are not on a request path.
///
/// `CMD_TIMEOUT` is sized for a cache read a user is waiting on. XAUTOCLAIM is neither: it runs on
/// the worker's own 60s clock, scans a consumer group's pending list, and nobody is blocked on it.
/// Held to 250ms against a managed Redis it timed out on EVERY attempt in production while every
/// other command in the fleet succeeded — so the stream's crashed-consumer redelivery never ran,
/// and the log filled with one failure per minute. Failing open kept merge work safe (the periodic
/// sweep is the floor) but the feature was silently dead.
async fn run_within<T: redis::FromRedisValue>(
    budget: Duration,
    cmd: &mut redis::Cmd,
    c: &mut redis::aio::ConnectionManager,
) -> redis::RedisResult<T> {
    match tokio::time::timeout(budget, cmd.query_async(c)).await {
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

    /// A disabled cache's `xreadgroup` must NOT block for `block_ms` — it has no connection to
    /// block on, so it fails open instantly. This is exactly the trap the merge worker's lane
    /// loop fell into: without its own `IDLE` backoff on the "nothing happened" path, a lane
    /// spun the merge claim as fast as Mongo answered whenever Redis was absent or down, because
    /// this call — its would-be pacing — returns immediately instead of blocking.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_disabled_cache_xreadgroup_does_not_block() {
        let c = Cache::connect(None).await;
        let started = Instant::now();
        let got = c.xreadgroup("events", "merge-worker", "consumer-1", 10).await;
        assert!(got.is_empty());
        assert!(started.elapsed() < Duration::from_millis(200), "must fail open instantly, not block");
    }

    /// Catches: a purge against an unreachable Redis reporting success, which is how a repo stays
    /// publicly cached after being made private.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_against_an_unreachable_redis_reports_failure() {
        // A refused port degrades to disabled, which is a correct no-op — so this needs a server
        // that connects and then refuses every command, the shape a real broken Redis has.
        let c = scripted_cache_for_test(&[("INCR", b"-ERR nope\r\n")]).await;
        assert!(c.bump_generation("alice/web").await.is_err());
    }

    /// Catches: a `GET gen:{repo}` that errors (not merely misses) making `generation` answer 0,
    /// which reopens a purged repo's pre-purge entries during a Redis blip. `generation` must
    /// answer `None` instead, and `get`/`put` must treat that as "skip the cache", not "gen 0".
    #[tokio::test(flavor = "multi_thread")]
    async fn generation_error_disables_cache_not_defaults_to_zero() {
        let c = scripted_cache_for_test(&[("GET", b"-ERR nope\r\n")]).await;
        assert_eq!(c.generation("alice/repo").await, None);
        // put would write under gen 0 if it fails open; instead it must be a no-op...
        c.put("alice/repo", "refs", b"stale", 60).await;
        // ...and get must not return that entry.
        assert_eq!(c.get("alice/repo", "refs").await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_redis_degrades_to_disabled() {
        // Port 1 refuses instantly; connect must swallow it rather than propagate.
        let c = Cache::connect(Some("redis://127.0.0.1:1")).await;
        assert!(c.get("alice/web", "refs").await.is_none());
    }

    /// A stub Redis that connects successfully (so `conn` is `Some`) and replies per the first
    /// rule whose command the request names — the shape a real BROKEN Redis has (`-ERR ...`), as
    /// opposed to a refused port, which degrades to `conn: None`, and the shape a real WORKING one
    /// has for `XREADGROUP`/`XAUTOCLAIM`, whose exact multi-bulk replies are what this crate's
    /// hand-written `FromRedisValue` parsing is checked against.
    async fn scripted_cache_for_test(rules: &'static [(&'static str, &'static [u8])]) -> Cache {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]).to_uppercase();
                        // Requests arrive pipelined and redis-rs expects one reply per command;
                        // commands are RESP arrays, so count the `*` at each command boundary.
                        // Multi-bulk rule bodies are only ever matched by an unpipelined call, so
                        // repeating them is a no-op there and the fix for a pipelined `-ERR`.
                        let cmds = req.matches("\r\n*").count() + 1;
                        let reply: Vec<u8> = match rules.iter().find(|(cmd, _)| req.contains(cmd)) {
                            Some((_, body)) => body.repeat(cmds),
                            // Anything unscripted (the connection handshake, etc.) gets one +OK.
                            None => b"+OK\r\n".repeat(cmds),
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

    /// The script call carries the right arguments in the right order, and the script body keys
    /// the entry the same way `key()` does — a stub that only counts round trips would pass on a
    /// script that read the wrong key.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_script_call_names_repo_version_and_suffix_in_order() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let rec = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let rec = rec.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let reply: Vec<u8> = if req.to_uppercase().contains("EVAL") {
                            rec.lock().unwrap().push(req.clone());
                            b"$4\r\nbody\r\n".to_vec()
                        } else {
                            b"+OK\r\n".repeat(req.matches("\r\n*").count() + 1)
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
        assert_eq!(c.get("alice/web", "refs").await.as_deref(), Some(&b"body"[..]));

        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "one script call, no fallback GETs: {calls:?}");
        // EVALSHA <sha> <numkeys> ARGV... — no KEYS, and the three ARGV in this order.
        let repo = calls[0].find("alice/web").expect("repo argument");
        let ver = calls[0].find(KEY_VERSION).expect("key-version argument");
        let suffix = calls[0].find("refs\r\n").expect("suffix argument");
        assert!(repo < ver && ver < suffix, "ARGV order repo, version, suffix: {}", calls[0]);
        assert!(calls[0].contains("\r\n0\r\n"), "numkeys is 0 — the keys go as ARGV: {}", calls[0]);

        // And the script builds exactly the key `key()` builds: {version}:{gen}:{repo}:{suffix}.
        let body = "local g = redis.call('GET', 'gen:' .. ARGV[1]) or '0'\n\
             return redis.call('GET', ARGV[2] .. ':' .. g .. ':' .. ARGV[1] .. ':' .. ARGV[3])";
        assert_eq!(GET_SCRIPT.get_hash(), redis::Script::new(body).get_hash(), "script body changed");
        assert_eq!(key(7, "alice/web", "refs"), format!("{KEY_VERSION}:7:alice/web:refs"));
    }

    /// One round trip per read: the generation and the entry are fetched by one server-side
    /// script. The stub answers the script call with a body; two sequential GETs would never
    /// see it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_is_one_script_call() {
        let c = scripted_cache_for_test(&[("EVAL", b"$4\r\nbody\r\n")]).await;
        assert_eq!(c.get("alice/web", "refs").await.as_deref(), Some(&b"body"[..]));
    }

    /// One delivered entry, the shape `XREADGROUP GROUP ... STREAMS events >` replies with:
    /// `[[stream_name, [[id, [field, value, ...]]]]]`.
    const XREADGROUP_ONE_ENTRY: &[u8] = b"*1\r\n*2\r\n$6\r\nevents\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*10\r\n$4\r\nkind\r\n$11\r\npull_opened\r\n$4\r\nrepo\r\n$3\r\na/b\r\n$6\r\nnumber\r\n$1\r\n3\r\n$5\r\nactor\r\n$1\r\nx\r\n$5\r\nat_ms\r\n$1\r\n0\r\n";

    #[tokio::test(flavor = "multi_thread")]
    async fn xreadgroup_parses_a_delivered_entry() {
        let c = scripted_cache_for_test(&[("XREADGROUP", XREADGROUP_ONE_ENTRY)]).await;
        let got = c.xreadgroup("events", "merge-worker", "consumer-1", 10).await;
        assert_eq!(got.len(), 1);
        let (id, fields) = &got[0];
        assert_eq!(id, "1-1");
        assert!(fields.contains(&("repo".to_string(), "a/b".to_string())));
        assert!(fields.contains(&("number".to_string(), "3".to_string())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn xreadgroup_empty_block_is_no_entries_not_an_error() {
        // A `BLOCK` timeout with nothing to deliver answers a nil array, not an empty one.
        let c = scripted_cache_for_test(&[("XREADGROUP", b"*-1\r\n")]).await;
        let got = c.xreadgroup("events", "merge-worker", "consumer-1", 10).await;
        assert!(got.is_empty());
    }

    /// `[cursor, [[id, [field, value, ...]]], deleted_ids]` — the shape a dead consumer's
    /// unacked entry comes back as when another consumer claims it.
    const XAUTOCLAIM_ONE_ENTRY: &[u8] = b"*3\r\n$3\r\n0-0\r\n*1\r\n*2\r\n$3\r\n2-1\r\n*2\r\n$4\r\nkind\r\n$10\r\nhead_moved\r\n*0\r\n";

    #[tokio::test(flavor = "multi_thread")]
    async fn xautoclaim_parses_a_reclaimed_entry() {
        let c = scripted_cache_for_test(&[("XAUTOCLAIM", XAUTOCLAIM_ONE_ENTRY)]).await;
        let got = c.xautoclaim("events", "merge-worker", "consumer-2", 30_000, 10).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "2-1");
        assert_eq!(got[0].1, vec![("kind".to_string(), "head_moved".to_string())]);
    }

    /// `XGROUP CREATE ... MKSTREAM` on a group that already exists must not surface as an error
    /// to the caller — every worker replica calls this on boot (see the doc comment).
    #[tokio::test(flavor = "multi_thread")]
    async fn xgroup_create_swallows_busygroup() {
        let c = scripted_cache_for_test(&[(
            "XGROUP",
            b"-BUSYGROUP Consumer Group name already exists\r\n",
        )])
        .await;
        c.xgroup_create_mkstream("events", "merge-worker").await; // must not panic
    }
}
