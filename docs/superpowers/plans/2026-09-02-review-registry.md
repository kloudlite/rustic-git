# Registry and Pull-Request Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every REGISTRY / PULLS / WORKER finding of the 2026-09-02 review (M1–M5, L1–L4, L6, L7) with the smallest correct diff each, and pin each one with a test that fails before the change. L5 is deferred (see Self-review).

**Architecture:** Nothing moves between crates and no new module appears. Four kinds of change: (a) one-line default and pagination corrections in `crates/registry` (M1, L2, L3, L6); (b) bounded concurrency where `crates/registry/src/gc.rs` already states the rule, reusing `gc::stats_of` (M2, M4); (c) row-cleanup symmetry in the image DB — one shared suffix-delete helper and one prefix sweep on the blob-delete path (L1, L4); (d) two validation gaps on the PR path — branch names at open, and a tip-stamped mergeability verdict so a lapsed lane cannot overwrite a newer answer (M3, M5). L7 is a pure assertion.

**Tech Stack:** Rust 2021 workspace; axum + `slatedb::object_store` 0.14.1 for the registry; `futures::StreamExt::buffered` for bounded fan-out; `reqwest` + `tower::ServiceExt::oneshot` in the integration suite under `tests/` (root package `kloudlite-git-tests`); `cargo test`, `cargo clippy --workspace --all-targets --locked -- -D warnings`.

**Spec:** docs/superpowers/reviews/2026-09-02-codebase-review.md (details: docs/superpowers/reviews/2026-09-02-details/registry-pulls-worker.md)

## Global Constraints
- No new blob deletion site: after this plan `blobs/` is still deleted in exactly two places — `crates/registry/src/blobs.rs:218` (client DELETE) and `crates/registry/src/gc.rs:332` (sweep). Task 6 deletes DB rows only.
- Manifest bytes are never re-emitted: no task parses-and-reserializes a manifest; Task 2 and Task 5 read manifest JSON for digest names only.
- `Digest::parse` stays the only path segment → object key: no task constructs a `blob_path`/`manifest_path` argument from anything but an already-parsed `Digest`.
- New `max_layer` default: `5 * 1024 * 1024 * 1024` = `5368709120` bytes (was `10 * 1024 * 1024 * 1024` = `10737418240`), matching the S3 CopyObject cap `uploads::complete`'s fast path relies on (`crates/registry/src/uploads.rs:691-697`).
- Bounded fan-out constant is 16 everywhere, the value `gc::STAT_CONCURRENCY` (`crates/registry/src/gc.rs:216`) and `gc::referenced` (`gc.rs:53`) already use. Do not introduce a second number.
- Branch-name rules rejected at PR open, exact list: empty; longer than 255 bytes; starts with `-`; contains `..`; ends with `.lock`; contains any ASCII control character (`c.is_ascii_control()`); contains a space; contains any of `~ ^ : ? * [ \`.
- Verdict tip stamps are advisory-lenient: an empty `base_oid` on a reported verdict means "unstamped" and is accepted, so a worker older than this change (or one that could not resolve either branch) keeps working through a roll.
- Every `/v2` refusal stays the OCI envelope via `registry::oci_err`; no task adds a bare `StatusCode` return on a `/v2` route.
- Comments explain WHY only (CLAUDE.md "House style"); keep any `// ponytail:` marker you edit near.

---

### Task 1: Default `max_layer` to the copy cap (M1)

**Files:**
- Modify: `crates/registry/src/blobs.rs:16-27` (the doc comment and the `unwrap_or`)
- Modify: `tests/registry_blobs.rs:32` (comment says "10 GiB by default")
- Test: `crates/registry/src/blobs.rs` new `#[cfg(test)] mod tests` at end of file

**Interfaces:**
- Produces: `pub const DEFAULT_MAX_LAYER: u64` in `crates/registry/src/blobs.rs`
- Consumes: nothing new. `max_layer()` keeps its signature `pub fn max_layer() -> u64`.

- [ ] **Step 1: Write the failing test** — append to `crates/registry/src/blobs.rs`:

```rust
#[cfg(test)]
mod tests {
    /// The default is not a taste: on the multipart fast path a verified blob reaches `blobs/`
    /// by a server-side CopyObject, which S3 caps at 5 GiB (`uploads::complete`'s ponytail note).
    /// A default above that accepts a layer, uploads it, hashes it, and only then 500s.
    #[test]
    fn the_default_layer_cap_is_the_copy_cap() {
        assert_eq!(super::DEFAULT_MAX_LAYER, 5 * 1024 * 1024 * 1024);
        assert_eq!(super::DEFAULT_MAX_LAYER, 5_368_709_120);
    }
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-registry the_default_layer_cap_is_the_copy_cap`. Expected: `error[E0425]: cannot find value 'DEFAULT_MAX_LAYER' in module 'super'`.

- [ ] **Step 3: Implement** — replace `crates/registry/src/blobs.rs:16-27` with:

```rust
/// Largest single layer accepted by default. NOT a taste: the multipart fast path verifies the
/// assembled blob on its staging key and then `copy`s it into `blobs/`, and that server-side
/// CopyObject is capped at 5 GiB (see `uploads::complete`) — a bigger default accepts a push,
/// pays the whole O(N) hash, and dies at the last step.
pub const DEFAULT_MAX_LAYER: u64 = 5 * 1024 * 1024 * 1024;

/// Largest single layer accepted, checked against the body's size BEFORE it is stored: an
/// unbounded push must not be able to fill a node's disk. Override with KLOUDLITE_GIT_MAX_LAYER.
///
/// Read once and cached: this is on the hot blob path and the env var never changes after
/// process start.
pub fn max_layer() -> u64 {
    static LAYER: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LAYER.get_or_init(|| {
        std::env::var("KLOUDLITE_GIT_MAX_LAYER").ok().and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_LAYER)
    })
}
```

Then fix the stale comment at `tests/registry_blobs.rs:32`:

```rust
    // was an anonymous memory-DoS for public images (max_layer is 5 GiB by default).
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-registry && cargo test --test registry_blobs && cargo test --test registry_limits && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/registry/src/blobs.rs tests/registry_blobs.rs && git commit -m "Default the layer cap to the 5 GiB copy limit"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 2: Lock and bound the blob-row backfill, and never 500 a pull on it (M2)

**Files:**
- Modify: `crates/registry/src/store.rs:203-242` (`image_holds_blob`), adding a private `backfill_blob_rows`
- Test: `tests/registry_store.rs` (new test plus a small failing-`get` object store wrapper)

**Interfaces:**
- Consumes: `Store::keyed_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>>` (`crates/storage/src/store.rs:153`); `crate::gc::digest_from_path`; `crate::gc::collect`
- Produces: `async fn backfill_blob_rows(store: &Store, owner: &str, name: &str, db: &Db) -> Result<()>` (private to `store.rs`). `image_holds_blob`'s signature is unchanged.

- [ ] **Step 1: Write the failing test** — append to `tests/registry_store.rs`:

```rust
/// An object store that answers one nominated key with a transient error, so the backfill's
/// failure direction can be exercised at all. Everything else delegates.
#[derive(Debug)]
struct FailsOneGet {
    inner: std::sync::Arc<slatedb::object_store::memory::InMemory>,
    bad: slatedb::object_store::path::Path,
}

impl std::fmt::Display for FailsOneGet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FailsOneGet")
    }
}

#[async_trait::async_trait]
impl slatedb::object_store::ObjectStore for FailsOneGet {
    async fn put_opts(
        &self,
        l: &slatedb::object_store::path::Path,
        p: slatedb::object_store::PutPayload,
        o: slatedb::object_store::PutOptions,
    ) -> slatedb::object_store::Result<slatedb::object_store::PutResult> {
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(
        &self,
        l: &slatedb::object_store::path::Path,
        o: slatedb::object_store::PutMultipartOptions,
    ) -> slatedb::object_store::Result<Box<dyn slatedb::object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(
        &self,
        l: &slatedb::object_store::path::Path,
        o: slatedb::object_store::GetOptions,
    ) -> slatedb::object_store::Result<slatedb::object_store::GetResult> {
        if *l == self.bad {
            return Err(slatedb::object_store::Error::Generic {
                store: "FailsOneGet",
                source: "injected transient failure".into(),
            });
        }
        self.inner.get_opts(l, o).await
    }
    async fn delete(&self, l: &slatedb::object_store::path::Path) -> slatedb::object_store::Result<()> {
        self.inner.delete(l).await
    }
    fn list(
        &self,
        p: Option<&slatedb::object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::ObjectMeta>> {
        self.inner.list(p)
    }
    async fn list_with_delimiter(
        &self,
        p: Option<&slatedb::object_store::path::Path>,
    ) -> slatedb::object_store::Result<slatedb::object_store::ListResult> {
        self.inner.list_with_delimiter(p).await
    }
    async fn copy_opts(
        &self,
        from: &slatedb::object_store::path::Path,
        to: &slatedb::object_store::path::Path,
        o: slatedb::object_store::CopyOptions,
    ) -> slatedb::object_store::Result<()> {
        self.inner.copy_opts(from, to, o).await
    }
}

/// A pre-rows image whose manifest cannot be read must answer "this image does not hold that
/// blob" — a 404 for the puller — not propagate the store's error into a 500. Under-granting is
/// the safe failure for authorization; a fault is not an answer.
#[tokio::test]
async fn an_unreadable_manifest_reads_as_not_held_not_as_a_fault() {
    use slatedb::object_store::{ObjectStoreExt, PutPayload};

    let layer = Digest::of(b"layer");
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "layers": [{"digest": layer.to_string(), "size": 5}]
    })
    .to_string()
    .into_bytes();
    let md = Digest::of(&manifest);
    let loc = kloudlite_git_registry::store::manifest_path("acme", "nginx", &md);

    let inner = std::sync::Arc::new(slatedb::object_store::memory::InMemory::new());
    inner.put(&loc, PutPayload::from(manifest)).await.unwrap();
    let os = std::sync::Arc::new(FailsOneGet { inner, bad: loc.clone() });
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        kloudlite_git_storage::store::Store::open(os, tmp.path().join("cache"), false).await.unwrap(),
    );

    let held = kloudlite_git_registry::store::image_holds_blob(&store, "acme", "nginx", &layer).await;
    assert_eq!(held.unwrap(), false, "an unreadable manifest names nothing; it is not a 500");
}

/// The walk happens once. A second call must answer from rows alone — proven by taking the
/// manifest object away and asking again for a blob it named.
#[tokio::test]
async fn the_backfill_marks_itself_done_and_does_not_walk_twice() {
    use slatedb::object_store::{ObjectStoreExt, PutPayload};
    let e = common::env().await;
    let layer = Digest::of(b"layer");
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "layers": [{"digest": layer.to_string(), "size": 5}]
    })
    .to_string()
    .into_bytes();
    let md = Digest::of(&manifest);
    let loc = kloudlite_git_registry::store::manifest_path("acme", "nginx", &md);
    e.store.os.put(&loc, PutPayload::from(manifest)).await.unwrap();

    assert!(kloudlite_git_registry::store::image_holds_blob(&e.store, "acme", "nginx", &layer)
        .await
        .unwrap());
    e.store.os.delete(&loc).await.unwrap();
    assert!(
        kloudlite_git_registry::store::image_holds_blob(&e.store, "acme", "nginx", &layer)
            .await
            .unwrap(),
        "the row survives the manifest; the walk must not run a second time"
    );
}
```

If `tests/registry_store.rs` does not already import them, add at the top of the file beside the existing `use` lines: `use kloudlite_git_registry::Digest;` and `use kloudlite_git_registry::store::ImageExt;` (drop either if it is already there — a duplicate import is a clippy failure). `async_trait` is already a workspace dependency of the root test package; if `cargo test` reports it missing, add `async-trait = "0.1"` to the root `Cargo.toml`'s `[dev-dependencies]`.

- [ ] **Step 2: Run it, expect failure** — `cargo test --test registry_store an_unreadable_manifest_reads_as_not_held_not_as_a_fault`. Expected: the assertion never runs; the test panics on `held.unwrap()` with `called 'Result::unwrap()' on an 'Err' value: ... injected transient failure`.

- [ ] **Step 3: Implement** — replace the body of `image_holds_blob` (`crates/registry/src/store.rs:210-242`, from `pub async fn image_holds_blob` to its closing brace) with:

```rust
pub async fn image_holds_blob(store: &Store, owner: &str, name: &str, d: &Digest) -> Result<bool> {
    let db = store.image_db(owner, name).await?;
    if has_blob_row(&db, d).await? {
        return Ok(true);
    }
    if db.get(BLOB_ROWS_BACKFILLED).await?.is_some() {
        return Ok(false);
    }
    // One walk per image, not one per concurrent stranger: the first pull of a pre-rows image
    // used to LIST and GET every manifest inside the blob request, and N simultaneous first
    // pulls each did the whole walk before any of them wrote the mark.
    let lock = store.keyed_lock(&format!("blobrows/{owner}/{name}"));
    let _guard = lock.lock().await;
    // Re-read under the lock: whoever held it before us may have just finished the walk.
    if db.get(BLOB_ROWS_BACKFILLED).await?.is_some() {
        return has_blob_row(&db, d).await;
    }
    backfill_blob_rows(store, owner, name, &db).await?;
    db.put(BLOB_ROWS_BACKFILLED, b"1".as_slice()).await?;
    has_blob_row(&db, d).await
}

/// The walk itself: every manifest of the image, the blob rows it implies.
///
/// Bounded exactly as `gc::referenced` is, and for the same reason — an image with hundreds of
/// manifests was hundreds of serial round trips. A manifest this cannot READ or PARSE names
/// nothing and is skipped: under-granting is the safe failure for authorization, unlike the
/// sweep, where the same manifest must abort. Propagating the store's error here answered a
/// pull with a 500 where the honest answer is a 404.
async fn backfill_blob_rows(store: &Store, owner: &str, name: &str, db: &Db) -> Result<()> {
    use slatedb::object_store::ObjectStore;
    let prefix = OsPath::from(format!("manifests/{owner}/{name}"));
    let mut listing = store.os.list(Some(&prefix));
    let mut paths = vec![];
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        paths.push(m?.location);
    }
    let mut fetched = futures::StreamExt::buffered(
        futures::StreamExt::map(futures::stream::iter(paths), |p| async move {
            let bytes = match store.os.get(&p).await {
                Ok(r) => r.bytes().await,
                Err(e) => Err(e),
            };
            (p, bytes)
        }),
        16,
    );
    while let Some((loc, bytes)) = futures::StreamExt::next(&mut fetched).await {
        let Some(via) = crate::gc::digest_from_path(&loc) else { continue };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(owner = %owner, name = %name, manifest = %loc, error = %e, "blob rows: skipping unreadable manifest");
                continue;
            }
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            tracing::warn!(owner = %owner, name = %name, manifest = %loc, "blob rows: skipping unparseable manifest");
            continue;
        };
        let mut named = std::collections::HashSet::new();
        crate::gc::collect(&v, &mut named);
        let digests: Vec<Digest> = named.iter().filter_map(|s| Digest::parse(s)).collect();
        let mut b = WriteBatch::new();
        note_blobs(&mut b, &digests, &via);
        if !b.is_empty() {
            db.write(b).await?;
        }
    }
    Ok(())
}
```

Keep the existing doc comment block above `image_holds_blob` (`store.rs:203-209`) exactly as it is — its `ponytail:` marker still names the ceiling and the upgrade path.

- [ ] **Step 4: Run tests and clippy** — `cargo test --test registry_store && cargo test --test registry_blobs && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/registry/src/store.rs tests/registry_store.rs Cargo.toml && git commit -m "Lock and bound the blob-row backfill"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 3: Bound the two `join_all` fan-outs (M4)

**Files:**
- Modify: `crates/registry/src/gc.rs:215-224` (`stats_of` visibility only)
- Modify: `crates/registry/src/routes.rs:47-52` (catalog stat fan-out)
- Modify: `crates/registry/src/manifests.rs:186-193` (presence probes)
- Test: `tests/registry_manifests.rs` (new test)

**Interfaces:**
- Consumes: `gc::stats_of(store: &Store, owner: &str, names: &[&str]) -> Vec<Result<(usize, Option<i64>)>>` — reused verbatim by `routes.rs`, only its visibility changes.
- Produces: no new public item.

- [ ] **Step 1: Write the failing test** — append to `tests/registry_manifests.rs`:

```rust
/// An index may name thousands of children, and every one of them is probed before the push is
/// accepted. The probe must stay bounded (16, the number `gc` uses and states the reason for) —
/// an unbounded `join_all` here opened one connection per declared digest. Correctness is the
/// observable half: every named blob is checked, so a manifest naming one absent child among
/// many is still refused.
#[tokio::test]
async fn an_index_naming_many_children_is_probed_without_fanning_out_unbounded() {
    let (base, e, c, token, m, d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();

    // 200 real children plus one that was never pushed.
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    for i in 0..200u32 {
        bodies.push(format!("child-{i}").into_bytes());
    }
    common::seed_blobs(&e, "acme", &bodies.iter().map(|b| b.as_slice()).collect::<Vec<_>>()).await;
    let mut children: Vec<serde_json::Value> = bodies
        .iter()
        .map(|b| serde_json::json!({"mediaType": MEDIA, "digest": Digest::of(b).to_string(), "size": b.len()}))
        .collect();

    let ok = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": children.clone()
    }).to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/wide"))
        .basic_auth("acme", Some(&token))
        .header("content-type", "application/vnd.oci.image.index.v1+json")
        .body(ok).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "every child is present");

    children.push(serde_json::json!({
        "mediaType": MEDIA,
        "digest": Digest::of(b"never pushed").to_string(),
        "size": 1
    }));
    let bad = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": children
    }).to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/wide2"))
        .basic_auth("acme", Some(&token))
        .header("content-type", "application/vnd.oci.image.index.v1+json")
        .body(bad).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "one absent child among 200 still refuses");
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_BLOB_UNKNOWN");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test registry_manifests an_index_naming_many_children_is_probed_without_fanning_out_unbounded`. Expected before the change: the wide push exhausts the test client/server's concurrent request budget, failing with a `MANIFEST_BLOB_UNKNOWN` on the first (`ok`) push — `assertion 'left == right' failed: every child is present, left: 404, right: 201`. (If the harness happens to survive 200 concurrent HEADs, the test still stands as the bounded-probe regression pin; proceed to Step 3.)

- [ ] **Step 3: Implement** —

`crates/registry/src/gc.rs:220`, change the signature line only:

```rust
pub(crate) async fn stats_of(store: &Store, owner: &str, names: &[&str]) -> Vec<Result<(usize, Option<i64>)>> {
```

`crates/registry/src/routes.rs:47-49`, replace the `join_all` with the reused helper:

```rust
    // One listing per image, bounded at 16 — the same cap and the same reason as
    // `gc::stats_of`, which this now IS: a serial loop put the catalog page behind N sequential
    // round trips, and an unbounded fan-out put it behind N simultaneous ones.
    let names: Vec<&str> = unmarked.iter().map(String::as_str).collect();
    let stats = crate::gc::stats_of(&app.store, owner, &names).await;
```

`crates/registry/src/manifests.rs:186-193`, replace the `join_all` with a bounded stream (keep the existing comment block above it, including its `ponytail:` note, and extend the first sentence as shown):

```rust
    // Concurrent, not serial: a 40-layer manifest was up to 80 sequential HEADs before the
    // write. Bounded at 16 for the same reason `gc` bounds its walk — an index may name
    // thousands of children, and one push must not open thousands of connections. Each probe is
    // independent; blob path first because that is where layers live — the manifest path is
    // only hit for an index's entries.
    // ponytail: a sweep can still delete an old blob between this head and the put below —
    // GC is keep-biased and this window is unchanged from the serial version, so it's not new risk.
    let present: Vec<bool> = futures::StreamExt::collect::<Vec<bool>>(futures::StreamExt::buffered(
        futures::stream::iter(digests.iter().map(|bd| async {
            app.store.os.head(&blob_path(&owner, bd)).await.is_ok()
                || app.store.os.head(&manifest_path(&owner, &name, bd)).await.is_ok()
        })),
        16,
    ))
    .await;
```

- [ ] **Step 4: Run tests and clippy** — `cargo test --test registry_manifests && cargo test --test registry_http && cargo test -p kloudlite-git-registry && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/registry/src/gc.rs crates/registry/src/routes.rs crates/registry/src/manifests.rs tests/registry_manifests.rs && git commit -m "Bound the catalog and manifest probe fan-outs at sixteen"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 4: One suffix-delete loop instead of two (L4)

**Files:**
- Modify: `crates/registry/src/store.rs:178-195` (`forget_manifest_blobs`)
- Modify: `crates/registry/src/referrers.rs:57-74` (`unindex`)
- Test: `tests/registry_manifests.rs` (new test)

**Interfaces:**
- Produces: `pub async fn delete_suffixed(db: &Db, prefix: &str, suffix: &str) -> Result<()>` in `crates/registry/src/store.rs`
- Consumes: `store::forget_manifest_blobs` and `referrers::unindex` keep their existing signatures and call sites (`crates/registry/src/manifests.rs:422,436`).

- [ ] **Step 1: Write the failing test** — append to `tests/registry_manifests.rs`:

```rust
/// The two row families a manifest delete has to drop — its blob holds and its referrer entry —
/// are the same scan-prefix / suffix-match / delete loop, and they must both actually run.
#[tokio::test]
async fn deleting_a_manifest_drops_both_its_blob_rows_and_its_referrer_rows() {
    let (base, e, c, token, m, d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();

    // A manifest with a `subject`, so a referrer row exists too.
    let sub = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": Digest::of(b"{}").to_string(), "size": 2},
        "layers": [],
        "subject": {"mediaType": MEDIA, "digest": d.to_string(), "size": m.len()}
    }).to_string().into_bytes();
    let sd = Digest::of(&sub);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{sd}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(sub).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let db = e.store.image_db("acme", "nginx").await.unwrap();
    let rows = |db: std::sync::Arc<slatedb::Db>, prefix: &'static str, suffix: String| async move {
        let mut it = db.scan_prefix(prefix, ..).await.unwrap();
        let mut n = 0;
        while let Some(kv) = it.next().await.unwrap() {
            if String::from_utf8_lossy(&kv.key).ends_with(&suffix) {
                n += 1;
            }
        }
        n
    };
    assert!(rows(db.clone(), "image/blob/", format!("/{d}")).await > 0, "blob rows before");
    assert!(rows(db.clone(), "image/referrer/", format!("/{sd}")).await > 0, "referrer row before");

    for target in [&sd, &d] {
        let r = c.delete(format!("{base}/v2/acme/nginx/manifests/{target}"))
            .basic_auth("acme", Some(&token)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
    }
    assert_eq!(rows(db.clone(), "image/blob/", format!("/{d}")).await, 0, "blob rows after");
    assert_eq!(rows(db, "image/referrer/", format!("/{sd}")).await, 0, "referrer row after");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test registry_manifests deleting_a_manifest_drops_both_its_blob_rows_and_its_referrer_rows`. Expected on a tree where either loop is missing: `assertion 'left == right' failed: blob rows after` / `referrer row after`. On the current tree this test passes — it is the pin that makes the refactor safe; if it passes at Step 2, record that and proceed (the refactor, not the test, is this task's product).

- [ ] **Step 3: Implement** —

Replace `crates/registry/src/store.rs:178-195` (the whole `forget_manifest_blobs` item including its doc comment) with:

```rust
/// Delete every row under `prefix` whose key ends `suffix`. A scan rather than a re-parse of the
/// thing being deleted: these are the rare paths, and they must work even when the bytes they
/// would re-parse are already gone.
pub async fn delete_suffixed(db: &Db, prefix: &str, suffix: &str) -> Result<()> {
    let mut it = db.scan_prefix(prefix.to_string(), ..).await?;
    let mut doomed = vec![];
    while let Some(kv) = it.next().await? {
        if String::from_utf8_lossy(&kv.key).ends_with(suffix) {
            doomed.push(kv.key.to_vec());
        }
    }
    for k in doomed {
        db.delete(k).await?;
    }
    Ok(())
}

/// Drop the rows manifest `m` contributed. Rows written `via` another manifest, or by an upload,
/// stay — which is why this matches on the `via` suffix rather than on the digest.
pub async fn forget_manifest_blobs(db: &Db, m: &Digest) -> Result<()> {
    delete_suffixed(db, BLOB_PREFIX, &format!("/{m}")).await
}
```

Replace `crates/registry/src/referrers.rs:57-74` (the whole `unindex` item including its doc comment) with:

```rust
/// Remove `d` from wherever it appears as a referrer. Scans the whole index rather than keeping a
/// reverse map: a manifest delete is rare, and a reverse map is state that can disagree with this
/// one.
pub async fn unindex(app: &App, owner: &str, name: &str, d: &Digest) -> crate::Result<()> {
    let db = app.store.image_db(owner, name).await?;
    crate::store::delete_suffixed(&db, PREFIX, &format!("/{d}")).await
}
```

- [ ] **Step 4: Run tests and clippy** — `cargo test --test registry_manifests && cargo test --test registry_store && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/registry/src/store.rs crates/registry/src/referrers.rs tests/registry_manifests.rs && git commit -m "Share one suffix-delete loop between the two row sweeps"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 5: A client blob DELETE drops the image's hold rows (L1)

**Files:**
- Modify: `crates/registry/src/store.rs` (add `forget_blob_rows` beside `delete_suffixed` from Task 4)
- Modify: `crates/registry/src/blobs.rs:206-226` (`delete_blob`)
- Test: `tests/registry_blobs.rs` (new test)

**Interfaces:**
- Produces: `pub async fn forget_blob_rows(db: &Db, d: &Digest) -> Result<()>` in `crates/registry/src/store.rs`
- Consumes: `Store::image_db`. `delete_blob`'s signature is unchanged.

- [ ] **Step 1: Write the failing test** — append to `tests/registry_blobs.rs`:

```rust
/// The mirror of `forget_manifest_blobs`: once the bytes are gone the image must stop claiming
/// to hold them, or `image_holds_blob` keeps answering true for a digest the store 404s.
#[tokio::test]
async fn deleting_a_blob_drops_the_images_hold_row() {
    let (base, e, c, token) = authed().await;
    let body = b"a layer somebody deletes".to_vec();
    let d = Digest::of(&body);
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert!(
        kloudlite_git_registry::store::image_holds_blob(&e.store, "acme", "nginx", &d).await.unwrap(),
        "the push records the hold"
    );

    let r = c.delete(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert!(
        !kloudlite_git_registry::store::image_holds_blob(&e.store, "acme", "nginx", &d).await.unwrap(),
        "the hold row must go with the bytes"
    );
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test registry_blobs deleting_a_blob_drops_the_images_hold_row`. Expected: `assertion failed: !kloudlite_git_registry::store::image_holds_blob(...)` — "the hold row must go with the bytes".

- [ ] **Step 3: Implement** —

Add to `crates/registry/src/store.rs`, immediately after `forget_manifest_blobs`:

```rust
/// Drop every hold row for `d`, whatever wrote it. The mirror of `forget_manifest_blobs` on the
/// blob-delete path: the rows say "this image holds these bytes", so they must not outlive them.
pub async fn forget_blob_rows(db: &Db, d: &Digest) -> Result<()> {
    let mut it = db.scan_prefix(format!("{BLOB_PREFIX}{d}/"), ..).await?;
    let mut doomed = vec![];
    while let Some(kv) = it.next().await? {
        doomed.push(kv.key.to_vec());
    }
    for k in doomed {
        db.delete(k).await?;
    }
    Ok(())
}
```

Replace the `match` at `crates/registry/src/blobs.rs:218-225` with:

```rust
    match app.store.os.delete(&blob_path(&owner, &d)).await {
        Ok(()) => {
            // The rows say this image HOLDS these bytes, so they must not outlive them — the
            // mirror of `forget_manifest_blobs` on the manifest path. A row cleanup that fails
            // is logged, never a failed delete: the object is already gone, and a stale row only
            // ever grants a pull the store then answers 404 for.
            match app.store.image_db(&owner, &name).await {
                Ok(db) => {
                    if let Err(e) = super::store::forget_blob_rows(&db, &d).await {
                        tracing::warn!(owner = %owner, name = %name, digest = %d, error = %e, "blob delete: hold rows");
                    }
                }
                Err(e) => {
                    tracing::warn!(owner = %owner, name = %name, digest = %d, error = %e, "blob delete: hold rows");
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => crate::oci_internal(e.into()),
    }
```

The doc comment at `blobs.rs:201-205` still holds and must not change: this is still the one client-facing deletion site, and it still deletes no manifest's blob.

- [ ] **Step 4: Run tests and clippy** — `cargo test --test registry_blobs && cargo test --test registry_gc && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/registry/src/store.rs crates/registry/src/blobs.rs tests/registry_blobs.rs && git commit -m "Drop the hold rows when a client deletes a blob"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 6: `?n=0` is no page size, not the end of the catalog (L2)

**Files:**
- Modify: `crates/registry/src/lib.rs:115-129` (`paginate`)
- Modify: `crates/registry/src/routes.rs:29-32` (`image_listing`'s `n`)
- Test: `crates/registry/src/lib.rs` `mod tests` (beside `a_non_numeric_page_size_is_no_page_size`, line 132+)

**Interfaces:**
- Consumes/Produces: `pub fn paginate(all: &[String], q: &HashMap<String, String>) -> (Vec<String>, Option<String>)` — signature unchanged.

- [ ] **Step 1: Write the failing test** — append inside `crates/registry/src/lib.rs`'s `mod tests`:

```rust
    /// `?n=0` used to return an empty page with no `Link`, which a paging client reads as "the
    /// catalog is exhausted" and stops on. Treated as an absent `n`, exactly as a non-numeric one
    /// already is: no page size, not a page of nothing.
    #[test]
    fn a_zero_page_size_is_no_page_size() {
        let all: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let q: std::collections::HashMap<String, String> =
            [("n".to_string(), "0".to_string())].into_iter().collect();
        let (page, truncated) = paginate(&all, &q);
        assert_eq!(page, all, "n=0 lists everything rather than ending the catalog");
        assert_eq!(truncated, None);
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-registry a_zero_page_size_is_no_page_size`. Expected: `assertion 'left == right' failed: n=0 lists everything rather than ending the catalog; left: [], right: ["a", "b", "c"]`.

- [ ] **Step 3: Implement** —

`crates/registry/src/lib.rs:126`, replace the `n` line with:

```rust
    // A zero page size is no page size, not a page of nothing: an empty page with no `Link` is
    // indistinguishable from an exhausted catalog, and a paging client stops on it.
    let n: usize = q.get("n").and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(rest.len());
```

`crates/registry/src/routes.rs:31`, the same filter so the index page is not pre-truncated to zero:

```rust
    let n = q.get("n").and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(usize::MAX);
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-registry && cargo test --test registry_http && cargo test --test registry_manifests && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/registry/src/lib.rs crates/registry/src/routes.rs && git commit -m "Read a zero page size as no page size"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 7: A foreign `last` does not become the catalog's marker (L6)

**Files:**
- Modify: `crates/registry/src/routes.rs:156-161` (`catalog`'s `page_q`)
- Test: `tests/registry_http.rs` (new test)

**Interfaces:**
- Consumes: `image_listing(&app, &who, true, &page_q)`. No signature changes.

- [ ] **Step 1: Write the failing test** — append to `tests/registry_http.rs` (use the file's existing `serve_public`/token helpers; if it names them differently, follow the file's own harness):

```rust
/// `last` on the wire is `{who}/{name}`. A client that pages with a bare name, or with another
/// owner's prefixed one, must not have that string used verbatim as this owner's marker — the
/// answer stays owner-scoped either way, but the page it returns has to be the right one.
#[tokio::test]
async fn a_catalog_marker_without_this_owners_prefix_is_not_used_as_a_name() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    for name in ["alpha", "beta", "gamma"] {
        e.store.refresh_image_marker("acme", name).await.unwrap();
    }
    // A bare `last` names no image of this owner; the page must still start from the top.
    let r = c.get(format!("{base}/v2/_catalog?last=zzz-not-an-owner-prefixed-name"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(
        b["repositories"],
        serde_json::json!(["acme/alpha", "acme/beta", "acme/gamma"]),
        "a marker that is not `{{who}}/{{name}}` must not truncate the catalog"
    );
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test registry_http a_catalog_marker_without_this_owners_prefix_is_not_used_as_a_name`. Expected: `assertion 'left == right' failed: a marker that is not '{who}/{name}' must not truncate the catalog; left: [], right: ["acme/alpha","acme/beta","acme/gamma"]` (the raw `zzz…` marker sorts past every name in the index).

- [ ] **Step 3: Implement** — replace `crates/registry/src/routes.rs:156-161` with:

```rust
    // `last` on the wire is `{who}/{name}`; the index knows the name alone. A marker that does
    // not carry this owner's prefix names nothing in the index, so it must not be handed down as
    // one — the page below re-filters against the full `{who}/{name}` strings and is the honest
    // answer for a foreign or malformed marker. `n` goes with it: an index page truncated
    // against a marker the index never saw would hide the rows the page then wants.
    let mut page_q = q.clone();
    match q.get("last") {
        Some(last) => match last.strip_prefix(&format!("{who}/")) {
            Some(name) => {
                page_q.insert("last".into(), name.to_string());
            }
            None => {
                page_q.remove("last");
                page_q.remove("n");
            }
        },
        None => {}
    }
```

- [ ] **Step 4: Run tests and clippy** — `cargo test --test registry_http && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/registry/src/routes.rs tests/registry_http.rs && git commit -m "Ignore a catalog marker that names no image of this owner"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 8: Count a pull only once the manifest is actually served (L3)

**Files:**
- Modify: `crates/registry/src/manifests.rs:319-372` (`manifest_response`'s tag arm and both success paths)
- Test: `tests/registry_manifests.rs` (new test)

**Interfaces:**
- Consumes: `Store::bump_pulls(&self, owner: &str, name: &str, tag: &str)`, `Store::pulls(&self, owner, name, tag) -> Result<u64>`. No signature changes.

- [ ] **Step 1: Write the failing test** — append to `tests/registry_manifests.rs`:

```rust
/// A pull is a manifest that was served. A tag whose manifest object is gone 404s, and a 404 is
/// not a pull — counting at tag resolution inflated the number the page shows.
#[tokio::test]
async fn a_tag_whose_manifest_is_gone_is_not_counted_as_a_pull() {
    use slatedb::object_store::ObjectStoreExt;
    let (base, e, c, token, m, d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();

    // One honest pull first, so the counter is provably live.
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(e.store.pulls("acme", "nginx", "latest").await.unwrap(), 1);

    // Take the bytes away behind the tag, and clear the cached copy so the GET reaches the store.
    e.store.manifests().remove(&format!("acme/nginx/{d}"));
    e.store.os.delete(&kloudlite_git_registry::store::manifest_path("acme", "nginx", &d)).await.unwrap();
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        e.store.pulls("acme", "nginx", "latest").await.unwrap(),
        1,
        "a 404 is not a pull"
    );
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test registry_manifests a_tag_whose_manifest_is_gone_is_not_counted_as_a_pull`. Expected: `assertion 'left == right' failed: a 404 is not a pull; left: 2, right: 1`.

- [ ] **Step 3: Implement** — in `crates/registry/src/manifests.rs`, replace the tag arm at lines 322-336 with a resolution that only remembers the tag:

```rust
    // The tag this GET resolved, if any — the pull is counted where the bytes are actually
    // served, not here: a tag whose manifest object is gone 404s below, and counting at
    // resolution inflated the number by every one of those.
    let mut pulled_tag: Option<String> = None;
    let d = match r {
        Reference::Digest(d) => d,
        Reference::Tag(t) => match app.store.tag(&owner, &name, &t).await {
            Ok(Some(d)) => {
                // GET by tag only — a HEAD is docker probing, and a GET by digest is docker
                // re-reading what the tag already resolved to; counting either would inflate.
                if with_body {
                    pulled_tag = Some(t);
                }
                d
            }
            Ok(None) => return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(e) => return crate::oci_internal(e),
        },
    };
```

Then bump on each of the two success paths. In the cache-hit block (currently `manifests.rs:339-346`), immediately before its `return`:

```rust
        // A map increment only — no lock, no write — so a hundred concurrent pulls of one tag do
        // not queue behind each other here.
        if let Some(t) = &pulled_tag {
            app.store.bump_pulls(&owner, &name, t);
        }
```

And in the store-fetch path, immediately after `app.store.manifests().insert(cache_key, (bytes.clone(), media.clone()));` (currently `manifests.rs:368`):

```rust
    if let Some(t) = &pulled_tag {
        app.store.bump_pulls(&owner, &name, t);
    }
```

- [ ] **Step 4: Run tests and clippy** — `cargo test --test registry_manifests && cargo test --test cache_invalidation && cargo test --test registry_http && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/registry/src/manifests.rs tests/registry_manifests.rs && git commit -m "Count a pull where the manifest is served"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 9: A manifest DELETE spares its blobs, asserted (L7)

**Files:**
- Modify: `tests/registry_manifests.rs:132-150` (`deleting_a_manifest_by_digest_drops_its_media_type_row`)

**Interfaces:**
- Consumes: `kloudlite_git_registry::store::blob_path(owner: &str, d: &Digest) -> OsPath`, `slatedb::object_store::ObjectStoreExt::head`. No production code changes.

- [ ] **Step 1: Write the failing test** — extend `deleting_a_manifest_by_digest_drops_its_media_type_row` (`tests/registry_manifests.rs:132`). Rename it and add the blob assertion; the rule the crate doc puts first has no direct test today:

```rust
/// Deleting a manifest by digest must drop its `image/manifest-type/{d}` row — otherwise it
/// orphans forever — and must leave every blob it named exactly where it is. Siblings share
/// layers, so only an explicit client DELETE and the GC sweep may ever remove one.
#[tokio::test]
async fn deleting_a_manifest_by_digest_drops_its_media_type_row_and_spares_its_blobs() {
```

and, immediately before the DELETE in that test's body, capture the layer path, then assert after it:

```rust
    use slatedb::object_store::ObjectStoreExt;
    let layer = kloudlite_git_registry::store::blob_path("acme", &Digest::of(b"layer"));
    let cfg = kloudlite_git_registry::store::blob_path("acme", &Digest::of(b"cfg"));
    assert!(e.store.os.head(&layer).await.is_ok(), "the layer is there before the delete");
```

and after the `assert_eq!(r.status(), StatusCode::ACCEPTED);`:

```rust
    assert!(
        e.store.os.head(&layer).await.is_ok(),
        "a manifest path must never delete a blob: a sibling image may share this layer"
    );
    assert!(e.store.os.head(&cfg).await.is_ok(), "and neither its config");
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test registry_manifests deleting_a_manifest_by_digest_drops_its_media_type_row_and_spares_its_blobs`. This passes on a correct tree (the rule holds today); prove it is a real guard by temporarily adding `let _ = app.store.os.delete(&blob_path(&owner, &Digest::of(b"layer"))).await;` to `manifests::delete_manifest`, watching it fail with "a manifest path must never delete a blob", then reverting that line.

- [ ] **Step 3: Implement** — no production change. Confirm `git diff --stat` touches `tests/registry_manifests.rs` only.

- [ ] **Step 4: Run tests and clippy** — `cargo test --test registry_manifests && cargo test --test registry_gc && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add tests/registry_manifests.rs && git commit -m "Assert a manifest delete spares its blobs"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 10: Refuse a branch name git will never accept (M3)

**Files:**
- Modify: `bins/server/src/browse_api/pulls.rs:160-165` (add the check after the `base == head` refusal), plus a new `#[cfg(test)] mod tests` at the end of the file
- Test: `tests/browse_http.rs` (new test beside `a_pull_opened_on_the_owner_is_readable_and_listed`, line 592+)

**Interfaces:**
- Produces: `fn valid_branch(b: &str) -> bool` in `bins/server/src/browse_api/pulls.rs` (private)
- Consumes: nothing new. `api_pull_open`'s signature is unchanged.

- [ ] **Step 1: Write the failing test** — append to `tests/browse_http.rs`:

```rust
/// `base`/`head` reach the worker's git argv and the owner's `refs/heads/{}` lookups. A name git
/// will never accept makes a change permanently unmergeable while still burning a claim, a fetch
/// and a full worker job on every merge request, re-announced every 30 s. Refused at open, with
/// the 400 the other field checks already use.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_on_a_branch_name_git_will_not_accept_is_refused() {
    let e = common::env().await;
    let router = kloudlite_git_server::router::peer_router(common::app(e.store.clone()).await);
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);

    for bad in [
        "",
        "-dashed",
        "a..b",
        "has space",
        "star*",
        "tilde~1",
        "caret^",
        "colon:x",
        "question?",
        "bracket[",
        "back\\slash",
        "wip.lock",
    ] {
        let (s, _) = post_json_as(
            &router,
            "alice",
            "/api/alice/widget/pulls",
            serde_json::json!({ "title": "t", "body": "", "base": "main", "head": bad }),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "head {bad:?} was accepted");

        let (s, _) = post_json_as(
            &router,
            "alice",
            "/api/alice/widget/pulls",
            serde_json::json!({ "title": "t", "body": "", "base": bad, "head": "topic" }),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "base {bad:?} was accepted");
    }

    // And the ordinary names still open, or this test would pass on a handler that refuses all.
    let (s, _) = open_pr(&router, "/api/alice/widget/pulls", "fine", "feature/ok-1.2").await;
    assert_eq!(s, StatusCode::CREATED);
}
```

and append to `bins/server/src/browse_api/pulls.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::valid_branch;

    /// The exact rule list, so a future edit to `valid_branch` is a decision rather than an
    /// accident. Control characters are in here because a `\n` in a ref name reaches git's argv.
    #[test]
    fn branch_names_git_will_never_accept_are_refused() {
        for ok in ["main", "feature/ok-1.2", "a.b", "release_2", "x".repeat(255).as_str()] {
            assert!(valid_branch(ok), "{ok:?} is a legal branch name");
        }
        for bad in [
            "",
            "-leading",
            "a..b",
            "sp ace",
            "tab\tted",
            "new\nline",
            "null\0byte",
            "tilde~",
            "caret^",
            "colon:",
            "question?",
            "star*",
            "bracket[",
            "back\\slash",
            "wip.lock",
        ] {
            assert!(!valid_branch(bad), "{bad:?} must be refused");
        }
        assert!(!valid_branch(&"x".repeat(256)), "256 bytes must be refused");
    }
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test browse_http a_change_on_a_branch_name_git_will_not_accept_is_refused` and `cargo test -p kloudlite-git-server branch_names_git_will_never_accept_are_refused`. Expected: `error[E0432]: unresolved import 'super::valid_branch'` for the unit test, and `assertion 'left == right' failed: head "" was accepted; left: 201, right: 400` for the integration one.

- [ ] **Step 3: Implement** — insert into `bins/server/src/browse_api/pulls.rs` immediately above `pub(super) async fn api_pull_open` (line 146):

```rust
/// git's `check-ref-format` basics, as much of it as matters here. A change opened on a name git
/// will never accept is permanently unmergeable while still costing a claim, a fetch and a full
/// worker job on every merge request, re-announced every 30 s — so it is refused at open rather
/// than failing forever at merge. Deliberately a denylist of git's own refusals, not a
/// character allowlist: the names people actually use carry `/`, `.`, `-` and unicode.
fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && b.len() <= 255
        && !b.starts_with('-')
        && !b.contains("..")
        && !b.ends_with(".lock")
        && !b.chars().any(|c| c.is_ascii_control() || c == ' ' || "~^:?*[\\".contains(c))
}
```

and in `api_pull_open`, immediately after the `base == head` refusal (currently `pulls.rs:161-165`):

```rust
    if !valid_branch(base) || !valid_branch(head) {
        return (StatusCode::BAD_REQUEST, "that is not a branch name git will accept")
            .into_response();
    }
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-server && cargo test --test browse_http && cargo test --test pulls && cargo test --test api_server && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add bins/server/src/browse_api/pulls.rs tests/browse_http.rs && git commit -m "Refuse a change opened on an illegal branch name"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 11: Stamp the mergeability verdict with the tips it was computed from (M5)

**Files:**
- Modify: `crates/pulls/src/merge_worker.rs:100-108` (`Verdict`), `:842-848` (`unknown`), `:854-894` (`check_local`)
- Modify: `bins/server/src/browse_api/pulls.rs:515-554` (`api_pull_mergeability`)
- Test: `tests/pulls.rs` (new test in the module that holds `a_diverged_change_is_answered_by_the_workers_trial_merge`, line 1185+)

**Interfaces:**
- Produces: two new fields on `pub struct Verdict` — `pub base_oid: String` and `pub head_oid: String`, both `#[serde(default)]`, serialized camelCase as `baseOid`/`headOid` if the struct carries `#[serde(rename_all = "camelCase")]` (match the struct's existing attribute exactly; do not add one). No field is added to `Mergeability` — it already records `base_oid`/`head_oid` (`crates/pulls/src/pulls/model.rs:78-80`), and those are what the handler compares against.
- Consumes: `crate::merge_worker::Verdict` in `api_pull_mergeability`; `bins/worker/src/main.rs:432` serializes the whole struct and needs no change.

- [ ] **Step 1: Write the failing test** — append inside the same `mod` in `tests/pulls.rs` that holds `a_diverged_change_is_answered_by_the_workers_trial_merge` (it already has `peer`, `open_pr`, `diverged` in scope):

```rust
/// The outcome route guards a lapsed worker's late report by matching `?by=` against the claim.
/// The verdict route has no claim to match, so it matches the TIPS instead: a slow lane's answer
/// is only true of the branches it was computed from, and must not overwrite a newer lane's.
#[tokio::test(flavor = "multi_thread")]
async fn a_verdict_computed_from_other_tips_is_refused() {
    if !common::have_git() {
        eprintln!("skipping: no git");
        return;
    }
    let e = common::env().await;
    let fleet = diverged(&e).await;
    let db = e.store.db_for("a", "r").await.unwrap();
    pulls::put(&db, &open_pr(1)).await.unwrap();
    assert_eq!(peer(&fleet, "/api/a/r/pulls/1/check", None).await.status(), 200);
    let pending = pulls::get(&db, 1).await.unwrap().unwrap().mergeability.unwrap();
    assert_eq!(pending.state, MergeableState::Unknown);

    // A verdict stamped with a head the row never saw: the branch moved on since.
    let stale = serde_json::json!({
        "state": "clean",
        "detail": "from an older lane",
        "fastForward": false,
        "baseOid": pending.base_oid,
        "headOid": "0".repeat(40),
    });
    let r = peer(&fleet, "/api/a/r/pulls/1/mergeability", Some(stale)).await;
    assert_eq!(r.status(), 409, "a verdict about other tips is not this change's answer");
    assert_eq!(
        pulls::get(&db, 1).await.unwrap().unwrap().mergeability.unwrap().state,
        MergeableState::Unknown,
        "the row is untouched"
    );

    // The same verdict, honestly stamped, lands.
    let fresh = serde_json::json!({
        "state": "clean",
        "detail": "ok",
        "fastForward": false,
        "baseOid": pending.base_oid,
        "headOid": pending.head_oid,
    });
    let r = peer(&fleet, "/api/a/r/pulls/1/mergeability", Some(fresh)).await;
    assert_eq!(r.status(), 204);
    assert_eq!(
        pulls::get(&db, 1).await.unwrap().unwrap().mergeability.unwrap().state,
        MergeableState::Clean
    );

    // And an UNSTAMPED verdict still lands, so a worker older than this field keeps working
    // through a roll.
    let unstamped = serde_json::json!({"state": "dirty", "detail": "old worker", "fastForward": false});
    let r = peer(&fleet, "/api/a/r/pulls/1/mergeability", Some(unstamped)).await;
    assert_eq!(r.status(), 204);
}
```

Then extend `a_diverged_change_is_answered_by_the_workers_trial_merge` (`tests/pulls.rs:1220-1226`) so the worker's own verdict carries the stamps, immediately after `assert!(!verdict.fast_forward);`:

```rust
        assert_eq!(
            verdict.base_oid, pending.base_oid,
            "the worker stamps the verdict with the tips it merged"
        );
        assert_eq!(verdict.head_oid, pending.head_oid);
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test pulls a_verdict_computed_from_other_tips_is_refused`. Expected: `assertion 'left == right' failed: a verdict about other tips is not this change's answer; left: 204, right: 409`. And `cargo test --test pulls a_diverged_change_is_answered_by_the_workers_trial_merge` fails with `error[E0609]: no field 'base_oid' on type 'Verdict'`.

- [ ] **Step 3: Implement** —

`crates/pulls/src/merge_worker.rs:100-108`, add the two stamps to `Verdict` (keep whatever derive/serde attributes the struct already carries above line 102):

```rust
pub struct Verdict {
    pub state: crate::directory::MergeableState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub fast_forward: bool,
    /// The tips this verdict was computed from. The owner records mergeability against the two
    /// oids it read, and this is what lets it tell a fresh answer from a lapsed lane's stale one —
    /// there is no claim on a check to match against. Empty when this worker could not resolve
    /// either branch, which the owner reads as "unstamped" and accepts.
    #[serde(default)]
    pub base_oid: String,
    #[serde(default)]
    pub head_oid: String,
}
```

`crates/pulls/src/merge_worker.rs:842-848`, `unknown` stays the unstamped constructor:

```rust
fn unknown(why: String) -> Verdict {
    Verdict {
        state: crate::directory::MergeableState::Unknown,
        detail: Some(why),
        fast_forward: false,
        base_oid: String::new(),
        head_oid: String::new(),
    }
}
```

`crates/pulls/src/merge_worker.rs:859-894`, capture the oids from the `rev-parse` that already runs and stamp every arm:

```rust
    let dir = cache_of(cache, &job.owner, &job.name);
    let refs = format!("refs/heads/{}", job.base);
    let head_ref = format!("refs/heads/{}", job.head);
    let tips = local(
        &dir,
        &[
            "rev-parse",
            &format!("{refs}^{{commit}}"),
            &format!("{head_ref}^{{commit}}"),
        ],
    )?;
    if !tips.status.success() {
        return Ok(unknown("one of the branches is gone".to_string()));
    }
    // `rev-parse` prints one oid per argument, in order — the two tips this verdict is about.
    let out = String::from_utf8_lossy(&tips.stdout);
    let mut lines = out.lines();
    let base_oid = lines.next().unwrap_or_default().trim().to_string();
    let head_oid = lines.next().unwrap_or_default().trim().to_string();
    // A verdict this worker could not actually compute is `Unknown` with the reason, never a guess
    // in either direction: "clean" would offer a button that fails, "dirty" would hide a merge
    // that works.
    Ok(match tree_merge(&dir, &refs, &head_ref) {
        Ok(Ok(_)) => Verdict {
            state: MergeableState::Clean,
            detail: Some(format!(
                "this can be merged into {}, but not fast-forwarded",
                job.base
            )),
            fast_forward: false,
            base_oid,
            head_oid,
        },
        Ok(Err(o)) => Verdict {
            state: MergeableState::Dirty,
            detail: o.detail,
            fast_forward: false,
            base_oid,
            head_oid,
        },
        Err(e) => Verdict { base_oid, head_oid, ..unknown(e.to_string()) },
    })
```

`bins/server/src/browse_api/pulls.rs`, replace the closure body inside `api_pull_mergeability`'s `update` call (lines 543-550) with:

```rust
    match update(&app, &owner, &name, number, |pr| {
        let Some(m) = pr.mergeability.as_mut() else {
            return Some(StatusCode::NO_CONTENT.into_response());
        };
        // The outcome route matches `?by=` against the claim; a check has no claim, so this
        // matches the tips instead. A lane whose lease lapsed can report long after another lane
        // answered from newer tips, and its `Clean`/`Dirty` would overwrite the fresher one.
        // An UNSTAMPED verdict is still accepted: a worker older than this field, and one that
        // could not resolve either branch, must keep working through a roll — and the next check
        // rewrites the row anyway.
        if !v.base_oid.is_empty() && (v.base_oid != m.base_oid || v.head_oid != m.head_oid) {
            return Some(
                (StatusCode::CONFLICT, "this verdict was computed from other tips").into_response(),
            );
        }
        m.state = v.state;
        m.detail = v.detail.clone();
        m.fast_forward = v.fast_forward;
        None
    })
```

Also extend the doc comment above `api_pull_mergeability` (`pulls.rs:513-518`): after "the two tips the answer belongs to were stamped by the check that asked for this", add "— and a verdict that names different ones is refused 409, the same shape of answer a lapsed claim gets on the outcome route."

- [ ] **Step 4: Run tests and clippy** — `cargo test --test pulls && cargo test --test browse_http && cargo test -p kloudlite-git-pulls && cargo test -p kloudlite-git-server && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/pulls/src/merge_worker.rs bins/server/src/browse_api/pulls.rs tests/pulls.rs && git commit -m "Stamp a mergeability verdict with the tips it came from"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

## Self-review

- **M1** (`max_layer` default above the 5 GiB copy cap) → Task 1
- **M2** (blob-row backfill inline, unbounded, unlocked, 500-on-error) → Task 2
- **M3** (`base`/`head` never validated at open) → Task 10
- **M4** (unbounded `join_all` fan-out ×2; `gc::stats_of` reused verbatim for the catalog one) → Task 3
- **M5** (`api_pull_mergeability` accepts a stale verdict; tip-stamping design, fields `Verdict.base_oid`/`Verdict.head_oid`, compared against the row's existing `Mergeability.base_oid`/`head_oid`) → Task 11
- **L1** (`delete_blob` leaves hold rows) → Task 5
- **L2** (`?n=0` looks like the end of the catalog) → Task 6
- **L3** (pull counted before the manifest exists) → Task 8
- **L4** (two suffix-delete loops) → Task 4
- **L5** (merge cache lock held across the whole job) → **deferred**: the report itself notes rather than recommends it, and no honest regression test exists. The lock's effect is contention, not a wrong answer, so a test would have to prove that a cheap mergeability nudge for repo X is *not* delayed by an in-flight merge for X — which means two worker lanes, a merge held open by a sleep, and a wall-clock deadline. That is a timing assertion in CI, i.e. a flake generator, and CLAUDE.md's own note says the lock "is really about" the claim. Revisit only with a measured queueing complaint, and then take the lock around the git work alone and let the claim lease do the cross-pod exclusion it already does.
- **L6** (`catalog`'s `last` passed through unstripped) → Task 7
- **L7** (no test that a manifest DELETE spares its blobs) → Task 9
