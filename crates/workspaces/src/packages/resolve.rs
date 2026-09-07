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
//! 3. **The mirror** (`index/pkgs/versions.json`, refreshed by the admin tier). It has `rev` and
//!    `attr_path` but no store path, so a mirror lock costs the agent a full nixpkgs evaluation
//!    (~28 s cold) on every node that builds it. Correct, just slower — hence second.
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
    /// Nobody published this. `nearest` is the three closest versions that DO exist.
    Unknown { entry: String, nearest: Vec<String> },
    /// Every index failed and no cache entry covered it. Nothing is written.
    Unavailable,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Unknown { entry, nearest } if nearest.is_empty() => {
                write!(f, "{entry} is not a version anyone published")
            }
            Refusal::Unknown { entry, nearest } => {
                write!(f, "{entry} is not a version anyone published; nearest: {}", nearest.join(", "))
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
        Ok(nixhub_lock(attr, version, &body))
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
            .map(|rs| rs.iter().filter_map(|r| r["version"].as_str().map(str::to_string)).collect())
            .unwrap_or_default())
    }
}

/// The shape verified against the live API on 2026-09-08. A body that does not carry our system
/// is `None` — the version exists, just not for us, which is "unknown" from here.
fn nixhub_lock(attr: &str, version: &VersionReq, body: &serde_json::Value) -> Option<Lock> {
    let sys = body["systems"].get(SYSTEM)?;
    let inst = &sys["flake_installable"];
    let outputs = sys["outputs"].as_array().map(Vec::as_slice).unwrap_or_default();
    let store_path = outputs
        .iter()
        .find(|o| o["default"].as_bool() == Some(true))
        .or(outputs.first())
        .and_then(|o| o["path"].as_str())
        .unwrap_or_default();
    Some(Lock {
        entry: entry_string(attr, version),
        version: body["version"].as_str()?.to_string(),
        attr_path: inst["attr_path"].as_str()?.to_string(),
        rev: inst["ref"]["rev"].as_str()?.to_string(),
        store_path: store_path.to_string(),
        resolved_at: String::new(), // stamped by the Resolver, the one holder of the clock
        source: LockSource::Nixhub,
    })
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
}

impl Mirror {
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
        under.into_iter().max_by_key(|r| numeric(&r.version)).cloned()
    }

    async fn index(&self) -> Result<MirrorIndex, String> {
        let bytes = self
            .os
            .get(&OsPath::from(MIRROR_KEY))
            .await
            .map_err(|e| e.to_string())?
            .bytes()
            .await
            .map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Index for Mirror {
    async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String> {
        // A missing or unparsable index is UNAVAILABLE, never "unknown": treating an absent file
        // as "no such package" would turn a missed refresh beat into a 422 blaming the person.
        let index = self.index().await?;
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
        let mut rows = self.index().await?.remove(attr).unwrap_or_default();
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
    pub async fn lock_one(&self, entry: &str) -> Result<Lock, Refusal> {
        // Callers pass entries that already parsed and already carry a version (`pinned`), so a
        // failure here is a bug upstream, not something to guess a lock for.
        let Some(req) = parse_entry(entry).ok().and_then(|e| e.version.map(|v| (e.attr, v))) else {
            return Err(Refusal::Unknown { entry: entry.to_string(), nearest: vec![] });
        };
        let (attr, req) = req;

        if let Some(lock) = self.cached(&attr, &req).await {
            return Ok(lock);
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
        Err(Refusal::Unknown { entry: entry.to_string(), nearest: self.nearest(&attr, &req).await })
    }

    /// Locks for `packages`. Entries whose string is unchanged keep their existing lock — editing
    /// one entry must never move another one's version. `refresh_all` is the update route: every
    /// `@` entry is re-resolved, exact pins included, since a newer nixpkgs revision can build the
    /// same version.
    pub async fn lock_all(
        &self,
        packages: &[String],
        prev: &[Lock],
        refresh_all: bool,
    ) -> Result<Vec<Lock>, Refusal> {
        let mut out = Vec::new();
        for p in packages.iter().filter(|p| p.contains('@')) {
            match prev.iter().find(|l| l.entry == *p) {
                Some(l) if !refresh_all => out.push(l.clone()),
                _ => out.push(self.lock_one(p).await?),
            }
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
    async fn store(&self, attr: &str, req: &VersionReq, lock: &Lock) {
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
        let want = req_str(req);
        // Shared prefix first (`20.99` is nearer 20.x than 18.x whatever the arithmetic says),
        // then numeric distance component by component.
        all.sort_by_key(|v| {
            (std::cmp::Reverse(shared_prefix(want, v)), distance(&numeric(want), &numeric(v)))
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
        .map(|i| a.get(i).copied().unwrap_or(0).abs_diff(b.get(i).copied().unwrap_or(0)))
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
            store_path: if source == LockSource::Nixhub { "/nix/store/x-nodejs".into() } else { String::new() },
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
            FakeIndex { down: true, ..Default::default() }
        }
        fn count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Index for FakeIndex {
        async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String> {
            assert!(!self.panic_on_call, "this index must not be asked");
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.down {
                return Err("down".into());
            }
            Ok(self.answers.get(&(attr.to_string(), req_str(version).to_string())).cloned().flatten())
        }
        async fn versions(&self, attr: &str) -> Result<Vec<String>, String> {
            if self.down {
                return Err("down".into());
            }
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
        Resolver { cache: Arc::new(InMemory::new()), nixhub, mirror, now }
    }

    async fn put_cache(r: &Resolver, attr: &str, req: &VersionReq, l: &Lock) {
        r.cache
            .put(&OsPath::from(cache_key(attr, req)), PutPayload::from(serde_json::to_vec(l).unwrap()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_hit_in_the_cache_asks_nobody() {
        let never = Arc::new(FakeIndex { panic_on_call: true, ..Default::default() });
        let r = resolver(never.clone(), never);
        let mut cached = lock("nodejs@20", "20.20.2", LockSource::Nixhub);
        cached.resolved_at = at("2026-09-08T09:00:00Z").to_rfc3339();
        put_cache(&r, "nodejs", &VersionReq::Prefix("20".into()), &cached).await;

        assert_eq!(r.lock_one("nodejs@20").await.unwrap(), cached);
    }

    #[tokio::test]
    async fn nixhub_answers_first_and_the_answer_is_cached() {
        let nix = Arc::new(FakeIndex::with(&[(
            "nodejs",
            "20",
            Some(lock("nodejs@20", "20.20.2", LockSource::Nixhub)),
        )]));
        let r = resolver(nix.clone(), Arc::new(FakeIndex::default()));

        let first = r.lock_one("nodejs@20").await.unwrap();
        assert_eq!(first.version, "20.20.2");
        assert_eq!(first.resolved_at, now().to_rfc3339());
        assert_eq!(r.lock_one("nodejs@20").await.unwrap(), first);
        assert_eq!(nix.count(), 1, "the second answer came from the cache");
    }

    #[tokio::test]
    async fn the_mirror_answers_when_nixhub_is_unavailable() {
        let mirror = Arc::new(FakeIndex::with(&[(
            "python3",
            "3.11",
            Some(lock("python3@3.11", "3.11.9", LockSource::Mirror)),
        )]));
        let r = resolver(Arc::new(FakeIndex::down()), mirror);

        let l = r.lock_one("python3@3.11").await.unwrap();
        assert_eq!(l.source, LockSource::Mirror);
        assert_eq!(l.store_path, "");
    }

    #[tokio::test]
    async fn unknown_everywhere_is_a_refusal_naming_the_nearest_versions() {
        let mut nix = FakeIndex::with(&[("nodejs", "20.99", None)]);
        nix.versions.insert(
            "nodejs".into(),
            ["20.20.2", "20.19.5", "20.18.3", "18.1.0"].iter().map(|s| s.to_string()).collect(),
        );
        let r = resolver(Arc::new(nix), Arc::new(FakeIndex::default()));

        assert_eq!(
            r.lock_one("nodejs@20.99").await.unwrap_err(),
            Refusal::Unknown {
                entry: "nodejs@20.99".into(),
                nearest: vec!["20.20.2".into(), "20.19.5".into(), "20.18.3".into()],
            }
        );
    }

    #[tokio::test]
    async fn unavailable_everywhere_with_no_cache_is_unavailable() {
        let r = resolver(Arc::new(FakeIndex::down()), Arc::new(FakeIndex::down()));
        assert_eq!(r.lock_one("nodejs@20").await.unwrap_err(), Refusal::Unavailable);
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

        assert_eq!(r.lock_one("nodejs@20").await.unwrap().version, "20.20.2");
        assert_eq!(nix.count(), 1);
    }

    #[tokio::test]
    async fn lock_all_keeps_untouched_locks_and_resolves_only_new_entries() {
        let nix = Arc::new(FakeIndex::with(&[
            ("nodejs", "20", Some(lock("nodejs@20", "20.20.2", LockSource::Nixhub))),
            ("python3", "3.11", Some(lock("python3@3.11", "3.11.9", LockSource::Nixhub))),
        ]));
        let r = resolver(nix.clone(), Arc::new(FakeIndex::default()));

        let mut prev = lock("nodejs@20", "20.5.0", LockSource::Nixhub);
        prev.resolved_at = at("2026-01-01T00:00:00Z").to_rfc3339();
        let packages: Vec<String> =
            ["nodejs@20", "jq", "python3@3.11"].iter().map(|s| s.to_string()).collect();

        let locks = r.lock_all(&packages, std::slice::from_ref(&prev), false).await.unwrap();
        assert_eq!(locks.len(), 2, "the bare entry gets no lock");
        assert_eq!(locks[0], prev, "an untouched entry keeps its lock, however old");
        assert_eq!(locks[1].version, "3.11.9");
        assert_eq!(nix.count(), 1);

        let refreshed = r.lock_all(&packages, std::slice::from_ref(&prev), true).await.unwrap();
        assert_eq!(refreshed[0].version, "20.20.2");
        assert_eq!(nix.count(), 2, "python3 came from the cache, nodejs was re-resolved");
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
        assert_eq!((got.version.as_str(), got.rev.as_str(), got.attr_path.as_str()), ("20.20.2", "bbb", "nodejs_20"));
        assert_eq!(Mirror::pick(&index, "nodejs", &VersionReq::Latest).unwrap().version, "200.0.1");
        assert_eq!(Mirror::pick(&index, "ruby", &VersionReq::Latest), None);
    }

    #[test]
    fn a_nixhub_body_without_our_system_is_unknown() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"name":"nodejs","version":"20.20.2","systems":{"aarch64-darwin":{}}}"#,
        )
        .unwrap();
        assert_eq!(nixhub_lock("nodejs", &VersionReq::Prefix("20".into()), &body), None);

        let body: serde_json::Value = serde_json::from_str(
            r#"{"version":"20.20.2","systems":{"x86_64-linux":{
                 "flake_installable":{"ref":{"rev":"389ed85"},"attr_path":"nodejs_20"},
                 "outputs":[{"name":"lib","path":"/nix/store/l"},{"name":"out","path":"/nix/store/o","default":true}]}}}"#,
        )
        .unwrap();
        let l = nixhub_lock("nodejs", &VersionReq::Prefix("20".into()), &body).unwrap();
        assert_eq!((l.entry.as_str(), l.rev.as_str(), l.store_path.as_str()), ("nodejs@20", "389ed85", "/nix/store/o"));
    }
}
