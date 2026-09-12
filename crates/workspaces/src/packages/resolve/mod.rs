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
//! A store path an index names is not a store path anyone BUILT: Hydra skips a release marked
//! insecure (nodejs 20.20.2, past its EOL, on 2026-09-08), and Nixhub evaluates commits Hydra never
//! saw. So every lock with a store path is checked against the binary cache (`BinaryCache`) here,
//! at write time, where the person can act on it: a prefix request walks down to the newest release
//! that IS cached, an exact pin with no cached build is a refusal naming the versions that have one.
//! The agent's `NotCached` stays as the backstop for a path that leaves the cache later.
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

mod cache;
pub use cache::*;
mod nixhub;
pub use nixhub::*;
mod mirror;
pub use mirror::*;
#[cfg(test)]
mod tests;


/// How long a resolution stays good. A day: long enough that a busy region asks Nixhub once per
/// package, short enough that `@latest` follows a release within a day.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);


/// The only system we resolve for today (see the spec's "out of scope").
pub(super) const SYSTEM: &str = "x86_64-linux";
/// Where the mirrored nixpkgs-multiverse index lives. Written by the admin tier's daily beat.
pub const MIRROR_KEY: &str = "index/pkgs/versions.json";
pub(super) const HTTP_TIMEOUT: Duration = Duration::from_secs(10);


/// How a version request appears in a URL and a cache key: exactly what the person typed.
pub(super) fn req_str(v: &VersionReq) -> &str {
    match v {
        VersionReq::Latest => "latest",
        VersionReq::Prefix(p) => p,
    }
}


/// How many releases under a prefix to try before giving up — bounds the index calls one write
/// can cost when a whole line is uncached.
pub(super) const WALK_LIMIT: usize = 6;


#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The entry is not something we can resolve at all — bad grammar, or no `@version` on an
    /// entry only pinned entries may reach. The caller's bug or the person's typo: a 400.
    Malformed(String),
    /// Nobody published this. `nearest` is the three closest versions that DO exist.
    Unknown { entry: String, nearest: Vec<String> },
    /// Every index failed and no cache entry covered it. Nothing is written.
    Unavailable,
    /// The version exists but nobody built a binary for it; `cached` names releases that have one.
    NotCached { entry: String, cached: Vec<String> },
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
            Refusal::NotCached { entry, cached } if cached.is_empty() => {
                write!(f, "{entry} has no cached build in cache.nixos.org; pick a version that does")
            }
            Refusal::NotCached { entry, cached } => write!(
                f,
                "{entry} has no cached build in cache.nixos.org; nearest cached: {}",
                cached.join(", ")
            ),
        }
    }
}

// ---------------------------------------------------------------------------- Nixhub


pub(super) fn entry_string(attr: &str, version: &VersionReq) -> String {
    format!("{attr}@{}", req_str(version))
}

// ---------------------------------------------------------------------------- Mirror


pub type MirrorIndex = HashMap<String, Vec<MirrorRow>>;


/// A dotted version as numbers, for "which of these is newest". Non-numeric components sort as 0
/// rather than failing: the mirror is a third party's file and one odd row must not break a pick.
pub(super) fn numeric(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

// ---------------------------------------------------------------------------- Resolver


pub struct Resolver {
    pub cache: Arc<dyn ObjectStore>,
    pub nixhub: Arc<dyn Index>,
    pub mirror: Arc<dyn Index>,
    pub binaries: Arc<dyn BinaryCache>,
    /// Injected so tests own the clock; production passes `Utc::now`.
    pub now: fn() -> DateTime<Utc>,
}


impl Resolver {
    /// The production wiring: the tier's own object store as both cache and mirror, Nixhub at
    /// `KLOUDLITE_NIXHUB_URL` (default `https://search.devbox.sh`). Here rather than in
    /// `bins/api` so the http client and the clock stay this crate's dependencies.
    pub fn from_env(os: Arc<dyn ObjectStore>) -> Self {
        // ONE client for both indexes: a `reqwest::Client` is a connection pool, and building a
        // second one meant Nixhub and the binary cache each warmed their own TLS sessions and
        // never shared an idle connection (2026-09-12). Cloning shares the pool.
        let client = reqwest::Client::new();
        Resolver {
            cache: os.clone(),
            nixhub: Arc::new(Nixhub {
                client: client.clone(),
                base: std::env::var("KLOUDLITE_NIXHUB_URL")
                    .unwrap_or_else(|_| "https://search.devbox.sh".to_string()),
            }),
            mirror: Arc::new(Mirror::new(os)),
            binaries: Arc::new(NixosCache {
                client,
                base: std::env::var("KLOUDLITE_BINARY_CACHE_URL")
                    .unwrap_or_else(|_| "https://cache.nixos.org".to_string()),
            }),
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
                // A cached lock is still only a claim about a binary: one written before a
                // release dropped out of the cache (or before this check existed) must not be
                // handed out for a day. An unverifiable cache is an ordinary miss.
                match self.binaries.has(&lock.store_path).await {
                    Ok(true) => return Ok(lock),
                    _ if lock.store_path.is_empty() => return Ok(lock),
                    _ => {}
                }
            }
        }

        let mut unavailable = false;
        for index in [&self.nixhub, &self.mirror] {
            match index.resolve(&attr, &req).await {
                Ok(Some(lock)) => {
                    let mut lock = self.cached_build(index, entry, &attr, &req, lock).await?;
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

    /// The lock an index answered, or an older release under the same prefix that the binary
    /// cache actually holds. A mirror lock has no store path to check and passes through: the
    /// agent evaluates and copies it, and reports `NotCached` itself.
    async fn cached_build(
        &self,
        index: &Arc<dyn Index>,
        entry: &str,
        attr: &str,
        req: &VersionReq,
        lock: Lock,
    ) -> Result<Lock, Refusal> {
        if lock.store_path.is_empty() {
            return Ok(lock);
        }
        let has = |p: String| async move { self.binaries.has(&p).await.map_err(|_| Refusal::Unavailable) };
        if has(lock.store_path.clone()).await? {
            return Ok(lock);
        }
        // Older releases under the prefix, newest first, the one just refused excluded. An exact
        // pin walks too — not to substitute (the person named a version) but to NAME what they
        // could pin instead.
        let exact = matches!(req, VersionReq::Prefix(p) if p.matches('.').count() >= 2);
        // An exact pin names its major line as the neighbourhood: "20.19.5 is cached" is the
        // useful answer to an uncached 20.20.2, and nothing under "20.20.2." ever could be.
        let prefix = match req {
            VersionReq::Latest => String::new(),
            VersionReq::Prefix(p) if exact => format!("{}.", p.split('.').next().unwrap_or(p)),
            VersionReq::Prefix(p) => format!("{p}."),
        };
        let candidates: Vec<String> = index
            .versions(attr)
            .await
            .map_err(|_| Refusal::Unavailable)?
            .into_iter()
            .filter(|v| v != &lock.version && (prefix.is_empty() || v.starts_with(&prefix)))
            .take(WALK_LIMIT)
            .collect();
        let mut cached = Vec::new();
        for v in candidates {
            let Ok(Some(l)) = index.resolve(attr, &VersionReq::Prefix(v.clone())).await else {
                continue;
            };
            if l.store_path.is_empty() || !has(l.store_path.clone()).await? {
                continue;
            }
            if !exact {
                return Ok(Lock { entry: entry.to_string(), ..l });
            }
            cached.push(v);
            if cached.len() == 3 {
                break;
            }
        }
        Err(Refusal::NotCached { entry: entry.to_string(), cached })
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


pub(super) fn shared_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}


pub(super) fn distance(a: &[u64], b: &[u64]) -> Vec<u64> {
    (0..a.len().max(b.len()))
        .map(|i| {
            a.get(i)
                .copied()
                .unwrap_or(0)
                .abs_diff(b.get(i).copied().unwrap_or(0))
        })
        .collect()
}
