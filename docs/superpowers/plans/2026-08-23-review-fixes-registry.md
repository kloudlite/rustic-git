# Registry Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every finding of the 2026-08-23 review that touches the container registry (`src/registry/*`), its GC/index/events plumbing, the worker, and the `tests/registry_*.rs` files — critical first.

**Architecture:** Each task is one fix behind its own failing test and its own commit. The two structural changes are (1) an upload session becomes *only* its staging object — no database row — which removes the worker's forbidden `image_db` open, the never-PATCHed row leak, and the phantom-image guard in one move; and (2) blob bodies stream through `object_store`'s `WriteMultipart` with an incremental hasher, so memory is one 5 MiB part, never a layer. Everything else is a local fix that copies the sibling pattern in the file it touches.

**Tech Stack:** Rust, axum 0.8, tokio, `object_store` 0.14 (via `slatedb::object_store`, `tokio` feature is on — verified with `cargo tree -e features -i object_store`), `sha2` via `russh::keys::ssh_key::sha2`, SlateDB, reqwest (tests).

**Spec:** `docs/code-review-2026-08-23.md` — sections 0 (#1, #6, #7, #8), 1 (auth.rs Low), 2 (registry Medium bullets), 3 (gc.rs, manifests.rs:83-91), 4 (registry redundancy), 5 (referrers, events doc, store.rs:286), 6 (registry test gaps). Executors read both.

## Global Constraints

- `cargo test` must pass after every task. Run the named `--test` file the task points at, then the full suite before each commit.
- Clippy bar (from `CLAUDE.md`): no NEW warnings in files you touch (`cargo clippy --lib`). `--all-targets -D warnings` has pre-existing errors — ignore those.
- House style: comments explain WHY, never what; density of `src/http.rs`. Deliberate shortcuts carry `// ponytail: <ceiling and upgrade path>` — keep existing markers when editing near one. Commit subjects imperative sentence case, no tool attribution, no "claude".
- **Invariants that must survive this work:** one SlateDB database per image, exactly one opener (the worker must never call `image_db`); `Digest::parse` is the only path→key mapping; manifest bytes stored and returned verbatim; only `delete_blob` and `gc::sweep_owner` ever delete a blob; every `/v2` error is the OCI envelope via `registry::oci_err`.
- The test harness is `tests/common/mod.rs`: `common::env()` (in-memory object store, cache disabled), `common::serve_public()` → `(base_url, TestEnv)`, `e.store.create_token(owner)` for a Basic password. Copy the shape of the neighbouring test in the file you add to.
- Spec deviations made here, deliberately: the review's "owner drops the row lazily" becomes "there is no row" (Task 1); `refresh_blob_mtime` is deleted rather than replaced (Task 3); the review's `MockSystemClock` suggestion for `registry_blobs.rs:274,288` is moot — that clock drives SlateDB, not the object store's `last_modified`, and the test it named is deleted with the mechanism it tested (Task 3).

---

## CRITICAL

### Task 1: An upload session is its staging object — no database row, no worker DB open

**Files:**
- Modify: `src/registry/uploads.rs` (module doc, `received`, `open_session`, `patch` tail, `discard`, `sweep_stale_uploads`)
- Test: `tests/registry_uploads.rs`

**Interfaces:**
- Consumes: `Store::os: Arc<dyn ObjectStore>`, `Store::touch_image(owner, name) -> Result<()>`, `Pool::exists(owner, name) -> Result<bool>` (`e.store.pool.exists("img", "acme/ghost")`).
- Produces: `fn staging(owner, name, uuid) -> object_store::path::Path` (unchanged, still private), `async fn received(app, owner, name, uuid) -> crate::Result<Option<u64>>` now answers from `os.head(staging)`. `Store::sweep_stale_uploads(owner, grace) -> Result<usize>` keeps its signature and touches only the object store. Task 5 builds on the row-less session.

**Context:** `sweep_stale_uploads` runs in the worker (`src/bin/worker.rs:283`) and calls `self.image_db(owner, &name)` to delete the `upload/{uuid}` row — opening a database the worker does not own fences the node serving the image. The row only ever stores `have`, which is exactly the staging object's size. Drop the row: a session is the staging object, `have` is its `size`, "no object" is "no session". That also fixes the never-PATCHed leak (the row was written at open but the object was not, so a never-PATCHed session was invisible to the sweep) and removes every `image_db` call from PATCH/GET/DELETE on a session.

- [ ] **Step 1: Write the failing tests**

Append to `tests/registry_uploads.rs`:

```rust
/// The sweep runs in the WORKER, which must never open an image database (single-opener
/// invariant — see CLAUDE.md). Staging an object for an image that has no database and sweeping
/// it proves the sweep reaches only the object store: if it opened `image_db`, the pool would now
/// have a `img/acme/ghost` database it conjured out of a stale upload.
#[tokio::test]
async fn the_upload_sweep_never_opens_an_image_database() {
    use slatedb::object_store::{path::Path as OsPath, ObjectStoreExt, PutPayload};
    let e = common::env().await;
    let uuid = "0".repeat(32);
    e.store.os
        .put(&OsPath::from(format!("uploads/acme/ghost/{uuid}")), PutPayload::from(b"stale".to_vec()))
        .await.unwrap();

    let n = e.store.sweep_stale_uploads("acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 1);
    assert!(
        !e.store.pool.exists("img", "acme/ghost").await.unwrap(),
        "the sweep must not create (or open) the image's database"
    );
}

/// A session opened and never PATCHed used to leave only a database row — no staging object for
/// the sweep to find — so it leaked forever. Now the session IS a (possibly empty) staging object.
#[tokio::test]
async fn a_session_that_was_never_patched_is_swept_too() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();

    let n = e.store.sweep_stale_uploads("acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 1, "an empty session is still a session, and still sweepable");
    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test registry_uploads the_upload_sweep_never_opens_an_image_database a_session_that_was_never_patched_is_swept_too`
Expected: both FAIL — the first because `pool.exists` is true after the sweep opened the DB; the second because `n == 0` (no staging object exists for a never-PATCHed session).

- [ ] **Step 3: Make the session the staging object**

In `src/registry/uploads.rs`:

Replace the module doc's second paragraph with:

```rust
//! A session is its staging object and nothing else: `uploads/{owner}/{name}/{uuid}` holds the
//! bytes received so far, and its size IS how many there are. Addressable from any node that owns
//! the image, so a session survives the image moving — and, because there is no row in the
//! image's database, the GC worker can sweep an abandoned one without opening a database it does
//! not own (which would fence the node that does).
```

Delete `SESSION_PREFIX` and `session_key`. Replace `received`:

```rust
/// How many bytes the session holds, or `None` when there is no session. The staging object is
/// the session: a `NotFound` here is a session that was never opened, was completed, was
/// cancelled, or was swept — all the same answer to a client.
async fn received(app: &App, owner: &str, name: &str, uuid: &str) -> crate::Result<Option<u64>> {
    match app.store.os.head(&staging(owner, name, uuid)).await {
        Ok(m) => Ok(Some(m.size)),
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
```

Replace `open_session`'s body after `let uuid = new_uuid();`:

```rust
    // The image must exist (even manifest-less) so a completed upload has somewhere to belong.
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::registry::oci_internal(e);
    }
    // An EMPTY staging object, written now: the object is the session, so a session with no
    // bytes yet must still be something `received` can find and the sweep can age out.
    if let Err(e) = app.store.os.put(&staging(owner, name, &uuid), PutPayload::default()).await {
        return crate::registry::oci_internal(e.into());
    }
    accepted(owner, name, &uuid, 0)
```

In `patch`, delete the block that opens `image_db` and writes `session_key` (the `let db = match app.store.image_db(...)` through its `db.put(...)`), leaving `accepted(&owner, &name, &uuid, len)` right after the `os.put` of the staging object.

Replace `discard`:

```rust
async fn discard(app: &App, owner: &str, name: &str, uuid: &str) {
    let _ = app.store.os.delete(&staging(owner, name, uuid)).await;
}
```

Replace the `impl Store` block:

```rust
impl Store {
    /// Delete this owner's abandoned upload sessions — the staging objects under
    /// `uploads/{owner}/` older than `grace`. Object-store reads and deletes ONLY: this runs in
    /// the GC worker, which must never open an image database (the single-opener invariant), and
    /// since the object is the whole session there is nothing else to remove. Keep-biased like
    /// `gc::sweep_owner`: an entry this can't read is skipped, never deleted on uncertainty, and
    /// one bad entry does not abort the rest.
    pub async fn sweep_stale_uploads(&self, owner: &str, grace: std::time::Duration) -> crate::Result<usize> {
        let prefix = slatedb::object_store::path::Path::from(format!("uploads/{owner}"));
        let mut listing = self.os.list(Some(&prefix));
        let cutoff = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now() - grace);
        let mut n = 0usize;
        while let Some(m) = futures::StreamExt::next(&mut listing).await {
            let Ok(m) = m else { continue };
            if m.last_modified > cutoff {
                continue;
            }
            if self.os.delete(&m.location).await.is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }
}
```

Remove the now-unused `use crate::store::Store;` only if the compiler says so (the `impl Store` still needs it — it stays).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test registry_uploads` then `cargo test --test registry_blobs`
Expected: PASS, including the pre-existing `stale_upload_sessions_are_swept`, `a_fresh_upload_session_survives_the_grace_window`, `a_session_reports_its_progress_and_can_be_cancelled`.

- [ ] **Step 5: Commit**

```bash
git add src/registry/uploads.rs tests/registry_uploads.rs
git commit -m "Make an upload session its staging object so the worker never opens an image database"
```

---

## HIGH

### Task 2: One hex encoder and an incremental hasher

**Files:**
- Modify: `src/lib.rs` (add `pub(crate) fn hex` next to `pub fn err`, plus a unit test in the existing `mod tests`)
- Modify: `src/registry/store.rs:41-64` (`Digest::of`, `Digest::of_algo`, new `Hasher`)
- Modify: `src/registry/uploads.rs:43-47` (`new_uuid`)
- Test: `src/lib.rs` tests module, `tests/registry_store.rs`

**Interfaces:**
- Produces: `pub(crate) fn hex(bytes: &[u8]) -> String` in `src/lib.rs` (lowercase, two chars per byte). `pub enum Hasher` in `src/registry/store.rs` with `pub fn new(algo: &str) -> Option<Hasher>`, `pub fn update(&mut self, bytes: &[u8])`, `pub fn finish(self) -> Digest`. Task 5 streams through `Hasher`. `src/gpg.rs:76` has a private twin of `hex`; a later plan swaps it for this one — do not touch `gpg.rs` here.

- [ ] **Step 1: Write the failing tests**

In `src/lib.rs`, inside the existing `mod tests` (line ~784), add:

```rust
    #[test]
    fn hex_is_lowercase_and_two_chars_per_byte() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(hex(&[]), "");
    }
```

Append to `tests/registry_store.rs`:

```rust
/// The streaming upload path hashes a layer in chunks; the result must be byte-for-byte what the
/// one-shot `of_algo` produces, for both algorithms `Digest::parse` accepts.
#[test]
fn an_incremental_hash_matches_the_one_shot_digest() {
    use kloudlite::registry::store::Hasher;
    for algo in ["sha256", "sha512"] {
        let mut h = Hasher::new(algo).unwrap();
        h.update(b"layer ");
        h.update(b"bytes");
        assert_eq!(h.finish(), Digest::of_algo(algo, b"layer bytes").unwrap(), "{algo}");
    }
    assert!(Hasher::new("md5").is_none(), "only the algorithms parse accepts");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib hex_is_lowercase` and `cargo test --test registry_store an_incremental_hash`
Expected: both FAIL to compile (`hex` / `Hasher` not found).

- [ ] **Step 3: Add `hex` and `Hasher`, route `of`/`of_algo` through them**

In `src/lib.rs`, directly after `pub fn err`:

```rust
/// Lowercase hex, the encoding every digest, fingerprint and token id in this crate uses on the
/// wire. One definition so a future change (or a faster one) happens in one place.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
```

In `src/registry/store.rs`, replace `Digest::of` and `Digest::of_algo` (lines 41-64) with:

```rust
    /// sha256 of `bytes`, for content this code digests itself (manifests keyed by digest, etc.) —
    /// there the algorithm is our choice, not a claim from the client.
    pub fn of(bytes: &[u8]) -> Digest {
        Self::of_algo("sha256", bytes).expect("sha256 is always supported")
    }

    /// Hash `bytes` with whatever algorithm the CLIENT claimed, so a push can be verified against
    /// the digest it was pushed under instead of always assuming sha256. `algo` is untrusted input
    /// here too — anything but the two `parse` accepts returns `None` rather than silently picking
    /// a hash.
    pub fn of_algo(algo: &str, bytes: &[u8]) -> Option<Digest> {
        let mut h = Hasher::new(algo)?;
        h.update(bytes);
        Some(h.finish())
    }
}

/// An incremental digest, so a layer can be hashed as it streams to the object store instead of
/// after it has been buffered whole. Same two algorithms `Digest::parse` admits, for the same
/// reason: the client names the algorithm, and an unknown one is refused, never guessed.
pub enum Hasher {
    Sha256(russh::keys::ssh_key::sha2::Sha256),
    Sha512(russh::keys::ssh_key::sha2::Sha512),
}

impl Hasher {
    pub fn new(algo: &str) -> Option<Hasher> {
        use russh::keys::ssh_key::sha2::Digest as _;
        Some(match algo {
            "sha256" => Hasher::Sha256(russh::keys::ssh_key::sha2::Sha256::new()),
            "sha512" => Hasher::Sha512(russh::keys::ssh_key::sha2::Sha512::new()),
            _ => return None,
        })
    }

    pub fn update(&mut self, bytes: &[u8]) {
        use russh::keys::ssh_key::sha2::Digest as _;
        match self {
            Hasher::Sha256(h) => h.update(bytes),
            Hasher::Sha512(h) => h.update(bytes),
        }
    }

    pub fn finish(self) -> Digest {
        use russh::keys::ssh_key::sha2::Digest as _;
        match self {
            Hasher::Sha256(h) => Digest { algo: "sha256".into(), hex: crate::hex(&h.finalize()) },
            Hasher::Sha512(h) => Digest { algo: "sha512".into(), hex: crate::hex(&h.finalize()) },
        }
    }
```

(The closing `}` of `impl Hasher` follows; the `impl Digest` block was closed above before `pub enum Hasher`.)

In `src/registry/uploads.rs`, replace `new_uuid`'s last line:

```rust
    crate::hex(&buf)
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib hex_is_lowercase && cargo test --test registry_store && cargo test --test registry_blobs`
Expected: PASS (`digest_of_bytes_matches_the_wire_format` and `a_sha512_blob_round_trips` pin that nothing changed on the wire).

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/registry/store.rs src/registry/uploads.rs tests/registry_store.rs
git commit -m "Add one hex encoder and an incremental digest hasher"
```

---

### Task 3: A manifest PUT verifies every blob it names; drop copy-to-self mtime refresh

**Files:**
- Modify: `src/registry/manifests.rs:49-164` (`put_manifest`)
- Modify: `src/registry/gc.rs:86-101` (`collect` becomes `pub(crate)`)
- Modify: `src/registry/blobs.rs` (delete `refresh_blob_mtime` and its two call sites)
- Modify: `tests/common/mod.rs` (add `seed_blobs`)
- Modify: `tests/registry_manifests.rs` (`pushed()`, catalog test), `tests/registry_http.rs` (three HTTP manifest pushes), `tests/registry_blobs.rs` (delete the mtime test)
- Test: `tests/registry_manifests.rs`

**Interfaces:**
- Consumes: `gc::collect(&serde_json::Value, &mut HashSet<String>)` (made `pub(crate)`), `store::blob_path`, `store::manifest_path`, `Digest::parse`.
- Produces: `pub async fn seed_blobs(e: &TestEnv, owner: &str, contents: &[&[u8]])` in `tests/common/mod.rs`. Task 4 reuses the parsed `serde_json::Value` this task introduces in `put_manifest` (`let v: serde_json::Value`).

**Context:** `put_manifest` stores whatever the client sends; a manifest naming a layer that was never pushed — or that the 1h `blob_grace` sweep removed during a slow push — gets a 201 and a broken image. The spec's answer is 404 `MANIFEST_BLOB_UNKNOWN`. `refresh_blob_mtime` tried to protect an old, re-referenced blob from the sweep with `copy(path, path)`, which S3 rejects (same-key copy needs a metadata directive) and the error was swallowed, so it only ever worked on `mem://`/`file://`. With the existence check it is unnecessary: the sweep's double `referenced()` read protects a manifest that lands before its second read, and a manifest that lands after a delete is now refused instead of accepted broken. The residual window (sweep deletes between this PUT's `head` and its `put`) is the same shape as before, now narrowed to one request's duration; it gets a `ponytail:` marker naming the upgrade (a `touch/` side-marker the sweep consults).

An image index names MANIFESTS, not blobs, in `manifests[].digest` — so "exists" means "at `blob_path` OR at `manifest_path` of this image". `subject` is excluded: the spec lets a referrer be pushed before its subject.

- [ ] **Step 1: Add the seeding helper and fix the existing fixtures**

Append to `tests/common/mod.rs`:

```rust
/// Puts each of `contents` into `owner`'s blob store directly, so a test manifest can name layers
/// without pushing them over HTTP — `put_manifest` refuses a manifest naming a blob it cannot find.
pub async fn seed_blobs(e: &TestEnv, owner: &str, contents: &[&[u8]]) {
    use slatedb::object_store::{ObjectStoreExt, PutPayload};
    for c in contents {
        let d = kloudlite::registry::Digest::of(c);
        e.store
            .os
            .put(&kloudlite::registry::store::blob_path(owner, &d), PutPayload::from(c.to_vec()))
            .await
            .unwrap();
    }
}
```

In `tests/registry_manifests.rs`, change `pushed()` to seed every blob its fixtures name:

```rust
async fn pushed() -> (String, common::TestEnv, reqwest::Client, String, Vec<u8>, Digest) {
    let (base, e) = common::serve_public().await;
    // Every blob a manifest in this file names: `manifest()`'s two, the empty config the referrer
    // tests use, and the second config `a_push_refreshes_the_image_marker` pushes.
    common::seed_blobs(&e, "acme", &[b"cfg", b"layer", b"{}", b"cfg2"]).await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let m = manifest();
    let d = Digest::of(&m);
    (base, e, c, token, m, d)
}
```

In `the_catalog_lists_only_what_the_caller_may_see`, before `let other = ...`, add:

```rust
    common::seed_blobs(&e, "other", &[b"cfg", b"layer"]).await;
```

In `tests/registry_http.rs`, in each of `image_listing_serves_marker_fields`, `deleting_a_tag_leaves_the_manifest_and_other_tags_alone`, and `deleting_an_image_leaves_a_sibling_image_completely_intact`, add directly after the `let (pub_base, peer_base, e) = common::serve_public_and_peer().await;` line:

```rust
    common::seed_blobs(&e, "acme", &[b"cfg", b"layer", b"cfg2", b"layer2"]).await;
```

In `tests/registry_blobs.rs`, delete the whole test `head_and_mount_refresh_an_aged_blobs_mtime_within_half_grace` (its doc comment included). The mechanism it pinned is removed in Step 4.

- [ ] **Step 2: Write the failing tests**

Append to `tests/registry_manifests.rs`:

```rust
/// Spec: a manifest naming a blob the registry does not hold is refused with
/// `MANIFEST_BLOB_UNKNOWN`. Before this, a slow push whose early layers the grace sweep had
/// removed still got a 201 — and a broken image.
#[tokio::test]
async fn a_manifest_naming_a_missing_blob_is_manifest_blob_unknown() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let m = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": Digest::of(b"cfg").to_string(), "size": 3},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": Digest::of(b"never pushed").to_string(), "size": 12}]
    }).to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_BLOB_UNKNOWN");
    // Nothing landed: the tag does not resolve.
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// An index names manifests, not blobs — `manifests[].digest` must be looked up where manifests
/// live. And `subject` is exempt: the spec allows a referrer to arrive before its subject.
#[tokio::test]
async fn an_index_entry_is_looked_up_as_a_manifest_and_subject_is_exempt() {
    let (base, _e, c, token, m, d) = pushed().await;
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{"mediaType": MEDIA, "digest": d.to_string(), "size": 1, "platform": {"architecture": "amd64", "os": "linux"}}],
        "subject": {"mediaType": MEDIA, "digest": Digest::of(b"not here yet").to_string(), "size": 1}
    }).to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/multi"))
        .basic_auth("acme", Some(&token)).header("content-type", "application/vnd.oci.image.index.v1+json")
        .body(index).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --test registry_manifests a_manifest_naming_a_missing_blob an_index_entry_is_looked_up`
Expected: first FAILS (201 instead of 404); second PASSES already (it is a guard against over-checking — keep it).

- [ ] **Step 4: Implement the check and delete the refresh**

In `src/registry/gc.rs`, change `fn collect(` to `pub(crate) fn collect(` and put this doc above it:

```rust
/// Every `"digest"` string anywhere in a manifest. Shared with `put_manifest`'s existence check so
/// the sweep and the push agree on what "referenced" means — a digest one walks and the other
/// does not is a blob one of them gets wrong.
```

In `src/registry/manifests.rs`, add to the imports:

```rust
use super::store::blob_path;
use std::collections::HashSet;
```

In `put_manifest`, replace the `let media = ...` statement and the `os.put(&manifest_path(...))` that follows it with:

```rust
    // Every blob the manifest names must already be here, or the 201 would promise bytes the
    // registry does not hold (the spec's MANIFEST_BLOB_UNKNOWN). An index names MANIFESTS in
    // `manifests[].digest`, so "here" is either store. `subject` is exempt: a referrer may be
    // pushed before the thing it refers to.
    // ponytail: a sweep can still delete an old blob between this head and the put below — the
    // window is one request wide, down from "forever" when the mtime refresh silently failed on
    // S3. If it ever bites, write a `touch/{owner}/{algo}/{hex}` marker here and have
    // `gc::sweep_owner` treat the marker's mtime as the blob's.
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let mut named = HashSet::new();
    let mut without_subject = v.clone();
    if let Some(m) = without_subject.as_object_mut() {
        m.remove("subject");
    }
    super::gc::collect(&without_subject, &mut named);
    for s in &named {
        let Some(bd) = Digest::parse(s) else {
            return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "malformed digest in manifest");
        };
        let here = app.store.os.head(&blob_path(&owner, &bd)).await.is_ok()
            || app.store.os.head(&manifest_path(&owner, &name, &bd)).await.is_ok();
        if !here {
            return oci_err(StatusCode::NOT_FOUND, "MANIFEST_BLOB_UNKNOWN", "manifest references a blob this registry does not hold");
        }
    }
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    if let Err(e) = app.store.os.put(&manifest_path(&owner, &name, &d), PutPayload::from(body.clone())).await {
        return crate::registry::oci_internal(e.into());
    }
```

(Task 4 replaces the `unwrap_or(Value::Null)` with a proper `MANIFEST_INVALID` refusal; leave it permissive here so this commit changes one behaviour.)

In `src/registry/blobs.rs`: delete the whole `refresh_blob_mtime` function and its doc comment. In `blob_response`, replace the `if !with_body { ... }` block with:

```rust
    if !with_body {
        return (StatusCode::OK, hdrs).into_response();
    }
```

In `start_upload`'s mount branch, replace the `if let Ok(meta) = app.store.os.head(&mount_path).await {` block with:

```rust
            if app.store.os.head(&mount_path).await.is_ok() {
                if let Err(e) = app.store.touch_image(&owner, &name).await {
                    return crate::registry::oci_internal(e);
                }
                return created(&owner, &name, &d);
            }
```

Update `sweep_owner`'s comment in `src/registry/gc.rs` (the paragraph beginning `// Grace protects a blob uploaded and not yet referenced.`) by appending one sentence:

```rust
    // The other half of that protection is `put_manifest`, which refuses a manifest naming a blob
    // that is already gone, so a delete that wins this race produces a 404 the client can retry,
    // never a 201 over a missing layer.
```

If `blob_grace()` in `gc.rs` now has no caller besides `worker.rs`, leave it: its doc says why it lives there, and the worker still uses it.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test registry_manifests && cargo test --test registry_http && cargo test --test registry_blobs && cargo test --test registry_gc`
Expected: PASS. Then `cargo test` (full suite) — `tests/proxy.rs` does not push manifests, but confirm.

- [ ] **Step 6: Commit**

```bash
git add src/registry/manifests.rs src/registry/gc.rs src/registry/blobs.rs tests/common/mod.rs tests/registry_manifests.rs tests/registry_http.rs tests/registry_blobs.rs
git commit -m "Refuse a manifest that names a blob the registry does not hold"
```

---

### Task 4: Refuse non-JSON manifests; hash sha512 only when sha256 is not already stored

**Files:**
- Modify: `src/registry/manifests.rs` (`put_manifest`: the `Reference::Tag` arm and the `let v` line from Task 3)
- Test: `tests/registry_manifests.rs`

**Interfaces:**
- Consumes: `let v: serde_json::Value` from Task 3.
- Produces: nothing others consume.

**Context:** A non-JSON manifest is accepted today and then aborts every GC sweep for that owner forever (`gc::referenced` errors on it, by design). Refuse it with 400 `MANIFEST_INVALID`. Separately, the by-tag push always computes a sha512 of the body and HEADs for it, to honour a client that earlier pushed the same bytes by sha512 digest — compute the sha512 lazily, only when the sha256 object is absent.

- [ ] **Step 1: Write the failing tests**

Append to `tests/registry_manifests.rs`:

```rust
/// A manifest that is not JSON cannot be walked for the blobs it names, so the GC sweep would
/// have to abort for this owner forever. Refuse it at the door instead.
#[tokio::test]
async fn a_manifest_that_is_not_json_is_manifest_invalid() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(b"not json at all".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_INVALID");
}

/// A client that pushed by sha512 digest and then pushes the same bytes by tag must get the tag
/// pointed at the sha512 identity it already uses — not a freshly minted sha256 one.
#[tokio::test]
async fn a_tag_push_after_a_sha512_digest_push_keeps_the_sha512_identity() {
    let (base, _e, c, token, m, _d) = pushed().await;
    let d512 = Digest::of_algo("sha512", &m).unwrap();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{d512}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());

    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d512.to_string());

    let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d512.to_string());
}
```

- [ ] **Step 2: Run the tests to verify they fail / pass**

Run: `cargo test --test registry_manifests a_manifest_that_is_not_json a_tag_push_after_a_sha512`
Expected: the first FAILS (201 today); the second PASSES today and must still pass after Step 3.

- [ ] **Step 3: Implement**

In `put_manifest`, the `v` parse from Task 3 moves UP to directly after the `MAX_MANIFEST` check (before `reference(...)`), and becomes a refusal:

```rust
    // Parsed once, to READ — never re-emitted (the digest is over the bytes as sent). Not JSON is
    // refused here because `gc::referenced` cannot walk it for the blobs it names and would
    // otherwise abort every sweep for this owner, forever, on one bad push.
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "manifest is not JSON");
    };
```

Delete the `let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);` line from Task 3.

Replace the `Reference::Tag(_) => { ... }` arm with:

```rust
        Reference::Tag(_) => {
            // A by-tag push declares no algorithm, so sha256 is the default — but these exact
            // bytes may already be stored under ANOTHER algorithm (a client that pushed by
            // sha512 digest and now pushes the same manifest by tag). Repointing the tag at a
            // freshly minted sha256 would silently strip the identity the client already uses,
            // so prefer whichever digest the store already knows these bytes by. The sha512 is
            // hashed only when the sha256 object is absent: the common case pays one HEAD.
            let sha256 = Digest::of(&body);
            if app.store.os.head(&manifest_path(&owner, &name, &sha256)).await.is_ok() {
                sha256
            } else {
                match Digest::of_algo("sha512", &body) {
                    Some(sha512) if app.store.os.head(&manifest_path(&owner, &name, &sha512)).await.is_ok() => sha512,
                    _ => sha256,
                }
            }
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test registry_manifests && cargo test --test registry_gc`
Expected: PASS. (`registry_gc`'s `a_manifest_that_is_not_valid_json_aborts_the_sweep` writes garbage straight to the object store, bypassing the handler, and must still abort — the sweep's rule is unchanged.)

- [ ] **Step 5: Commit**

```bash
git add src/registry/manifests.rs tests/registry_manifests.rs
git commit -m "Refuse non-JSON manifests and hash sha512 only when needed"
```

---

### Task 5: Stream blob bodies through multipart uploads instead of buffering a layer

**Files:**
- Modify: `src/registry/uploads.rs` (new `pour`, `staged`, `body_stream`, `declared_chunk`, `length_mismatch`; rewrite `patch` and `complete`)
- Modify: `src/registry/blobs.rs` (`start_upload`, `finish_upload`, `finish_blob` take `axum::body::Body`)
- Modify: `src/registry/routes.rs:145-152` (comment on the body limit)
- Create: `tests/registry_limits.rs`
- Test: `tests/registry_uploads.rs`, `tests/registry_blobs.rs`, `tests/registry_limits.rs`

**Interfaces:**
- Consumes: `store::Hasher` (Task 2), row-less sessions (Task 1), `blobs::max_layer() -> u64`, `object_store::WriteMultipart` (`new(Box<dyn MultipartUpload>)`, `put(Bytes)`, `wait_for_capacity(usize)`, `finish()`, `abort()` — verified in `~/.cargo/registry/src/*/object_store-0.14.1/src/upload.rs`).
- Produces (all `pub(super)` in `uploads.rs`):
  - `enum Refused { TooLarge, WrongDigest, Failed(crate::Error) }`
  - `async fn pour<S>(os: &Arc<dyn ObjectStore>, dest: &Path, expect: Option<&Digest>, src: S) -> Result<u64, Refused>` where `S: Stream<Item = crate::Result<Bytes>> + Unpin`
  - `async fn staged(os, path) -> crate::Result<BoxStream<'static, crate::Result<Bytes>>>`
  - `fn body_stream(body: axum::body::Body) -> BoxStream<'static, crate::Result<Bytes>>`
  - `fn declared_chunk(headers, owner, name, uuid, have) -> Result<Option<u64>, Response>`
  - `fn content_length(headers: &HeaderMap) -> Option<u64>`, `fn length_mismatch() -> Response`
  - `pub async fn complete(app, owner, name, uuid, digest, headers, body: axum::body::Body) -> Response`

**Context:** Every blob handler takes `body: Bytes`, so axum buffers up to `max_layer` (10 GiB) per request, and PATCH re-reads the whole staging object into a `Vec` to append. The fix streams: the request body (`Body::into_data_stream`) — chained behind the existing staging bytes (`GetResult::into_stream`) where a session is being continued — goes through `WriteMultipart` in 5 MiB parts, hashed as it passes. The object lands only on `finish()`; every refusal calls `abort()`, so the "digest checked before the object lands" rule holds. Writing a multipart upload to the same key the staging stream is reading from is safe on all three backends in use (InMemory snapshots on `get`; LocalFileSystem writes a temp file and renames on complete; S3 parts are invisible until `CompleteMultipartUpload`).

Two consequences to know about. `DefaultBodyLimit` does not apply to the `Body` extractor (axum-core `default_body_limit.rs`: "if an extractor consumes the body directly ... the default limit is not applied"), so `pour` enforces `max_layer` itself — a byte over aborts. And the Content-Range "declared length equals body length" check can no longer run before the bytes move; it is pre-checked against `Content-Length` when the client sent one (every real client does — `reqwest` with a `Vec` body does too), and post-checked after the stream for a chunked body, where a mismatch answers 400 with the session advanced by what actually arrived: the client's next `GET`/`PATCH` sees the true `Range`, which is the resume protocol doing its job.

- [ ] **Step 1: Write the failing tests**

Create `tests/registry_limits.rs`. It is its own binary on purpose: `max_layer()` is a process-wide `OnceLock` read from `KLOUDLITE_MAX_LAYER`, so a tiny cap cannot share a process with the other registry tests.

```rust
//! `max_layer` is a process-global `OnceLock`, so these run in their own binary with a cap small
//! enough to trip from a test body. Both tests set the same value: the first caller wins and the
//! second must agree.
mod common;
use axum::http::StatusCode;
use kloudlite::registry::Digest;

const CAP: &str = "16";

async fn authed() -> (String, common::TestEnv, reqwest::Client, String) {
    std::env::set_var("KLOUDLITE_MAX_LAYER", CAP);
    let (base, e) = common::serve_public().await;
    let token = e.store.create_token("acme").await.unwrap();
    (base, e, reqwest::Client::new(), token)
}

/// One byte over the layer cap is refused with the OCI envelope, and nothing lands — the cap is
/// enforced by the streaming writer, since axum's body limit does not cover the `Body` extractor.
#[tokio::test]
async fn an_oversized_single_shot_blob_is_413_and_stores_nothing() {
    let (base, _e, c, token) = authed().await;
    let body = vec![0u8; 17];
    let d = Digest::of(&body);
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "SIZE_INVALID");
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// The cap is on the LAYER, not the chunk: a session grown past it one small chunk at a time is
/// refused on the chunk that crosses, and the session stays where it was.
#[tokio::test]
async fn a_chunked_upload_that_crosses_the_cap_is_413() {
    let (base, _e, c, token) = authed().await;
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    let r = c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "0-9").body(vec![1u8; 10]).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "10-19").body(vec![2u8; 10]).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), "0-9", "the refused chunk must not advance the session");
}

/// The manifest route keeps its own, separate cap (`MAX_MANIFEST`, 4 MiB), enforced by axum's
/// body limit before the handler runs — so the 413 here is axum's, not the OCI envelope. Status
/// is what a client acts on.
#[tokio::test]
async fn an_oversized_manifest_is_413() {
    let (base, _e, c, token) = authed().await;
    let body = vec![b'x'; 4 * 1024 * 1024 + 1];
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).body(body).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
```

Append to `tests/registry_uploads.rs`:

```rust
/// The spec allows a PATCH with no `Content-Range` (a client streaming one chunk). It must append
/// at the session's current end and report the new range.
#[tokio::test]
async fn a_patch_without_content_range_appends_at_the_end() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();

    let r = c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .body(b"abc".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), "0-2");
    let r = c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .body(b"def".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), "0-5");

    let d = Digest::of(b"abcdef");
    let r = c.put(format!("{base}{loc}?digest={d}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), b"abcdef");
}

/// A layer bigger than one multipart part (5 MiB) must cross the streaming writer in several
/// parts and still come back byte-identical — both as a single PUT and as two PATCHed halves.
#[tokio::test]
async fn a_multi_part_layer_round_trips_through_the_streaming_writer() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let whole: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let d = Digest::of(&whole);

    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    let (a, b) = whole.split_at(7 * 1024 * 1024);
    let r = c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", format!("0-{}", a.len() - 1)).body(a.to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = c.put(format!("{base}{loc}?digest={d}")).basic_auth("acme", Some(&token))
        .header("content-range", format!("{}-{}", a.len(), whole.len() - 1)).body(b.to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), whole);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test registry_limits && cargo test --test registry_uploads a_patch_without_content_range a_multi_part_layer`
Expected: `an_oversized_single_shot_blob_is_413_and_stores_nothing` and `a_chunked_upload_that_crosses_the_cap_is_413` PASS already (the old buffered checks work); `an_oversized_manifest_is_413` PASSES; `a_patch_without_content_range_appends_at_the_end` and `a_multi_part_layer_round_trips` PASS. These are regression guards for the rewrite — the failing-then-passing signal for this task is the full suite after Step 3, plus the two limit tests that exercise the new enforcement path. Confirm all green BEFORE Step 3 so any red after is yours.

- [ ] **Step 3: Rewrite the upload path to stream**

In `src/registry/uploads.rs`, replace the `use` block with:

```rust
use super::{auth, blobs, oci_err, store::blob_path, store::Hasher, Digest};
use crate::http::Trusted;
use crate::store::Store;
use crate::App;
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use futures::{stream::BoxStream, Stream, StreamExt, TryStreamExt};
use rand::RngCore;
use slatedb::object_store::{path::Path as OsPath, ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use std::sync::Arc;
```

Change `fn staging(...) -> slatedb::object_store::path::Path` to `-> OsPath` and its body to `OsPath::from(format!(...))`.

Add, after `valid_uuid`:

```rust
/// Why a stream could not be stored. Split so the handler can pick the status: the size cap and
/// a digest mismatch are the client's fault and keep the session; anything else is a 500.
pub(super) enum Refused {
    TooLarge,
    WrongDigest,
    Failed(crate::Error),
}

/// How many parts may be in flight before `pour` waits: bounds memory at `(1 + this) * 5 MiB`
/// per request while still overlapping network with hashing.
const IN_FLIGHT: usize = 4;

/// Streams `src` to `dest` through a multipart upload, hashing as it goes when `expect` names a
/// digest to verify against. Memory is one 5 MiB part plus `IN_FLIGHT` more, never the layer.
/// The object lands only on `finish`; every refusal aborts first, so nothing half-written — or
/// wrongly named — is ever readable under `dest`. Returns the byte count written.
pub(super) async fn pour<S>(
    os: &Arc<dyn ObjectStore>,
    dest: &OsPath,
    expect: Option<&Digest>,
    mut src: S,
) -> Result<u64, Refused>
where
    S: Stream<Item = crate::Result<Bytes>> + Unpin,
{
    let upload = os.put_multipart(dest).await.map_err(|e| Refused::Failed(e.into()))?;
    let mut w = WriteMultipart::new(upload);
    let mut hasher = expect.and_then(|d| Hasher::new(&d.algo));
    let mut n = 0u64;
    while let Some(chunk) = src.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = w.abort().await;
                return Err(Refused::Failed(e));
            }
        };
        n += chunk.len() as u64;
        if n > blobs::max_layer() {
            let _ = w.abort().await;
            return Err(Refused::TooLarge);
        }
        if let Some(h) = hasher.as_mut() {
            h.update(&chunk);
        }
        if let Err(e) = w.wait_for_capacity(IN_FLIGHT).await {
            let _ = w.abort().await;
            return Err(Refused::Failed(e.into()));
        }
        w.put(chunk);
    }
    if let Some(want) = expect {
        if hasher.map(Hasher::finish).as_ref() != Some(want) {
            let _ = w.abort().await;
            return Err(Refused::WrongDigest);
        }
    }
    w.finish().await.map_err(|e| Refused::Failed(e.into()))?;
    Ok(n)
}

/// The session's bytes so far, as a stream — empty when there is no staging object, which is how a
/// two-request push (no PATCH ever sent) arrives at `complete`.
pub(super) async fn staged(os: &Arc<dyn ObjectStore>, path: &OsPath) -> crate::Result<BoxStream<'static, crate::Result<Bytes>>> {
    match os.get(path).await {
        Ok(r) => Ok(r.into_stream().map_err(crate::Error::from).boxed()),
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(futures::stream::empty().boxed()),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn body_stream(body: Body) -> BoxStream<'static, crate::Result<Bytes>> {
    body.into_data_stream().map_err(|e| crate::err(e.to_string())).boxed()
}

pub(super) fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
}

/// The spec's `Content-Range` on a chunk. A start that is not where the session left off is 416
/// (with the headers a client resumes from — see `range_not_satisfiable`); absent is allowed, a
/// client streaming one chunk need not send it. Returns the length the header DECLARES, if it
/// declares one, so the caller can hold the body to it: a header claiming more (or fewer) bytes
/// than arrive means the client's own bookkeeping is wrong, and advancing the session by the real
/// length while it believes otherwise desyncs it from what is stored.
pub(super) fn declared_chunk(
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    uuid: &str,
    have: u64,
) -> Result<Option<u64>, Response> {
    let Some(cr) = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let mut parts = cr.trim_start_matches("bytes ").split('-');
    let start: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
    let end: Option<u64> = parts.next().and_then(|s| s.parse().ok());
    if start != have {
        return Err(range_not_satisfiable(owner, name, uuid, have));
    }
    let Some(end) = end else { return Ok(None) };
    // `end + 1` overflows on `bytes 0-18446744073709551615`: a real chunk can never be that long,
    // so an overflow here is a malformed header, not a valid range — refuse it cleanly instead of
    // panicking in debug / wrapping in release. Same for an end before the start.
    match end.checked_add(1).and_then(|e| e.checked_sub(have)) {
        Some(len) => Ok(Some(len)),
        None => Err(oci_err(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID", "declared range end is out of bounds")),
    }
}

pub(super) fn length_mismatch() -> Response {
    oci_err(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID", "declared range length does not match body length")
}
```

Replace `patch` entirely:

```rust
/// `PATCH` — one chunk. Ranges must be contiguous, per the spec: a gap is 416, and so is a chunk
/// that would rewrite bytes already received.
pub async fn patch(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
    body: Body,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if !valid_uuid(&uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    // Two PATCHes to the same session racing would both read the same `have`, both append to the
    // staging object from that offset, and last-writer-wins clobbers the other's bytes (the digest
    // check at PUT time catches it eventually, but as a confusing failure far from the cause).
    // Serialize the whole read-have -> append -> write sequence per session.
    let lock = app.store.keyed_lock(&format!("upload/{owner}/{name}/{uuid}"));
    let _guard = lock.lock().await;
    let have = match received(&app, &owner, &name, &uuid).await {
        Ok(Some(n)) => n,
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::registry::oci_internal(e),
    };
    let declared = match declared_chunk(&headers, &owner, &name, &uuid, have) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Checked against Content-Length BEFORE any byte moves, when the client declared one (every
    // real client does). A chunked body is checked after it lands — see below.
    if let (Some(d), Some(cl)) = (declared, content_length(&headers)) {
        if d != cl {
            return length_mismatch();
        }
    }
    // ponytail: the staging object is re-streamed behind each chunk, so a chunked push of an
    // N-byte layer moves O(N * chunks) bytes through the store — but never through memory.
    // Stateless, which is what lets a session survive the image moving nodes. Persist the
    // multipart id + part list in the staging object's sidecar if large chunked pushes get slow.
    let path = staging(&owner, &name, &uuid);
    let src = match staged(&app.store.os, &path).await {
        Ok(s) => s,
        Err(e) => return crate::registry::oci_internal(e),
    };
    let len = match pour(&app.store.os, &path, None, src.chain(body_stream(body))).await {
        Ok(len) => len,
        Err(Refused::TooLarge) => {
            return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large")
        }
        Err(Refused::WrongDigest) => unreachable!("no digest expected on a chunk"),
        Err(Refused::Failed(e)) => return crate::registry::oci_internal(e),
    };
    // A chunked body with a Content-Range that lied: the session has advanced by what really
    // arrived, and the 400 tells the client so. Its next GET/PATCH sees the true `Range` — that
    // is the resume protocol working, not a corrupted session.
    if declared.is_some_and(|d| d != len - have) {
        return length_mismatch();
    }
    accepted(&owner, &name, &uuid, len)
}
```

Replace `complete` entirely (keep its doc comment, dropping the `ponytail:` paragraph about re-reading to hash — it still re-reads, but now as a stream; rewrite that paragraph as):

```rust
/// `PUT /v2/{o}/{n}/blobs/uploads/{uuid}?digest=` — completes a session. A body here is the last
/// chunk, which is how the two-request push (no PATCH ever sent) arrives.
///
// ponytail: completion re-streams the staged object to hash it — one extra read per layer. The
// alternative is a resumable hasher (sha2 has no serializable state) or holding the hasher in
// node memory, which loses the session when the image moves nodes. Revisit if layer pushes show
// up in a profile.
pub async fn complete(
    app: &App,
    owner: &str,
    name: &str,
    uuid: &str,
    digest: &str,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    if !valid_uuid(uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    // Same session lock `patch` takes (identical key), held across the same read-have -> read
    // staging -> write sequence: a PATCH racing this PUT would otherwise interleave with the
    // append below, surfacing as a DIGEST_INVALID far from the real cause.
    let lock = app.store.keyed_lock(&format!("upload/{owner}/{name}/{uuid}"));
    let _guard = lock.lock().await;
    let have = match received(app, owner, name, uuid).await {
        Ok(Some(n)) => n,
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::registry::oci_internal(e),
    };
    // A PUT may carry the final chunk WITH a Content-Range. A start that is not where the
    // session left off is the out-of-order error, not a digest error — the client re-sends the
    // chunk on a 416 but restarts the whole upload on a 400, so conflating them is expensive.
    let declared = match declared_chunk(headers, owner, name, uuid, have) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if let (Some(d), Some(cl)) = (declared, content_length(headers)) {
        if d != cl {
            return length_mismatch();
        }
    }
    let src = match staged(&app.store.os, &staging(owner, name, uuid)).await {
        Ok(s) => s,
        Err(e) => return crate::registry::oci_internal(e),
    };
    // Hashed with the CLAIMED algorithm (`d.algo`), not assumed sha256, so a sha512 push is
    // checked as sha512. A mismatch aborts the upload before anything lands under the digest,
    // and the session stays open: a client that mis-stated the digest may retry the PUT.
    let len = match pour(&app.store.os, &blob_path(owner, &d), Some(&d), src.chain(body_stream(body))).await {
        Ok(len) => len,
        Err(Refused::TooLarge) => {
            return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large")
        }
        Err(Refused::WrongDigest) => {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest")
        }
        Err(Refused::Failed(e)) => return crate::registry::oci_internal(e),
    };
    // The blob has landed under a digest that matched — content-addressed, so a lying
    // Content-Range on a chunked body costs the client a 400 and a retry, never a wrong object.
    if declared.is_some_and(|d| d != len - have) {
        return length_mismatch();
    }
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::registry::oci_internal(e);
    }
    discard(app, owner, name, uuid).await;
    blobs::created(owner, name, &d)
}
```

In `src/registry/blobs.rs`: change the import line `body::Bytes,` to `body::Body,` and `use slatedb::object_store::{ObjectStoreExt, PutPayload};` to `use slatedb::object_store::ObjectStoreExt;`. Change `body: Bytes` to `body: Body` in `start_upload`, `finish_upload`, and `finish_blob`. Replace `finish_blob`'s body after the `Digest::parse` check with:

```rust
    // Verified against the algorithm the client CLAIMED (`d.algo`, from the digest it pushed
    // under), not assumed sha256. `pour` lands the object only after the hash matches, so a
    // corrupt layer never becomes readable under a name that promises different bytes.
    match super::uploads::pour(&app.store.os, &blob_path(owner, &d), Some(&d), super::uploads::body_stream(body)).await {
        Ok(_) => {}
        Err(super::uploads::Refused::TooLarge) => {
            return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large")
        }
        Err(super::uploads::Refused::WrongDigest) => {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest")
        }
        Err(super::uploads::Refused::Failed(e)) => return crate::registry::oci_internal(e),
    }
    // The image now exists, even with no manifest yet: a push that uploads layers and then fails
    // should leave something the owner can see and clean up. `touch_image`, never
    // `set_image_visibility` — a push must not flip a public image back to private.
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::registry::oci_internal(e);
    }
    created(owner, name, &d)
```

In `src/registry/routes.rs`, replace the comment above `let blob_routes` with:

```rust
    // Blob routes get their own body cap, `max_layer()`, not the git-sized `max_body()` from
    // `http.rs`: a layer push and a git push are different sizes of thing and must not share one
    // knob. The handlers take the raw `Body` (they stream), and axum's `DefaultBodyLimit` does
    // NOT apply to that extractor — so the cap that actually holds is `uploads::pour`'s own
    // count. This layer stays for the day a `Bytes` extractor sneaks back onto one of these
    // routes: it would then be capped at the right number instead of axum's 2 MB default.
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test registry_uploads && cargo test --test registry_blobs && cargo test --test registry_limits && cargo test --test registry_gc`
Expected: PASS, including `a_chunk_whose_declared_length_disagrees_with_its_body_is_refused` (the Content-Length pre-check), `a_completing_puts_content_range_end_must_match_its_body`, `a_content_range_end_at_u64_max_is_refused_cleanly`, `a_large_blob_streams_back_exact_bytes`. Then `cargo clippy --lib` — no new warnings in `uploads.rs`/`blobs.rs`. Then `./tests/registry_e2e.sh` if a docker daemon is available (exit 77 = skipped, not a pass).

- [ ] **Step 5: Commit**

```bash
git add src/registry/uploads.rs src/registry/blobs.rs src/registry/routes.rs tests/registry_limits.rs tests/registry_uploads.rs
git commit -m "Stream blob uploads through multipart writes instead of buffering a layer"
```

---

## MEDIUM

### Task 6: A dead worker lane takes the worker down; Redis-down lanes are inert

**Files:**
- Modify: `src/bin/worker.rs:98-107` (the `for t in tasks` loop) plus a `#[cfg(test)]` module
- Test: `src/bin/worker.rs` (unit), `tests/registry_gc.rs` (Redis-down)

**Interfaces:**
- Produces: `async fn first_exit(tasks: Vec<tokio::task::JoinHandle<()>>) -> String` in `worker.rs` — resolves when ANY task finishes, with a one-line reason.

**Context:** The comment says "a lane that dies takes the worker with it", but the handles are awaited in order: lane 3 panicking while lane 0 runs forever is never observed. `select_all` resolves on the first completion; since every lane loops forever, any completion is a death, and `run` returning `Err` makes `main` exit 2 so Kubernetes restarts the pod at full capacity.

- [ ] **Step 1: Write the failing tests**

Add to `src/bin/worker.rs` (a second `#[cfg(test)]` module, after `targets_whole_repo_tests`):

```rust
#[cfg(test)]
mod first_exit_tests {
    use super::first_exit;

    /// The point of the worker's supervision: a panic in ANY lane is noticed while the others
    /// are still running — not after they finish, which for a lane is never.
    #[tokio::test]
    async fn a_panicking_lane_is_noticed_while_another_still_runs() {
        let forever = tokio::spawn(async { std::future::pending::<()>().await });
        let dies = tokio::spawn(async { panic!("lane died") });
        let reason = tokio::time::timeout(std::time::Duration::from_secs(2), first_exit(vec![forever, dies]))
            .await
            .expect("must resolve while the other lane is still running");
        assert!(reason.contains("lane 1"), "got {reason}");
    }
}
```

Append to `tests/registry_gc.rs`:

```rust
/// CLAUDE.md calls the Redis-down fallback load-bearing: with Redis unreachable, every stream call
/// the worker's lanes make must be inert (empty, no panic, no hang), and the GC lane — which
/// touches only the object store — must keep sweeping. `redis://127.0.0.1:1` is a port nothing
/// listens on; `Cache::connect` gives up on it in 250ms.
#[tokio::test]
async fn worker_lanes_are_inert_and_gc_still_sweeps_with_redis_down() {
    let cache = kloudlite::cache::Cache::connect(Some("redis://127.0.0.1:1")).await;
    assert!(!cache.connected());
    cache.xgroup_create_mkstream("events", "merge-worker").await;
    assert!(cache.xreadgroup("events", "merge-worker", "t/0", 16).await.is_empty());
    assert!(cache.xautoclaim("events", "merge-worker", "t/0", 30_000, 16).await.is_empty());
    cache.xack("events", "merge-worker", "0-0").await;

    let e = common::env().await;
    assert!(!e.store.cache.connected());
    let orphan = b"nothing points at me".to_vec();
    let od = Digest::of(&orphan);
    e.store.os.put(&blob_path("acme", &od), PutPayload::from(orphan)).await.unwrap();
    assert_eq!(gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap(), 1);
    assert_eq!(e.store.sweep_stale_uploads("acme", Duration::ZERO).await.unwrap(), 0);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin worker a_panicking_lane` — Expected: FAIL to compile (`first_exit` not found).
Run: `cargo test --test registry_gc worker_lanes_are_inert` — Expected: PASS already (it pins existing fallback behaviour; keep it).

- [ ] **Step 3: Implement `first_exit`**

In `src/bin/worker.rs`, replace the tail of `run` (from the `// A lane that dies takes the worker with it` comment through `Ok(())`) with:

```rust
    // Every lane loops forever, so the FIRST one to finish — panic or return — is a dead lane.
    // Awaiting the handles in order would only notice lane N after lanes 0..N had finished,
    // which is never; this resolves on any of them, and the `Err` exits the process so the pod
    // restarts at full capacity instead of quietly running short.
    Err(kloudlite::err(first_exit(tasks).await))
}

async fn first_exit(tasks: Vec<tokio::task::JoinHandle<()>>) -> String {
    let (result, index, _rest) = futures::future::select_all(tasks).await;
    match result {
        Ok(()) => format!("worker lane {index} returned"),
        Err(e) => format!("worker lane {index} died: {e}"),
    }
}
```

`futures` is already a dependency (`Cargo.toml:42`). `run`'s signature (`-> Result<()>`) is unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin worker && cargo test --test registry_gc`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/bin/worker.rs tests/registry_gc.rs
git commit -m "Exit the worker when any lane dies"
```

---

### Task 7: Manifest DELETE on a missing image must not conjure the image

**Files:**
- Modify: `src/registry/manifests.rs` (`delete_manifest`, directly after `reference(...)`)
- Test: `tests/registry_manifests.rs`

**Interfaces:**
- Consumes: `Store::image_exists(owner, name) -> Result<bool>`.

**Context:** `image_db` CREATES a database. `delete_manifest` by digest calls `referrers::unindex` and `image_db` on an image that may not exist, so `DELETE /v2/acme/ghost/manifests/sha256:...` leaves a phantom `acme/ghost` (and the worker's reconcile then gives it a private marker that shows up in listings). The upload handlers had the same bug and lost it with Task 1; `tags_list` already guards. Guard here too, and pin `tags/list` → `NAME_UNKNOWN` while in the file.

- [ ] **Step 1: Write the failing tests**

Append to `tests/registry_manifests.rs`:

```rust
/// Opening an image's database creates it. A DELETE aimed at an image that was never pushed must
/// 404 without leaving a phantom image behind for the listing to find.
#[tokio::test]
async fn deleting_a_manifest_of_a_missing_image_creates_nothing() {
    let (base, e, c, token, _m, d) = pushed().await;
    let r = c.delete(format!("{base}/v2/acme/ghost/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_UNKNOWN");
    assert!(!e.store.pool.exists("img", "acme/ghost").await.unwrap(), "a DELETE must not create the image");
}

/// Spec: `tags/list` for a repository that does not exist is `NAME_UNKNOWN`, not an empty list.
#[tokio::test]
async fn tags_list_of_a_missing_image_is_name_unknown() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let r = c.get(format!("{base}/v2/acme/ghost/tags/list"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "NAME_UNKNOWN");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test registry_manifests deleting_a_manifest_of_a_missing_image tags_list_of_a_missing_image`
Expected: the first FAILS on the `pool.exists` assertion (the 404 is already right, the phantom is not); the second PASSES (pin).

- [ ] **Step 3: Guard with `image_exists`**

In `delete_manifest`, directly after the `let Some(r) = reference(&reference_str) else { ... };` block:

```rust
    // `image_db` creates what it opens; a delete aimed at nothing must not leave a phantom image
    // for the listing (and the worker's reconcile) to find.
    match app.store.image_exists(&owner, &name).await {
        Ok(true) => {}
        Ok(false) => return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest"),
        Err(e) => return crate::registry::oci_internal(e),
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test registry_manifests && cargo test --test registry_http`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/registry/manifests.rs tests/registry_manifests.rs
git commit -m "Guard manifest delete with image_exists so it cannot create the image"
```

---

### Task 8: GC — write new markers in place, and skip the sweep when no blob is old enough

**Files:**
- Modify: `src/registry/gc.rs` (`reconcile_owner` case (a), `reconcile_repo_owner` case (a), `sweep_owner`)
- Test: `tests/registry_gc.rs`, `tests/registry_uploads.rs` (existing reconcile tests)

**Interfaces:**
- Consumes: `index::put_in_place(os, kind, owner, &Marker)`.

**Context:** Case (a) of both reconciles creates a missing marker with `index::write`, which deletes the OTHER visibility's path first — racing a concurrent `set_image_visibility` on the owning node exactly the way case (c)'s comment explains it must not. `put_in_place` is the fix case (c) already uses. Separately, `sweep_owner` reads every manifest of every owner twice per pass, forever, even when no blob could possibly be deleted; a listing is far cheaper than reading manifests, and a blob younger than `grace` is never deleted whatever the manifests say, so deciding "nothing old enough" from a listing alone is safe. The manifest-then-list order for the actual sweep stays exactly as documented.

- [ ] **Step 1: Write the failing test**

Append to `tests/registry_gc.rs`:

```rust
/// When no blob is older than `grace` nothing can be deleted, so the manifests are not read at
/// all. Observable through the keep-biased rule: an unparseable manifest aborts a sweep that
/// reads it — and must NOT abort one that had no reason to.
#[tokio::test]
async fn a_sweep_with_nothing_old_enough_reads_no_manifests() {
    let e = common::env().await;
    let fresh = b"just uploaded".to_vec();
    e.store.os.put(&blob_path("acme", &Digest::of(&fresh)), PutPayload::from(fresh)).await.unwrap();
    let garbage = b"not json at all".to_vec();
    e.store.os
        .put(&kloudlite::registry::store::manifest_path("acme", "broken", &Digest::of(&garbage)), PutPayload::from(garbage))
        .await.unwrap();

    let n = gc::sweep_owner(&e.store, "acme", Duration::from_secs(3600)).await.unwrap();
    assert_eq!(n, 0);
    // And with everything old enough, the same manifest aborts the sweep as before.
    assert!(gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.is_err());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test registry_gc a_sweep_with_nothing_old_enough`
Expected: FAIL — the first `sweep_owner` returns `Err` (it read the garbage manifest).

- [ ] **Step 3: Implement both changes**

In `sweep_owner`, insert at the top (before `let keep = referenced(...)`):

```rust
    let prefix = slatedb::object_store::path::Path::from(format!("blobs/{owner}"));
    let cutoff = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now() - grace);
    // Nothing past grace means nothing deletable whatever the manifests say, so do not read
    // them. A listing is cheap; reading every manifest of every owner twice a minute, forever,
    // on an idle registry is not. Listing BEFORE the manifests is safe here only because this
    // pass decides whether to sweep, never what to delete — the sweep below keeps the
    // manifests-first order the module doc calls load-bearing.
    let mut listing = store.os.list(Some(&prefix));
    let mut any_old = false;
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        if m?.last_modified <= cutoff {
            any_old = true;
            break;
        }
    }
    if !any_old {
        return Ok(0);
    }
```

Then delete the now-duplicate `let prefix = ...;` and `let cutoff = std::time::SystemTime::now() - grace;` lines that followed `let keep`, and change the existing age check to use the new `cutoff` directly:

```rust
        if m.last_modified > cutoff {
            continue;
        }
```

In `reconcile_owner` case (a), change `if index::write(&store.os, Kind::Img, owner, &m).await.is_ok() {` to `if index::put_in_place(&store.os, Kind::Img, owner, &m).await.is_ok() {` and put this comment above the `for name in image_set...` loop:

```rust
    // `put_in_place`, not `index::write`: write deletes the other visibility's path first, and
    // this worker shares no lock with a visibility flip landing on the owning node at the same
    // moment — same reasoning as case (c) below.
```

Make the identical change in `reconcile_repo_owner` case (a) (`Kind::Repo`), with the same comment.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test registry_gc && cargo test --test registry_uploads`
Expected: PASS (`an_unmarked_image_gains_a_private_marker`, `an_unmarked_repo_gains_a_private_marker` still pass — a missing marker has no other side to delete, so `put_in_place` produces the same result outside the race).

- [ ] **Step 5: Commit**

```bash
git add src/registry/gc.rs tests/registry_gc.rs
git commit -m "Skip idle sweeps and create missing markers in place"
```

---

### Task 9: Reconcile markers for every image owner, not just owners with blobs

**Files:**
- Modify: `src/bin/worker.rs` (`blob_owners` → `image_owners`, `gc_lane`)
- Test: `src/bin/worker.rs` unit test

**Interfaces:**
- Produces: `async fn image_owners(store: &Store) -> BTreeSet<String>` — union of the top-level names under `blobs/`, `manifests/`, `repo/img/`.

**Context:** `gc_lane` drives both `sweep_owner` and `reconcile_owner` off `blob_owners` (`blobs/` only). An owner whose images have manifests but whose blobs were all deleted, or whose image directory exists with no blobs yet, never gets its markers reconciled. Union the three prefixes. `sweep_owner` on an owner with no blobs is a no-op listing (and after Task 8, returns before reading manifests).

- [ ] **Step 1: Write the failing test**

Add to `src/bin/worker.rs`, a third `#[cfg(test)]` module:

```rust
#[cfg(test)]
mod image_owners_tests {
    use super::image_owners;
    use slatedb::object_store::{memory::InMemory, path::Path as OsPath, ObjectStoreExt, PutPayload};
    use std::sync::Arc;

    /// An owner is anyone with anything under ANY of the image prefixes: blobs-only (mid-push),
    /// manifests-only (blobs deleted), or a bare image directory (DB created, nothing pushed).
    #[tokio::test]
    async fn owners_are_the_union_of_blobs_manifests_and_image_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = kloudlite::store::Store::open(Arc::new(InMemory::new()), tmp.path().join("cache"), false)
            .await
            .unwrap();
        for p in ["blobs/alpha/sha256/aa", "manifests/beta/nginx/sha256/bb", "repo/img/gamma/nginx/manifest/0.sst"] {
            store.os.put(&OsPath::from(p), PutPayload::from("x")).await.unwrap();
        }
        let owners: Vec<String> = image_owners(&store).await.into_iter().collect();
        assert_eq!(owners, vec!["alpha", "beta", "gamma"]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin worker owners_are_the_union`
Expected: FAIL to compile (`image_owners` not found).

- [ ] **Step 3: Implement**

Replace `blob_owners` (and its doc) in `src/bin/worker.rs` with:

```rust
/// Every owner with anything under any image prefix. `blobs/` alone misses an owner whose layers
/// were all deleted but whose manifests remain, and one whose image database exists with nothing
/// pushed yet — both still need their listing markers reconciled. A prefix that fails to list is
/// logged and skipped: the others still get their turn.
async fn image_owners(store: &kloudlite::store::Store) -> std::collections::BTreeSet<String> {
    let mut owners = std::collections::BTreeSet::new();
    for prefix in ["blobs/", "manifests/", "repo/img/"] {
        match kloudlite::registry::list_dir_names(&store.os, prefix).await {
            Ok(o) => owners.extend(o),
            Err(e) => eprintln!("gc: listing {prefix}: {e}"), // ponytail: eprintln
        }
    }
    owners
}
```

In `gc_lane`, replace the `let owners = match blob_owners(store).await { ... };` block with:

```rust
        let owners = image_owners(store).await;
```

The `for owner in &owners` loop and the `owners.is_empty()` check work unchanged on a `BTreeSet`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin worker`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/bin/worker.rs
git commit -m "Reconcile markers for every image owner, not only owners with blobs"
```

---

## LOW

### Task 10: Basic auth checks the username and accepts any scheme case

**Files:**
- Modify: `src/registry/auth.rs:45-62` (`caller`)
- Test: `tests/registry_blobs.rs`

**Interfaces:**
- Consumes: `Store::owner_for_token(token) -> Result<Option<String>>`.
- Produces: `fn scheme<'a>(v: &'a str, name: &str) -> Option<&'a str>` (private) — the credential after a case-insensitive scheme prefix.

**Context:** `Basic` decoding throws the username away, so `bob:<acme's token>` authenticates as acme — harmless today (the token IS the secret) but it means the username on the wire is a lie nothing checks, and a leaked token is usable under any name. RFC 7235 says the scheme is case-insensitive; `strip_prefix("Basic ")` is not. While here, pin the anonymous-HEAD and unauthenticated-DELETE behaviours the review found untested.

- [ ] **Step 1: Write the failing tests**

Append to `tests/registry_blobs.rs`:

```rust
/// The Basic username must be the owner the token belongs to. The token is the secret, but a
/// credential whose two halves disagree is a credential that did not verify — a refusal.
#[tokio::test]
async fn basic_auth_with_the_wrong_username_is_refused() {
    let (base, _e, c, token) = authed().await;
    let r = c.get(format!("{base}/v2/")).basic_auth("bob", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = c.get(format!("{base}/v2/")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

/// RFC 7235: the auth scheme is case-insensitive.
#[tokio::test]
async fn a_lowercase_basic_scheme_is_accepted() {
    use base64::Engine;
    let (base, _e, c, token) = authed().await;
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("acme:{token}"));
    let r = c.get(format!("{base}/v2/")).header("authorization", format!("basic {cred}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

/// docker probes with HEAD before every pull; a public image must answer one anonymously.
#[tokio::test]
async fn an_anonymous_head_of_a_public_blob_is_200() {
    let (base, e, c, token) = authed().await;
    let body = b"public layer".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();
    let r = c.head(format!("{base}/v2/acme/nginx/blobs/{d}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("content-length").unwrap().to_str().unwrap(), body.len().to_string());
}

/// Deleting is a write: anonymous gets the challenge, a stranger gets denied, and the blob stays.
#[tokio::test]
async fn deleting_a_blob_requires_the_owner() {
    let (base, e, c, token) = authed().await;
    let body = b"keep me".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body).send().await.unwrap();
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();

    let r = c.delete(format!("{base}/v2/acme/nginx/blobs/{d}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let other = e.store.create_token("other").await.unwrap();
    let r = c.delete(format!("{base}/v2/acme/nginx/blobs/{d}")).basic_auth("other", Some(&other)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = c.head(format!("{base}/v2/acme/nginx/blobs/{d}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "the blob must survive both refusals");
}
```

`base64` is a dependency of the crate; if the test binary cannot see it, add it under `[dev-dependencies]` in `Cargo.toml` with the same version as the main dependency.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test registry_blobs basic_auth_with_the_wrong_username a_lowercase_basic_scheme an_anonymous_head deleting_a_blob_requires`
Expected: the first two FAIL; the last two PASS (pins).

- [ ] **Step 3: Implement**

In `src/registry/auth.rs`, add above `caller`:

```rust
/// The credential after an auth scheme, matched case-insensitively: RFC 7235 says `basic` and
/// `Basic` are the same scheme, and some proxies lowercase it.
fn scheme<'a>(v: &'a str, name: &str) -> Option<&'a str> {
    let (head, rest) = v.split_at_checked(name.len())?;
    (head.eq_ignore_ascii_case(name) && rest.starts_with(' ')).then(|| rest.trim_start())
}
```

Replace the two `strip_prefix` branches in `caller` with:

```rust
    if let Some(b64) = scheme(v, "Basic") {
        let cred = base64::engine::general_purpose::STANDARD
            .decode(b64).ok()
            .and_then(|d| String::from_utf8(d).ok())
            .and_then(|s| s.split_once(':').map(|(u, p)| (u.to_string(), p.to_string())));
        let Some((user, token)) = cred else { return Err(challenge(None)) };
        // The token is the secret, but the username must be the owner it belongs to: a credential
        // whose halves disagree did not verify, and a leaked token must not work under any name.
        return match app.store.owner_for_token(&token).await {
            Ok(Some(o)) if o == user => Ok(Some(o)),
            Ok(_) => Err(challenge(None)),
            Err(e) => Err(crate::registry::oci_internal(e)),
        };
    }
    if let Some(jwt) = scheme(v, "Bearer") {
```

(`str::split_at_checked` is stable since Rust 1.80; if the toolchain is older, use `if v.len() < name.len() { return None }` then `split_at`.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test registry_blobs && cargo test --test registry_http && cargo test --test registry_manifests`
Expected: PASS — every existing test already uses the owner as the Basic username.

- [ ] **Step 5: Commit**

```bash
git add src/registry/auth.rs tests/registry_blobs.rs
git commit -m "Check the Basic username and accept any auth scheme case"
```

---

### Task 11: Referrers omit `artifactType` when there is none

**Files:**
- Modify: `src/registry/referrers.rs:37-50` (`index`)
- Test: `tests/registry_manifests.rs`

**Context:** The index entry is built with `serde_json::json!`, which turns a `None` into `"artifactType": null`. The spec says the field is omitted when absent; `null` is what a strict client rejects. Pin `unindex` while in the file: deleting a referrer by digest must drop it from the list.

- [ ] **Step 1: Write the failing tests**

Append to `tests/registry_manifests.rs`:

```rust
/// Spec: `artifactType` is omitted from a referrers entry when the manifest has neither it nor a
/// config media type — never emitted as `null`.
#[tokio::test]
async fn a_referrer_without_an_artifact_type_omits_the_field() {
    let (base, _e, c, token, m, subject) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    let sig = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "layers": [],
        "subject": {"mediaType": MEDIA, "digest": subject.to_string(), "size": 1}
    }).to_string().into_bytes();
    let sig_d = Digest::of(&sig);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{sig_d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(sig).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{subject}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    let entry = &b["manifests"][0];
    assert_eq!(entry["digest"], sig_d.to_string());
    assert!(entry.get("artifactType").is_none(), "got {entry}");

    // And deleting the referrer by digest unindexes it.
    let r = c.delete(format!("{base}/v2/acme/nginx/manifests/{sig_d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{subject}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["manifests"], serde_json::json!([]));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test registry_manifests a_referrer_without_an_artifact_type`
Expected: FAIL on the `artifactType` assertion (`null` is present).

- [ ] **Step 3: Implement**

In `referrers::index`, replace the `let entry = serde_json::json!({ ... });` with:

```rust
    let mut entry = serde_json::json!({
        "mediaType": v.get("mediaType").and_then(|m| m.as_str())
            .unwrap_or("application/vnd.oci.image.manifest.v1+json"),
        "digest": d.to_string(),
        "size": bytes.len(),
        "annotations": v.get("annotations").cloned().unwrap_or(serde_json::json!({})),
    });
    // Spec: omitted when absent — `json!` would emit `null`, which strict clients reject.
    let artifact_type = v.get("artifactType").and_then(|a| a.as_str())
        .or_else(|| v.get("config").and_then(|c| c.get("mediaType")).and_then(|m| m.as_str()));
    if let Some(t) = artifact_type {
        entry["artifactType"] = serde_json::Value::String(t.to_string());
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test registry_manifests`
Expected: PASS (`a_manifest_with_a_subject_is_listed_as_its_referrer` still sees its `artifactType`, and the `?artifactType=` filter still works).

- [ ] **Step 5: Commit**

```bash
git add src/registry/referrers.rs tests/registry_manifests.rs
git commit -m "Omit artifactType from a referrers entry when the manifest has none"
```

---

## CLEANUPS

### Task 12: Catalog pagination pin, stale comments, a wrong assertion message, a tautological test

**Files:**
- Modify: `src/registry/store.rs:279-290` (move the `ponytail:` comment above the doc comment)
- Modify: `src/registry/blobs.rs:29-38` (delete `max_layer_tests`)
- Modify: `src/events.rs:1-6` (module doc)
- Modify: `tests/registry_store.rs:12`
- Test: `tests/registry_manifests.rs` (catalog pagination)

**Context:** Four things that are wrong but cheap. `store.rs:286` has a `// ponytail:` block wedged between a `///` doc comment and its `fn`, splitting the doc. `max_layer_is_stable` asserts `x == x` and also trips clippy's `items_after_test_module`. `events.rs` says consumers "fall back to scanning Mongo" — there is no Mongo; the fallback is the owning node's own periodic sweep. `registry_store.rs:12` labels a 64-hex sha512 as "unsupported algorithm" when the parser rejects it for LENGTH (sha512 is supported). And `_catalog` pagination has no test.

- [ ] **Step 1: Write the catalog pagination test**

Append to `tests/registry_manifests.rs`:

```rust
/// `_catalog` pages exactly like `tags/list`: `n` caps, `last` is exclusive, a truncated page
/// carries `Link`. `n=0` is an empty page — nothing to continue from, so no `Link` either.
#[tokio::test]
async fn the_catalog_paginates() {
    let (base, _e, c, token, m, _d) = pushed().await;
    for image in ["api", "nginx", "web"] {
        c.put(format!("{base}/v2/acme/{image}/manifests/latest"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(m.clone()).send().await.unwrap();
    }
    let r = c.get(format!("{base}/v2/_catalog?n=2")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert!(r.headers().get("link").unwrap().to_str().unwrap().contains("last=acme/nginx"));
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["repositories"], serde_json::json!(["acme/api", "acme/nginx"]));

    let r = c.get(format!("{base}/v2/_catalog?n=2&last=acme/nginx")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert!(r.headers().get("link").is_none());
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["repositories"], serde_json::json!(["acme/web"]));

    let r = c.get(format!("{base}/v2/_catalog?n=0")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert!(r.headers().get("link").is_none());
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["repositories"], serde_json::json!([]));
}
```

Run: `cargo test --test registry_manifests the_catalog_paginates` — Expected: PASS (a pin; `paginate` already behaves this way). If it fails, `paginate` in `src/registry/mod.rs` is the bug, not the test.

- [ ] **Step 2: Fix the assertion message**

In `tests/registry_store.rs`, change line 12 to:

```rust
    assert!(Digest::parse(&format!("sha512:{hex}")).is_none(), "sha512 with a sha256-length hex");
```

And add directly below it:

```rust
    assert!(Digest::parse("md5:d41d8cd98f00b204e9800998ecf8427e").is_none(), "unsupported algorithm");
```

- [ ] **Step 3: Move the misplaced ponytail comment**

In `src/registry/store.rs`, cut the four-line `// ponytail: a push or page-load racing this delete ...` block (currently between the `///` doc and `pub async fn delete_image_rows`) and paste it ABOVE the doc comment's first `///` line, as `//` comments, then restore the `    ` indentation on `pub async fn delete_image_rows`. Result shape:

```rust
    // ponytail: a push or page-load racing this delete can re-open the database between the
    // evict and the file removal, leaving a db whose manifest names SSTs that are gone — a
    // broken image rather than a deleted one. The window is one node and milliseconds wide;
    // a delete-in-progress marker in the image db closes it if it ever bites.
    /// Wipes every database row this image owns: the bare `image` marker, `image/public`, every
    ...
    pub async fn delete_image_rows(&self, owner: &str, name: &str) -> Result<()> {
```

- [ ] **Step 4: Delete the tautological test module**

In `src/registry/blobs.rs`, delete the whole `#[cfg(test)] mod max_layer_tests { ... }` block (lines 29-38). `max_layer`'s own doc already says it is read once.

- [ ] **Step 5: Fix the events doc**

In `src/events.rs`, replace lines 1-6 with:

```rust
//! A nudge, never the record. Publishing to `events` tells the merge worker "something changed,
//! go look" — it never carries the authoritative state of what changed. Redis can drop the
//! stream, evict it, or simply be absent (`Cache::connect(None)`), and every consumer must keep
//! working: the worker's nudges are a speed-up over the owning node's own periodic lanes
//! (`App::check_owned_pulls`, `App::merge_owned_pulls`), and the activity feed falls back to
//! `pulls_across`. `publish` is fire-and-forget for exactly this reason — a failed XADD costs a
//! consumer one sweep interval, never a lost event.
```

- [ ] **Step 6: Run everything**

Run: `cargo test` and `cargo clippy --lib`
Expected: PASS; no new clippy warnings in `blobs.rs`, `store.rs`, `events.rs` (the `items_after_test_module` warning on `blobs.rs` is gone).

- [ ] **Step 7: Commit**

```bash
git add src/registry/store.rs src/registry/blobs.rs src/events.rs tests/registry_store.rs tests/registry_manifests.rs
git commit -m "Pin catalog pagination and fix stale comments and a wrong assertion"
```

---

### Task 13: `?tag=` on a by-digest push

**Files:**
- Test: `tests/registry_manifests.rs`

**Context:** The handler already honours `?tag=` (repeated) on a by-digest PUT and refuses a malformed one with `TAG_INVALID` — with no test. Pin it; no source change expected.

- [ ] **Step 1: Write the test**

Append to `tests/registry_manifests.rs`:

```rust
/// A push by digest may name tags as `?tag=` (repeatable). Each valid one resolves; a malformed
/// one fails the whole request — a 201 that silently dropped a tag would be a lie.
#[tokio::test]
async fn a_by_digest_push_can_name_its_tags() {
    let (base, _e, c, token, m, d) = pushed().await;
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{d}?tag=v1&tag=stable"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());
    for t in ["v1", "stable"] {
        let r = c.get(format!("{base}/v2/acme/nginx/manifests/{t}"))
            .basic_auth("acme", Some(&token)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "tag {t}");
        assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d.to_string());
    }

    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{d}?tag=-bad"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "TAG_INVALID");
}
```

- [ ] **Step 2: Run it**

Run: `cargo test --test registry_manifests a_by_digest_push_can_name_its_tags`
Expected: PASS. If it fails, the handler's `?tag=` loop in `put_manifest` (the `form_urlencoded::parse` block) is what to read — fix there, not in the test.

- [ ] **Step 3: Commit**

```bash
git add tests/registry_manifests.rs
git commit -m "Pin tag parameters on a by-digest manifest push"
```

---

## Self-review

**Spec coverage** (registry-scoped findings in `docs/code-review-2026-08-23.md`):

| Finding | Task |
|---|---|
| #1 Critical worker opens image DB (`uploads.rs:395`, `worker.rs:283`) | 1 |
| #6 High manifest PUT never verifies blobs | 3 |
| #7 High `refresh_blob_mtime` copy-to-self | 3 (deleted) |
| #8 High blob bodies buffered to `max_layer` | 5 |
| Medium `worker.rs:102-107` panicked lane not observed | 6 |
| Medium `manifests.rs:99` non-JSON accepted | 4 |
| Medium `uploads.rs:374-401` never-PATCHed rows leak | 1 |
| Medium `uploads.rs:134,221,260` phantom image via `image_db` | 1 (no DB access left) |
| Medium `manifests.rs:281` phantom image on DELETE | 7 |
| Medium `gc.rs:151-167` case (a) `index::write` race | 8 |
| Perf `gc.rs:36-73` manifests read twice per pass when idle | 8 |
| Perf `manifests.rs:83-91` sha512 probe | 4 |
| Low `auth.rs:45` username ignored, scheme case | 10 |
| Low `referrers.rs:47` `artifactType: null` | 11 |
| Low `blobs.rs:29` `max_layer_is_stable` | 12 |
| Low `events.rs:1` Mongo doc | 12 |
| Low `store.rs:46` `Digest::of` via `of_algo` | 2 |
| Low `store.rs:286` ponytail placement | 12 |
| Low `tests/registry_store.rs:12` message | 12 |
| Low `worker.rs:261` owners union | 9 |
| Redundancy: Content-Range + staging-append duplicated | 5 (`declared_chunk`, `pour`) |
| Redundancy: hex encoder ×4 (`uploads.rs:46`, `store.rs:48,59`; `gpg.rs` deferred) | 2 |
| Tests: PATCH without Content-Range | 5 |
| Tests: `?tag=` on by-digest push | 13 |
| Tests: unindex | 11 |
| Tests: `_catalog` pagination / `n=0` | 12 |
| Tests: `tags/list` missing image → NAME_UNKNOWN | 7 |
| Tests: 413 oversized blob / manifest | 5 |
| Tests: anon HEAD public blob | 10 |
| Tests: DELETE blob unauth | 10 |
| Tests: sha512 digest-then-tag | 4 |
| Tests: worker lanes with Redis down | 6 |
| Tests: sleeps in `registry_blobs.rs:274,288` | 3 — test deleted with the mechanism; `MockSystemClock` cannot drive object-store mtimes |

**Type consistency:** `Hasher::{new, update, finish}` (Task 2) is what `pour` (Task 5) calls; `pour`/`staged`/`body_stream`/`Refused` names match between `uploads.rs` and `blobs.rs` in Task 5; `received` returns `crate::Result<Option<u64>>` in Task 1 and is consumed unchanged in Task 5; `seed_blobs(&e, owner, &[&[u8]])` (Task 3) is used by Tasks 3, 4, 7, 11, 12, 13 via `pushed()`; `first_exit`/`image_owners` (Tasks 6, 9) are `worker.rs`-private and tested in place; `gc::collect` is `pub(crate)` from Task 3 on.

**Ordering:** Task 2 (LOW) sits second because Task 5 needs `Hasher`; Task 1 does not, so the critical fix still lands first. Tasks 6–13 are independent of each other and of 3–5, except Task 4 edits the `put_manifest` lines Task 3 introduces.
