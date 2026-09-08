//! Turning `nodejs@20` into a `Lock`: a concrete version, a nixpkgs revision, and — when the
//! index can give one — the store path the agent substitutes instead of evaluating anything.
//!
//! Three places are asked, in this order, and the order is the whole design:
//!
//! 1. **The object-store cache.** `@20` is what a person TYPED, so it is the cache key: everyone
//!    asking for `nodejs@20` on the same day gets the same lock, and Nixhub is asked once per
//!    package per day per region instead of once per workspace write.
//! 2. **Nixhub** (`search.devbox.sh`), because it is the only source that carries a store path —
//!    a Nixhub lock costs the agent one `nix copy`, no nixpkgs evaluation at all.
//! 3. **The mirror** (`index/pkgs/versions.json`, refreshed daily by the api's `user` role). It has `rev` and
//!    `attr_path` but no store path, so a mirror lock costs the agent a full nixpkgs evaluation
//!    (~28 s cold) on every node that builds it. Correct, just slower — hence second, and hence
//!    never cached (see `Resolver::store`).
//!
//! Nothing here guesses. Unknown everywhere is a refusal naming the nearest versions; unavailable
//! everywhere is a refusal to write at all, because a workspace with a wrong lock is worse than a
//! workspace that could not be created.
//!
//! Version requests are matched as STRING prefixes, deliberately: a leading-zero request like
//! `@01` is legal grammar and is passed through as typed, so it matches a release literally named
//! `01…` and nothing else. There is no numeric normalisation of the request — only of the
//! comparison between candidate releases, when picking the newest one under a prefix.

use crate::crd::{Lock, LockSource};
use crate::packages::{parse_entry, VersionReq};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use slatedb::object_store::{path::Path as OsPath, ObjectStore, ObjectStoreExt, PutPayload};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long a resolution stays good. A day: long enough that a busy region asks Nixhub once per
/// package, short enough that `@latest` follows a release within a day.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);

/// The only system we resolve for today (see the spec's "out of scope").
const SYSTEM: &str = "x86_64-linux";
/// Where the mirrored nixpkgs-multiverse index lives. Written by the admin tier's daily beat.
pub const MIRROR_KEY: &str = "index/pkgs/versions.json";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// How a version request appears in a URL and a cache key: exactly what the person typed.
fn req_str(v: &VersionReq) -> &str {
    match v {
        VersionReq::Latest => "latest",
        VersionReq::Prefix(p) => p,
    }
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The entry is not something we can resolve at all — bad grammar, or no `@version` on an
    /// entry only pinned entries may reach. The caller's bug or the person's typo: a 400.
    Malformed(String),
    /// Nobody published this. `nearest` is the three closest versions that DO exist.
    Unknown { entry: String, nearest: Vec<String> },
    /// Every index failed and no cache entry covered it. Nothing is written.
    Unavailable,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Malformed(e) => write!(f, "{e} is not a package pinned to a version"),
            Refusal::Unknown { entry, nearest } if nearest.is_empty() => {
                write!(f, "{entry} is not a version anyone published")
            }
            Refusal::Unknown { entry, nearest } => {
                write!(
                    f,
                    "{entry} is not a version anyone published; nearest: {}",
                    nearest.join(", ")
                )
            }
            Refusal::Unavailable => write!(f, "the package index is unavailable; try again"),
        }
    }
}

// ---------------------------------------------------------------------------- Nixhub

pub struct Nixhub {
    pub client: reqwest::Client,
    /// `https://search.devbox.sh`, no trailing slash.
    pub base: String,
}

#[async_trait]
impl Index for Nixhub {
    async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String> {
        let url = format!("{}/v2/resolve", self.base);
        let r = self
            .client
            .get(&url)
            .query(&[("name", attr), ("version", req_str(version))])
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(format!("nixhub answered {}", r.status()));
        }
        let body: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
        nixhub_lock(attr, version, &body)
    }

    async fn versions(&self, attr: &str) -> Result<Vec<String>, String> {
        let url = format!("{}/v2/pkg", self.base);
        let r = self
            .client
            .get(&url)
            .query(&[("name", attr)])
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("nixhub answered {}", r.status()));
        }
        let body: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
        // Already newest-first from the API; we keep its order rather than re-sorting, so a
        // version string it knows how to order and we do not (a date-shaped release, say) is not
        // scrambled by our numeric comparison.
        Ok(body["releases"]
            .as_array()
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| r["version"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// The shape verified against the live API on 2026-09-08.
///
/// Two different failures, deliberately kept apart: a body that simply does not carry OUR system
/// is `Ok(None)` — the version exists, just not for x86_64-linux, which is "unknown" from here —
/// while a body that carries the system but is missing a field we need is `Err`. A broken upstream
/// response is not the person's typo, and answering it with "nobody published that" would send
/// them hunting for a version that exists.
fn nixhub_lock(
    attr: &str,
    version: &VersionReq,
    body: &serde_json::Value,
) -> Result<Option<Lock>, String> {
    let Some(sys) = body["systems"].get(SYSTEM) else {
        return Ok(None);
    };
    let inst = &sys["flake_installable"];
    let outputs = sys["outputs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let store_path = outputs
        .iter()
        .find(|o| o["default"].as_bool() == Some(true))
        .or(outputs.first())
        .and_then(|o| o["path"].as_str());
    let (Some(v), Some(attr_path), Some(rev), Some(store_path)) = (
        body["version"].as_str(),
        inst["attr_path"].as_str(),
        inst["ref"]["rev"].as_str(),
        store_path,
    ) else {
        return Err(format!(
            "nixhub answered for {attr} without version, attr_path, rev or an output path"
        ));
    };
    Ok(Some(Lock {
        entry: entry_string(attr, version),
        version: v.to_string(),
        attr_path: attr_path.to_string(),
        rev: rev.to_string(),
        store_path: store_path.to_string(),
        resolved_at: String::new(), // stamped by the Resolver, the one holder of the clock
        source: LockSource::Nixhub,
    }))
}

fn entry_string(attr: &str, version: &VersionReq) -> String {
    format!("{attr}@{}", req_str(version))
}

// ---------------------------------------------------------------------------- Mirror

/// One release row of the mirrored index. Normalised on WRITE (the admin beat), so this reader
/// knows exactly one shape.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MirrorRow {
    pub version: String,
    pub rev: String,
    pub attr_path: String,
}

pub type MirrorIndex = HashMap<String, Vec<MirrorRow>>;

pub struct Mirror {
    pub os: Arc<dyn ObjectStore>,
    /// The parsed index, good for `CACHE_TTL`. Every `@` entry of every write asks for it, and it
    /// is one multi-megabyte file refreshed once a day — re-fetching and re-parsing it per entry
    /// was pure load.
    cached: tokio::sync::Mutex<Option<(std::time::Instant, Arc<MirrorIndex>)>>,
}

impl Mirror {
    pub fn new(os: Arc<dyn ObjectStore>) -> Self {
        Mirror {
            os,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// The newest row for `attr` under the request. `None` = the attribute or the prefix is not in
    /// the mirror at all.
    pub fn pick(index: &MirrorIndex, attr: &str, version: &VersionReq) -> Option<MirrorRow> {
        let rows = index.get(attr)?;
        let under: Vec<&MirrorRow> = match version {
            VersionReq::Latest => rows.iter().collect(),
            // String prefix on purpose: `@20` means releases named 20 or 20.something, never
            // 200.x, and a leading-zero request matches only a literally leading-zero release.
            VersionReq::Prefix(p) => rows
                .iter()
                .filter(|r| r.version == *p || r.version.starts_with(&format!("{p}.")))
                .collect(),
        };
        under
            .into_iter()
            .max_by_key(|r| numeric(&r.version))
            .cloned()
    }

    /// `Ok(None)` = there is no index at all. A region whose refresh beat has never run knows
    /// nothing; it has not FAILED to tell us, so a typo there is still the person's 422 with
    /// Nixhub's nearest versions rather than a 503 blaming the platform.
    ///
    /// A not-found or a failed refresh keeps the copy already parsed, if there is one: a stale
    /// index resolves an older revision, which is slower to build and never wrong.
    async fn index(&self) -> Result<Option<Arc<MirrorIndex>>, String> {
        let mut slot = self.cached.lock().await;
        if let Some((at, idx)) = slot.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return Ok(Some(idx.clone()));
            }
        }
        let previous = || slot.as_ref().map(|(_, i)| i.clone());
        match self.fetch().await {
            Ok(Some(idx)) => {
                let idx = Arc::new(idx);
                *slot = Some((std::time::Instant::now(), idx.clone()));
                Ok(Some(idx))
            }
            Ok(None) => Ok(previous()),
            Err(e) => previous().map(Some).ok_or(e),
        }
    }

    async fn fetch(&self) -> Result<Option<MirrorIndex>, String> {
        let got = match self.os.get(&OsPath::from(MIRROR_KEY)).await {
            Ok(g) => g,
            Err(slatedb::object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let bytes = got.bytes().await.map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map(Some).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Index for Mirror {
    async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String> {
        // An UNPARSABLE index is unavailable (see `index`); an absent one is simply a mirror with
        // nothing in it, which is an answer.
        let Some(index) = self.index().await? else {
            return Ok(None);
        };
        Ok(Mirror::pick(&index, attr, version).map(|row| Lock {
            entry: entry_string(attr, version),
            version: row.version,
            attr_path: row.attr_path,
            rev: row.rev,
            store_path: String::new(), // the mirror has none; the agent evaluates `rev` instead
            resolved_at: String::new(),
            source: LockSource::Mirror,
        }))
    }

    async fn versions(&self, attr: &str) -> Result<Vec<String>, String> {
        let Some(index) = self.index().await? else {
            return Ok(Vec::new());
        };
        let mut rows = index.get(attr).cloned().unwrap_or_default();
        rows.sort_by_key(|a| std::cmp::Reverse(numeric(&a.version)));
        Ok(rows.into_iter().map(|r| r.version).collect())
    }
}

/// A dotted version as numbers, for "which of these is newest". Non-numeric components sort as 0
/// rather than failing: the mirror is a third party's file and one odd row must not break a pick.
fn numeric(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

// ---------------------------------------------------------------------------- Resolver

pub struct Resolver {
    pub cache: Arc<dyn ObjectStore>,
    pub nixhub: Arc<dyn Index>,
    pub mirror: Arc<dyn Index>,
    /// Injected so tests own the clock; production passes `Utc::now`.
    pub now: fn() -> DateTime<Utc>,
}

impl Resolver {
    /// The production wiring: the tier's own object store as both cache and mirror, Nixhub at
    /// `KLOUDLITE_NIXHUB_URL` (default `https://search.devbox.sh`). Here rather than in
    /// `bins/api` so the http client and the clock stay this crate's dependencies.
    pub fn from_env(os: Arc<dyn ObjectStore>) -> Self {
        Resolver {
            cache: os.clone(),
            nixhub: Arc::new(Nixhub {
                client: reqwest::Client::new(),
                base: std::env::var("KLOUDLITE_NIXHUB_URL")
                    .unwrap_or_else(|_| "https://search.devbox.sh".to_string()),
            }),
            mirror: Arc::new(Mirror::new(os)),
            now: Utc::now,
        }
    }

    /// One entry. `skip_cache` is the update route's whole point: the cache is what makes an
    /// ordinary write cheap, and it is exactly what would make "update my packages" a no-op for
    /// the next 24 h.
    pub async fn lock_one(&self, entry: &str, skip_cache: bool) -> Result<Lock, Refusal> {
        // Callers pass entries that already parsed and already carry a version (`pinned`), so
        // anything else is a bug upstream or a typo — never something to guess a lock for, and
        // never "nobody published that", which would send a person hunting for a version.
        let Some((attr, req)) = parse_entry(entry)
            .ok()
            .and_then(|e| e.version.map(|v| (e.attr, v)))
        else {
            return Err(Refusal::Malformed(entry.to_string()));
        };

        if !skip_cache {
            if let Some(lock) = self.cached(&attr, &req).await {
                return Ok(lock);
            }
        }

        let mut unavailable = false;
        for index in [&self.nixhub, &self.mirror] {
            match index.resolve(&attr, &req).await {
                Ok(Some(mut lock)) => {
                    lock.resolved_at = (self.now)().to_rfc3339();
                    self.store(&attr, &req, &lock).await;
                    return Ok(lock);
                }
                Ok(None) => {}
                Err(_) => unavailable = true,
            }
        }
        if unavailable {
            // At least one index could not answer, so "unknown" would be a lie.
            return Err(Refusal::Unavailable);
        }
        Err(Refusal::Unknown {
            entry: entry.to_string(),
            nearest: self.nearest(&attr, &req).await,
        })
    }

    /// Locks for `packages`. Entries whose string is unchanged keep their existing lock — editing
    /// one entry must never move another one's version. `refresh_all` is the update route: every
    /// `@` entry is re-resolved, exact pins included (a newer nixpkgs revision can build the same
    /// version), and it bypasses the cache so the answer is actually new.
    ///
    /// An update that runs during an index outage KEEPS the locks it already had rather than
    /// failing the whole write: "I could not check" is not a reason to take a working version away
    /// from a workspace. `Unknown` and `Malformed` still refuse — those are answers, not outages.
    pub async fn lock_all(
        &self,
        packages: &[String],
        prev: &[Lock],
        refresh_all: bool,
    ) -> Result<Vec<Lock>, Refusal> {
        let mut out = Vec::new();
        let mut kept_on_outage = 0usize;
        for p in packages.iter().filter(|p| p.contains('@')) {
            let existing = prev.iter().find(|l| l.entry == *p);
            if let Some(l) = existing.filter(|_| !refresh_all) {
                out.push(l.clone());
                continue;
            }
            match (self.lock_one(p, refresh_all).await, existing) {
                (Ok(lock), _) => out.push(lock),
                (Err(Refusal::Unavailable), Some(l)) => {
                    out.push(l.clone());
                    kept_on_outage += 1;
                }
                (Err(e), _) => return Err(e),
            }
        }
        // Once per call, not per entry: an outage hits every entry and one line per package would
        // bury the fact that the update silently did nothing.
        if kept_on_outage > 0 {
            tracing::warn!(
                reason = "index-unavailable",
                kept = kept_on_outage,
                "package update kept existing locks"
            );
        }
        Ok(out)
    }

    async fn cached(&self, attr: &str, req: &VersionReq) -> Option<Lock> {
        let key = OsPath::from(cache_key(attr, req));
        let bytes = self.cache.get(&key).await.ok()?.bytes().await.ok()?;
        let lock: Lock = serde_json::from_slice(&bytes).ok()?;
        let at = DateTime::parse_from_rfc3339(&lock.resolved_at).ok()?;
        let age = (self.now)().signed_duration_since(at.with_timezone(&Utc));
        (age >= chrono::Duration::zero() && age.to_std().ok()? < CACHE_TTL).then_some(lock)
    }

    /// A cache write that fails costs one extra index call later — never the resolution itself.
    ///
    /// A MIRROR lock is not written at all: it is a degraded answer (no store path, so every node
    /// that builds it pays a full nixpkgs evaluation) produced by a Nixhub outage, and caching it
    /// would outlive the outage by a day. Not caching it means Nixhub is retried on the very next
    /// resolve, at the cost of re-reading the mirror index while the outage lasts.
    // ponytail: all-or-nothing by source; give mirror locks their own short TTL if the mirror read
    // ever shows up as a cost.
    async fn store(&self, attr: &str, req: &VersionReq, lock: &Lock) {
        if lock.source == LockSource::Mirror {
            return;
        }
        let key = OsPath::from(cache_key(attr, req));
        if let Ok(bytes) = serde_json::to_vec(lock) {
            let _ = self.cache.put(&key, PutPayload::from(bytes)).await;
        }
    }

    /// The three published versions closest to what was asked, from whichever index will talk.
    async fn nearest(&self, attr: &str, req: &VersionReq) -> Vec<String> {
        let mut all = match self.nixhub.versions(attr).await {
            Ok(v) if !v.is_empty() => v,
            _ => self.mirror.versions(attr).await.unwrap_or_default(),
        };
        let VersionReq::Prefix(want) = req else {
            // `@latest` is not near anything in particular: the newest three ARE the answer, in
            // the order the index gave them.
            all.truncate(3);
            return all;
        };
        // Shared prefix first (`20.99` is nearer 20.x than 18.x whatever the arithmetic says),
        // then numeric distance component by component.
        all.sort_by_key(|v| {
            (
                std::cmp::Reverse(shared_prefix(want, v)),
                distance(&numeric(want), &numeric(v)),
            )
        });
        all.truncate(3);
        all
    }
}

fn shared_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

fn distance(a: &[u64], b: &[u64]) -> Vec<u64> {
    (0..a.len().max(b.len()))
        .map(|i| {
            a.get(i)
                .copied()
                .unwrap_or(0)
                .abs_diff(b.get(i).copied().unwrap_or(0))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::memory::InMemory;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn lock(entry: &str, version: &str, source: LockSource) -> Lock {
        Lock {
            entry: entry.into(),
            version: version.into(),
            attr_path: "nodejs_20".into(),
            rev: "389ed853".into(),
            store_path: if source == LockSource::Nixhub {
                "/nix/store/x-nodejs".into()
            } else {
                String::new()
            },
            resolved_at: String::new(),
            source,
        }
    }

    #[derive(Default)]
    struct FakeIndex {
        answers: HashMap<(String, String), Option<Lock>>,
        versions: HashMap<String, Vec<String>>,
        /// Everything fails — an index that is up but knows nothing is `answers` returning None.
        down: bool,
        panic_on_call: bool,
        /// Every call, `resolve` and `versions` alike, so a test can pin which index was asked.
        calls: AtomicUsize,
    }

    impl FakeIndex {
        fn with(entries: &[(&str, &str, Option<Lock>)]) -> Self {
            FakeIndex {
                answers: entries
                    .iter()
                    .map(|(a, v, l)| ((a.to_string(), v.to_string()), l.clone()))
                    .collect(),
                ..Default::default()
            }
        }
        fn down() -> Self {
            FakeIndex {
                down: true,
                ..Default::default()
            }
        }
        fn knows(mut self, attr: &str, versions: &[&str]) -> Self {
            self.versions.insert(
                attr.into(),
                versions.iter().map(|s| s.to_string()).collect(),
            );
            self
        }
        fn count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        fn enter(&self) -> Result<(), String> {
            assert!(!self.panic_on_call, "this index must not be asked");
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.down {
                return Err("down".into());
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Index for FakeIndex {
        async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String> {
            self.enter()?;
            Ok(self
                .answers
                .get(&(attr.to_string(), req_str(version).to_string()))
                .cloned()
                .flatten())
        }
        async fn versions(&self, attr: &str) -> Result<Vec<String>, String> {
            self.enter()?;
            Ok(self.versions.get(attr).cloned().unwrap_or_default())
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn now() -> DateTime<Utc> {
        at("2026-09-08T12:00:00Z")
    }

    fn resolver(nixhub: Arc<dyn Index>, mirror: Arc<dyn Index>) -> Resolver {
        Resolver {
            cache: Arc::new(InMemory::new()),
            nixhub,
            mirror,
            now,
        }
    }

    async fn put_cache(r: &Resolver, attr: &str, req: &VersionReq, l: &Lock) {
        r.cache
            .put(
                &OsPath::from(cache_key(attr, req)),
                PutPayload::from(serde_json::to_vec(l).unwrap()),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_hit_in_the_cache_asks_nobody() {
        let never = Arc::new(FakeIndex {
            panic_on_call: true,
            ..Default::default()
        });
        let r = resolver(never.clone(), never);
        let mut cached = lock("nodejs@20", "20.20.2", LockSource::Nixhub);
        cached.resolved_at = at("2026-09-08T09:00:00Z").to_rfc3339();
        put_cache(&r, "nodejs", &VersionReq::Prefix("20".into()), &cached).await;

        assert_eq!(r.lock_one("nodejs@20", false).await.unwrap(), cached);
    }

    #[tokio::test]
    async fn nixhub_answers_first_and_the_answer_is_cached() {
        let nix = Arc::new(FakeIndex::with(&[(
            "nodejs",
            "20",
            Some(lock("nodejs@20", "20.20.2", LockSource::Nixhub)),
        )]));
        let r = resolver(nix.clone(), Arc::new(FakeIndex::default()));

        let first = r.lock_one("nodejs@20", false).await.unwrap();
        assert_eq!(first.version, "20.20.2");
        assert_eq!(first.resolved_at, now().to_rfc3339());
        assert_eq!(r.lock_one("nodejs@20", false).await.unwrap(), first);
        assert_eq!(nix.count(), 1, "the second answer came from the cache");
    }

    #[tokio::test]
    async fn skipping_the_cache_re_resolves_and_refreshes_it() {
        let nix = Arc::new(FakeIndex::with(&[(
            "nodejs",
            "20",
            Some(lock("nodejs@20", "20.20.2", LockSource::Nixhub)),
        )]));
        let r = resolver(nix.clone(), Arc::new(FakeIndex::default()));
        let mut stale = lock("nodejs@20", "20.5.0", LockSource::Nixhub);
        stale.resolved_at = at("2026-09-08T11:00:00Z").to_rfc3339(); // fresh, an hour old
        put_cache(&r, "nodejs", &VersionReq::Prefix("20".into()), &stale).await;

        assert_eq!(
            r.lock_one("nodejs@20", true).await.unwrap().version,
            "20.20.2"
        );
        assert_eq!(nix.count(), 1);
        // and the fresh answer replaced the cached one, so the next ordinary read sees it too
        assert_eq!(
            r.lock_one("nodejs@20", false).await.unwrap().version,
            "20.20.2"
        );
        assert_eq!(nix.count(), 1);
    }

    #[tokio::test]
    async fn the_mirror_answers_when_nixhub_is_unavailable() {
        let mirror = Arc::new(FakeIndex::with(&[(
            "python3",
            "3.11",
            Some(lock("python3@3.11", "3.11.9", LockSource::Mirror)),
        )]));
        let r = resolver(Arc::new(FakeIndex::down()), mirror);

        let l = r.lock_one("python3@3.11", false).await.unwrap();
        assert_eq!(l.source, LockSource::Mirror);
        assert_eq!(l.store_path, "");
    }

    #[tokio::test]
    async fn a_mirror_lock_is_never_cached_so_nixhub_is_retried() {
        let mirror = Arc::new(FakeIndex::with(&[(
            "python3",
            "3.11",
            Some(lock("python3@3.11", "3.11.9", LockSource::Mirror)),
        )]));
        let r = resolver(Arc::new(FakeIndex::down()), mirror.clone());

        r.lock_one("python3@3.11", false).await.unwrap();
        r.lock_one("python3@3.11", false).await.unwrap();
        assert_eq!(
            mirror.count(),
            2,
            "a degraded answer must not outlive the outage"
        );
    }

    #[tokio::test]
    async fn unknown_everywhere_is_a_refusal_naming_the_nearest_versions() {
        let nix = FakeIndex::with(&[("nodejs", "20.99", None)])
            .knows("nodejs", &["20.20.2", "20.19.5", "20.18.3", "18.1.0"]);
        let r = resolver(Arc::new(nix), Arc::new(FakeIndex::default()));

        assert_eq!(
            r.lock_one("nodejs@20.99", false).await.unwrap_err(),
            Refusal::Unknown {
                entry: "nodejs@20.99".into(),
                nearest: vec!["20.20.2".into(), "20.19.5".into(), "20.18.3".into()],
            }
        );
    }

    #[tokio::test]
    async fn nearest_for_latest_is_the_index_order_untouched() {
        let nix = FakeIndex::with(&[("nodejs", "latest", None)])
            .knows("nodejs", &["26.8.1", "24.2.0", "22.1.0", "20.20.2"]);
        let r = resolver(Arc::new(nix), Arc::new(FakeIndex::default()));

        let Refusal::Unknown { nearest, .. } =
            r.lock_one("nodejs@latest", false).await.unwrap_err()
        else {
            panic!("expected Unknown")
        };
        assert_eq!(nearest, vec!["26.8.1", "24.2.0", "22.1.0"]);
    }

    #[tokio::test]
    async fn nearest_asks_nixhub_and_falls_back_to_the_mirror() {
        // nixhub knows the versions: the mirror is asked to RESOLVE and nothing more.
        let mirror = Arc::new(FakeIndex::with(&[("nodejs", "20.99", None)]));
        let nix = FakeIndex::with(&[("nodejs", "20.99", None)]).knows("nodejs", &["20.20.2"]);
        let r = resolver(Arc::new(nix), mirror.clone());
        r.lock_one("nodejs@20.99", false).await.unwrap_err();
        assert_eq!(
            mirror.count(),
            1,
            "nixhub answered nearest, so the mirror was only resolved"
        );

        // nixhub knows none: the mirror answers nearest too.
        let mirror = Arc::new(
            FakeIndex::with(&[("nodejs", "20.99", None)]).knows("nodejs", &["20.20.2", "18.1.0"]),
        );
        let r = resolver(
            Arc::new(FakeIndex::with(&[("nodejs", "20.99", None)])),
            mirror.clone(),
        );
        assert_eq!(
            r.lock_one("nodejs@20.99", false).await.unwrap_err(),
            Refusal::Unknown {
                entry: "nodejs@20.99".into(),
                nearest: vec!["20.20.2".into(), "18.1.0".into()],
            }
        );
        assert_eq!(mirror.count(), 2, "resolve, then versions");
    }

    #[tokio::test]
    async fn unavailable_everywhere_with_no_cache_is_unavailable() {
        let r = resolver(Arc::new(FakeIndex::down()), Arc::new(FakeIndex::down()));
        assert_eq!(
            r.lock_one("nodejs@20", false).await.unwrap_err(),
            Refusal::Unavailable
        );
    }

    #[tokio::test]
    async fn an_entry_that_is_not_a_pinned_package_is_malformed_not_unknown() {
        let never = Arc::new(FakeIndex {
            panic_on_call: true,
            ..Default::default()
        });
        let r = resolver(never.clone(), never);

        assert_eq!(
            r.lock_one("nodejs", false).await.unwrap_err(),
            Refusal::Malformed("nodejs".into()),
            "no version to resolve"
        );
        assert_eq!(
            r.lock_one("nodejs@^20", false).await.unwrap_err(),
            Refusal::Malformed("nodejs@^20".into()),
        );
    }

    #[tokio::test]
    async fn a_stale_cache_entry_is_ignored_but_a_fresh_one_wins() {
        let nix = Arc::new(FakeIndex::with(&[(
            "nodejs",
            "20",
            Some(lock("nodejs@20", "20.20.2", LockSource::Nixhub)),
        )]));
        let r = resolver(nix.clone(), Arc::new(FakeIndex::default()));

        let mut stale = lock("nodejs@20", "20.1.0", LockSource::Nixhub);
        stale.resolved_at = at("2026-09-07T11:00:00Z").to_rfc3339(); // 25 h old
        put_cache(&r, "nodejs", &VersionReq::Prefix("20".into()), &stale).await;

        assert_eq!(
            r.lock_one("nodejs@20", false).await.unwrap().version,
            "20.20.2"
        );
        assert_eq!(nix.count(), 1);
    }

    #[tokio::test]
    async fn lock_all_keeps_untouched_locks_and_resolves_only_new_entries() {
        let nix = Arc::new(FakeIndex::with(&[
            (
                "nodejs",
                "20",
                Some(lock("nodejs@20", "20.20.2", LockSource::Nixhub)),
            ),
            (
                "python3",
                "3.11",
                Some(lock("python3@3.11", "3.11.9", LockSource::Nixhub)),
            ),
        ]));
        let r = resolver(nix.clone(), Arc::new(FakeIndex::default()));

        let mut prev = lock("nodejs@20", "20.5.0", LockSource::Nixhub);
        prev.resolved_at = at("2026-01-01T00:00:00Z").to_rfc3339();
        let packages: Vec<String> = ["nodejs@20", "jq", "python3@3.11"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let locks = r
            .lock_all(&packages, std::slice::from_ref(&prev), false)
            .await
            .unwrap();
        assert_eq!(locks.len(), 2, "the bare entry gets no lock");
        assert_eq!(
            locks[0], prev,
            "an untouched entry keeps its lock, however old"
        );
        assert_eq!(locks[1].version, "3.11.9");
        assert_eq!(nix.count(), 1);

        let refreshed = r
            .lock_all(&packages, std::slice::from_ref(&prev), true)
            .await
            .unwrap();
        assert_eq!(refreshed[0].version, "20.20.2");
        // BOTH went to the index: an update that answered from the cache would be a no-op for a
        // day, which is exactly what the update route exists not to be.
        assert_eq!(nix.count(), 3);
    }

    #[tokio::test]
    async fn a_refresh_during_an_outage_keeps_the_locks_it_had() {
        let r = resolver(Arc::new(FakeIndex::down()), Arc::new(FakeIndex::down()));
        let mut prev = lock("nodejs@20", "20.5.0", LockSource::Nixhub);
        prev.resolved_at = at("2026-01-01T00:00:00Z").to_rfc3339();
        let packages = vec!["nodejs@20".to_string()];

        let locks = r
            .lock_all(&packages, std::slice::from_ref(&prev), true)
            .await
            .unwrap();
        assert_eq!(
            locks,
            vec![prev],
            "an outage must not take a working version away"
        );

        // ...but with nothing to keep there is no lock to write, and the write must not proceed
        assert_eq!(
            r.lock_all(&packages, &[], true).await.unwrap_err(),
            Refusal::Unavailable
        );
    }

    const INDEX: &str = r#"{"nodejs":[{"version":"20.9.0","rev":"aaa","attr_path":"nodejs_20"}]}"#;

    async fn mirror_over(body: Option<&str>) -> (Arc<InMemory>, Mirror) {
        let os = Arc::new(InMemory::new());
        if let Some(b) = body {
            os.put(&OsPath::from(MIRROR_KEY), PutPayload::from(b.as_bytes().to_vec()))
                .await
                .unwrap();
        }
        let m = Mirror::new(os.clone());
        (os, m)
    }

    #[tokio::test]
    async fn a_mirror_with_no_index_knows_nothing_rather_than_failing() {
        let (_os, m) = mirror_over(None).await;
        assert_eq!(m.resolve("nodejs", &VersionReq::Prefix("20".into())).await, Ok(None));
        assert_eq!(m.versions("nodejs").await, Ok(vec![]));

        // ...and end to end: a typo on a region whose mirror has never been written is the
        // person's 422 with nearest versions, never a 503.
        let nix = FakeIndex::with(&[("nodejs", "20.99", None)]).knows("nodejs", &["20.20.2"]);
        let r = resolver(Arc::new(nix), Arc::new(m));
        assert_eq!(
            r.lock_one("nodejs@20.99", false).await.unwrap_err(),
            Refusal::Unknown {
                entry: "nodejs@20.99".into(),
                nearest: vec!["20.20.2".into()],
            }
        );
    }

    #[tokio::test]
    async fn the_index_is_read_once_and_then_served_from_memory() {
        let (os, m) = mirror_over(Some(INDEX)).await;
        let req = VersionReq::Prefix("20".into());
        assert_eq!(m.resolve("nodejs", &req).await.unwrap().unwrap().version, "20.9.0");

        // A changed file the reader never sees is the proof there was no second GET.
        os.put(
            &OsPath::from(MIRROR_KEY),
            PutPayload::from(r#"{"nodejs":[{"version":"20.99.0","rev":"bbb","attr_path":"nodejs_20"}]}"#.as_bytes().to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(m.resolve("nodejs", &req).await.unwrap().unwrap().version, "20.9.0");
        assert_eq!(m.versions("nodejs").await.unwrap(), vec!["20.9.0"]);
    }

    #[test]
    fn mirror_picks_the_newest_release_under_the_prefix() {
        let index: MirrorIndex = serde_json::from_str(
            r#"{"nodejs":[
                 {"version":"20.9.0","rev":"aaa","attr_path":"nodejs_20"},
                 {"version":"20.20.2","rev":"bbb","attr_path":"nodejs_20"},
                 {"version":"200.0.1","rev":"ccc","attr_path":"nodejs_200"},
                 {"version":"22.1.0","rev":"ddd","attr_path":"nodejs_22"}]}"#,
        )
        .unwrap();

        let got = Mirror::pick(&index, "nodejs", &VersionReq::Prefix("20".into())).unwrap();
        assert_eq!(
            (
                got.version.as_str(),
                got.rev.as_str(),
                got.attr_path.as_str()
            ),
            ("20.20.2", "bbb", "nodejs_20")
        );
        assert_eq!(
            Mirror::pick(&index, "nodejs", &VersionReq::Latest)
                .unwrap()
                .version,
            "200.0.1"
        );
        assert_eq!(Mirror::pick(&index, "ruby", &VersionReq::Latest), None);
    }

    #[test]
    fn a_nixhub_body_without_our_system_is_unknown_but_a_broken_one_is_an_error() {
        let req = VersionReq::Prefix("20".into());
        let body: serde_json::Value = serde_json::from_str(
            r#"{"name":"nodejs","version":"20.20.2","systems":{"aarch64-darwin":{}}}"#,
        )
        .unwrap();
        assert_eq!(nixhub_lock("nodejs", &req, &body), Ok(None));

        // our system, but no revision: upstream is broken, not the person's version
        let body: serde_json::Value = serde_json::from_str(
            r#"{"version":"20.20.2","systems":{"x86_64-linux":{
                 "flake_installable":{"ref":{},"attr_path":"nodejs_20"},
                 "outputs":[{"name":"out","path":"/nix/store/o","default":true}]}}}"#,
        )
        .unwrap();
        assert!(nixhub_lock("nodejs", &req, &body).is_err());

        let body: serde_json::Value = serde_json::from_str(
            r#"{"version":"20.20.2","systems":{"x86_64-linux":{
                 "flake_installable":{"ref":{"rev":"389ed85"},"attr_path":"nodejs_20"},
                 "outputs":[{"name":"lib","path":"/nix/store/l"},
                            {"name":"out","path":"/nix/store/o","default":true}]}}}"#,
        )
        .unwrap();
        let l = nixhub_lock("nodejs", &req, &body).unwrap().unwrap();
        assert_eq!(
            (l.entry.as_str(), l.rev.as_str(), l.store_path.as_str()),
            ("nodejs@20", "389ed85", "/nix/store/o")
        );
    }
}
