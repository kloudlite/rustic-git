# Registry Performance Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the redundant round trips on the registry's hot paths — HEAD-then-GET pairs, serial per-item awaits, and re-derived existence checks — without changing any observable behavior.

**Architecture:** Every fix is local to the function it names: pull paths take size/mtime from the GET's own `meta` instead of a preceding HEAD; independent per-digest/per-image store calls go through `futures` combinators already used elsewhere in the repo; a small per-node manifest cache keyed by digest exploits content addressing (immutable bytes) with explicit invalidation on the only two mutating paths, both of which run on the owning node. No new crates, no new state beyond one `Mutex<HashMap>` field.

**Tech Stack:** Rust, axum 0.8, `futures` 0.3 (already a dep), `object_store` via `slatedb::object_store` (0.15's re-export; `delete_stream` available), SlateDB 0.15.

**Spec:** docs/perf-review-2026-08-24.md — P0-2, P0-3, P0-6; P1 registry bullets (manifest cache, `image_exists` tax, uploads HEAD+GET, `manifest_stat` concurrency, delete paths, tag delete-by-digest, `imagetags`, `put_tag`, `tags()` sort); P2 GC `get_bytes` copy.

## Global Constraints

- **Single-opener invariant:** one SlateDB database per image, exactly one node may have it open. Nothing here may add an `image_db` open on a path that did not already have one; the GC worker and `sweep_stale_uploads` stay object-store-only.
- **Manifest bytes are stored and returned verbatim.** The cache stores the exact bytes; nothing re-emits parsed JSON.
- **`Digest::parse` is the only way a path segment becomes an object-store key.**
- **Only two things ever delete a blob:** the client `DELETE /v2/.../blobs/{digest}` handler and `gc::sweep_owner`. The GC sweep stays keep-biased: any unreadable/unparseable manifest aborts it.
- Every `/v2` error stays the OCI envelope via `registry::oci_err`; auth still flows through `registry::auth::allow`.
- Preserve existing `// ponytail:` markers; adjust/remove one only when the fix removes its ceiling; add one when a fix takes a deliberate shortcut.
- Comments explain WHY, never what; density of `src/http.rs`.
- `cargo clippy --lib -- -D warnings` green; `cargo test` green before every commit.
- Commit subjects imperative sentence case, no tool attribution.
- **Perf fixes must not change behavior.** Most tasks lean on the existing suites (`tests/registry_blobs.rs`, `tests/registry_http.rs`, `tests/registry_manifests.rs`, `tests/registry_uploads.rs`, `tests/registry_gc.rs`, `tests/registry_store.rs`, `tests/registry_limits.rs`) as the safety net and add no assertion-free tests. The one task with new observable behavior (cache invalidation, Task 4) gets real tests.

## Spec deviations (decided after reading the code)

- **P0-6's "compute the referenced set once and reuse it" is wrong.** `sweep_owner` reads `referenced()` twice *on purpose*: the second read closes the mount-race window (a blob a client skipped uploading, then referenced from a manifest written after the first scan), documented in the function and pinned by `tests/registry_gc.rs::a_blob_referenced_between_the_two_manifest_reads_survives_the_mount_race`. Both reads stay. The real wins implemented instead: concurrent manifest GETs inside `referenced()` (`buffered(16)`), and one blob listing instead of two in `sweep_owner`.
- **The spec's `uploads.rs:195,292,432` line refs describe the current code as `received()` (HEAD) followed by `staged()` (GET) in `patch` and `complete`.** The fix is to fold the size into `staged()`'s return (the GET's `meta.size`) and drop the `received()` call from those two paths; `status` keeps `received()` (HEAD is right there — no body wanted).
- **`imagetags` is already concurrent (`buffered(8)`)** — the remaining waste is the per-tag HEAD+GET pair, fixed in Task 9.

## Deliberately excluded

- **P2 sha512 probe skip on by-tag push:** the current shape already pays only one HEAD in the common re-push case; skipping the sha512 branch on a *first* push needs a signal ("does this image have sha512 manifests?") that costs a listing — more than it saves. Not simple, skipped.
- **P2 referrers `unindex` collect→delete inline:** deleting rows while a `DbIterator` over the same prefix is live is unverified SlateDB behavior, the path is rare (manifest delete), and the Vec holds a handful of small keys. Risk > reward, skipped.
- **P1 forwarded-request HeaderMap clone and pulls-sweep deserialization** are `src/http.rs`/`src/pulls.rs` findings — outside the registry area, left to their own plan.

---

### Task 1: Blob pull answers from one GET (P0-2)

**Files:**
- Modify: `src/registry/blobs.rs` — `blob_response` (currently lines ~48-93: unconditional `os.head(&path)`, then a second `os.get(&path)` when `with_body`).

**Interfaces:**
- Consumes: `object_store::GetResult { meta: ObjectMeta { size: u64, .. }, .. }` — the GET already carries the size the HEAD was fetching.
- Produces: no signature change; `head_blob` keeps its HEAD.

**Steps:**

- [ ] Read `src/registry/blobs.rs:48-93`. Replace the head-then-get shape with: HEAD only when `!with_body`, single GET otherwise. The `hdrs` construction moves into each arm because the size source differs:

```rust
    let path = blob_path(&owner, &d);
    let hdrs = |size: u64| {
        [
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CONTENT_TYPE, "application/octet-stream".into()),
            (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
        ]
    };
    if !with_body {
        return match app.store.os.head(&path).await {
            Ok(m) => (StatusCode::OK, hdrs(m.size)).into_response(),
            Err(slatedb::object_store::Error::NotFound { .. }) => {
                oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
            }
            Err(e) => crate::registry::oci_internal(e.into()),
        };
    }
    // One GET, not HEAD-then-GET: the GET's own meta carries the size, and this is the hottest
    // registry path — the HEAD was a pure extra round trip per layer pulled.
    // Stream the layer straight through: buffering the whole object here is an anonymous
    // memory-DoS for public images (a few concurrent pulls of a large layer OOM the node).
    match app.store.os.get(&path).await {
        Ok(r) => {
            let size = r.meta.size;
            (StatusCode::OK, hdrs(size), axum::body::Body::from_stream(r.into_stream())).into_response()
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => crate::registry::oci_internal(e.into()),
    }
```

- [ ] Pure perf refactor — no new test; `tests/registry_blobs.rs` (pull round trip, 404 shape, HEAD/GET agreement) is the net. Run `cargo test --test registry_blobs`.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Serve blob pulls from a single GET"`

---

### Task 2: Manifest push checks referenced blobs concurrently (P0-3)

**Files:**
- Modify: `src/registry/manifests.rs` — the `for s in &named` loop in `put_manifest` (currently the block ending in `MANIFEST_BLOB_UNKNOWN`).

**Interfaces:**
- Consumes: `futures::future::join_all` (crate already imported repo-wide), `gc::collect`'s `HashSet<String>`.
- Produces: same responses in the same cases (`MANIFEST_INVALID` on a malformed digest, `MANIFEST_BLOB_UNKNOWN` on a missing one).

**Steps:**

- [ ] Replace the serial loop. Parse first (fail fast, no store calls for garbage), then fan out — blob path first, manifest path as the fallback, exactly the order the serial code probed:

```rust
    // Parsed before any store round trip: one malformed digest refuses the push without paying
    // for the valid ones' probes.
    let mut digests = Vec::with_capacity(named.len());
    for s in &named {
        let Some(bd) = Digest::parse(s) else {
            return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "malformed digest in manifest");
        };
        digests.push(bd);
    }
    // Concurrent, not serial: a 40-layer manifest was up to 80 sequential HEADs before the
    // write. Each probe is independent; blob path first because that is where layers live —
    // the manifest path is only hit for an index's entries.
    let present = futures::future::join_all(digests.iter().map(|bd| async {
        app.store.os.head(&blob_path(&owner, bd)).await.is_ok()
            || app.store.os.head(&manifest_path(&owner, &name, bd)).await.is_ok()
    }))
    .await;
    if present.iter().any(|ok| !ok) {
        return oci_err(StatusCode::NOT_FOUND, "MANIFEST_BLOB_UNKNOWN", "manifest references a blob this registry does not hold");
    }
```

- [ ] Keep the `// ponytail: a sweep can still delete an old blob between this head and the put below …` marker directly above this block — the window it names is unchanged.
- [ ] No new test — behavior identical; `tests/registry_manifests.rs::a_manifest_naming_a_missing_blob_is_manifest_blob_unknown`, `::an_index_entry_is_looked_up_as_a_manifest_and_subject_is_exempt`, `::a_foreign_layer_is_not_required_to_be_present` are the net. Run `cargo test --test registry_manifests`.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Probe manifest-referenced blobs concurrently on push"`

---

### Task 3: Upload PATCH/complete read the session once (P1 uploads HEAD+GET)

**Files:**
- Modify: `src/registry/uploads.rs` — `staged` (~line 128), `patch` (~271-296), `complete` (~415-436). `status` and `received` unchanged.

**Interfaces:**
- Produces: `staged` becomes `crate::Result<Option<(u64, BoxStream<'static, crate::Result<Bytes>>)>>` — size from the GET's `meta`, stream as before.

**Steps:**

- [ ] Change `staged`:

```rust
/// The session's bytes so far — its size (from the GET's own meta, so no separate HEAD) and a
/// stream. `None` is no session: the staging object IS the session and `open_session` writes an
/// empty one up front, so a `NotFound` here means it was cancelled or swept — not a fresh
/// two-request push. Resuming at offset 0 in that case would silently resurrect a session the
/// client already gave up on.
pub(super) async fn staged(
    os: &Arc<dyn ObjectStore>,
    path: &OsPath,
) -> crate::Result<Option<(u64, BoxStream<'static, crate::Result<Bytes>>)>> {
    match os.get(path).await {
        Ok(r) => {
            let size = r.meta.size;
            Ok(Some((size, r.into_stream().map_err(crate::Error::from).boxed())))
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
```

- [ ] In `patch`, delete the `received(...)` call; after taking the lock, read the session once and destructure — the `declared_chunk`/`content_length` checks run before `pour`, dropping the unused stream on refusal is free:

```rust
    let path = staging(&owner, &name, &uuid);
    let (have, src) = match staged(&app.store.os, &path).await {
        Ok(Some(s)) => s,
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::registry::oci_internal(e),
    };
```
  (then the existing `declared` / `Content-Length` checks, then `pour(..., src.chain(body_stream(body)))` — the second `staged` call and its match are gone.) Keep the `// ponytail: the staging object is re-streamed behind each chunk …` marker: that ceiling (O(N×chunks) store IO) is untouched; this task only removes the extra HEAD.
- [ ] In `complete`, same change: delete the `received(...)` block, replace the `staged` match with the destructuring form above (path is `staging(owner, name, uuid)` inline, as now). The `len.checked_sub(have)` logic downstream is unchanged in both.
- [ ] No new test — `tests/registry_uploads.rs` (chunk sequencing, 416s, lying Content-Range, sweep, cancel) exercises every branch touched. Run `cargo test --test registry_uploads`.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Fold the upload session's size into its single GET"`

---

### Task 4: Manifest pulls hit a per-node cache keyed by digest (P1)

**Files:**
- Modify: `src/store.rs` — `Store` struct (~line 8-28) and `Store::open` (~line 205-221): one new field.
- Modify: `src/registry/manifests.rs` — `manifest_response` (lookup + fill), `put_manifest` (invalidate), `delete_manifest` digest arm (invalidate).
- Test: `tests/registry_manifests.rs`.

**Interfaces:**
- Produces: `pub(crate) manifest_cache: std::sync::Mutex<std::collections::HashMap<String, (slatedb::bytes::Bytes, String)>>` on `Store` (`slatedb::bytes` is slatedb 0.15's public re-export of the same `bytes` crate `object_store`'s `GetResult::bytes()` returns — verified in the vendored source, `slatedb-0.15.0/src/lib.rs:20`). Key: `{owner}/{name}/{digest}`; value: (verbatim bytes, media type).

**Why this is safe:** manifest bytes are content-addressed — the digest IS the bytes, so a cached body can never be stale, on any node. The two mutable companions are handled explicitly: the media-type row (a re-push of the same digest may declare a new Content-Type) and existence (delete by digest). Both mutations are `put_manifest`/`delete_manifest`, which run only on the node that owns the image — the same node serving the GETs — so per-node invalidation is complete, not best-effort. Tag→digest resolution stays uncached (tags are mutable) and `bump_pulls` runs before the cache is consulted, so pull counting is unchanged.

**Steps:**

- [ ] Write the failing tests (append to `tests/registry_manifests.rs`, copying `pushed()`'s shape):

```rust
/// The digest-keyed manifest cache must not outlive the two things that can change an answer:
/// a re-push of the same bytes with a new declared Content-Type, and a delete by digest.
#[tokio::test]
async fn a_repush_with_a_new_content_type_is_visible_after_a_cached_pull() {
    let (base, e, c, token, body, d) = pushed().await;
    let _ = e;
    // Prime the cache.
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    // Same bytes, same digest, different declared type.
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token))
        .header("content-type", "application/vnd.example.custom+json")
        .body(body.clone()).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::CREATED);
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.headers()["content-type"], "application/vnd.example.custom+json");
    assert_eq!(r.bytes().await.unwrap().as_ref(), body.as_slice(), "bytes verbatim, always");
}

#[tokio::test]
async fn a_deleted_manifest_is_gone_even_after_a_cached_pull() {
    let (base, e, c, token, _body, d) = pushed().await;
    let _ = e;
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    let r = c.delete(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);
}
```
  (Adjust to `pushed()`'s real return tuple — read the helper at the top of the file first; it currently returns `(String, common::TestEnv, reqwest::Client, String, Vec<u8>, Digest)`.)
- [ ] Run `cargo test --test registry_manifests a_repush_with_a_new_content_type` — the first test may already pass pre-cache; both must pass AFTER the cache lands. Note the delete test may exist in spirit (`deleting_a_manifest_by_digest_drops_its_media_type_row`) — these two pin the *cache* not regressing it.
- [ ] Add the field to `src/store.rs`:

```rust
    /// Manifest pull cache, digest-addressed. The bytes are immutable by construction (the digest
    /// is over them), and the two mutable companions — media type and existence — are invalidated
    /// by `put_manifest`/`delete_manifest`, which only ever run on the node serving these GETs
    /// (single-opener routing). Per-node and unbounded-in-time on purpose; see the cap at fill.
    pub(crate) manifest_cache:
        std::sync::Mutex<std::collections::HashMap<String, (slatedb::bytes::Bytes, String)>>,
```
  and `manifest_cache: Default::default(),` in `Store::open`.
- [ ] In `manifest_response`, after the tag/digest resolution (so `bump_pulls` still fires) and before the object-store GET:

```rust
    let cache_key = format!("{owner}/{name}/{d}");
    if let Some((bytes, media)) = app.store.manifest_cache.lock().unwrap().get(&cache_key).cloned() {
        let hdrs = [
            (header::CONTENT_TYPE, media),
            (header::CONTENT_LENGTH, bytes.len().to_string()),
            (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
        ];
        return if with_body { (StatusCode::OK, hdrs, bytes).into_response() }
               else { (StatusCode::OK, hdrs).into_response() };
    }
```
  and after `bytes` and `media` are both fetched, fill it:

```rust
    {
        let mut c = app.store.manifest_cache.lock().unwrap();
        // ponytail: clear-on-full at 256 entries (≤ 256 × 4 MiB worst case, ~a few MiB real) —
        // the same sweep-don't-evict shape as auth_cache. A real LRU if hit rate ever matters.
        if c.len() >= 256 {
            c.clear();
        }
        c.insert(cache_key, (bytes.clone(), media.clone()));
    }
```
  (`bytes` is already `Bytes` here — `r.bytes().await` — so the clone is a refcount bump. `media` needs a `clone()` before it moves into `hdrs`.)
- [ ] In `put_manifest`, after the media-type row is written (the `db.put(format!("{MEDIA_TYPE_KEY_PREFIX}{d}") ...)` call succeeds): `app.store.manifest_cache.lock().unwrap().remove(&format!("{owner}/{name}/{d}"));` — a re-push of the same digest may have changed the declared type.
- [ ] In `delete_manifest`'s `Reference::Digest(d)` arm, before returning ACCEPTED (place it right after the media-type row delete): the same `remove` line.
- [ ] Run the two new tests, then `cargo test --test registry_manifests --test registry_http`.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Cache manifest pulls by digest with owner-side invalidation"`

---

### Task 5: Drop the redundant probes inside the tag/visibility reads (P1 image_exists tax, put_tag, tags() sort)

**Files:**
- Modify: `src/registry/store.rs` — `put_tag` (~182), `tag` (~191), `tags` (~205), `image_is_public` (~249).

**Why the rewrite is right:** `image_exists` = `pool.exists` (warm-map hit or one LIST) **plus** an `image_db` open **plus** an `IMAGE_KEY` get. For `tag`/`tags`/`image_is_public` the `IMAGE_KEY` get proves nothing the target row doesn't already prove: a missing tag row is `None`, an empty scan is `[]`, a missing `PUBLIC_KEY` is private — all identical answers with or without the pre-check. The `pool.exists` probe **must stay** (opening creates a database; probing an unknown image through `image_db` would conjure one per bad request). `image_exists` itself is unchanged — `delete_manifest`, `tags_list`'s NAME_UNKNOWN, `imagedelete` still need the existence distinction.

**Steps:**

- [ ] Rewrite the four methods:

```rust
    pub async fn put_tag(&self, owner: &str, name: &str, tag: &str, d: &Digest) -> Result<()> {
        // One handle for both puts: `touch_image` would resolve the pool entry a second time on
        // the hottest write path for no gain.
        let db = self.image_db(owner, name).await?;
        db.put(IMAGE_KEY, b"1".as_slice()).await?;
        db.put(tag_key(tag), d.to_string().into_bytes()).await?;
        Ok(())
    }

    pub async fn tag(&self, owner: &str, name: &str, tag: &str) -> Result<Option<Digest>> {
        // `pool.exists`, not `image_exists`: the probe only has to keep `image_db` from CREATING
        // a database for an image nobody pushed. A missing tag row already answers `None`, so the
        // extra IMAGE_KEY read `image_exists` adds proves nothing here — and this runs on every
        // pull.
        let (o, n) = crate::registry::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(None);
        }
        let v = self.image_db(owner, name).await?.get(tag_key(tag)).await?;
        Ok(v.and_then(|v| Digest::parse(&String::from_utf8_lossy(&v))))
    }
```
  `tags`: same `pool.exists` swap for its `image_exists` pre-check; keep the scan. Replace `out.sort();` with a comment and nothing: SlateDB's `scan_prefix` yields keys in ascending byte order (vendored `slatedb-0.15.0/src/db.rs` `scan_prefix` doc example: `ab` before `aba`; verify it is still there before deleting the sort), and tags are ASCII (`reference()`'s grammar), so byte order IS the lexical order the spec wants:

```rust
    /// Sorted lexically, which is the order the spec requires `tags/list` to return — free here:
    /// `scan_prefix` yields ascending byte order and the tag grammar is ASCII, where byte order
    /// and lexical order agree.
```
  `image_is_public`: same swap — `pool.exists` guard, then the `PUBLIC_KEY` get; a missing row was already `false`.
- [ ] No new tests — `tests/registry_manifests.rs::tags_list_sorts_and_paginates` pins the sort claim; `::tags_list_of_a_missing_image_is_name_unknown`, `::deleting_a_manifest_of_a_missing_image_creates_nothing`, `::a_stranger_cannot_read_a_private_image`, and `tests/registry_store.rs` cover the rest. Run `cargo test --test registry_manifests --test registry_store --test registry_http`.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Drop redundant existence probes from tag and visibility reads"`

---

### Task 6: Delete-by-digest finds its tags in one scan (P1)

**Files:**
- Modify: `src/registry/store.rs` — new method `tags_pointing_at`.
- Modify: `src/registry/manifests.rs` — `delete_manifest`'s `Reference::Digest` arm (currently `tags()` then per-tag `tag()` — each of which was 3 DB ops — then `delete_tag`).

**Interfaces:**
- Produces: `pub async fn tags_pointing_at(&self, owner: &str, name: &str, d: &Digest) -> Result<Vec<String>>`.

**Steps:**

- [ ] Add to `src/registry/store.rs` (next to `tags`):

```rust
    /// The tags resolving to `d`, from ONE scan — the delete-by-digest path was re-reading every
    /// tag row individually (list, then a get per tag) to learn what this reads in a single pass.
    pub async fn tags_pointing_at(&self, owner: &str, name: &str, d: &Digest) -> Result<Vec<String>> {
        let (o, n) = crate::registry::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(vec![]);
        }
        let db = self.image_db(owner, name).await?;
        let want = d.to_string();
        let mut it = db.scan_prefix(TAG_PREFIX, ..).await?;
        let mut out = vec![];
        while let Some(kv) = it.next().await? {
            if String::from_utf8_lossy(&kv.value) == want {
                if let Some(t) = std::str::from_utf8(&kv.key).ok().and_then(|k| k.strip_prefix(TAG_PREFIX)) {
                    out.push(t.to_string());
                }
            }
        }
        Ok(out)
    }
```
- [ ] In `delete_manifest`'s digest arm, replace the `tags()` + inner `tag()` loop with:

```rust
            let tags = match app.store.tags_pointing_at(&owner, &name, &d).await {
                Ok(t) => t,
                Err(e) => return crate::registry::oci_internal(e),
            };
            for t in tags {
                if let Err(e) = app.store.delete_tag(&owner, &name, &t).await {
                    return crate::registry::oci_internal(e);
                }
            }
```
  Note the behavior nuance and preserve it: the old code's `.ok().flatten()` swallowed per-tag read errors (skipping that tag); the new scan surfaces a scan error as a 500. That is a tightening, not a loosening — an unreadable tag map failing the delete loudly beats leaving a dangling tag silently — and no test pins the old swallow.
- [ ] `cargo test --test registry_manifests deleting_a_tag_leaves_the_manifest_and_deleting_the_manifest_takes_its_tags` — the existing test is the net; no new test for a pure round-trip reduction.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Find a deleted manifest's tags in a single scan"`

---

### Task 7: GC reads manifests concurrently and lists blobs once (P0-6 + P2 get_bytes)

**Files:**
- Modify: `src/registry/gc.rs` — `get_bytes` (~20), `referenced` (~37-72), `sweep_owner` (~262-307).

**Steps:**

- [ ] `get_bytes` returns `Bytes` (drop the `.to_vec()` copy — the caller only ever reads it):

```rust
async fn get_bytes(store: &Store, p: &slatedb::object_store::path::Path) -> Result<slatedb::bytes::Bytes> {
    Ok(store.os.get(p).await?.bytes().await?)
}
```
- [ ] In `referenced`, keep the listing pass as is, then replace the serial `for p in paths` GET loop with a bounded-concurrency stream. Keep-biased abort semantics are UNCHANGED: any error still aborts the whole sweep (the `?`/`return Err` paths survive verbatim, they just fire from a stream item):

```rust
    // Concurrent GETs, bounded: 500 manifests were 500 serial round trips per sweep tick.
    // `buffered` (ordered) rather than `buffer_unordered` so at most 16 manifest bodies are in
    // memory at once and the abort-on-first-error below fires deterministically.
    let mut fetched = futures::stream::iter(paths)
        .map(|p| async move { let b = get_bytes(store, &p).await; (p, b) })
        .buffered(16);
    while let Some((p, bytes)) = futures::StreamExt::next(&mut fetched).await {
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gc: aborting sweep of {owner}: unreadable manifest {p}: {e}"); // ponytail: eprintln
                return Err(e);
            }
        };
        // ... existing digest_from_path insert, serde parse-or-abort, and collect() — unchanged.
    }
```
  (The closure captures `store: &Store` by reference; the stream lives inside the function so the borrow is fine. Adjust the existing `futures::StreamExt::next` import style to match the file.)
- [ ] In `sweep_owner`, list the blobs ONCE: collect the metas, answer the any-old probe from them, and reuse them for the doomed pass instead of a second listing. **The two `referenced()` reads stay — see the spec-deviation note at the top of this plan.** The safety argument for listing before the first `referenced()` read is the one the existing code already relies on for the probe: this listing never decides what to delete on its own — a blob is only doomed if BOTH referenced() reads (each taken after the listing) miss it, and a blob PUT after the listing simply is not in the list. Extend the existing probe comment to say so:

```rust
    // One listing serves both the "anything old enough?" probe and the doomed pass below. It is
    // taken BEFORE the manifests are read, which the module doc forbids for deciding deletions —
    // but this list never decides one alone: a blob is only deleted if both `referenced()` reads
    // (each newer than this listing) miss it, and a blob written after the listing is simply
    // absent from it, i.e. kept. Nothing past grace means nothing deletable whatever the
    // manifests say, so an idle registry still reads no manifests at all.
    let mut listing = store.os.list(Some(&prefix));
    let mut metas = vec![];
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        metas.push(m?);
    }
    if !metas.iter().any(|m| m.last_modified <= cutoff) {
        return Ok(0);
    }

    let keep = referenced(store, owner).await?;
    let mut doomed = vec![];
    for m in metas {
        let Some(digest) = digest_from_path(&m.location) else { continue };
        if keep.contains(&digest) || m.last_modified > cutoff {
            continue;
        }
        doomed.push(m.location);
    }
    // ... existing keep_again re-read, retain, and delete loop — unchanged.
```
- [ ] Existing tests are the net and they pin every property touched: `tests/registry_gc.rs::a_blob_referenced_between_the_two_manifest_reads_survives_the_mount_race` (double read kept), `::a_sweep_with_nothing_old_enough_reads_no_manifests` (probe still manifest-free), `::a_manifest_that_is_not_valid_json_aborts_the_sweep_and_deletes_nothing` (keep-biased abort), plus the grace/sharing tests. Run `cargo test --test registry_gc`.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Read GC manifests concurrently and list blobs once per sweep"`

---

### Task 8: `manifest_stat` fan-out in the listing fallback and the worker reconcile (P1)

**Files:**
- Modify: `src/registry/routes.rs` — `image_listing` (~22-43): serial `manifest_stat` per unmarked image.
- Modify: `src/registry/gc.rs` — `reconcile_owner` cases (a) and (c): serial `manifest_stat` per image.

**Steps:**

- [ ] `image_listing`: keep the `// ponytail: fallback dies with the backfill` marker; parallelize the stats:

```rust
    // ponytail: fallback dies with the backfill
    let unmarked: Vec<String> =
        image_names(app, owner).await?.into_iter().filter(|n| !marked.contains(n)).collect();
    // One listing per image, fanned out — a serial loop here put the whole catalog page behind
    // N sequential round trips.
    let stats = futures::future::join_all(
        unmarked.iter().map(|n| super::store::manifest_stat(&app.store, owner, n)),
    )
    .await;
    for (name, stat) in unmarked.into_iter().zip(stats) {
        let (count, newest) = stat.unwrap_or((0, None));
        markers.push(crate::index::Marker {
            name,
            public: false,
            created_by: String::new(),
            created_ms: 0,
            description: String::new(),
            manifests: count as u64,
            updated_ms: newest.unwrap_or(0),
        });
    }
```
- [ ] `reconcile_owner` case (a): gather the missing names into a `Vec`, `futures::future::join_all` the `manifest_stat` calls the same way, then run the existing serial `put_in_place` writes over the zipped results (keep-biased: a stat `Err` still just skips that entry via `let Ok((count, newest)) = … else { continue }` on the zipped value). Case (c): same shape — join_all the stats for the retained markers, zip, keep the compare-and-rewrite loop serial. Writes stay serial on purpose: they are rare (only drifted entries) and ordering churn buys nothing.
- [ ] No new tests — `tests/registry_gc.rs` reconcile coverage and `tests/registry_http.rs` catalog/listing tests are the net. Run `cargo test --test registry_gc --test registry_http --test browse_http`.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Fan out manifest stats in listings and the worker reconcile"`

---

### Task 9: SKIPPED — covered by the server plan

The `imagetags` HEAD+GET pair in `src/http/browse_api/images.rs` is fixed by
`2026-08-24-perf-server.md` Task 6 (same file, same change). Do nothing here; if that
task already landed, verify `os.head` is gone from the per-tag future and move on.

### Task 10: Batched deletes via delete_stream (P1)

**Files:**
- Modify: `src/registry/store.rs` — `delete_image` (~367-382): list-collect then serial deletes.
- Modify: `src/registry/uploads.rs` — `sweep_stale_uploads` (~480-495): serial deletes in the listing loop.
- Modify: `src/http/browse_api/images.rs` — `imagedelete` (~200-212): serial manifest-object deletes.

**Interfaces:**
- Consumes: `ObjectStore::delete_stream(BoxStream<Result<Path>>) -> BoxStream<Result<Path>>` (object_store ≥0.13 has a real batched default; present in the vendored version — confirm with `grep -n delete_stream` in the vendored `object_store` src before starting, and fall back to `futures::stream::iter(doomed).map(|p| os.delete(&p)).buffer_unordered(8)` if this deployment's version only has the naive default — the concurrency is the win either way).

**Steps:**

- [ ] `delete_image` — the listing feeds the delete stream directly, no collect:

```rust
        let prefix = OsPath::from(crate::pool::path(o, &n));
        // Streamed, not collected-then-serial: the store batches (or at least overlaps) the
        // deletes, and an image's DB prefix can hold hundreds of SST objects.
        let locations = self.os.list(Some(&prefix)).map_ok(|m| m.location).boxed();
        futures::TryStreamExt::try_collect::<Vec<_>>(self.os.delete_stream(locations)).await?;
        Ok(())
```
- [ ] `sweep_stale_uploads` — keep-biased filter feeds the stream; count successes only, exactly the old semantics (unreadable entry skipped, failed delete uncounted):

```rust
        let cutoff = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now() - grace);
        let stale = self
            .os
            .list(Some(&prefix))
            .filter_map(|m| async move {
                let m = m.ok()?; // keep-biased: an entry this can't read is skipped, never deleted
                (m.last_modified <= cutoff).then_some(Ok(m.location))
            })
            .boxed();
        let n = self
            .os
            .delete_stream(stale)
            .fold(0usize, |n, r| async move { n + r.is_ok() as usize })
            .await;
        Ok(n)
```
  Keep the `// ponytail: upload/{uuid} rows written by the pre-row-less build are orphaned …` marker above the function — untouched by this change.
- [ ] `imagedelete` — the existing collect stays (a listing error must still 500 before anything is deleted); the serial delete loop becomes:

```rust
    let stream = futures::stream::iter(doomed.into_iter().map(Ok)).boxed();
    if let Err(e) = futures::TryStreamExt::try_collect::<Vec<_>>(app.store.os.delete_stream(stream)).await {
        return internal(e.into());
    }
```
- [ ] No new tests — `tests/registry_uploads.rs` sweep tests, the image-delete tests in `tests/browse_http.rs`/`tests/registry_http.rs`, and `tests/registry_store.rs` are the net. Run `cargo test --test registry_uploads --test browse_http --test registry_store`.
- [ ] `cargo clippy --lib -- -D warnings && cargo test`
- [ ] Commit: `git commit -am "Batch object-store deletes on the image and upload sweeps"`
