//! The resolver's tests: both indexes and the cache are stubbed, so every refusal shape is
//! exercised without a network.

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

/// Holds every path except the ones named — the cache is the common case.
struct FakeCache(Vec<String>);
#[async_trait]
impl BinaryCache for FakeCache {
    async fn has(&self, store_path: &str) -> Result<bool, String> {
        Ok(!self.0.iter().any(|p| p == store_path))
    }
}

fn resolver(nixhub: Arc<dyn Index>, mirror: Arc<dyn Index>) -> Resolver {
    resolver_missing(nixhub, mirror, &[])
}

fn resolver_missing(nixhub: Arc<dyn Index>, mirror: Arc<dyn Index>, missing: &[&str]) -> Resolver {
    Resolver {
        cache: Arc::new(InMemory::new()),
        nixhub,
        mirror,
        binaries: Arc::new(FakeCache(missing.iter().map(|s| s.to_string()).collect())),
        now,
    }
}

fn lock_at(entry: &str, version: &str, path: &str) -> Lock {
    Lock {
        store_path: path.into(),
        ..lock(entry, version, LockSource::Nixhub)
    }
}

#[tokio::test]
async fn a_cached_lock_whose_binary_is_gone_is_re_resolved() {
    let nix = Arc::new(
        FakeIndex::with(&[
            ("nodejs", "20", Some(lock_at("nodejs@20", "20.20.2", "/nix/store/a-nodejs"))),
            ("nodejs", "20.19.5", Some(lock_at("nodejs@20.19.5", "20.19.5", "/nix/store/c-nodejs"))),
        ])
        .knows("nodejs", &["20.20.2", "20.19.5"]),
    );
    let r = resolver_missing(nix, Arc::new(FakeIndex::default()), &["/nix/store/a-nodejs"]);
    let mut stale = lock_at("nodejs@20", "20.20.2", "/nix/store/a-nodejs");
    stale.resolved_at = at("2026-09-08T11:00:00Z").to_rfc3339();
    put_cache(&r, "nodejs", &VersionReq::Prefix("20".into()), &stale).await;
    let l = r.lock_one("nodejs@20", false).await.unwrap();
    assert_eq!((l.version.as_str(), l.store_path.as_str()), ("20.19.5", "/nix/store/c-nodejs"));
}

#[tokio::test]
async fn a_prefix_walks_down_to_the_newest_release_the_cache_holds() {
    // nodejs 20.20.2 on 2026-09-08: marked insecure, never built by Hydra, 20.19.5 was.
    let nix = Arc::new(
        FakeIndex::with(&[
            ("nodejs", "20", Some(lock_at("nodejs@20", "20.20.2", "/nix/store/a-nodejs"))),
            ("nodejs", "20.20.1", Some(lock_at("nodejs@20.20.1", "20.20.1", "/nix/store/b-nodejs"))),
            ("nodejs", "20.19.5", Some(lock_at("nodejs@20.19.5", "20.19.5", "/nix/store/c-nodejs"))),
        ])
        .knows("nodejs", &["22.1.0", "20.20.2", "20.20.1", "20.19.5", "18.4.0"]),
    );
    let r = resolver_missing(nix, Arc::new(FakeIndex::default()), &["/nix/store/a-nodejs", "/nix/store/b-nodejs"]);
    let l = r.lock_one("nodejs@20", false).await.unwrap();
    assert_eq!((l.entry.as_str(), l.version.as_str(), l.store_path.as_str()), ("nodejs@20", "20.19.5", "/nix/store/c-nodejs"));
}

#[tokio::test]
async fn an_exact_pin_with_no_cached_build_names_the_ones_that_have() {
    let nix = Arc::new(
        FakeIndex::with(&[
            ("nodejs", "20.20.2", Some(lock_at("nodejs@20.20.2", "20.20.2", "/nix/store/a-nodejs"))),
            ("nodejs", "20.19.5", Some(lock_at("nodejs@20.19.5", "20.19.5", "/nix/store/c-nodejs"))),
        ])
        .knows("nodejs", &["20.20.2", "20.19.5"]),
    );
    let r = resolver_missing(nix, Arc::new(FakeIndex::default()), &["/nix/store/a-nodejs"]);
    let e = r.lock_one("nodejs@20.20.2", false).await.unwrap_err();
    assert_eq!(e, Refusal::NotCached { entry: "nodejs@20.20.2".into(), cached: vec!["20.19.5".into()] });
    assert_eq!(e.to_string(), "nodejs@20.20.2 has no cached build in cache.nixos.org; nearest cached: 20.19.5");
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
