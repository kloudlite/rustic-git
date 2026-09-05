# Security Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the correctness, security, and DoS findings from the 2026-08-21 whole-codebase review, ordered highest-severity first.

**Architecture:** Each task is an independent fix landing behind its own test and commit. Tasks are grouped High → Medium → Low, but have no cross-dependencies except where an `Interfaces` block says so (only Task 13's leader mutex is shared by Tasks 13–14). Fixes follow existing patterns in the file they touch — copy the sibling, don't invent.

**Tech Stack:** Rust, axum, tokio, SlateDB, gix, reqwest, redis.

**Spec:** This plan IS the spec — it encodes the four review reports of 2026-08-21. No separate design doc.

## Global Constraints

- `cargo test` must pass after every task. Run `cargo test` (full suite) or the named `--test` file the task points at.
- Clippy bar (from `CLAUDE.md`): no NEW warnings in files you touch. `--all-targets -D warnings` has ~13 pre-existing errors — ignore those, don't fix them here.
- House style (from `CLAUDE.md`): comments explain WHY not what; match `src/http.rs` density. Deliberate shortcuts keep/gain a `// ponytail: <ceiling and upgrade path>` marker. Commit subjects: imperative sentence case, NO tool attribution, no "claude" reference.
- Manifest/blob/digest invariants are unchanged by this work — do not touch `Digest::parse`, verbatim-manifest storage, or the blob-deletion rule.
- One SlateDB DB per repo, one node open at a time — Tasks 13–15 defend this invariant; do not weaken it.

---

## HIGH SEVERITY

### Task 1: Authenticate `api_protections` (unauthenticated private-repo read)

**Files:**
- Modify: `src/http/browse_api.rs:645-658` (`api_protections`)
- Test: `src/http/browse_api.rs` (inline `#[cfg(test)]` module, or the crate's existing browse_api integration test — grep for `api_protect` tests first and colocate)

**Interfaces:**
- Consumes: `open_ro(app, trusted, headers, owner, name) -> Result<Repo, Response>` (browse_api.rs:35), `Trusted` extension, `HeaderMap`.
- Produces: nothing others consume.

**Context:** `api_protections` currently parses the path and calls `app.store.protections(...)` with no auth and no existence check. Its sibling writer `api_protect` (line 604) already takes `repo_exists` + a caller check; `api_compare` (line 662) uses the full `open_ro` gate. `protect` is in `BROWSE_TAILS` (http.rs:183) and the api tier's anonymous GET fallback (api.rs:154 `.fallback(get(handle))`) forwards it upstream with the peer secret — so the leak is anonymously reachable. Fix = gate it with `open_ro` exactly like `api_compare`.

- [ ] **Step 1: Write the failing test**

Add to the browse_api test module. Assert an anonymous GET of a private repo's protections is NOT 200 (it should be 404 via `hidden()`), and an authorized owner still gets 200. If the existing test harness spins a router, model it on the nearest `api_compare`/`api_protect` test. Minimal shape:

```rust
#[tokio::test]
async fn protections_require_visibility() {
    let app = test_app().await; // reuse whatever helper the sibling tests use
    create_private_repo(&app, "alice", "secret").await;
    // anonymous: no bearer, no peer identity
    let resp = get(&app, "/api/alice/secret/protect", &[]).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // owner authorized
    let resp = get(&app, "/api/alice/secret/protect", &owner_auth("alice")).await;
    assert_eq!(resp.status(), StatusCode::OK);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib protections_require_visibility`
Expected: FAIL — anonymous currently returns 200 with the JSON list.

- [ ] **Step 3: Add the `open_ro` gate**

Change the signature to take the same extractors as `api_compare` and gate before reading:

```rust
async fn api_protections(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    // A repo's protection rules are as private as the repo. Gate exactly like `api_compare`:
    // 404 for a caller who may not see it, 401 to prompt for a token.
    let _repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    match app.store.protections(&owner, &name).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib protections_require_visibility`
Expected: PASS. Also run `cargo test --lib` to confirm no route-wiring test broke.

- [ ] **Step 5: Commit**

```bash
git add src/http/browse_api.rs
git commit -m "Gate branch-protection reads behind repo visibility"
```

---

### Task 2: Stop the anonymous-forwardable cache from marking gated tails public

**Files:**
- Modify: `src/api.rs` (the `handle` fallback path, ~lines 154 + 570-595 — grep `fn handle` and `TTL_IMMUTABLE`)
- Test: `src/api.rs` inline tests

**Context:** The review flagged a compounding effect: an anonymous 200 through `handle` writes `META=1` (public) into the 30s visibility cache and caches the body immutable. After Task 1, `protect` no longer returns 200 anonymously, which removes the poisoning vector for that tail. But `compare` (a mutable answer) is still cached with `TTL_IMMUTABLE` (7 days). Fix the TTL classification so only oid-keyed tails are immutable.

**Interfaces:**
- Consumes: `Parsed { repo, suffix, path }` (api.rs:159).
- Produces: nothing.

- [ ] **Step 1: Find the TTL decision**

Run: `grep -n "TTL_IMMUTABLE\|TTL_\|immutable\|max-age" src/api.rs`
Read the block that picks the TTL from the suffix (around 585-595). Current rule: suffix not starting with `refs` → `TTL_IMMUTABLE`.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn only_oid_keyed_tails_are_immutable() {
    // branch-resolving reads change on every push — never immutable
    assert!(!is_immutable_suffix("compare:base=main:head=dev"));
    assert!(!is_immutable_suffix("protect"));
    assert!(!is_immutable_suffix("refs"));
    // an object addressed by oid is content-addressed — safe to pin
    assert!(is_immutable_suffix("blob:3a5f...:README.md"));
    assert!(is_immutable_suffix("tree:9c1e..."));
}
```

If the TTL logic is inline (not a named fn), extract it into `fn is_immutable_suffix(suffix: &str) -> bool` first so it's testable.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib only_oid_keyed_tails_are_immutable`
Expected: FAIL — `compare`/`protect` currently classify as immutable.

- [ ] **Step 4: Implement an allowlist**

Replace the "not refs → immutable" heuristic with an explicit allowlist of oid-addressed tails. A tail is immutable only if its first `:`-segment is one of the object-addressed views:

```rust
/// Only content-addressed answers may be cached immutable. A view whose answer depends on a
/// branch (compare, protect, anything that resolves a ref name) changes on every push and must
/// carry a short TTL. Defaulting to immutable is how a public repo serves a week-old diff.
fn is_immutable_suffix(suffix: &str) -> bool {
    matches!(
        suffix.split(':').next().unwrap_or(""),
        "blob" | "tree" | "commit" | "raw"
    )
}
```

Verify against the real tail vocabulary: `grep -n '"blob"\|"tree"\|"commit"\|"raw"\|"compare"\|suffix' src/http/browse_api.rs src/api.rs` and adjust the allowlist to the actual oid-keyed view names this codebase emits. Non-immutable tails get the short/`refs` TTL that `refs` already uses.

- [ ] **Step 5: Run test + full suite**

Run: `cargo test --lib` then `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/api.rs
git commit -m "Cache only content-addressed reads as immutable"
```

---

### Task 3: Stream blob GET instead of buffering whole layers

**Files:**
- Modify: `src/registry/blobs.rs:75-84` (`blob_response` body branch)
- Test: `tests/registry_blobs.rs` (existing integration file) or inline

**Context:** `get(&path).await` then `.bytes().await` buffers up to `max_layer` (10 GiB) in RAM, reachable anonymously for public images. `GetResult` already exposes a `ByteStream`; stream it into the axum body. The `ponytail:` comment there ("if large-layer memory ever shows up in a profile") is downgrading a real anonymous DoS — replace it, don't keep it.

**Interfaces:**
- Consumes: `app.store.os.get(&path) -> Result<GetResult>`, `GetResult::into_stream()` (object_store).
- Produces: nothing.

- [ ] **Step 1: Confirm the stream API**

Run: `grep -rn "into_stream\|ByteStream\|StreamBody\|body::Body" src/ Cargo.toml`
Confirm `object_store::GetResult::into_stream()` is available (it is on the re-exported `slatedb::object_store`) and that axum's `Body::from_stream` is usable. axum 0.7+ has `axum::body::Body::from_stream`.

- [ ] **Step 2: Write the failing/guarding test**

A pure streaming assertion needs a large object; instead assert the pull still returns correct bytes for a normal blob AND that the handler no longer calls `.bytes()`. The behavioral guard: push a blob, GET it, assert body equals pushed bytes and `Content-Length` matches. Model on the nearest existing blob-pull test in `tests/registry_blobs.rs`.

```rust
#[tokio::test]
async fn blob_get_streams_exact_bytes() {
    let (app, owner, name) = registry_fixture().await;
    let data = vec![0xABu8; 5 * 1024 * 1024];
    let digest = push_blob(&app, &owner, &name, &data).await;
    let resp = get_blob(&app, &owner, &name, &digest).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, data);
}
```

- [ ] **Step 3: Run to verify current behavior passes (regression guard)**

Run: `cargo test --test registry_blobs blob_get_streams_exact_bytes`
Expected: PASS on current code (this test guards the refactor; it must stay green).

- [ ] **Step 4: Replace the buffering branch with a stream**

```rust
    match app.store.os.get(&path).await {
        // Stream the layer straight through: buffering the whole object here is an anonymous
        // memory-DoS for public images (a few concurrent pulls of a large layer OOM the node).
        Ok(r) => (StatusCode::OK, hdrs, axum::body::Body::from_stream(r.into_stream())).into_response(),
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => crate::http::internal_pub(e.into()),
    }
```

Remove the `// ponytail: whole-blob read` comment. Keep the head/meta path unchanged.

- [ ] **Step 5: Run test + registry suite**

Run: `cargo test --test registry_blobs` then `cargo test --test registry_http`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/registry/blobs.rs
git commit -m "Stream blob pulls instead of buffering whole layers"
```

---

### Task 4: Cap total bytes in `read_lines_until_flush`

**Files:**
- Modify: `src/pktline.rs:76-101` (`read_lines_until_flush`)
- Test: `src/pktline.rs` inline tests

**Context:** The loop caps line COUNT (100k) but not total bytes: 100k × ~65KB ≈ 6.5 GiB heap from one client, and SSH has no `max_body` in front. Add a running byte total with a cap.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn read_lines_rejects_oversized_stream() {
    // Build a stream of many max-size data pkts with no flush; total exceeds the byte cap.
    let mut buf = Vec::new();
    let big = vec![b'x'; 65515];
    for _ in 0..600 { // 600 * ~65KB ≈ 39 MiB, over a 32 MiB cap
        write_pkt(&mut buf, &big).unwrap();
    }
    let mut r: &[u8] = &buf;
    let err = read_lines_until_flush(&mut r).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
}
```

Pick the cap constant to sit above any legitimate command list (ref-update command lists and upload-pack args are kilobytes, not megabytes). Use `const MAX_BYTES: usize = 32 * 1024 * 1024;`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib read_lines_rejects_oversized_stream`
Expected: FAIL — currently reads all 39 MiB.

- [ ] **Step 3: Add the byte cap**

```rust
pub fn read_lines_until_flush(r: &mut dyn BufRead) -> io::Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    // Cap both the count AND the total bytes: SSH has no HTTP body limit in front of this, so a
    // client streaming max-size pkt-lines before a flush would otherwise grow this unbounded.
    const MAX_LINES: usize = 100_000;
    const MAX_BYTES: usize = 32 * 1024 * 1024;
    let mut total: usize = 0;
    loop {
        match read_pkt(r)? {
            Some(Pkt::Data(mut d)) => {
                if d.last() == Some(&b'\n') {
                    d.pop();
                }
                total = total.saturating_add(d.len());
                if total > MAX_BYTES {
                    return Err(io::Error::other("pkt-line stream too large"));
                }
                out.push(d);
                if out.len() > MAX_LINES {
                    return Err(io::Error::other("too many pkt-lines"));
                }
            }
            Some(Pkt::Flush) => return Ok(out),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated pkt-line stream (no flush)",
                ))
            }
            Some(Pkt::Delim) => {}
        }
    }
}
```

- [ ] **Step 4: Run test + existing pktline tests**

Run: `cargo test --lib pktline`
Expected: PASS (including the existing truncation/flush tests).

- [ ] **Step 5: Commit**

```bash
git add src/pktline.rs
git commit -m "Cap total bytes read before a pkt-line flush"
```

---

### Task 5: Refuse empty peer secret in the api tier's `caller`

**Files:**
- Modify: `src/api.rs:722-755` (`caller`)
- Test: `src/api.rs` inline tests

**Context:** `caller`'s constant-time compare accepts when `peer.len() == api.secret.len()` and bytes match — with no `is_empty()` guard that `trust_peer` (http.rs:537) and `proxy` have. A misconfigured empty `secret` lets any caller with an empty peer header + owner header assume any identity on every `/v1/*` route. Best fix: refuse to boot on an empty secret (defense at construction) AND keep an `is_empty()` guard in `caller` (defense in depth).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn empty_peer_secret_never_authenticates() {
    let api = test_api_with_secret("");
    let mut h = HeaderMap::new();
    h.insert(crate::proxy::PEER_HEADER, "".parse().unwrap());
    h.insert(crate::proxy::OWNER_HEADER, "alice".parse().unwrap());
    assert!(caller(&api, &h).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib empty_peer_secret_never_authenticates`
Expected: FAIL — empty==empty passes the compare and returns `Ok("alice")`.

- [ ] **Step 3: Add the guard in `caller`**

Insert before the length/xor compare:

```rust
    let peer = headers
        .get(crate::proxy::PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // An empty secret must never authenticate anyone, even against an empty presented value —
    // a misconfigured empty secret would otherwise make the peer header a free identity.
    if api.secret.is_empty() || peer.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "peer secret required").into_response());
    }
    if peer.len() != api.secret.len()
        || peer.bytes().zip(api.secret.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0
    {
        return Err((StatusCode::UNAUTHORIZED, "peer secret required").into_response());
    }
```

- [ ] **Step 4: Refuse to boot on empty secret**

Find where `Api { secret, .. }` is constructed (api.rs:64-73, the `serve`/`new` entry). Add, near the start of that function:

```rust
    if secret.is_empty() {
        return Err(crate::err("api peer secret must not be empty"));
    }
```

Match the surrounding error type (the fn returns `Result`). Grep `fn serve` / `fn new` in api.rs to place it correctly.

- [ ] **Step 5: Run test + full suite**

Run: `cargo test --lib empty_peer_secret_never_authenticates` then `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/api.rs
git commit -m "Refuse an empty api peer secret at boot and in caller"
```

---

### Task 6: Skip the cache on a generation-read error (stop failing open)

**Files:**
- Modify: `src/cache.rs:86-97` (`generation`), and `get`/`put` (98-113) to honor an error signal
- Test: `src/cache.rs` inline tests

**Context:** `generation()` returns `0` on any Redis error. For a purged repo (generation ≥ 1), a transient read failure makes pre-purge `v1:0:{repo}:*` entries reachable again — private content served from cache during a Redis blip, defeating the purge. Fix: `generation` returns `Option<u64>`; `None` = "cannot determine" → `get` returns `None` (miss) and `put` is a no-op for that request. Never substitute a real generation on error.

**Interfaces:**
- Produces: `async fn generation(&self, repo: &str) -> Option<u64>` — `None` means "cache disabled for this call".
- Consumes: called by `get` and `put` in the same file.

- [ ] **Step 1: Write the failing test**

Use the `mem` cache which can't error, so add a small seam: a test that a generation read error disables the cache. If the mem backend can't simulate an error, test the contract directly on a cache whose `conn` is a broken/closed connection, asserting `get` returns `None` rather than serving a generation-0 entry. Minimal contract test:

```rust
#[tokio::test]
async fn generation_error_disables_cache_not_defaults_to_zero() {
    // A cache pointed at an unreachable redis: conn is Some but every command errors.
    let cache = Cache::broken_for_test();
    // put would write under gen 0 if it fails open; instead it must be a no-op...
    cache.put("alice/repo", "refs", b"stale", 60).await;
    // ...and get must not return that entry.
    assert_eq!(cache.get("alice/repo", "refs").await, None);
}
```

If `Cache::broken_for_test()` doesn't exist, add a tiny test-only constructor that yields a `conn` guaranteed to error (e.g. a client for `redis://127.0.0.1:1`). Keep it `#[cfg(test)]`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib generation_error_disables_cache`
Expected: FAIL — put writes under gen 0, get reads it back.

- [ ] **Step 3: Change `generation` to `Option<u64>`**

```rust
    /// The repo's current generation, or `None` when it cannot be read. A miss is generation 0
    /// (see the INCR discipline); a *failure* is `None` — callers must then skip the cache for
    /// this request rather than fall back to a real generation, or a purged repo's pre-purge
    /// entries become reachable again during a backend blip.
    pub async fn generation(&self, repo: &str) -> Option<u64> {
        if let Some(m) = &self.mem {
            return Some(
                mem_get(m, &format!("gen:{repo}"))
                    .and_then(|v| String::from_utf8(v).ok()?.parse().ok())
                    .unwrap_or(0),
            );
        }
        let mut c = self.conn.clone()?;
        // `Ok(None)` is a real miss → generation 0. `Err` is a backend failure → None (skip cache).
        match run(redis::cmd("GET").arg(format!("gen:{repo}")), &mut c).await {
            Ok(v) => Some(v.unwrap_or(0)),
            Err(_) => None,
        }
    }
```

- [ ] **Step 4: Make `get`/`put` honor `None`**

```rust
    pub async fn get(&self, repo: &str, suffix: &str) -> Option<Vec<u8>> {
        let gen = self.generation(repo).await?; // None => treat as a miss, do not read
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
```

Update the module doc that says "fallback is 1" / "does NOT fail open" to match: a read failure now disables the cache for that call.

- [ ] **Step 5: Run test + full suite**

Run: `cargo test --lib cache` then `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cache.rs
git commit -m "Disable the cache on a generation-read error instead of failing open"
```

---

### Task 7: Redact the Redis URL in the unreachable log

**Files:**
- Modify: `src/cache.rs:68` (`eprintln!("cache: {url} unreachable...")`)

**Context:** `redis://:password@host` puts the password in pod logs. Log host only.

- [ ] **Step 1: Redact before logging**

Replace the log line. Parse the host out; if parsing fails, log a fixed string, never the raw URL:

```rust
        if conn.is_none() {
            let host = url::Url::parse(url).ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| "redis".to_string());
            eprintln!("cache: {host} unreachable; serving without it"); // ponytail: eprintln
        }
```

Check `url` is already a dependency (`grep '^url' Cargo.toml`); if not, don't add it — instead redact by truncating at `@`: `url.rsplit('@').next().unwrap_or("redis")` gives the host portion without credentials.

- [ ] **Step 2: Verify it builds**

Run: `cargo build`
Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
git add src/cache.rs
git commit -m "Log redis host without credentials on connect failure"
```

---

### Task 8: Verify GPG UID and subkey binding signatures

**Files:**
- Modify: `src/gpg.rs:125-182` (`emails_of`, subkey use, expiry/revocation)
- Test: `src/gpg.rs` inline tests

**Context (verify scope first):** UIDs are free text the key holder controls; `emails_of` reads UID packets WITHOUT checking their self-signatures, and subkeys are used for verification WITHOUT checking binding signatures. If the caller ties a key to an account-verified email this is lower risk, but the crypto layer should not vouch for an unverified UID. Also: only the primary key's expiry is checked (expired signing subkeys still verify); revocation packets are trusted unverified; expiry uses the OLDEST self-sig duration (GPG semantics: newest self-sig wins).

- [ ] **Step 1: Determine the caller's trust binding**

Run: `grep -rn "emails_of\|signer_by_any\|fingerprint\|register.*key\|verify_commit" src/ | grep -iv test`
Read how a registered GPG key is bound to an account (directory.rs `signer_by_any`, api.rs key registration). Write findings as a comment at the top of the task's commit message body. If keys ARE bound to an account-verified email at registration and UIDs are never trusted for identity, narrow this task to the subkey-binding + expiry-semantics fixes only. If UIDs feed identity, the self-signature check is mandatory.

- [ ] **Step 2: Write failing tests for what you're fixing**

For subkey binding (always in scope):

```rust
#[test]
fn subkey_without_valid_binding_is_not_a_signer() {
    let key = key_with_unbound_subkey(); // fixture: primary + subkey lacking a binding sig
    assert!(!signing_capable_subkeys(&key).iter().any(|s| s == &UNBOUND_SUBKEY_ID));
}

#[test]
fn newest_self_sig_expiry_wins() {
    let key = key_expiry_extended(); // old self-sig: 1y; newest self-sig: 10y, key is 2y old
    assert_eq!(validity(&key, now()), Validity::Valid); // not ExpiredKey
}
```

Use whatever OpenPGP parsing crate gpg.rs already uses (`grep '^\(sequoia\|pgp\|openpgp\)' Cargo.toml`); build fixtures with that crate's test helpers or checked-in `.asc` bytes under `tests/fixtures/`.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test --lib gpg`
Expected: FAIL on the new cases.

- [ ] **Step 4: Implement the checks**

- Subkeys: before using a subkey for verification, require a valid subkey-binding signature from the primary key (and, for signing subkeys, the embedded primary-key-binding "back signature"). Use the PGP crate's binding-verification API rather than reading raw packets.
- Expiry: select the MOST RECENT valid self-signature per component and read its expiry from that; drop the `any(|d| created + d < now)` over-all-durations logic.
- Revocation: only honor a revocation signature that verifies against the key it revokes.
- UIDs (only if in scope per Step 1): only return an email from a UID whose self-signature verifies.

Prefer the crate's high-level "validated cert at time T" view (e.g. sequoia's `Cert::with_policy(...).keys()`) which does binding/expiry/revocation correctly, over hand-walking packets.

- [ ] **Step 5: Run tests + full suite**

Run: `cargo test --lib gpg` then `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/gpg.rs tests/fixtures/
git commit -m "Verify GPG subkey bindings, revocations, and newest-self-sig expiry"
```

---

## MEDIUM SEVERITY

### Task 9: Expire abandoned upload sessions

**Files:**
- Modify: `src/registry/gc.rs` (add a staging sweep) and `src/bin/worker.rs:159` (call it), or `src/registry/uploads.rs`
- Test: `tests/registry_blobs.rs` or a new `tests/registry_gc.rs`

**Context:** Abandoned upload sessions leave the `upload/{uuid}` DB row and the `uploads/{owner}/{name}/{uuid}` staging object forever; GC sweeps only `blobs/`. Up to 10 GiB per session, unlimited sessions per authenticated owner — storage leak + DoS. Add a keep-biased sweep: delete staging objects (and their DB rows) whose session mtime is older than a grace period.

**Interfaces:**
- Consumes: the upload-session listing (grep `upload/` key prefix in `src/registry/uploads.rs` / `store.rs`), object `mtime`.
- Produces: `async fn sweep_stale_uploads(owner, grace: Duration) -> Result<usize>` (returns count deleted).

- [ ] **Step 1: Locate the session key/object layout**

Run: `grep -rn "uploads/\|upload/\|open_session\|session\|uuid" src/registry/uploads.rs src/registry/store.rs`
Note the exact object prefix for staging bytes and the DB key for the session row.

- [ ] **Step 2: Write the failing test**

```rust
#[tokio::test]
async fn stale_upload_sessions_are_swept() {
    let (app, owner, name) = registry_fixture().await;
    let uuid = open_session(&app, &owner, &name).await; // POST .../blobs/uploads/
    patch_chunk(&app, &owner, &name, &uuid, &[0u8; 1024]).await; // stages bytes, never completes
    // age the staging object past the grace window (test seam: set_mtime or inject a clock)
    age_session(&app, &owner, &uuid, Duration::from_secs(48 * 3600)).await;
    let n = app.store.sweep_stale_uploads(&owner, Duration::from_secs(24 * 3600)).await.unwrap();
    assert_eq!(n, 1);
    assert!(session_gone(&app, &owner, &name, &uuid).await);
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --test registry_blobs stale_upload_sessions_are_swept`
Expected: FAIL — no such method.

- [ ] **Step 4: Implement `sweep_stale_uploads`**

Mirror `gc::sweep_owner`'s style: list staging objects under the owner's `uploads/` prefix, for each read mtime, and if `now - mtime > grace` delete the object AND the matching `upload/{uuid}` DB row. Keep-biased: on any list/read error for an entry, skip that entry (don't abort the whole sweep, but never delete on uncertainty). Default grace: 24h (const, overridable via env like `max_layer`).

- [ ] **Step 5: Wire it into the GC worker**

In `src/bin/worker.rs` where the blob sweep runs (~line 159), call `sweep_stale_uploads` for each owner in the same loop. Log the count.

- [ ] **Step 6: Run tests**

Run: `cargo test --test registry_blobs` then `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/registry/gc.rs src/bin/worker.rs src/registry/store.rs
git commit -m "Sweep abandoned upload sessions in GC"
```

---

### Task 10: Clean up packs from rejected pushes; repack tip-less repos

**Files:**
- Modify: `src/protocol/receive.rs:213-260` (delete pack when the push is ultimately rejected), `src/gc.rs:68` (don't skip repack purely because `tips.is_empty()`)
- Test: `tests/` push integration (grep for existing receive-pack tests) + `src/gc.rs` inline

**Context:** The pack is indexed and uploaded to S3 BEFORE the connectivity/isolation check. A rejected push leaves the full pack in S3 permanently; and `gc.rs:68` skips repack when `tips.is_empty()`, so a repo that never had a successful push accumulates garbage forever. Two fixes.

- [ ] **Step 1: Write the failing test (rejected push leaves no pack)**

```rust
#[tokio::test]
async fn rejected_push_leaves_no_pack_in_store() {
    let (app, repo) = repo_fixture().await;
    let before = count_pack_objects(&app, &repo).await;
    // a push whose objects are not connectivity-complete → rejected after upload
    let resp = receive_pack(&app, &repo, incomplete_pack()).await;
    assert!(resp.reports_rejection());
    assert_eq!(count_pack_objects(&app, &repo).await, before); // nothing left behind
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test rejected_push_leaves_no_pack_in_store`
Expected: FAIL — pack remains in the store.

- [ ] **Step 3: Delete the uploaded pack on rejection**

In `receive.rs`, after the connectivity/isolation check rejects the push (the branch that currently returns the "missing necessary objects"/protection error), delete the just-uploaded `.pack`/`.idx` from the object store and local cache — mirror the existing S3-upload-failure cleanup at lines 218-220, but for the rejection path. Only delete THIS push's freshly-written pack (track its path), never anything reachable from an existing ref.

- [ ] **Step 4: Fix the tip-less repack skip**

In `gc.rs:68`, change the `tips.is_empty()` early return so it still runs the sweep/repack that removes unreachable packs (a repo with no tips = everything is unreachable = safe to drop), while keeping the keep-biased abort-on-unreadable-manifest/ref guard. Add a test:

```rust
#[test]
fn empty_tips_still_reclaims_unreachable_packs() {
    let repo = repo_with_unreachable_pack_and_no_refs();
    let plan = gc::plan(&repo).unwrap();
    assert!(!plan.keep_everything());
}
```

Adjust to the real `gc` API names (grep `fn` in gc.rs).

- [ ] **Step 5: Run tests**

Run: `cargo test --test registry_gc 2>/dev/null; cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/protocol/receive.rs src/gc.rs
git commit -m "Delete packs from rejected pushes and reclaim tip-less repos"
```

---

### Task 11: Close the registry GC mount/HEAD race

**Files:**
- Modify: `src/registry/blobs.rs` (mount path ~106-119, HEAD path) to bump blob mtime; optionally `src/registry/gc.rs:104-127`
- Test: `tests/registry_blobs.rs`

**Context:** HEAD and cross-repo mount don't refresh a blob's mtime, so the sweep's double-`referenced()` read doesn't actually close the window it claims to: HEAD an old unreferenced blob → sweep reads `referenced()=false` → client PUTs a manifest referencing it → sweep deletes it → tagged manifest missing a layer. Fix per the reviewer's suggested style: have the mount/HEAD paths bump the blob's timestamp (copy-to-self), so grace protects it.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn head_then_reference_survives_sweep() {
    let (app, owner, name) = registry_fixture().await;
    let digest = push_blob(&app, &owner, &name, &[7u8; 4096]).await;
    delete_all_manifests(&app, &owner, &name).await; // blob now unreferenced + old
    age_blob(&app, &owner, &digest, Duration::from_secs(48*3600)).await;
    head_blob(&app, &owner, &name, &digest).await;    // client checks it exists...
    let sweep = app.store.sweep_owner(&owner, Duration::from_secs(24*3600)).await.unwrap();
    push_manifest_referencing(&app, &owner, &name, &digest).await; // ...then references it
    assert!(blob_exists(&app, &owner, &digest).await, "swept a blob a live manifest needs");
    let _ = sweep;
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test registry_blobs head_then_reference_survives_sweep`
Expected: FAIL (flaky by construction — the race). If timing makes it non-deterministic, assert the invariant directly: after `head_blob`, the blob's mtime is fresh (within grace).

- [ ] **Step 3: Bump mtime on HEAD and mount**

In `blob_response` HEAD branch and the mount no-op branch (blobs.rs ~106-119), after confirming the blob exists, refresh its mtime via a copy-to-self (`os.copy(&path, &path)` or the object store's touch equivalent — grep how `touch_image` does it in `store.rs`). Guard cost: only bump when the existing mtime is older than, say, half the grace window, so hot pulls don't rewrite constantly. Add a `// ponytail:` note if you cap it.

- [ ] **Step 4: Run tests**

Run: `cargo test --test registry_blobs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/registry/blobs.rs
git commit -m "Refresh blob mtime on HEAD and mount so GC cannot race a reference"
```

---

### Task 12: Serialize PATCH chunks and the pull counter

**Files:**
- Modify: `src/registry/uploads.rs:102-186` (PATCH read-modify-write), `src/registry/store.rs:186-196` (`bump_pulls`)
- Test: `tests/registry_blobs.rs`

**Context:** Concurrent PATCHes to one session both read the same `have` and RMW the staging object → lost/interleaved chunk (digest check backstops correctness but the push fails confusingly). `bump_pulls` is a read-increment-write that drops counts under concurrent pulls despite "cannot race" — single owning NODE, not single request. Both need per-key serialization within the node.

- [ ] **Step 1: Write the failing test (lost counter increments)**

```rust
#[tokio::test]
async fn concurrent_pulls_count_every_hit() {
    let (app, owner, name) = registry_fixture().await;
    let n = 50;
    let tasks: Vec<_> = (0..n).map(|_| {
        let app = app.clone();
        tokio::spawn(async move { app.store.bump_pulls(&owner_c, &name_c).await })
    }).collect();
    for t in tasks { t.await.unwrap().unwrap(); }
    assert_eq!(app.store.pull_count(&owner, &name).await.unwrap(), n);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test registry_blobs concurrent_pulls_count_every_hit`
Expected: FAIL — final count < 50.

- [ ] **Step 3: Serialize with a per-key async mutex**

Add a `tokio::sync::Mutex`-keyed map (or a striped lock) on the image store keyed by `{owner}/{name}` for `bump_pulls`, and by `{owner}/{name}/{uuid}` for PATCH. Since a single node owns the image DB, an in-process mutex is sufficient and correct. Prefer reusing any existing lock registry in `store.rs`; if none, a `Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>` is the small idiomatic version. Mark it `// ponytail: in-process lock; correct because one node owns the image DB`.

- [ ] **Step 4: Apply the same guard to PATCH**

Wrap the read-`have` → append → write sequence in uploads.rs under the per-session lock so two PATCHes to one uuid can't interleave; return `416 Range Not Satisfiable` if the incoming chunk's start doesn't match the current `have` (matches the OCI chunked-upload contract better than the confusing digest failure).

- [ ] **Step 5: Run tests**

Run: `cargo test --test registry_blobs` then `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/registry/uploads.rs src/registry/store.rs
git commit -m "Serialize upload chunks and pull-counter updates per key"
```

---

### Task 13: Serialize the leader's ownership read-modify-write paths

**Files:**
- Modify: `src/lib.rs` (add a leader mutex to `App`; hold it across `grant_claim` 451-478, `grant_renew`, `grant_release`, `prune_once` 342-350)
- Test: `src/ownership/tests.rs` or `src/lib.rs` inline

**Context:** All four leader paths are read-decide-write with no serialization. Concurrent claims can grant one repo to two nodes; prune can delete a freshly-renewed lease → the node opens the DB and fences the legitimate holder ("detected newer DB client"). The comment already CLAIMS "leader-mediated compare-and-set" — this makes it true. One `tokio::Mutex` held across each of the four, since the leader is one process.

**Interfaces:**
- Produces: a field on `App`, e.g. `leader_lock: tokio::sync::Mutex<()>`, guarded by `if self.is_leader()`. Task 14 does NOT need this (different fix) — this is the only shared-state task.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn concurrent_claims_never_grant_one_repo_twice() {
    let leader = test_leader_app(/* replicas */ 3).await;
    // two different askers race for the same, currently-unowned repo
    let (a, b) = tokio::join!(
        leader.grant_claim("alice/web", "kloudlite-1", false),
        leader.grant_claim("alice/web", "kloudlite-2", false),
    );
    // exactly one Granted; the map names a single holder
    let granted = [a.unwrap(), b.unwrap()].iter().filter(|g| matches!(g, Grant::Granted(_))).count();
    assert_eq!(granted, 1);
}
```

- [ ] **Step 2: Run to verify it fails (or is racy)**

Run: `cargo test --lib concurrent_claims_never_grant_one_repo_twice`
Expected: FAIL intermittently — both can read `None` and both `put`. If the in-memory ownership store used by tests happens to serialize, inject a yield between get and put to expose it, or assert via a counter of `put`s.

- [ ] **Step 3: Add the leader lock**

Add `leader_lock: tokio::sync::Mutex<()>` to `App` (init `Mutex::new(())`). At the top of `grant_claim`, `grant_renew`, `grant_release`, and `prune_once`, take `let _g = self.leader_lock.lock().await;` before the first `self.ownership.get`/`all`. Update the `grant_claim` comment: it is now genuinely a serialized compare-and-set.

```rust
    pub async fn grant_claim(&self, repo: &str, asker: &str, force: bool) -> Result<Grant> {
        // Serialize every leader read-modify-write: concurrent claims/renews/prunes on the same
        // repo could otherwise both read a stale map and both write, granting one repo to two
        // nodes — which fences the loser's live database. One process, one lock: cheap and total.
        let _g = self.leader_lock.lock().await;
        let now = ownership::now_ms();
        ...
```

- [ ] **Step 4: Run test + ownership suite**

Run: `cargo test --lib ownership` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs
git commit -m "Serialize the leader's ownership read-modify-write paths"
```

---

### Task 14: Guard admin commands against fencing a live node

**Files:**
- Modify: `src/main.rs:270-297` (`admin repack`/`fork`/`delete-repo`/`create-repo`)
- Test: manual/doc — these are CLI paths; add an assertion-level guard, not an integration test

**Context:** These admin subcommands open a repo's SlateDB from a separate process with zero ownership coordination — run against a live fleet, `admin repack alice/web` fences the serving node. `set-visibility`/`set-image-visibility` got fleet guards; these destructive ones got nothing.

- [ ] **Step 1: Read the existing guard**

Run: `grep -n "set-visibility\|set_visibility\|drain\|fleet\|--force\|confirm\|is_leader\|route" src/main.rs`
Read how `set-visibility` coordinates (it likely asks the leader / refuses when the fleet is live, or routes the change through the owner). Reuse that exact mechanism.

- [ ] **Step 2: Apply the same guard to the four commands**

For each of `admin repack`/`fork`/`delete-repo`/`create-repo`, before opening the repo DB: route the operation through the owning node (preferred — issue it as a fleet request the way `set-visibility` does), OR, if these must stay offline-only tools, refuse to run when the ownership map shows the repo is live on some node, with a clear message and a `--force` escape hatch that prints the fencing warning. Match whichever pattern `set-visibility` established; do not invent a third.

- [ ] **Step 3: Verify**

Run: `cargo build && cargo test`
Expected: builds; existing tests pass. Manually confirm `admin repack` on a live-owned repo now refuses/routes.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "Coordinate destructive admin commands with repo ownership"
```

---

### Task 15: Make pool `evict` conditional on the observed handle

**Files:**
- Modify: `src/pool.rs:164-172` (`get`), `src/pool.rs:224` (`evict`)
- Test: `src/pool.rs` inline

**Context:** `get()` fence-evict race: A's `on_fenced` evicts and reopens a fresh handle; B's `get()` then runs `evict` on the same key and closes A's fresh healthy DB — both requests fail, avoidable flap. Fix: only evict if the entry still holds the SAME `Arc<Db>` that was observed closed (`Arc::ptr_eq`).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn evict_spares_a_freshly_reopened_handle() {
    let pool = test_pool().await;
    let h1 = pool.get("alice", "web").await.unwrap(); // handle A
    // simulate: A observed as fenced; a fresh handle B replaces it in the map
    let h2 = force_reopen(&pool, "alice", "web").await; // handle B, distinct Arc
    assert!(!Arc::ptr_eq(&h1, &h2));
    pool.evict_if_same("alice", "web", &h1).await; // stale evict keyed on A
    let h3 = pool.get("alice", "web").await.unwrap();
    assert!(Arc::ptr_eq(&h2, &h3), "evict closed the fresh handle");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib evict_spares_a_freshly_reopened_handle`
Expected: FAIL — current `evict` removes by key unconditionally.

- [ ] **Step 3: Add `evict_if_same` and use it in `get`**

Add a variant that removes the map entry only if its initialized `Db` Arc is `ptr_eq` to the observed one:

```rust
    /// Evict only if the map still holds the exact handle the caller saw as closed. A blind evict
    /// races a concurrent reopen: two requests observing the same fenced handle would otherwise
    /// have the second one close the first's fresh, healthy database.
    pub async fn evict_if_same(&self, owner: &str, name: &str, observed: &Arc<Db>) {
        let key = format!("{owner}/{name}");
        let mut map = self.entries.lock().unwrap();
        if let Some(e) = map.get(&key) {
            if e.db.get().map(|cur| Arc::ptr_eq(cur, observed)).unwrap_or(false) {
                map.remove(&key);
            }
        }
    }
```

In `get`, replace `self.evict(owner, name).await` with `self.evict_if_same(owner, name, &h).await` — but note `h` is dropped above; capture the `Arc<Db>` before dropping, pass a clone. Reorder:

```rust
    pub async fn get(self: &Arc<Self>, owner: &str, name: &str) -> Result<Arc<Db>> {
        let h = self.get_once(owner, name).await?;
        if h.status().close_reason.is_none() {
            return Ok(h);
        }
        self.evict_if_same(owner, name, &h).await;
        drop(h);
        Err(FencedError { repo: format!("{owner}/{name}") }.into())
    }
```

Keep `on_fenced`'s existing `evict` call (lib.rs:505) as-is or switch it too if it holds the observed handle — check whether it does before changing.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib pool` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/pool.rs
git commit -m "Evict a fenced pool handle only when it is still the current one"
```

---

### Task 16: Bound the negative auth cache

**Files:**
- Modify: `src/auth.rs:40-58` + the `auth_cache` in `src/store.rs`
- Test: `src/auth.rs` inline

**Context:** Negative lookups are cached keyed by sha256 of the presented token, in an unbounded map with no sweeper — spraying random bearer tokens grows it without bound (memory DoS). Cap it or stop caching `None`.

- [ ] **Step 1: Decide: cap vs. don't-cache-None**

Simplest correct fix (ponytail): don't cache negative results at all unless there's a measured hot-path reason. Check whether anything depends on negative caching (`grep -n "None\|negative\|miss" src/auth.rs`). If negative caching exists purely to avoid re-hitting the store on repeated bad tokens, replace the unbounded map with a bounded LRU.

- [ ] **Step 2: Write the failing test**

```rust
#[tokio::test]
async fn negative_auth_cache_is_bounded() {
    let store = test_store().await;
    for i in 0..10_000 {
        let _ = store.authenticate(&format!("bogus-token-{i}")).await; // all invalid
    }
    assert!(store.auth_cache_len() <= AUTH_CACHE_CAP);
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --lib negative_auth_cache_is_bounded`
Expected: FAIL — map grows to 10k.

- [ ] **Step 4: Implement the bound**

Either drop negative caching (delete the `insert(None)` path — positive results are the valuable ones and there aren't unbounded valid tokens), or swap the map for `lru::LruCache` with a fixed cap (add `lru` only if not already present; `grep '^lru' Cargo.toml` — if absent, prefer the delete-negative-caching route, no new dep). Add `auth_cache_len()` test accessor `#[cfg(test)]`.

- [ ] **Step 5: Run tests**

Run: `cargo test --lib auth` then `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/auth.rs src/store.rs
git commit -m "Bound the negative auth cache"
```

---

### Task 17: Negative-cache nonexistent repos in `route`

**Files:**
- Modify: `src/lib.rs:178-181` (`route` → `pool.exists`)
- Test: `src/lib.rs` inline

**Context:** `route()` does a pre-auth object-store LIST per request for any repo not in the live map; spraying nonexistent names drives one LIST per request. Add a short negative cache (repo → not-exists, few seconds).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn repeated_missing_repo_lookups_hit_store_once() {
    let app = test_app_counting_lists().await;
    for _ in 0..5 { let _ = app.route("ghost/repo").await; }
    assert_eq!(app.list_calls(), 1); // within the negative-cache TTL
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib repeated_missing_repo_lookups_hit_store_once`
Expected: FAIL — 5 LISTs.

- [ ] **Step 3: Add a small TTL negative cache**

Add a `Mutex<HashMap<String, Instant>>` (repo → time-first-seen-missing) on `App`, checked before `pool.exists`; entries expire after a few seconds (const `NEG_TTL = Duration::from_secs(5)`). On a positive existence result, don't cache. Mark `// ponytail: 5s negative cache; a repo created within the window still 404s briefly — acceptable, it's just-created`. Keep it tiny; evict lazily on lookup like `cache::memory` does.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs
git commit -m "Negative-cache missing repos to cut pre-auth store LISTs"
```

---

### Task 18: Compact the ownership map periodically

**Files:**
- Modify: `src/ownership.rs:176-194` (map DB open) or the leader loop
- Test: manual/doc

**Context:** The ownership map DB runs with compaction AND GC permanently off; every node renews every warm repo every 3s (a `put` each), so L0 SSTs and dead objects accumulate for the leader's whole life. The "no compaction for follower safety" reason applies to followers, not the leader.

- [ ] **Step 1: Confirm the safety boundary**

Read the comment at ownership.rs:176-194 explaining why compaction is off. Confirm the reason is follower read-consistency and that the LEADER (pod zero, sole writer) can safely compact. If SlateDB exposes a manual/one-shot compaction API, the leader can call it on a timer.

- [ ] **Step 2: Add a leader-only periodic compaction**

In the leader's background loop (where `prune_once` runs), add a lower-frequency (e.g. every few minutes) manual compaction of the ownership DB, guarded by `if self.is_leader()`. If SlateDB has no manual-compaction API, instead document a hard ceiling and open a `// ponytail:` marker: `// ponytail: ownership map never compacts; L0 grows with the leader's lifetime — bounded only by restarts. Compact from the leader when SlateDB exposes a manual API.` and STOP (don't fabricate an API).

- [ ] **Step 3: Verify**

Run: `cargo build && cargo test`
Expected: builds, tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/ownership.rs
git commit -m "Compact the ownership map from the leader on a timer"
```

---

### Task 19: Fix `signer_by_any` key-id vs. fingerprint matching

**Files:**
- Modify: `src/directory.rs:770-793` (`signer_by_any`) OR its callers in `src/gpg.rs`
- Test: `src/directory.rs` inline

**Context:** Comments promise suffix matching ("a key id matches the fingerprint that contains it") but the query is an exact lowercase `$in`. A signature naming its issuer by 16-hex key id won't match a stored full fingerprint. Either the comment or the query is wrong — Task 8 touches this area, so reconcile them.

- [ ] **Step 1: Determine intended behavior**

Read what the callers pass in (`grep -n "signer_by_any" src/`). If callers pass full fingerprints only, fix the COMMENT (exact match is correct). If callers pass key ids that must match a stored fingerprint's suffix, fix the QUERY to do suffix matching (store key-id suffixes alongside fingerprints at registration, or query with a suffix/`$regex` anchored at the end).

- [ ] **Step 2: Write the failing test for the chosen behavior**

If suffix matching is required:

```rust
#[tokio::test]
async fn key_id_matches_stored_fingerprint() {
    let dir = test_directory().await;
    register_key(&dir, "alice", FULL_FINGERPRINT).await;
    let signer = dir.signer_by_any(&[SHORT_KEY_ID_SUFFIX]).await.unwrap();
    assert_eq!(signer.owner, "alice");
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --lib key_id_matches_stored_fingerprint`
Expected: FAIL if suffix matching was intended.

- [ ] **Step 4: Implement the fix (query or comment)**

Preferred (avoids a slow scan): at key registration, store the derived 16-hex and 8-hex key-id suffixes as additional indexed lookup keys, and have `signer_by_any` query the exact set including those. This keeps the `$in` fast. Only fall back to a suffix regex if registration can't be changed.

- [ ] **Step 5: Run tests**

Run: `cargo test --lib` then `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/directory.rs
git commit -m "Match GPG signers by key id as well as full fingerprint"
```

---

## LOW SEVERITY (batched)

### Task 20: Correct HTTP status codes for client protocol errors

**Files:**
- Modify: `src/http.rs:952-963` (`respond_first`), `src/registry/*` internal-error paths

- [ ] **Step 1:** In `respond_first`, distinguish a `ClientError` (malformed pkt-line, truncated gzip) → `400`, mirroring `info_refs`, instead of mapping everything to `internal(...)` → 500. Grep the protocol error enum for a client-vs-server variant (`grep -n "ClientError\|enum.*Error" src/protocol/*.rs src/lib.rs`).
- [ ] **Step 2:** Write a test: a truncated push body returns 400, not 500. Run it (fails), implement, run (passes).
- [ ] **Step 3:** Commit: `Return 400 for client protocol errors on push and fetch`

### Task 21: OCI error envelope for registry 500s

**Files:**
- Modify: `src/http.rs` `internal_pub` callers in `src/registry/*`

- [ ] **Step 1:** `internal_pub` returns plain-text "internal error"; CLAUDE.md requires every `/v2` error to be the OCI JSON envelope via `oci_err`. Add an `oci_internal(e)` helper returning the `{"errors":[...]}` envelope with a 500, and replace `internal_pub` in registry handlers. Grep: `grep -rn "internal_pub" src/registry/`.
- [ ] **Step 2:** Test one registry handler's 500 returns the envelope. Fail → implement → pass.
- [ ] **Step 3:** Commit: `Return the OCI error envelope for registry 500s`

### Task 22: Overflow-safe size arithmetic

**Files:**
- Modify: `src/http/browse_api.rs:286-295` (`declared_size`), `src/protocol/upload.rs:421-429` (`parse_size`)

- [ ] **Step 1:** `declared_size` uses `sum::<u64>()` → panics (debug) / wraps (release) on attacker manifests near `u64::MAX`; switch to `fold(0u64, |a, s| a.saturating_add(s))`. `parse_size` uses unchecked `n * mult`; switch to `n.checked_mul(mult)` returning an error/None on overflow.
- [ ] **Step 2:** Tests: a manifest with two near-`u64::MAX` sizes doesn't panic; `parse_size("18014398509481984g")` doesn't wrap to a small value. Fail → implement → pass.
- [ ] **Step 3:** Commit: `Use saturating/checked arithmetic for declared sizes and size filters`

### Task 23: Harden the `.git` path filter

**Files:**
- Modify: `src/objects.rs:333-349` (`split_path`)

- [ ] **Step 1:** Block the checkout-escape variants git itself refuses: `.git.`, `.git ` (trailing dot/space), `git~1` (8.3 short name), and ignorable-codepoint forms. Normalize the component (trim trailing dots/spaces, reject `~` short-name pattern, strip zero-width joiners) before the case-insensitive `.git` compare.
- [ ] **Step 2:** Test each variant is rejected. Fail → implement → pass.
- [ ] **Step 3:** Commit: `Reject .git path-filter bypass variants`

### Task 24: Escape push options before logging; reject non-UTF-8 ref names

**Files:**
- Modify: `src/protocol/receive.rs:105` (push-option log), `src/protocol/receive.rs:56` (ref name from `from_utf8_lossy`)

- [ ] **Step 1:** Push options go to stderr via `eprintln!` unescaped → ANSI/log injection. Escape control bytes (or use `{:?}` debug formatting) before logging.
- [ ] **Step 2:** Ref names decoded with `from_utf8_lossy` silently substitute U+FFFD, so the stored name differs from the client's bytes; reject a ref-update line whose name isn't valid UTF-8 instead (`std::str::from_utf8(...).is_err()` → error the command).
- [ ] **Step 3:** Test: a non-UTF-8 ref name is rejected; a push option with `\x1b[` is logged escaped. Fail → implement → pass.
- [ ] **Step 4:** Commit: `Escape logged push options and reject non-UTF-8 ref names`

### Task 25: Memoize `max_layer` and small hygiene fixes

**Files:**
- Modify: `src/registry/blobs.rs:18-21` (`max_layer`), `src/proxy.rs:31-36` (`is_connect_error`), `src/bin/worker.rs:225-240` (refs error handling), `src/main.rs:167-171` (watchdog exit code)

- [ ] **Step 1:** `max_layer` re-parses the env var per request → wrap the parse in a `static LAYER: OnceLock<u64>`.
- [ ] **Step 2:** `is_connect_error` matches reqwest error-message substrings (breaks across versions) → use `err.is_connect()` via the reqwest API instead of string matching.
- [ ] **Step 3:** `worker.rs check_one` treats any non-404 refs error as empty refs → check `resp.status().is_success()` and skip/record-transient on 5xx/403.
- [ ] **Step 4:** `main.rs` shutdown watchdog `exit(0)` masks a hung shutdown → `exit(1)` so the restart is visible.
- [ ] **Step 5:** Each has a tiny test where practical (OnceLock value, `is_connect_error` on a synthesized connect error). Fail → implement → pass. Commit: `Memoize max_layer and fix connect-error, worker, and watchdog hygiene`

### Task 26: Share a constant-time peer-secret compare

**Files:**
- Modify: `src/http.rs:535-538` (`trust_peer`), `src/proxy.rs` (stream secret check), reuse api.rs's constant-time logic

- [ ] **Step 1:** api.rs's `caller` has a constant-time compare; `trust_peer` and the proxy stream check use plain `!=` (both `ponytail:`-marked). Extract one `pub fn secret_eq(a: &str, b: &str) -> bool` (length check + xor-fold, plus the empty guard from Task 5) into a shared module (`proxy.rs` or a small `util`), and call it from all three sites. Add `subtle` only if you'd rather use `ConstantTimeEq` — but the hand-rolled fold is already in the tree, so reuse it, no new dep.
- [ ] **Step 2:** Test `secret_eq("", "")` is false and `secret_eq(x, x)` is true. Fail → implement → pass.
- [ ] **Step 3:** Commit: `Share one constant-time peer-secret comparison`

### Task 27: Protocol-correctness niceties

**Files:**
- Modify: `src/protocol/upload.rs:576-589` (deepen-since off-by-one), `src/registry/manifests.rs:264-286` (orphaned manifest-type row), `src/registry/uploads.rs:282-291` (Content-Range end check)

- [ ] **Step 1:** `deepen-since` includes one too-old commit per branch and names IT shallow; move the `too_old` check before inserting into `depth_of` so the boundary is the youngest commit ≥ since.
- [ ] **Step 2:** `delete_manifest` by digest orphans the `image/manifest-type/{d}` DB row; delete that row alongside the manifest object.
- [ ] **Step 3:** `complete`'s Content-Range checks `start` but not declared end vs. body length; validate the end like `patch` does, returning `400` instead of the confusing `DIGEST_INVALID`.
- [ ] **Step 4:** One test per fix. Fail → implement → pass. Commit: `Fix deepen-since boundary, orphaned manifest-type rows, and Content-Range end check`

---

## Final verification (after all tasks)

- [ ] Run the full suite: `cargo test`
- [ ] Run one real registry round-trip if docker is available: `./tests/registry_e2e.sh` (exit 77 = docker half skipped, that's fine)
- [ ] `cargo clippy --lib` — confirm no NEW warnings in touched files
- [ ] Re-read the four review reports and check each High/Medium finding maps to a landed task
