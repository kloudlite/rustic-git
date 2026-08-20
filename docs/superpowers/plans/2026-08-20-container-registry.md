# Container Registry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve an OCI Distribution v1.1 registry from the existing git nodes, so `docker push`/`docker pull` work against `{host}/v2/{owner}/{image}`.

**Architecture:** Images are their own namespace, `{owner}/{name}`, unrelated to git repos. `/v2/...` paths derive the routing key `img/{owner}/{name}` and travel through the SAME ownership middleware, claim, and pool that repos do — the pool coordinates are `("img", "{owner}/{name}")`, which `repo.split_once('/')` round-trips unchanged, so `lib.rs` needs no changes at all. Tags, upload sessions, and the referrers index live in the image's SlateDB (routed, therefore single-writer, therefore atomic); blobs and manifest bytes are objects in the object store under `blobs/{owner}/` and `manifests/{owner}/{name}/`.

**Tech Stack:** Rust, axum 0.8, SlateDB, `object_store` (multipart via `ObjectStore::put_multipart`), `sha2`, `serde_json`. Web: Next.js app router under `web/apps/web`.

**Spec:** `docs/superpowers/specs/2026-08-20-container-registry-design.md`

## Global Constraints

- Image name: ONE segment, `store::valid_segment`. `/v2/{owner}/{name}/...` — nested names are refused with `NAME_INVALID`.
- Reserved owner names become `["api", "v2", "img"]` in `store::valid_owner`. Reserving `img` is load-bearing: it stops a git repo's database from nesting inside the image prefix.
- Routing key for every image path: `format!("img/{owner}/{name}")`. Pool coordinates: `("img", &format!("{owner}/{name}"))`.
- Blob objects: `blobs/{owner}/sha256/{hex}`. Manifest objects: `manifests/{owner}/{name}/sha256/{hex}`. Never any other layout — the GC sweep and team deletion both assume these prefixes.
- Every error response is the OCI envelope `{"errors":[{"code":"...","message":"...","detail":null}]}` with `Content-Type: application/json`, built by ONE helper (`registry::oci_err`). No bare-string errors on `/v2`.
- Every digest that appears in a path or query is parsed by `registry::Digest::parse` before use. A digest is `sha256:` + 64 lowercase hex, nothing else — the parser is the only place a path segment becomes an object-store key.
- Both auth modes are always accepted: `Authorization: Bearer <jwt from /v2/token>` and `Authorization: Basic <user:token>`. A 401 always carries the Bearer challenge.
- `cargo test` and `cargo clippy --all-targets -- -D warnings` must pass at the end of every task.
- Commit messages: imperative sentence-case subject, no tool attribution.

---

### Task 1: Registry keys and routing

The bug class this task exists to prevent: a `/v2` path deriving a key that collides with a git repo's, serving one image's data under another name. Everything else is built on it.

**Files:**
- Create: `src/registry/mod.rs`
- Modify: `src/lib.rs` (add `pub mod registry;`)
- Modify: `src/store.rs:86` (`valid_owner`)
- Modify: `src/http.rs:167-247` (`repo_of`, `is_git_route`, `route_inner`)
- Test: `tests/routing.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `registry::image_route(path: &str) -> Option<(&str, &str)>` — `(owner, name)` when the path is `/v2/{owner}/{name}/{blobs|manifests|tags|referrers}...`
  - `registry::routing_key(owner: &str, name: &str) -> String` — `img/{owner}/{name}`
  - `registry::pool_coords(owner: &str, name: &str) -> (&'static str, String)` — `("img", "{owner}/{name}")`
  - `registry::is_v2_path(path: &str) -> bool`
  - `registry::LOCAL_V2: [&str; 3]` — the `/v2` paths answered locally, never routed: `["", "token", "_catalog"]`

- [ ] **Step 1: Write the failing tests**

In `src/registry/mod.rs`, at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_paths_parse() {
        assert_eq!(image_route("/v2/acme/nginx/blobs/sha256:aa"), Some(("acme", "nginx")));
        assert_eq!(image_route("/v2/acme/nginx/manifests/latest"), Some(("acme", "nginx")));
        assert_eq!(image_route("/v2/acme/nginx/blobs/uploads/"), Some(("acme", "nginx")));
        assert_eq!(image_route("/v2/acme/nginx/tags/list"), Some(("acme", "nginx")));
        assert_eq!(image_route("/v2/acme/nginx/referrers/sha256:aa"), Some(("acme", "nginx")));
    }

    #[test]
    fn non_image_v2_paths_do_not_route() {
        // These are answered locally on whichever node receives them.
        assert_eq!(image_route("/v2/"), None);
        assert_eq!(image_route("/v2"), None);
        assert_eq!(image_route("/v2/token"), None);
        assert_eq!(image_route("/v2/_catalog"), None);
        // A nested name is not a two-segment image, so it never routes.
        assert_eq!(image_route("/v2/acme/team/nginx/manifests/latest"), None);
        // An unknown tail is not a registry endpoint.
        assert_eq!(image_route("/v2/acme/nginx/frobnicate"), None);
    }

    #[test]
    fn keys_cannot_collide_with_a_repo() {
        // The image acme/nginx and the repo acme/nginx are different objects.
        assert_eq!(routing_key("acme", "nginx"), "img/acme/nginx");
        assert_ne!(routing_key("acme", "nginx"), "acme/nginx");
        // The key round-trips through split_once exactly as lib.rs does it.
        let key = routing_key("acme", "nginx");
        let (o, n) = key.split_once('/').unwrap();
        assert_eq!((o, n), ("img", "acme/nginx"));
        assert_eq!(pool_coords("acme", "nginx"), ("img", "acme/nginx".to_string()));
        // And no repo can be owned by `img`, so no repo database nests under one.
        assert!(!crate::store::valid_owner("img"));
        assert!(!crate::store::valid_owner("v2"));
    }
}
```

In `tests/routing.rs`, append:

```rust
#[test]
fn v2_paths_derive_the_image_key() {
    // repo_of is private, so assert through the public helper the middleware uses.
    assert_eq!(
        rustic_git::registry::image_route("/v2/acme/nginx/blobs/sha256:ab").map(|(o, n)| rustic_git::registry::routing_key(o, n)),
        Some("img/acme/nginx".to_string())
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test registry:: && cargo test --test routing v2_paths`
Expected: FAIL — `unresolved import`/`cannot find module registry`.

- [ ] **Step 3: Write the module**

`src/registry/mod.rs`:

```rust
//! An OCI Distribution v1.1 registry, served by the git nodes.
//!
//! An image is `{owner}/{name}` in a namespace of its own: no git repo is required, and a repo of
//! the same name grants no claim on it. What makes the two safe to serve from one process is this
//! module's key derivation — see `routing_key`.

/// The tails that make a `/v2/{owner}/{name}/...` path an IMAGE path (one that must be routed to
/// the node holding that image's database). A path whose tail is missing here is not a registry
/// endpoint, is not routable, and is refused before any handler sees it — exactly as `BROWSE_TAILS`
/// does for the browse API.
const IMAGE_TAILS: [&str; 4] = ["blobs", "manifests", "tags", "referrers"];

/// The `/v2` paths that name no image. They are answered locally by whichever node receives them:
/// `/v2/` and `/v2/token` touch no database, and `_catalog` is an object-store listing.
pub const LOCAL_V2: [&str; 3] = ["", "token", "_catalog"];

pub fn is_v2_path(path: &str) -> bool {
    let p = path.trim_start_matches('/');
    p == "v2" || p.starts_with("v2/")
}

/// `Some((owner, name))` when the path names an image. Deliberately strict: the name is ONE
/// segment, so `/v2/a/b/c/manifests/x` is None rather than being folded into some other image.
pub fn image_route(path: &str) -> Option<(&str, &str)> {
    let mut it = path.trim_start_matches('/').strip_prefix("v2/")?.split('/');
    let (owner, name, tail) = (it.next()?, it.next()?, it.next()?);
    if !IMAGE_TAILS.contains(&tail) {
        return None;
    }
    (crate::store::valid_owner(owner) && crate::store::valid_segment(name))
        .then_some((owner, name))
}

/// The ownership-map key for an image.
///
/// `img/` is a prefix no git route can produce: `repo_of` emits it only for `/v2/` paths, and
/// `img` is a reserved owner name so no repo key begins with it either. `lib.rs` turns a key back
/// into pool coordinates with `split_once('/')`, which yields `("img", "{owner}/{name}")` — the
/// same pair `pool_coords` returns, so claim, renew, evict, and release need no knowledge of
/// images at all.
pub fn routing_key(owner: &str, name: &str) -> String {
    format!("img/{owner}/{name}")
}

pub fn pool_coords(owner: &str, name: &str) -> (&'static str, String) {
    ("img", format!("{owner}/{name}"))
}
```

Add to `src/lib.rs`, beside the other module declarations:

```rust
pub mod registry;
```

- [ ] **Step 4: Reserve the owner names**

In `src/store.rs`, replace `valid_owner`'s body:

```rust
/// Owner names the URL space has already spent.
///
/// `api` is the browse prefix. `v2` is the registry prefix, for the same reason: a repo owned by
/// `v2` would make `/v2/alice/info/refs` both that repo's git route and an image path. `img` is
/// not a URL prefix at all — it is the routing key registry paths derive, and a repo owned by
/// `img` would put its database at `repo/img/{name}`, nesting it inside the prefix every image
/// database lives under.
pub const RESERVED_OWNERS: [&str; 3] = ["api", "v2", "img"];

pub fn valid_owner(s: &str) -> bool {
    valid_segment(s) && !RESERVED_OWNERS.contains(&s)
}
```

- [ ] **Step 5: Teach the middleware about `/v2`**

In `src/http.rs`, extend `repo_of` — add this branch FIRST, before the `api_prefixed` branch:

```rust
    if crate::registry::is_v2_path(path) {
        let (owner, name) = crate::registry::image_route(path)?;
        return Some(crate::registry::routing_key(owner, name));
    }
```

Extend `is_git_route` so a malformed image path is refused rather than falling through to a handler:

```rust
fn is_git_route(path: &str) -> bool {
    git_shape(path) || api_route(path).is_some() || crate::registry::image_route(path).is_some()
}
```

And in `route_inner`, immediately after the two `api_prefixed` guards, add:

```rust
    // A `/v2/` path that names no image is either one of the three local endpoints — answered
    // here, on any node — or nothing at all. It must not fall through to `repo_of`'s git branch,
    // where `/v2/alice/info/refs` would otherwise be served as owner=`v2` having never routed.
    if crate::registry::is_v2_path(&path) && crate::registry::image_route(&path).is_none() {
        let tail = path.trim_start_matches('/').trim_start_matches("v2").trim_start_matches('/');
        let tail = tail.split('?').next().unwrap_or("");
        if crate::registry::LOCAL_V2.contains(&tail) {
            return next.run(req).await;
        }
        return crate::registry::oci_err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such image");
    }
```

`oci_err` does not exist yet — Task 3 adds it. For THIS task, return
`(StatusCode::NOT_FOUND, "not found").into_response()` and change it in Task 3.

- [ ] **Step 6: Run every test**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. If an existing test asserts `valid_owner("v2")` or `valid_owner("img")` is true, it was asserting the old reservation list — update it to the new list.

- [ ] **Step 7: Commit**

```bash
git add src/registry/mod.rs src/lib.rs src/store.rs src/http.rs tests/routing.rs
git commit -m "Route a registry path without letting it become a repo's"
```

---

### Task 2: Image storage — digests, keys, and the image database

**Files:**
- Create: `src/registry/store.rs`
- Modify: `src/registry/mod.rs` (`pub mod store;` and re-exports)
- Test: `tests/registry_store.rs`

**Interfaces:**
- Consumes: `registry::pool_coords` (Task 1), `crate::store::Store`.
- Produces:
  - `registry::Digest` — `{ algo: String, hex: String }`, `Digest::parse(&str) -> Option<Digest>`, `Display` as `sha256:{hex}`, `Digest::of(bytes: &[u8]) -> Digest`
  - `registry::store::blob_path(owner: &str, d: &Digest) -> OsPath`
  - `registry::store::manifest_path(owner: &str, name: &str, d: &Digest) -> OsPath`
  - `impl Store { pub async fn image_db(&self, owner: &str, name: &str) -> Result<Arc<Db>> }`
  - `impl Store { pub async fn image_exists(&self, owner: &str, name: &str) -> Result<bool> }`
  - `impl Store { pub async fn put_tag(&self, owner, name, tag: &str, d: &Digest) -> Result<()> }`
  - `impl Store { pub async fn tag(&self, owner, name, tag: &str) -> Result<Option<Digest>> }`
  - `impl Store { pub async fn tags(&self, owner, name) -> Result<Vec<String>> }` — sorted lexically
  - `impl Store { pub async fn delete_tag(&self, owner, name, tag: &str) -> Result<()> }`
  - `impl Store { pub async fn image_is_public(&self, owner, name) -> Result<bool> }`
  - `impl Store { pub async fn set_image_visibility(&self, owner, name, public: bool) -> Result<()> }`

- [ ] **Step 1: Write the failing test**

`tests/registry_store.rs`:

```rust
mod common;
use rustic_git::registry::{store as rstore, Digest};

#[test]
fn digests_parse_strictly() {
    let hex = "a".repeat(64);
    let d = Digest::parse(&format!("sha256:{hex}")).unwrap();
    assert_eq!(d.to_string(), format!("sha256:{hex}"));
    // Everything a path segment could smuggle in is refused.
    assert!(Digest::parse("sha256:short").is_none());
    assert!(Digest::parse(&format!("sha256:{}", "A".repeat(64))).is_none(), "uppercase hex");
    assert!(Digest::parse(&format!("sha512:{hex}")).is_none(), "unsupported algorithm");
    assert!(Digest::parse(&format!("sha256:{}/../../etc", "a".repeat(56))).is_none());
    assert!(Digest::parse("").is_none());
}

#[test]
fn digest_of_bytes_matches_the_wire_format() {
    // sha256 of the empty string, the value every registry client knows.
    assert_eq!(
        Digest::of(b"").to_string(),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn object_paths_are_owner_scoped() {
    let d = Digest::of(b"layer");
    assert_eq!(rstore::blob_path("acme", &d).to_string(), format!("blobs/acme/sha256/{}", d.hex));
    assert_eq!(
        rstore::manifest_path("acme", "nginx", &d).to_string(),
        format!("manifests/acme/nginx/sha256/{}", d.hex)
    );
}

#[tokio::test]
async fn tags_round_trip_and_sort() {
    let e = common::env().await;
    let d = Digest::of(b"manifest");
    e.store.put_tag("acme", "nginx", "v2", &d).await.unwrap();
    e.store.put_tag("acme", "nginx", "latest", &d).await.unwrap();
    assert_eq!(e.store.tags("acme", "nginx").await.unwrap(), vec!["latest", "v2"]);
    assert_eq!(e.store.tag("acme", "nginx", "latest").await.unwrap().unwrap().hex, d.hex);
    e.store.delete_tag("acme", "nginx", "latest").await.unwrap();
    assert_eq!(e.store.tags("acme", "nginx").await.unwrap(), vec!["v2"]);
    assert!(e.store.tag("acme", "nginx", "latest").await.unwrap().is_none());
}

#[tokio::test]
async fn an_image_and_a_repo_of_one_name_are_two_things() {
    let e = common::env().await;
    e.store.put_tag("acme", "nginx", "latest", &Digest::of(b"m")).await.unwrap();
    // The image exists; the repo of the same name does not.
    assert!(e.store.image_exists("acme", "nginx").await.unwrap());
    assert!(!e.store.repo_exists("acme", "nginx").await.unwrap());
}

#[tokio::test]
async fn images_are_private_until_told_otherwise() {
    let e = common::env().await;
    e.store.put_tag("acme", "nginx", "latest", &Digest::of(b"m")).await.unwrap();
    assert!(!e.store.image_is_public("acme", "nginx").await.unwrap());
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();
    assert!(e.store.image_is_public("acme", "nginx").await.unwrap());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_store`
Expected: FAIL — `registry::store` does not exist.

- [ ] **Step 3: Implement**

`src/registry/store.rs`:

```rust
//! Where an image's bytes and metadata live.
//!
//! Blobs are per-owner (`blobs/{owner}/sha256/{hex}`): a team that pushes twenty images off one
//! base layer stores it once, and the garbage collector only ever has to read one team's images to
//! know what is unreferenced. Manifest BYTES are objects; the tag map is not — tags live in the
//! image's database, where the single-writer guarantee makes two pushes to `:latest` order against
//! each other instead of racing in the object store.
use crate::store::Store;
use crate::Result;
use slatedb::object_store::path::Path as OsPath;
use slatedb::Db;
use std::sync::Arc;

/// A content digest, as it appears on the wire.
///
/// Parsing is the ONLY way a path segment becomes part of an object key, so it is strict on
/// purpose: lowercase hex, exactly 64 of it, algorithm `sha256`. Anything else — an upper-case
/// digest, a `..`, a second colon — is not a digest and never reaches the object store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    pub algo: String,
    pub hex: String,
}

impl Digest {
    pub fn parse(s: &str) -> Option<Digest> {
        let (algo, hex) = s.split_once(':')?;
        if algo != "sha256" || hex.len() != 64 {
            return None;
        }
        if !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return None;
        }
        Some(Digest { algo: algo.to_string(), hex: hex.to_string() })
    }

    pub fn of(bytes: &[u8]) -> Digest {
        use russh::keys::ssh_key::sha2::{Digest as _, Sha256};
        let hex: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
        Digest { algo: "sha256".into(), hex }
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.algo, self.hex)
    }
}

pub fn blob_path(owner: &str, d: &Digest) -> OsPath {
    OsPath::from(format!("blobs/{owner}/{}/{}", d.algo, d.hex))
}

pub fn manifest_path(owner: &str, name: &str, d: &Digest) -> OsPath {
    OsPath::from(format!("manifests/{owner}/{name}/{}/{}", d.algo, d.hex))
}

const IMAGE_KEY: &[u8] = b"image";
const PUBLIC_KEY: &[u8] = b"image/public";
fn tag_key(tag: &str) -> Vec<u8> {
    format!("image/tag/{tag}").into_bytes()
}
const TAG_PREFIX: &str = "image/tag/";

impl Store {
    /// The image's database. Opening one CREATES it, so callers that merely probe must go through
    /// `image_exists` — the same rule `db_for`/`repo_exists` follow for repos.
    pub async fn image_db(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        let (o, n) = crate::registry::pool_coords(owner, name);
        self.pool.get(o, &n).await
    }

    pub async fn image_exists(&self, owner: &str, name: &str) -> Result<bool> {
        let (o, n) = crate::registry::pool_coords(owner, name);
        if !self.pool.exists(o, &n).await? {
            return Ok(false);
        }
        Ok(self.image_db(owner, name).await?.get(IMAGE_KEY).await?.is_some())
    }

    /// Marks the image as existing. Registries create on first write, so every write path calls
    /// this rather than there being a create endpoint.
    async fn touch_image(&self, owner: &str, name: &str) -> Result<()> {
        self.image_db(owner, name).await?.put(IMAGE_KEY, b"1".as_slice()).await?;
        Ok(())
    }

    pub async fn put_tag(&self, owner: &str, name: &str, tag: &str, d: &Digest) -> Result<()> {
        self.touch_image(owner, name).await?;
        self.image_db(owner, name)
            .await?
            .put(tag_key(tag), d.to_string().into_bytes())
            .await?;
        Ok(())
    }

    pub async fn tag(&self, owner: &str, name: &str, tag: &str) -> Result<Option<Digest>> {
        if !self.image_exists(owner, name).await? {
            return Ok(None);
        }
        let v = self.image_db(owner, name).await?.get(tag_key(tag)).await?;
        Ok(v.and_then(|v| Digest::parse(&String::from_utf8_lossy(&v))))
    }

    pub async fn delete_tag(&self, owner: &str, name: &str, tag: &str) -> Result<()> {
        self.image_db(owner, name).await?.delete(tag_key(tag)).await?;
        Ok(())
    }

    /// Sorted lexically, which is the order the spec requires `tags/list` to return.
    pub async fn tags(&self, owner: &str, name: &str) -> Result<Vec<String>> {
        if !self.image_exists(owner, name).await? {
            return Ok(vec![]);
        }
        let db = self.image_db(owner, name).await?;
        let mut it = db.scan(tag_key("")..tag_key("\u{7f}")).await?;
        let mut out = vec![];
        while let Some(kv) = it.next().await? {
            let k = String::from_utf8_lossy(&kv.key).to_string();
            if let Some(t) = k.strip_prefix(TAG_PREFIX) {
                out.push(t.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    pub async fn image_is_public(&self, owner: &str, name: &str) -> Result<bool> {
        if !self.image_exists(owner, name).await? {
            return Ok(false);
        }
        Ok(self.image_db(owner, name).await?.get(PUBLIC_KEY).await?.as_deref() == Some(b"1"))
    }

    pub async fn set_image_visibility(&self, owner: &str, name: &str, public: bool) -> Result<()> {
        self.touch_image(owner, name).await?;
        self.image_db(owner, name)
            .await?
            .put(PUBLIC_KEY, if public { b"1".as_slice() } else { b"0".as_slice() })
            .await?;
        Ok(())
    }
}
```

Add to `src/registry/mod.rs`:

```rust
pub mod store;
pub use store::Digest;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --test registry_store -- --nocapture`
Expected: PASS. If `db.scan` has a different signature in the pinned SlateDB version, mirror exactly what `ownership::all` in `src/ownership.rs:322` does — that is the working scan in this codebase.

- [ ] **Step 5: Commit**

```bash
git add src/registry/store.rs src/registry/mod.rs tests/registry_store.rs
git commit -m "Give an image a database, a tag map, and a place for its bytes"
```

---

### Task 3: The error envelope, `GET /v2/`, and authentication

**Files:**
- Create: `src/registry/auth.rs`
- Create: `src/registry/routes.rs`
- Modify: `src/registry/mod.rs`, `src/http.rs` (mount the router on BOTH listeners; replace the Task 1 placeholder 404)
- Test: `tests/registry_http.rs`

**Interfaces:**
- Consumes: Task 1 and Task 2 exports, `crate::http::Trusted` (make it `pub(crate)` if it is not already).
- Produces:
  - `registry::oci_err(status: StatusCode, code: &str, message: &str) -> Response`
  - `registry::auth::challenge(scope: Option<&str>) -> Response` — 401 with `WWW-Authenticate: Bearer`
  - `registry::auth::caller(app: &App, trusted: &Trusted, headers: &HeaderMap) -> Result<Option<String>, Response>` — the authenticated owner, `None` for anonymous
  - `registry::auth::allow(app, trusted, headers, owner, name, write: bool) -> Result<(), Response>`
  - `registry::routes::v2_routes() -> Router<Arc<App>>`

- [ ] **Step 1: Add the test harness helpers**

`tests/common/mod.rs` has `env`/`app` but no "spin a router on a port" helper — `tests/http_e2e.rs`
has one inline. MOVE it to `tests/common/mod.rs` (do not write a second one) and add its peer twin:

```rust
/// Serve the PUBLIC router on an ephemeral port. Returns its base URL and the env behind it.
pub async fn serve_public() -> (String, TestEnv) {
    let e = env().await;
    let app = app(e.store.clone()).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, rustic_git::http::router(app)).await.unwrap();
    });
    (base, e)
}

/// The PEER router, where the browse API lives. Requests to it must carry the shared secret,
/// which `peer_get` adds.
pub async fn serve_peer() -> (String, TestEnv) {
    let e = env().await;
    let app = app(e.store.clone()).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, rustic_git::http::peer_router(app)).await.unwrap();
    });
    (base, e)
}

pub async fn peer_get(base: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .header(rustic_git::proxy::PEER_HEADER, "test-peer-secret")
        .send()
        .await
        .unwrap()
}
```

Update `tests/http_e2e.rs` to call the moved helper. Run `cargo test --test http_e2e` — it must
still pass before you go on.

- [ ] **Step 2: Write the failing test**

`tests/registry_http.rs`:

```rust
mod common;
use axum::http::StatusCode;

// Spins the public router on an ephemeral port and returns its base URL.
// (Mirror the harness in tests/http_e2e.rs — reuse its helper rather than writing a second one.)
async fn serve() -> (String, common::TestEnv) { common::serve_public().await }

#[tokio::test]
async fn v2_root_says_the_api_version() {
    let (base, _e) = serve().await;
    let r = reqwest::get(format!("{base}/v2/")).await.unwrap();
    // Anonymous: 401 with a challenge is correct and so is 200. This registry challenges.
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let www = r.headers().get("www-authenticate").unwrap().to_str().unwrap();
    assert!(www.starts_with("Bearer realm="), "got {www}");
    assert!(www.contains("/v2/token"), "the realm must point at the token endpoint: {www}");
    assert_eq!(r.headers().get("docker-distribution-api-version").unwrap(), "registry/2.0");
}

#[tokio::test]
async fn v2_root_with_a_token_is_200() {
    let (base, e) = serve().await;
    let token = e.store.create_token("acme").await.unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/"))
        .basic_auth("acme", Some(&token))
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn errors_use_the_oci_envelope() {
    let (base, _e) = serve().await;
    let r = reqwest::get(format!("{base}/v2/acme/nope/manifests/latest")).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn a_stranger_cannot_read_a_private_image() {
    let (base, e) = serve().await;
    e.store.put_tag("acme", "nginx", "latest", &rustic_git::registry::Digest::of(b"m")).await.unwrap();
    let other = e.store.create_token("other").await.unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/acme/nginx/tags/list"))
        .basic_auth("other", Some(&other))
        .send().await.unwrap();
    // Authenticated but not the owner: DENIED, not a challenge.
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "DENIED");
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test --test registry_http`
Expected: FAIL — no `/v2` routes; every request 404s.

- [ ] **Step 4: Implement the envelope**

In `src/registry/mod.rs`:

```rust
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub mod auth;
pub mod routes;

/// The spec's error body. Every `/v2` refusal goes through here: a client that gets a bare string
/// where it expects this JSON reports a confusing error and retries nothing.
pub fn oci_err(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"errors": [{"code": code, "message": message, "detail": null}]})
            .to_string(),
    )
        .into_response()
}
```

Replace the Task 1 placeholder in `route_inner` with `crate::registry::oci_err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such image")`.

- [ ] **Step 5: Implement authentication**

`src/registry/auth.rs`:

```rust
//! Both credential shapes, ending at one authorization call.
//!
//! Clients that follow the spec take the Bearer challenge and fetch a scoped token from
//! `/v2/token`. Clients that do not — and every `curl` in a debugging session — send Basic
//! directly. Accepting both costs one extra branch and removes a whole class of "docker login
//! worked but push did not" reports.
use crate::http::Trusted;
use crate::App;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use base64::Engine;

fn realm() -> String {
    // The externally reachable base URL. The challenge must name a URL the CLIENT can reach, not
    // this pod's address, so it is configuration rather than something derived from the request.
    std::env::var("RUSTIC_GIT_EXTERNAL_URL").unwrap_or_else(|_| "http://localhost:8080".into())
}

pub fn challenge(scope: Option<&str>) -> Response {
    let base = realm();
    let host = base.split("://").nth(1).unwrap_or("registry").to_string();
    let mut v = format!("Bearer realm=\"{base}/v2/token\",service=\"{host}\"");
    if let Some(s) = scope {
        v.push_str(&format!(",scope=\"{s}\""));
    }
    let mut r = crate::registry::oci_err(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "authentication required");
    r.headers_mut().insert(header::WWW_AUTHENTICATE, v.parse().unwrap());
    r
}

/// The authenticated owner, or `None` for an anonymous caller. `Err` is a response to return
/// as-is: a credential that was PRESENTED and did not verify is a refusal, not anonymity.
pub async fn caller(
    app: &App,
    trusted: &Trusted,
    headers: &HeaderMap,
) -> Result<Option<String>, Response> {
    // A peer already authenticated this client; `trust_peer` checked the shared secret.
    if let Some(o) = trusted.0.clone() {
        return Ok(Some(o));
    }
    let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    if let Some(b64) = v.strip_prefix("Basic ") {
        let token = base64::engine::general_purpose::STANDARD
            .decode(b64).ok()
            .and_then(|d| String::from_utf8(d).ok())
            .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()));
        let Some(token) = token else { return Err(challenge(None)) };
        return match app.store.owner_for_token(&token).await {
            Ok(Some(o)) => Ok(Some(o)),
            Ok(None) => Err(challenge(None)),
            Err(e) => Err(crate::http::internal_pub(e)),
        };
    }
    if let Some(jwt) = v.strip_prefix("Bearer ") {
        return match super::routes::verify_registry_token(app, jwt) {
            Some(o) => Ok(Some(o)),
            None => Err(challenge(None)),
        };
    }
    Err(challenge(None))
}

/// Authorize a caller against one image. `write` is false for pulls.
///
/// Anonymous on a private image gets the CHALLENGE (so the client knows to log in); an
/// authenticated stranger gets DENIED (so it knows logging in again will not help).
pub async fn allow(
    app: &App,
    trusted: &Trusted,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<Option<String>, Response> {
    let who = caller(app, trusted, headers).await?;
    if who.as_deref() == Some(owner) {
        return Ok(who);
    }
    let public = !write && app.store.image_is_public(owner, name).await.unwrap_or(false);
    if public {
        return Ok(who);
    }
    let scope = format!("repository:{owner}/{name}:{}", if write { "pull,push" } else { "pull" });
    Err(match who {
        None => challenge(Some(&scope)),
        Some(_) => crate::registry::oci_err(StatusCode::FORBIDDEN, "DENIED", "insufficient scope"),
    })
}
```

`internal_pub` is `http::internal` made `pub(crate)` — rename nothing, just widen its visibility, and widen `Trusted` to `pub(crate)` too if needed.

- [ ] **Step 6: Implement the router and mount it**

`src/registry/routes.rs` — for this task only `/v2/` and the token stub:

```rust
use crate::http::Trusted;
use crate::App;
use axum::{extract::State, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::get, Extension, Router};
use std::sync::Arc;

/// `GET /v2/` — the version check every client makes before anything else. It carries no image, so
/// it is answered by whichever node receives it.
async fn v2_root(State(app): State<Arc<App>>, Extension(trusted): Extension<Trusted>, headers: HeaderMap) -> Response {
    match super::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(_)) => (
            StatusCode::OK,
            [("docker-distribution-api-version", "registry/2.0")],
            "{}",
        ).into_response(),
        Ok(None) => with_version(super::auth::challenge(None)),
        Err(r) => with_version(r),
    }
}

fn with_version(mut r: Response) -> Response {
    r.headers_mut().insert("docker-distribution-api-version", "registry/2.0".parse().unwrap());
    r
}

pub fn v2_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2", get(v2_root))
}

/// Verifies a token minted by `/v2/token`; `Some(owner)` when it is ours and unexpired.
/// Task 4 replaces this stub with the real verification.
pub fn verify_registry_token(_app: &App, _jwt: &str) -> Option<String> {
    None
}
```

In `src/http.rs`, add `.merge(crate::registry::routes::v2_routes())` to BOTH `router` and `peer_router`, alongside `git_routes()`.

- [ ] **Step 7: Run the tests**

Run: `cargo test --test registry_http && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/registry tests/registry_http.rs src/http.rs
git commit -m "Answer /v2/ and say how to authenticate against it"
```

---

### Task 4: `/v2/token`

**Files:**
- Modify: `src/registry/routes.rs`, `src/jwt.rs`
- Test: `tests/registry_http.rs`

**Interfaces:**
- Consumes: Task 3's `challenge`, `caller`.
- Produces: `GET /v2/token?service=&scope=&account=` → `{"token","access_token","expires_in","issued_at"}`; a working `verify_registry_token(app, jwt) -> Option<String>`.

- [ ] **Step 1: Write the failing test**

Append to `tests/registry_http.rs`:

```rust
#[tokio::test]
async fn the_token_endpoint_mints_a_usable_bearer() {
    let (base, e) = serve().await;
    let token = e.store.create_token("acme").await.unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/token?service=localhost&scope=repository:acme/nginx:pull,push"))
        .basic_auth("acme", Some(&token))
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: serde_json::Value = r.json().await.unwrap();
    let bearer = body["token"].as_str().unwrap().to_string();
    // Both field names, because clients disagree about which one they read.
    assert_eq!(body["access_token"].as_str().unwrap(), bearer);
    assert!(body["expires_in"].as_u64().unwrap() > 0);

    let r = reqwest::Client::new()
        .get(format!("{base}/v2/"))
        .bearer_auth(&bearer)
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_bad_credential_gets_no_token() {
    let (base, _e) = serve().await;
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/token?scope=repository:acme/nginx:pull"))
        .basic_auth("acme", Some("not-a-token"))
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_forged_bearer_is_refused() {
    let (base, _e) = serve().await;
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/"))
        .bearer_auth("not.a.jwt")
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_http the_token_endpoint`
Expected: FAIL — 404, no such route.

- [ ] **Step 3: Add registry claims to the JWT**

In `src/jwt.rs`, beside `mint`:

```rust
/// A registry bearer token: it names the owner it authenticates and nothing else.
///
/// Scope is recorded but NOT enforced from the token — authorization is re-checked per request
/// against the image, so a token that over-claims grants nothing extra. Recording it keeps the
/// response honest to clients that read it back.
pub fn mint_registry(&self, owner: &str, scope: &str, ttl_secs: u64) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| crate::err(e.to_string()))?
        .as_secs();
    let claims = serde_json::json!({
        "sub": owner,
        "scope": scope,
        "iat": now,
        "exp": now + ttl_secs,
        "typ": "registry",
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &self.encoding,
    )
    .map_err(|e| crate::err(e.to_string()))
}

/// `Some(owner)` when the token is ours, unexpired, and of the registry type.
pub fn verify_registry(&self, token: &str) -> Option<String> {
    let mut v = jsonwebtoken::Validation::default();
    v.set_required_spec_claims(&["exp", "sub"]);
    let data = jsonwebtoken::decode::<serde_json::Value>(token, &self.decoding, &v).ok()?;
    (data.claims["typ"] == "registry")
        .then(|| data.claims["sub"].as_str().map(str::to_string))?
}
```

Match the field names `Jwt` actually uses for its keys (`self.encoding`/`self.decoding` here) — read `src/jwt.rs:35-70` and use whatever is there.

- [ ] **Step 4: Implement the endpoint**

In `src/registry/routes.rs`:

```rust
/// How long a registry bearer lives. Long enough for a large push to finish on a slow link, short
/// enough that a leaked one is not a standing credential.
const TOKEN_TTL: u64 = 15 * 60;

#[derive(serde::Deserialize)]
struct TokenQuery {
    #[serde(default)]
    scope: String,
    #[serde(default)]
    service: String,
}

/// `GET /v2/token` — exchange a long-lived credential for a short-lived bearer.
async fn token(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<TokenQuery>,
) -> Response {
    let _ = q.service;
    let who = match super::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(o)) => o,
        // Anonymous is allowed to ask, and gets a token for nobody: it can still pull public
        // images. Refusing here would break anonymous pull for spec-following clients, which
        // always visit the token endpoint before the pull.
        Ok(None) => String::new(),
        Err(r) => return r,
    };
    let jwt = match app.jwt.mint_registry(&who, &q.scope, TOKEN_TTL) {
        Ok(t) => t,
        Err(e) => return crate::http::internal_pub(e),
    };
    let issued = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    axum::Json(serde_json::json!({
        "token": jwt,
        "access_token": jwt,
        "expires_in": TOKEN_TTL,
        "issued_at": issued,
    }))
    .into_response()
}

pub fn verify_registry_token(app: &App, jwt: &str) -> Option<String> {
    let owner = app.jwt.verify_registry(jwt)?;
    // A token minted for the anonymous caller authenticates nobody.
    (!owner.is_empty()).then_some(owner)
}
```

Register it: `.route("/v2/token", get(token))`.

If `App` has no `jwt` field, add one — `pub jwt: Arc<crate::jwt::Jwt>`, built in `App::new` from `RUSTIC_GIT_JWT_SECRET` and, when unset, from a per-process random secret (tokens then die with the process, which is correct for a dev run and visible in a fleet as "log in again").

- [ ] **Step 5: Run the tests**

Run: `cargo test --test registry_http && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/registry/routes.rs src/jwt.rs src/lib.rs tests/registry_http.rs
git commit -m "Exchange a token for a bearer the way the spec says"
```

---

### Task 5: Pull a blob, and push one in a single request

**Files:**
- Create: `src/registry/blobs.rs`
- Modify: `src/registry/routes.rs`, `src/registry/mod.rs`
- Test: `tests/registry_blobs.rs`

**Interfaces:**
- Consumes: `Digest`, `blob_path`, `auth::allow`.
- Produces:
  - `GET|HEAD /v2/{o}/{n}/blobs/{digest}`
  - `POST /v2/{o}/{n}/blobs/uploads/?digest=` (single-request push)
  - `POST /v2/{o}/{n}/blobs/uploads/` then `PUT .../{uuid}?digest=` (two-request push)
  - `registry::blobs::max_layer() -> u64` — from `RUSTIC_GIT_MAX_LAYER`, default 10 GiB

- [ ] **Step 1: Write the failing test**

`tests/registry_blobs.rs`:

```rust
mod common;
use axum::http::StatusCode;
use rustic_git::registry::Digest;

async fn authed() -> (String, common::TestEnv, reqwest::Client, String) {
    let (base, e) = common::serve_public().await;
    let token = e.store.create_token("acme").await.unwrap();
    (base, e, reqwest::Client::new(), token)
}

#[tokio::test]
async fn a_blob_pushed_in_one_request_comes_back() {
    let (base, _e, c, token) = authed().await;
    let body = b"layer bytes".to_vec();
    let d = Digest::of(&body);
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("location").unwrap().to_str().unwrap(), format!("/v2/acme/nginx/blobs/{d}"));

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d.to_string());
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}

#[tokio::test]
async fn a_blob_whose_digest_lies_is_refused() {
    let (base, _e, c, token) = authed().await;
    let wrong = Digest::of(b"something else");
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={wrong}"))
        .basic_auth("acme", Some(&token)).body(b"layer bytes".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "DIGEST_INVALID");
}

#[tokio::test]
async fn head_answers_size_without_the_body() {
    let (base, _e, c, token) = authed().await;
    let body = b"layer bytes".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    let r = c.head(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("content-length").unwrap().to_str().unwrap(), body.len().to_string());
    assert!(r.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_absent_blob_is_blob_unknown() {
    let (base, _e, c, token) = authed().await;
    let d = Digest::of(b"never pushed");
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "BLOB_UNKNOWN");
}

#[tokio::test]
async fn the_two_request_push_works() {
    let (base, _e, c, token) = authed().await;
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    assert!(loc.contains("/blobs/uploads/"), "got {loc}");

    let body = b"whole layer".to_vec();
    let d = Digest::of(&body);
    let r = c.put(format!("{base}{loc}?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}

#[tokio::test]
async fn a_stranger_cannot_push() {
    let (base, e, c, _token) = authed().await;
    let other = e.store.create_token("other").await.unwrap();
    let body = b"x".to_vec();
    let d = Digest::of(&body);
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("other", Some(&other)).body(body).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_blobs`
Expected: FAIL — 404 on every route.

- [ ] **Step 3: Implement**

`src/registry/blobs.rs`:

```rust
//! Blob pull and the two single-shot push forms. Chunked upload is `uploads.rs` (next task).
use super::{auth, oci_err, store::blob_path, Digest};
use crate::http::Trusted;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use slatedb::object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use std::collections::HashMap;
use std::sync::Arc;

/// Largest single layer accepted, checked against Content-Length BEFORE any byte is read: an
/// unbounded push must not be able to fill a node's disk. Override with RUSTIC_GIT_MAX_LAYER.
pub fn max_layer() -> u64 {
    std::env::var("RUSTIC_GIT_MAX_LAYER").ok().and_then(|v| v.parse().ok())
        .unwrap_or(10 * 1024 * 1024 * 1024)
}

pub async fn get_blob(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
) -> Response {
    blob_response(app, trusted, headers, owner, name, digest, true).await
}

pub async fn head_blob(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
) -> Response {
    blob_response(app, trusted, headers, owner, name, digest, false).await
}

async fn blob_response(
    app: Arc<App>,
    trusted: Trusted,
    headers: HeaderMap,
    owner: String,
    name: String,
    digest: String,
    with_body: bool,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let Some(d) = Digest::parse(&digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    let path = blob_path(&owner, &d);
    let meta = match app.store.os.head(&path).await {
        Ok(m) => m,
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            return oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    let hdrs = [
        (header::CONTENT_LENGTH, meta.size.to_string()),
        (header::CONTENT_TYPE, "application/octet-stream".into()),
        (
            header::HeaderName::from_static("docker-content-digest"),
            d.to_string(),
        ),
    ];
    if !with_body {
        return (StatusCode::OK, hdrs).into_response();
    }
    // ponytail: whole-blob read. Layers are capped by max_layer, and the object store client
    // buffers anyway; stream with `get`'s ByteStream if large-layer memory ever shows up in a
    // profile.
    match app.store.os.get(&path).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => (StatusCode::OK, hdrs, b).into_response(),
            Err(e) => crate::http::internal_pub(e.into()),
        },
        Err(e) => crate::http::internal_pub(e.into()),
    }
}

/// `POST /v2/{o}/{n}/blobs/uploads/`
///
/// Three shapes arrive here: `?digest=` with a body (push it now), `?mount=&from=` (Task 7), and
/// bare (open a session, Task 6). Only the first is implemented in this task; the bare form opens
/// a session whose uuid the PUT below completes.
pub async fn start_upload(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if let Some(digest) = q.get("digest") {
        return finish_blob(&app, &owner, &name, digest, body).await;
    }
    super::uploads::open_session(&app, &owner, &name).await
}

/// `PUT /v2/{o}/{n}/blobs/uploads/{uuid}?digest=` — completes a session. When the body carries the
/// whole blob and no chunk was PATCHed, this is the two-request push.
pub async fn finish_upload(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    let Some(digest) = q.get("digest") else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "digest query parameter required");
    };
    super::uploads::complete(&app, &owner, &name, &uuid, digest, body).await
}

/// Verify and store one whole blob. The digest is checked BEFORE the object lands, so a corrupt
/// layer never becomes readable under a name that promises different bytes.
pub(super) async fn finish_blob(
    app: &App,
    owner: &str,
    name: &str,
    digest: &str,
    body: Bytes,
) -> Response {
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    if body.len() as u64 > max_layer() {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large");
    }
    if Digest::of(&body) != d {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest");
    }
    if let Err(e) = app.store.os.put(&blob_path(owner, &d), PutPayload::from(body)).await {
        return crate::http::internal_pub(e.into());
    }
    // The image now exists, even with no manifest yet: a push that uploads layers and then fails
    // should leave something the owner can see and clean up.
    if let Err(e) = app.store.set_image_visibility(owner, name, false).await {
        return crate::http::internal_pub(e);
    }
    created(owner, name, &d)
}

pub(super) fn created(owner: &str, name: &str, d: &Digest) -> Response {
    (
        StatusCode::CREATED,
        [
            (header::LOCATION, format!("/v2/{owner}/{name}/blobs/{d}")),
            (
                header::HeaderName::from_static("docker-content-digest"),
                d.to_string(),
            ),
        ],
    )
        .into_response()
}
```

Note: `set_image_visibility(.., false)` would clobber a public image back to private on every push. Use `touch_image` instead — make it `pub(crate)` in `registry/store.rs` and call `app.store.touch_image(owner, name)`.

Register in `v2_routes()`:

```rust
.route("/v2/{owner}/{name}/blobs/{digest}", get(blobs::get_blob).head(blobs::head_blob))
.route("/v2/{owner}/{name}/blobs/uploads/", post(blobs::start_upload))
.route("/v2/{owner}/{name}/blobs/uploads/{uuid}", put(blobs::finish_upload))
.layer(axum::extract::DefaultBodyLimit::max(crate::http::max_body()))
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --test registry_blobs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/registry tests/registry_blobs.rs
git commit -m "Push a layer and pull it back"
```

---

### Task 6: Chunked, resumable uploads

**Files:**
- Create: `src/registry/uploads.rs`
- Modify: `src/registry/routes.rs`, `src/registry/mod.rs`
- Test: `tests/registry_uploads.rs`

**Interfaces:**
- Consumes: Task 5's `finish_blob`, `created`; Task 2's `image_db`.
- Produces:
  - `uploads::open_session(app, owner, name) -> Response` (202 + Location + `Docker-Upload-UUID`)
  - `uploads::complete(app, owner, name, uuid, digest, body) -> Response`
  - `PATCH /v2/{o}/{n}/blobs/uploads/{uuid}`, `GET` (status), `DELETE` (cancel)

**Design note for the implementer:** a session's accumulated bytes are held as a *staging object* at `uploads/{owner}/{name}/{uuid}`, appended by rewriting — NOT `put_multipart`. Reason: the running sha256 cannot be resumed across requests (`sha2` state does not serialize), so completion re-reads the staged object and hashes it once. That is one extra read of the layer and it removes all in-memory session state, which means a session survives the image moving nodes. Mark it:

```rust
// ponytail: completion re-reads the staged object to hash it — one extra read per layer. The
// alternative is a resumable hasher (sha2 has no serializable state) or holding the hasher in
// node memory, which loses the session when the image moves nodes. Revisit if layer pushes show
// up in a profile.
```

- [ ] **Step 1: Write the failing test**

`tests/registry_uploads.rs`:

```rust
mod common;
use axum::http::StatusCode;
use rustic_git::registry::Digest;

#[tokio::test]
async fn a_layer_uploads_in_chunks() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();

    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert!(r.headers().get("docker-upload-uuid").is_some());
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();

    let (a, b) = (b"first half ".to_vec(), b"second half".to_vec());
    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", format!("0-{}", a.len() - 1))
        .body(a.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), format!("0-{}", a.len() - 1));

    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", format!("{}-{}", a.len(), a.len() + b.len() - 1))
        .body(b.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);

    let whole = [a.clone(), b.clone()].concat();
    let d = Digest::of(&whole);
    let r = c.put(format!("{base}{loc}?digest={d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), whole);
}

#[tokio::test]
async fn a_chunk_out_of_order_is_416() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    // Starts at 50 when the session holds 0 bytes: a gap.
    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "50-59")
        .body(b"0123456789".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn a_session_reports_its_progress_and_can_be_cancelled() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "0-4").body(b"hello".to_vec()).send().await.unwrap();

    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), "0-4");

    let r = c.delete(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_completed_upload_whose_digest_lies_is_refused_and_stores_nothing() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "0-4").body(b"hello".to_vec()).send().await.unwrap();
    let lie = Digest::of(b"not hello");
    let r = c.put(format!("{base}{loc}?digest={lie}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{lie}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_uploads`
Expected: FAIL — PATCH/GET/DELETE on a session are 404 or 405.

- [ ] **Step 3: Implement**

`src/registry/uploads.rs`:

```rust
//! Resumable blob uploads.
//!
//! A session is two things: a staging object holding the bytes received so far, and a row in the
//! image's database recording how many they are. Both are addressable from any node that owns the
//! image, so a session survives the image moving — nothing about it lives in this process.
use super::{auth, blobs, oci_err, store::blob_path, Digest};
use crate::http::Trusted;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use slatedb::object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use std::sync::Arc;

fn staging(owner: &str, name: &str, uuid: &str) -> slatedb::object_store::path::Path {
    slatedb::object_store::path::Path::from(format!("uploads/{owner}/{name}/{uuid}"))
}

fn session_key(uuid: &str) -> Vec<u8> {
    format!("image/upload/{uuid}").into_bytes()
}

/// A uuid, and nothing that could be a path. Generated here, checked on the way back in: a session
/// id from a client is a path segment, and a path segment is never trusted.
fn valid_uuid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

async fn received(app: &App, owner: &str, name: &str, uuid: &str) -> crate::Result<Option<u64>> {
    let db = app.store.image_db(owner, name).await?;
    Ok(db
        .get(session_key(uuid))
        .await?
        .and_then(|v| String::from_utf8_lossy(&v).parse().ok()))
}

pub async fn open_session(app: &App, owner: &str, name: &str) -> Response {
    let mut b = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
    let uuid: String = b.iter().map(|x| format!("{x:02x}")).collect();
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::http::internal_pub(e);
    }
    let db = match app.store.image_db(owner, name).await {
        Ok(d) => d,
        Err(e) => return crate::http::internal_pub(e),
    };
    if let Err(e) = db.put(session_key(&uuid), b"0".as_slice()).await {
        return crate::http::internal_pub(e.into());
    }
    accepted(owner, name, &uuid, 0)
}

/// 202 with the session's URL and how much of the blob it holds. `Range` is inclusive and a
/// session holding nothing has no range at all — a `0-0` there would claim one byte.
fn accepted(owner: &str, name: &str, uuid: &str, len: u64) -> Response {
    let mut r = (
        StatusCode::ACCEPTED,
        [
            (header::LOCATION, format!("/v2/{owner}/{name}/blobs/uploads/{uuid}")),
            (header::HeaderName::from_static("docker-upload-uuid"), uuid.to_string()),
        ],
    )
        .into_response();
    if len > 0 {
        r.headers_mut().insert(header::RANGE, format!("0-{}", len - 1).parse().unwrap());
    }
    r
}

/// `PATCH` — one chunk. Ranges must be contiguous, per the spec: a gap is 416, and so is a chunk
/// that would rewrite bytes already received.
pub async fn patch(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if !valid_uuid(&uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    let have = match received(&app, &owner, &name, &uuid).await {
        Ok(Some(n)) => n,
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::http::internal_pub(e),
    };
    // A Content-Range that does not continue where the session left off is 416. Absent is allowed:
    // a client streaming one chunk need not send it.
    if let Some(cr) = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
        let start: u64 = cr.trim_start_matches("bytes ").split('-').next()
            .and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
        if start != have {
            return oci_err(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "BLOB_UPLOAD_INVALID",
                "chunk does not continue the upload",
            );
        }
    }
    if have + body.len() as u64 > blobs::max_layer() {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large");
    }
    // ponytail: read-modify-write of the staging object, so a chunked push of an N-byte layer
    // moves O(N * chunks) bytes. Correct and stateless, which is what makes a session survive the
    // image moving nodes. Swap for the object store's multipart API if large pushes get slow —
    // that needs the part list persisted alongside the byte count.
    let path = staging(&owner, &name, &uuid);
    let mut buf = match app.store.os.get(&path).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => return crate::http::internal_pub(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => vec![],
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    buf.extend_from_slice(&body);
    let len = buf.len() as u64;
    if let Err(e) = app.store.os.put(&path, PutPayload::from(buf)).await {
        return crate::http::internal_pub(e.into());
    }
    let db = match app.store.image_db(&owner, &name).await {
        Ok(d) => d,
        Err(e) => return crate::http::internal_pub(e),
    };
    if let Err(e) = db.put(session_key(&uuid), len.to_string().into_bytes()).await {
        return crate::http::internal_pub(e.into());
    }
    accepted(&owner, &name, &uuid, len)
}

/// `GET` — how far the session got. 204 with a `Range`, per the spec.
pub async fn status(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    match received(&app, &owner, &name, &uuid).await {
        Ok(Some(n)) => {
            let mut r = (
                StatusCode::NO_CONTENT,
                [(header::HeaderName::from_static("docker-upload-uuid"), uuid.clone())],
            )
                .into_response();
            if n > 0 {
                r.headers_mut().insert(header::RANGE, format!("0-{}", n - 1).parse().unwrap());
            }
            r
        }
        Ok(None) => oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => crate::http::internal_pub(e),
    }
}

/// `DELETE` — cancel. Idempotent in effect: the staged bytes and the row both go.
pub async fn cancel(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if !valid_uuid(&uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    discard(&app, &owner, &name, &uuid).await;
    StatusCode::NO_CONTENT.into_response()
}

async fn discard(app: &App, owner: &str, name: &str, uuid: &str) {
    let _ = app.store.os.delete(&staging(owner, name, uuid)).await;
    if let Ok(db) = app.store.image_db(owner, name).await {
        let _ = db.delete(session_key(uuid)).await;
    }
}

/// `PUT` — finish. A body here is the last chunk, which is how the two-request push arrives.
pub async fn complete(
    app: &App,
    owner: &str,
    name: &str,
    uuid: &str,
    digest: &str,
    body: Bytes,
) -> Response {
    if !valid_uuid(uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    match received(app, owner, name, uuid).await {
        Ok(Some(_)) => {}
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::http::internal_pub(e),
    }
    let path = staging(owner, name, uuid);
    let mut buf = match app.store.os.get(&path).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => return crate::http::internal_pub(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => vec![],
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    buf.extend_from_slice(&body);
    // Hashed here, from the staged bytes, because the running hash cannot be carried across
    // requests. See the module note.
    if Digest::of(&buf) != d {
        // The session stays open: a client that mis-stated the digest may retry the PUT. Only the
        // successful path retires it.
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest");
    }
    if let Err(e) = app.store.os.put(&blob_path(owner, &d), PutPayload::from(buf)).await {
        return crate::http::internal_pub(e.into());
    }
    discard(app, owner, name, uuid).await;
    blobs::created(owner, name, &d)
}
```

Register the remaining methods on the session route:

```rust
.route(
    "/v2/{owner}/{name}/blobs/uploads/{uuid}",
    put(blobs::finish_upload).patch(uploads::patch).get(uploads::status).delete(uploads::cancel),
)
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --test registry_uploads && cargo test --test registry_blobs`
Expected: PASS both — the two-request push from Task 5 now goes through the session path.

- [ ] **Step 5: Commit**

```bash
git add src/registry tests/registry_uploads.rs
git commit -m "Take a layer in chunks, and let a client resume one"
```

---

### Task 7: Cross-repo mount and blob delete

**Files:**
- Modify: `src/registry/blobs.rs`, `src/registry/routes.rs`
- Test: `tests/registry_blobs.rs`

**Interfaces:**
- Consumes: Task 5's `start_upload`, `created`.
- Produces: `POST .../blobs/uploads/?mount={digest}&from={owner}/{name}`; `DELETE /v2/{o}/{n}/blobs/{digest}`.

- [ ] **Step 1: Write the failing test**

Append to `tests/registry_blobs.rs`:

```rust
#[tokio::test]
async fn a_layer_mounts_from_another_image_in_the_same_team() {
    let (base, _e, c, token) = authed().await;
    let body = b"shared base layer".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();

    let r = c.post(format!("{base}/v2/acme/api/blobs/uploads/?mount={d}&from=acme/nginx"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("location").unwrap().to_str().unwrap(), format!("/v2/acme/api/blobs/{d}"));

    // Readable through the mounting image without a byte having moved.
    let r = c.get(format!("{base}/v2/acme/api/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}

#[tokio::test]
async fn mounting_across_teams_falls_back_to_a_session() {
    let (base, e, c, token) = authed().await;
    let other = e.store.create_token("other").await.unwrap();
    let body = b"other team's layer".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/other/thing/blobs/uploads/?digest={d}"))
        .basic_auth("other", Some(&other)).body(body).send().await.unwrap();

    // Blobs are per-owner, so this cannot be a metadata-only mount. The spec's answer is 202:
    // "upload it yourself".
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?mount={d}&from=other/thing"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert!(r.headers().get("location").is_some());
}

#[tokio::test]
async fn a_blob_can_be_deleted() {
    let (base, _e, c, token) = authed().await;
    let body = b"delete me".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body).send().await.unwrap();
    let r = c.delete(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_blobs mount`
Expected: FAIL — `?mount=` currently opens an ordinary session and returns 202 with no blob check; the delete route does not exist.

- [ ] **Step 3: Implement**

In `blobs::start_upload`, before the `digest` branch:

```rust
    // Cross-repo mount. Blobs are per-OWNER, so a mount inside the team is a no-op — the bytes are
    // already at the path the mounting image reads. Across teams there is nothing to point at, and
    // the spec's fallback is exactly right: 202, and the client uploads it.
    if let (Some(mount), Some(from)) = (q.get("mount"), q.get("from")) {
        let Some(d) = Digest::parse(mount) else {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
        };
        let from_owner = from.split('/').next().unwrap_or_default();
        if from_owner == owner
            && app.store.os.head(&blob_path(&owner, &d)).await.is_ok()
        {
            if let Err(e) = app.store.touch_image(&owner, &name).await {
                return crate::http::internal_pub(e);
            }
            return created(&owner, &name, &d);
        }
        return super::uploads::open_session(&app, &owner, &name).await;
    }
```

Add the delete handler:

```rust
/// `DELETE /v2/{o}/{n}/blobs/{digest}` — remove the object.
///
/// Deleting here does NOT check whether a manifest still references it: the client asked, the
/// client owns it. What is never done is the reverse — no manifest delete removes a blob. That is
/// the sweeper's job, because only it can see every image that might share the layer.
pub async fn delete_blob(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    let Some(d) = Digest::parse(&digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    match app.store.os.delete(&blob_path(&owner, &d)).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => crate::http::internal_pub(e.into()),
    }
}
```

Route: `.delete(blobs::delete_blob)` on the existing `/blobs/{digest}` route.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test registry_blobs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/registry tests/registry_blobs.rs
git commit -m "Mount a layer a team already has, and let one be deleted"
```

---

### Task 8: Manifests and tags

**Files:**
- Create: `src/registry/manifests.rs`
- Modify: `src/registry/routes.rs`, `src/registry/mod.rs`
- Test: `tests/registry_manifests.rs`

**Interfaces:**
- Consumes: `Digest`, `manifest_path`, `put_tag`/`tag`/`tags`/`delete_tag`, `auth::allow`.
- Produces: `PUT|GET|HEAD|DELETE /v2/{o}/{n}/manifests/{reference}`, `GET /v2/{o}/{n}/tags/list?n=&last=`.

- [ ] **Step 1: Write the failing test**

`tests/registry_manifests.rs`:

```rust
mod common;
use axum::http::StatusCode;
use rustic_git::registry::Digest;

const MEDIA: &str = "application/vnd.oci.image.manifest.v1+json";

fn manifest() -> Vec<u8> {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": Digest::of(b"cfg").to_string(), "size": 3},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": Digest::of(b"layer").to_string(), "size": 5}]
    }).to_string().into_bytes()
}

async fn pushed() -> (String, common::TestEnv, reqwest::Client, String, Vec<u8>, Digest) {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let m = manifest();
    let d = Digest::of(&m);
    (base, e, c, token, m, d)
}

#[tokio::test]
async fn a_manifest_pushed_by_tag_comes_back_by_tag_and_by_digest() {
    let (base, _e, c, token, m, d) = pushed().await;
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d.to_string());
    assert_eq!(r.headers().get("location").unwrap().to_str().unwrap(), format!("/v2/acme/nginx/manifests/{d}"));

    for reference in ["latest", &d.to_string()] {
        let r = c.get(format!("{base}/v2/acme/nginx/manifests/{reference}"))
            .basic_auth("acme", Some(&token)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "reading {reference}");
        assert_eq!(r.headers().get("content-type").unwrap().to_str().unwrap(), MEDIA);
        assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d.to_string());
        assert_eq!(r.bytes().await.unwrap().to_vec(), m, "bytes must be byte-identical: the digest is over them");
    }
}

#[tokio::test]
async fn a_manifest_put_by_digest_that_does_not_match_is_refused() {
    let (base, _e, c, token, m, _d) = pushed().await;
    let lie = Digest::of(b"different");
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{lie}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "DIGEST_INVALID");
}

#[tokio::test]
async fn an_unknown_manifest_is_manifest_unknown() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/nope"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_UNKNOWN");
}

#[tokio::test]
async fn tags_list_sorts_and_paginates() {
    let (base, _e, c, token, m, _d) = pushed().await;
    for t in ["v3", "v1", "v2"] {
        c.put(format!("{base}/v2/acme/nginx/manifests/{t}"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(m.clone()).send().await.unwrap();
    }
    let r = c.get(format!("{base}/v2/acme/nginx/tags/list"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["name"], "acme/nginx");
    assert_eq!(b["tags"], serde_json::json!(["v1", "v2", "v3"]));

    let r = c.get(format!("{base}/v2/acme/nginx/tags/list?n=2"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["tags"], serde_json::json!(["v1", "v2"]));
    assert!(r.headers().get("link").is_some(), "a truncated list must carry a Link header");

    let r = c.get(format!("{base}/v2/acme/nginx/tags/list?n=2&last=v2"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["tags"], serde_json::json!(["v3"]));
}

#[tokio::test]
async fn deleting_a_tag_leaves_the_manifest_and_deleting_the_manifest_takes_its_tags() {
    let (base, _e, c, token, m, d) = pushed().await;
    for t in ["latest", "v1"] {
        c.put(format!("{base}/v2/acme/nginx/manifests/{t}"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(m.clone()).send().await.unwrap();
    }
    let r = c.delete(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    // The tag is gone; the manifest and the other tag are not.
    assert_eq!(
        c.get(format!("{base}/v2/acme/nginx/manifests/latest")).basic_auth("acme", Some(&token))
            .send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        c.get(format!("{base}/v2/acme/nginx/manifests/v1")).basic_auth("acme", Some(&token))
            .send().await.unwrap().status(),
        StatusCode::OK
    );

    // By digest: the manifest goes, and every tag pointing at it goes with it.
    let r = c.delete(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(
        c.get(format!("{base}/v2/acme/nginx/manifests/v1")).basic_auth("acme", Some(&token))
            .send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_public_image_pulls_anonymously_and_still_refuses_a_push() {
    let (base, e, c, token, m, _d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();

    let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/v9"))
        .header("content-type", MEDIA).body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}
```

Also append the concurrency case the spec calls for — the reason tags live in the database and not
in the object store:

```rust
#[tokio::test]
async fn two_pushes_to_one_tag_leave_it_pointing_at_exactly_one_of_them() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let a = serde_json::json!({"schemaVersion": 2, "mediaType": MEDIA, "layers": [], "annotations": {"who": "a"}})
        .to_string().into_bytes();
    let b = serde_json::json!({"schemaVersion": 2, "mediaType": MEDIA, "layers": [], "annotations": {"who": "b"}})
        .to_string().into_bytes();
    let (da, db) = (Digest::of(&a), Digest::of(&b));
    let put = |body: Vec<u8>| {
        let (c, base, token) = (c.clone(), base.clone(), token.clone());
        async move {
            c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
                .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
                .body(body).send().await.unwrap().status()
        }
    };
    let (ra, rb) = tokio::join!(put(a), put(b));
    assert_eq!((ra, rb), (StatusCode::CREATED, StatusCode::CREATED));

    // Whichever won, the tag resolves to ONE of them and reading it twice agrees.
    let read = || async {
        let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
            .basic_auth("acme", Some(&token)).send().await.unwrap();
        r.headers().get("docker-content-digest").unwrap().to_str().unwrap().to_string()
    };
    let first = read().await;
    assert!(first == da.to_string() || first == db.to_string(), "got {first}");
    assert_eq!(read().await, first, "the tag must not flap between the two");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_manifests`
Expected: FAIL — no manifest routes.

- [ ] **Step 3: Implement**

`src/registry/manifests.rs`:

```rust
//! Manifests and the tag map.
//!
//! Manifest BYTES are stored verbatim and returned verbatim. The digest is over those exact bytes,
//! so re-serializing a parsed manifest — even to identical-looking JSON — changes the digest and
//! breaks every client that verifies one. Nothing here parses a manifest except to read `subject`
//! for the referrers index.
use super::{
    auth, oci_err,
    store::{manifest_path, Digest},
};
use crate::http::Trusted;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use slatedb::object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use std::collections::HashMap;
use std::sync::Arc;

const MEDIA_TYPE_KEY_PREFIX: &str = "image/manifest-type/";

/// The largest manifest accepted. Manifests are lists of digests; anything approaching this is not
/// a manifest.
const MAX_MANIFEST: usize = 4 * 1024 * 1024;

/// A reference is either a digest or a tag. Tags are the same shape as any other name segment.
enum Reference {
    Digest(Digest),
    Tag(String),
}

fn reference(s: &str) -> Option<Reference> {
    if let Some(d) = Digest::parse(s) {
        return Some(Reference::Digest(d));
    }
    // OCI tag grammar: [a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}
    let ok = s.len() <= 128
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    ok.then(|| Reference::Tag(s.to_string()))
}

pub async fn put_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, reference_str)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if body.len() > MAX_MANIFEST {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "manifest too large");
    }
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "malformed reference");
    };
    let d = Digest::of(&body);
    if let Reference::Digest(asked) = &r {
        if asked != &d {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest");
        }
    }
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    if let Err(e) = app.store.os.put(&manifest_path(&owner, &name, &d), PutPayload::from(body.clone())).await {
        return crate::http::internal_pub(e.into());
    }
    // The media type travels with the manifest: a GET must answer the same Content-Type the push
    // declared, and the bytes themselves are not re-parsed to recover it.
    let db = match app.store.image_db(&owner, &name).await {
        Ok(d) => d,
        Err(e) => return crate::http::internal_pub(e),
    };
    if let Err(e) = db
        .put(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes(), media.into_bytes())
        .await
    {
        return crate::http::internal_pub(e.into());
    }
    if let Err(e) = super::referrers::index(&app, &owner, &name, &d, &body).await {
        return crate::http::internal_pub(e);
    }
    if let Reference::Tag(t) = &r {
        if let Err(e) = app.store.put_tag(&owner, &name, t, &d).await {
            return crate::http::internal_pub(e);
        }
    } else if let Err(e) = app.store.touch_image(&owner, &name).await {
        return crate::http::internal_pub(e);
    }
    (
        StatusCode::CREATED,
        [
            (header::LOCATION, format!("/v2/{owner}/{name}/manifests/{d}")),
            (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
        ],
    )
        .into_response()
}

pub async fn get_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path(p): Path<(String, String, String)>,
) -> Response {
    manifest_response(app, trusted, headers, p, true).await
}

pub async fn head_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path(p): Path<(String, String, String)>,
) -> Response {
    manifest_response(app, trusted, headers, p, false).await
}

async fn manifest_response(
    app: Arc<App>,
    trusted: Trusted,
    headers: HeaderMap,
    (owner, name, reference_str): (String, String, String),
    with_body: bool,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest");
    };
    let d = match r {
        Reference::Digest(d) => d,
        Reference::Tag(t) => match app.store.tag(&owner, &name, &t).await {
            Ok(Some(d)) => d,
            Ok(None) => return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(e) => return crate::http::internal_pub(e),
        },
    };
    let bytes = match app.store.os.get(&manifest_path(&owner, &name, &d)).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(e) => return crate::http::internal_pub(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
        }
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    let media = match app.store.image_db(&owner, &name).await {
        Ok(db) => db
            .get(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes())
            .await
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).to_string())
            .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into()),
        Err(e) => return crate::http::internal_pub(e),
    };
    let hdrs = [
        (header::CONTENT_TYPE, media),
        (header::CONTENT_LENGTH, bytes.len().to_string()),
        (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
    ];
    if with_body {
        (StatusCode::OK, hdrs, bytes).into_response()
    } else {
        (StatusCode::OK, hdrs).into_response()
    }
}

/// By tag: unlink the tag. By digest: remove the manifest AND every tag that pointed at it —
/// leaving a tag resolving to bytes that are gone would turn every pull of it into a 404 the owner
/// cannot explain.
pub async fn delete_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, reference_str)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest");
    };
    match r {
        Reference::Tag(t) => match app.store.tag(&owner, &name, &t).await {
            Ok(Some(_)) => match app.store.delete_tag(&owner, &name, &t).await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(e) => crate::http::internal_pub(e),
            },
            Ok(None) => oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(e) => crate::http::internal_pub(e),
        },
        Reference::Digest(d) => {
            let tags = match app.store.tags(&owner, &name).await {
                Ok(t) => t,
                Err(e) => return crate::http::internal_pub(e),
            };
            for t in tags {
                if app.store.tag(&owner, &name, &t).await.ok().flatten().as_ref() == Some(&d) {
                    if let Err(e) = app.store.delete_tag(&owner, &name, &t).await {
                        return crate::http::internal_pub(e);
                    }
                }
            }
            if let Err(e) = super::referrers::unindex(&app, &owner, &name, &d).await {
                return crate::http::internal_pub(e);
            }
            match app.store.os.delete(&manifest_path(&owner, &name, &d)).await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(slatedb::object_store::Error::NotFound { .. }) => {
                    oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
                }
                Err(e) => crate::http::internal_pub(e.into()),
            }
        }
    }
}

/// `GET /tags/list?n=&last=` — lexical order, `last` exclusive, `Link` when truncated.
pub async fn tags_list(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let all = match app.store.tags(&owner, &name).await {
        Ok(t) => t,
        Err(e) => return crate::http::internal_pub(e),
    };
    if all.is_empty() && !app.store.image_exists(&owner, &name).await.unwrap_or(false) {
        return oci_err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such image");
    }
    let (page, truncated) = super::paginate(&all, &q);
    let body = serde_json::json!({"name": format!("{owner}/{name}"), "tags": page});
    let mut r = axum::Json(body).into_response();
    if let Some(last) = truncated {
        let n = q.get("n").cloned().unwrap_or_default();
        r.headers_mut().insert(
            header::LINK,
            format!("</v2/{owner}/{name}/tags/list?n={n}&last={last}>; rel=\"next\"")
                .parse()
                .unwrap(),
        );
    }
    r
}
```

Add the shared pagination helper to `src/registry/mod.rs`:

```rust
/// `n`/`last` pagination over a sorted list, shared by `tags/list` and `_catalog`.
/// Returns the page and, when the list was truncated, the value the next `last` should be.
pub(crate) fn paginate(
    all: &[String],
    q: &std::collections::HashMap<String, String>,
) -> (Vec<String>, Option<String>) {
    let start = match q.get("last") {
        Some(last) => all.partition_point(|v| v.as_str() <= last.as_str()),
        None => 0,
    };
    let rest = &all[start.min(all.len())..];
    let n: usize = q.get("n").and_then(|v| v.parse().ok()).unwrap_or(rest.len());
    let page: Vec<String> = rest.iter().take(n).cloned().collect();
    let truncated = (page.len() < rest.len()).then(|| page.last().cloned()).flatten();
    (page, truncated)
}
```

Routes:

```rust
.route(
    "/v2/{owner}/{name}/manifests/{reference}",
    get(manifests::get_manifest)
        .head(manifests::head_manifest)
        .put(manifests::put_manifest)
        .delete(manifests::delete_manifest),
)
.route("/v2/{owner}/{name}/tags/list", get(manifests::tags_list))
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --test registry_manifests`
Expected: PASS. `referrers::index`/`unindex` do not exist yet — stub them in Task 8 as `async fn index(..) -> crate::Result<()> { Ok(()) }` and fill them in Task 9.

- [ ] **Step 5: Commit**

```bash
git add src/registry tests/registry_manifests.rs
git commit -m "Store a manifest, name it with a tag, list what is there"
```

---

### Task 9: Referrers and `_catalog`

**Files:**
- Create: `src/registry/referrers.rs`
- Modify: `src/registry/routes.rs`, `src/registry/mod.rs`
- Test: `tests/registry_manifests.rs`

**Interfaces:**
- Consumes: `image_db`, `manifest_path`, `paginate`.
- Produces: `referrers::index(app, owner, name, digest, bytes) -> Result<()>`, `referrers::unindex(...)`, `GET /v2/{o}/{n}/referrers/{digest}?artifactType=`, `GET /v2/_catalog?n=&last=`.

- [ ] **Step 1: Write the failing test**

Append to `tests/registry_manifests.rs`:

```rust
#[tokio::test]
async fn a_manifest_with_a_subject_is_listed_as_its_referrer() {
    let (base, _e, c, token, m, subject) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();

    let sig = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "artifactType": "application/vnd.example.signature",
        "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": Digest::of(b"{}").to_string(), "size": 2},
        "layers": [],
        "subject": {"mediaType": MEDIA, "digest": subject.to_string(), "size": 1}
    }).to_string().into_bytes();
    let sig_d = Digest::of(&sig);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{sig_d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(sig.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{subject}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers().get("content-type").unwrap().to_str().unwrap(),
        "application/vnd.oci.image.index.v1+json"
    );
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["manifests"][0]["digest"], sig_d.to_string());
    assert_eq!(b["manifests"][0]["artifactType"], "application/vnd.example.signature");

    // Filtered, and the filter is announced.
    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{subject}?artifactType=application/vnd.other"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["manifests"], serde_json::json!([]));
    assert!(r.headers().get("oci-filters-applied").is_some());
}

#[tokio::test]
async fn referrers_of_an_unreferenced_digest_is_an_empty_index() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let d = Digest::of(b"nothing points here");
    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    // Empty, not 404: the spec is explicit about this.
    assert_eq!(r.status(), StatusCode::OK);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["manifests"], serde_json::json!([]));
}

#[tokio::test]
async fn the_catalog_lists_only_what_the_caller_may_see() {
    let (base, e, c, token, m, _d) = pushed().await;
    for image in ["nginx", "api"] {
        c.put(format!("{base}/v2/acme/{image}/manifests/latest"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(m.clone()).send().await.unwrap();
    }
    let other = e.store.create_token("other").await.unwrap();
    c.put(format!("{base}/v2/other/secret/manifests/latest"))
        .basic_auth("other", Some(&other)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();

    let r = c.get(format!("{base}/v2/_catalog"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["repositories"], serde_json::json!(["acme/api", "acme/nginx"]));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_manifests referrer`
Expected: FAIL — the referrers route 404s and the index is a stub.

- [ ] **Step 3: Implement**

`src/registry/referrers.rs`:

```rust
//! The referrers index: which manifests declare another as their `subject`.
//!
//! Kept in the image's database rather than computed by listing manifests, because the answer must
//! be cheap on every pull of a signed image and a listing is not. Written by the manifest PUT that
//! creates the referrer, removed by the DELETE that removes it.
use super::store::Digest;
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use crate::http::Trusted;
use std::collections::HashMap;
use std::sync::Arc;

/// One row per (subject, referrer). The value is the index ENTRY — the descriptor a client
/// receives — so answering needs no manifest reads at all.
fn key(subject: &Digest, referrer: &Digest) -> Vec<u8> {
    format!("image/referrer/{subject}/{referrer}").into_bytes()
}
fn prefix(subject: &Digest) -> String {
    format!("image/referrer/{subject}/")
}

/// Record `d` as a referrer, if its manifest names a subject. A manifest with no `subject` is not
/// an error and not a referrer — most manifests are that.
pub async fn index(app: &App, owner: &str, name: &str, d: &Digest, bytes: &[u8]) -> crate::Result<()> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Ok(()); // not JSON: nothing to index, and PUT already accepted the bytes
    };
    let Some(subject) = v.get("subject").and_then(|s| s.get("digest")).and_then(|d| d.as_str())
    else {
        return Ok(());
    };
    let Some(subject) = Digest::parse(subject) else { return Ok(()) };
    let entry = serde_json::json!({
        "mediaType": v.get("mediaType").and_then(|m| m.as_str())
            .unwrap_or("application/vnd.oci.image.manifest.v1+json"),
        "digest": d.to_string(),
        "size": bytes.len(),
        "artifactType": v.get("artifactType").and_then(|a| a.as_str())
            .or_else(|| v.get("config").and_then(|c| c.get("mediaType")).and_then(|m| m.as_str())),
        "annotations": v.get("annotations").cloned().unwrap_or(serde_json::json!({})),
    });
    app.store
        .image_db(owner, name)
        .await?
        .put(key(&subject, d), entry.to_string().into_bytes())
        .await?;
    Ok(())
}

/// Remove `d` from wherever it appears as a referrer. Scans the whole index: a manifest delete is
/// rare, and keeping a reverse map to make it cheap is state that can disagree with this one.
pub async fn unindex(app: &App, owner: &str, name: &str, d: &Digest) -> crate::Result<()> {
    let db = app.store.image_db(owner, name).await?;
    let mut it = db
        .scan("image/referrer/".to_string().into_bytes()..b"image/referrer0".to_vec())
        .await?;
    let mut doomed = vec![];
    while let Some(kv) = it.next().await? {
        let k = String::from_utf8_lossy(&kv.key).to_string();
        if k.ends_with(&format!("/{d}")) {
            doomed.push(kv.key.to_vec());
        }
    }
    for k in doomed {
        db.delete(k).await?;
    }
    Ok(())
}

/// `GET /referrers/{digest}` — an image index of everything pointing at that digest. Empty is a
/// 200 with an empty `manifests`, never a 404.
pub async fn list(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = super::auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let Some(d) = Digest::parse(&digest) else {
        return super::oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    let db = match app.store.image_db(&owner, &name).await {
        Ok(db) => db,
        Err(e) => return crate::http::internal_pub(e),
    };
    let p = prefix(&d);
    let mut it = match db.scan(p.clone().into_bytes()..format!("{p}\u{7f}").into_bytes()).await {
        Ok(it) => it,
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    let mut out = vec![];
    loop {
        match it.next().await {
            Ok(Some(kv)) => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&kv.value) {
                    out.push(v);
                }
            }
            Ok(None) => break,
            Err(e) => return crate::http::internal_pub(e.into()),
        }
    }
    let filter = q.get("artifactType").cloned();
    if let Some(f) = &filter {
        out.retain(|v| v.get("artifactType").and_then(|a| a.as_str()) == Some(f.as_str()));
    }
    let body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": out,
    });
    let mut r = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/vnd.oci.image.index.v1+json")],
        body.to_string(),
    )
        .into_response();
    // Announcing the filter is required: a client must be able to tell a filtered answer from a
    // server that ignored the parameter.
    if filter.is_some() {
        r.headers_mut().insert(
            header::HeaderName::from_static("oci-filters-applied"),
            "artifactType".parse().unwrap(),
        );
    }
    r
}
```

`_catalog`, in `src/registry/routes.rs`:

```rust
/// `GET /v2/_catalog` — the caller's images.
///
/// A listing of `repo/img/{owner}/` rather than a maintained index: an index is state that can
/// disagree with what was actually pushed, and this cannot. Scoped to the caller's own owner —
/// there is no cross-team catalog, because there is no cross-team read.
async fn catalog(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(who) = (match super::auth::caller(&app, &trusted, &headers).await {
        Ok(w) => w,
        Err(r) => return r,
    }) else {
        return super::auth::challenge(Some("registry:catalog:*"));
    };
    let prefix = slatedb::object_store::path::Path::from(format!("repo/img/{who}"));
    let mut names = std::collections::BTreeSet::new();
    let listing = match slatedb::object_store::ObjectStore::list_with_delimiter(
        app.store.os.as_ref(),
        Some(&prefix),
    )
    .await
    {
        Ok(l) => l,
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    for p in listing.common_prefixes {
        if let Some(n) = p.parts().last() {
            names.insert(format!("{who}/{}", n.as_ref()));
        }
    }
    let all: Vec<String> = names.into_iter().collect();
    let (page, _) = super::paginate(&all, &q);
    axum::Json(serde_json::json!({"repositories": page})).into_response()
}
```

Routes: `.route("/v2/_catalog", get(catalog))` and `.route("/v2/{owner}/{name}/referrers/{digest}", get(referrers::list))`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test registry_manifests && cargo clippy --all-targets -- -D warnings`
Expected: PASS. If `list_with_delimiter` returns prefixes that include the image name only for images whose database has files, push a manifest first in the test — which `the_catalog_lists_only_what_the_caller_may_see` already does.

- [ ] **Step 5: Commit**

```bash
git add src/registry tests/registry_manifests.rs
git commit -m "Answer who refers to a manifest, and what a team has pushed"
```

---

### Task 10: Sweep unreferenced blobs

**Files:**
- Create: `src/registry/gc.rs`
- Modify: `src/bin/worker.rs`
- Test: `tests/registry_gc.rs`

**Interfaces:**
- Consumes: `image_db`, `blob_path`, `manifest_path`.
- Produces: `registry::gc::sweep_owner(store: &Store, owner: &str, grace: Duration) -> Result<usize>` — returns how many blobs it deleted.

- [ ] **Step 1: Write the failing test**

`tests/registry_gc.rs`:

```rust
mod common;
use rustic_git::registry::{gc, store::blob_path, Digest};
use slatedb::object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use std::time::Duration;

#[tokio::test]
async fn an_unreferenced_blob_is_swept_and_a_referenced_one_is_not() {
    let e = common::env().await;
    let layer = b"referenced layer".to_vec();
    let ld = Digest::of(&layer);
    let orphan = b"nothing points at me".to_vec();
    let od = Digest::of(&orphan);
    e.store.os.put(&blob_path("acme", &ld), PutPayload::from(layer)).await.unwrap();
    e.store.os.put(&blob_path("acme", &od), PutPayload::from(orphan)).await.unwrap();

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": ld.to_string(), "size": 1},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": ld.to_string(), "size": 1}]
    }).to_string().into_bytes();
    let md = Digest::of(&manifest);
    e.store.os
        .put(&rustic_git::registry::store::manifest_path("acme", "nginx", &md), PutPayload::from(manifest))
        .await.unwrap();
    e.store.put_tag("acme", "nginx", "latest", &md).await.unwrap();

    // Grace zero: everything is old enough to consider.
    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 1, "exactly the orphan");
    assert!(e.store.os.head(&blob_path("acme", &ld)).await.is_ok(), "the referenced layer survives");
    assert!(e.store.os.head(&blob_path("acme", &od)).await.is_err(), "the orphan is gone");
}

#[tokio::test]
async fn a_blob_inside_the_grace_window_survives() {
    let e = common::env().await;
    let fresh = b"just uploaded, manifest still coming".to_vec();
    let d = Digest::of(&fresh);
    e.store.os.put(&blob_path("acme", &d), PutPayload::from(fresh)).await.unwrap();
    let n = gc::sweep_owner(&e.store, "acme", Duration::from_secs(3600)).await.unwrap();
    assert_eq!(n, 0, "an in-flight push must not be swept out from under itself");
    assert!(e.store.os.head(&blob_path("acme", &d)).await.is_ok());
}

#[tokio::test]
async fn a_layer_two_images_share_survives_one_of_them_being_emptied() {
    let e = common::env().await;
    let shared = b"base layer".to_vec();
    let sd = Digest::of(&shared);
    e.store.os.put(&blob_path("acme", &sd), PutPayload::from(shared)).await.unwrap();
    for image in ["nginx", "api"] {
        let m = serde_json::json!({
            "schemaVersion": 2,
            "config": {"digest": sd.to_string(), "size": 1},
            "layers": [{"digest": sd.to_string(), "size": 1}]
        }).to_string().into_bytes();
        let md = Digest::of(&m);
        e.store.os
            .put(&rustic_git::registry::store::manifest_path("acme", image, &md), PutPayload::from(m))
            .await.unwrap();
        e.store.put_tag("acme", image, "latest", &md).await.unwrap();
    }
    // Empty one image entirely.
    e.store.delete_tag("acme", "nginx", "latest").await.unwrap();
    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 0);
    assert!(e.store.os.head(&blob_path("acme", &sd)).await.is_ok(), "the other image still needs it");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_gc`
Expected: FAIL — `registry::gc` does not exist.

- [ ] **Step 3: Implement**

`src/registry/gc.rs`:

```rust
//! Sweeping blobs no manifest references.
//!
//! Scoped to ONE owner, which is the whole reason blobs are per-owner: a global content-addressed
//! store would make this sweep read every image in the fleet before it could delete anything, and
//! a sweep that must be right about everything is a sweep nobody dares run.
//!
//! The order is load-bearing. Read every manifest FIRST, then list the blobs, then delete only
//! blobs that are both unreferenced and older than the grace window. Listing first would let a
//! manifest written mid-sweep reference a blob the sweep had already decided was an orphan.
use super::store::{blob_path, Digest};
use crate::store::Store;
use crate::Result;
use slatedb::object_store::{ObjectStore, ObjectStoreExt};
use std::collections::HashSet;
use std::time::Duration;

/// Every digest referenced by any manifest of any of this owner's images — the manifests
/// themselves included, since a manifest in an index is referenced by digest too.
async fn referenced(store: &Store, owner: &str) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let prefix = slatedb::object_store::path::Path::from(format!("manifests/{owner}"));
    let mut listing = store.os.list(Some(&prefix));
    let mut paths = vec![];
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        paths.push(m?.location);
    }
    for p in paths {
        let bytes = store.os.get(&p).await?.bytes().await?;
        // The manifest itself.
        if let Some(hex) = p.parts().last() {
            out.insert(format!("sha256:{}", hex.as_ref()));
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        // config, layers, manifests (an index), and subject all name digests. Walking the JSON for
        // every "digest" string catches all four without a schema per media type — and a digest
        // this over-collects is a blob kept, never one deleted.
        collect(&v, &mut out);
    }
    Ok(out)
}

fn collect(v: &serde_json::Value, out: &mut HashSet<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, v) in m {
                if k == "digest" {
                    if let Some(s) = v.as_str() {
                        out.insert(s.to_string());
                    }
                }
                collect(v, out);
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|v| collect(v, out)),
        _ => {}
    }
}

/// Delete this owner's unreferenced blobs. `grace` protects an in-flight push: a blob uploaded
/// before its manifest exists is unreferenced for as long as the push takes.
pub async fn sweep_owner(store: &Store, owner: &str, grace: Duration) -> Result<usize> {
    let keep = referenced(store, owner).await?;
    let prefix = slatedb::object_store::path::Path::from(format!("blobs/{owner}"));
    let mut listing = store.os.list(Some(&prefix));
    let mut doomed = vec![];
    let cutoff = std::time::SystemTime::now() - grace;
    while let Some(m) = futures::StreamExt::next(&mut listing).await {
        let m = m?;
        let Some(hex) = m.location.parts().last() else { continue };
        let digest = format!("sha256:{}", hex.as_ref());
        if keep.contains(&digest) {
            continue;
        }
        if m.last_modified > chrono::DateTime::<chrono::Utc>::from(cutoff) {
            continue;
        }
        doomed.push(m.location);
    }
    let n = doomed.len();
    for p in doomed {
        match store.os.delete(&p).await {
            Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}
```

If `chrono` is not already a dependency, compare with `m.last_modified` in whatever type `object_store` uses in this pin (it is `chrono::DateTime<Utc>` in every recent version, and SlateDB pulls chrono in transitively — check `cargo tree -p chrono` before adding it to `Cargo.toml`).

Wire it into `src/bin/worker.rs` next to the existing repo maintenance pass: one sweep per owner per cycle, with the grace window from `RUSTIC_GIT_BLOB_GRACE_SECS` (default 3600), logging how many it deleted.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test registry_gc && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/registry/gc.rs src/bin/worker.rs tests/registry_gc.rs Cargo.toml
git commit -m "Sweep the layers nothing points at"
```

---

### Task 11: The page the tab points at

**Files:**
- Create: `web/apps/web/src/app/(shell)/[owner]/(org)/registries/page.tsx`
- Create: `web/apps/web/src/app/(shell)/[owner]/(org)/registries/[image]/page.tsx`
- Create: `web/apps/web/src/components/app/image-list.tsx`
- Modify: `src/http/browse_api.rs` (add `images` and `image` browse routes), `src/http.rs` (`BROWSE_TAILS`), `src/api.rs` (proxy them)
- Test: `tests/registry_http.rs`, and the existing `every_browse_route_is_routable` test proves the tails list stays honest.

**Interfaces:**
- Consumes: `store.tags`, `image_exists`, `image_is_public`, `_catalog`'s listing logic.
- Produces:
  - `GET /api/{owner}/images` → `[{name, tags, updated_ms}]`
  - `GET /api/{owner}/{image}/imagetags` → `[{tag, digest, size, pushed_ms}]`

**Note on the tails list:** `BROWSE_TAILS` in `src/http.rs:172` is length-annotated (`[&str; 15]`). Adding tails means bumping that number, and `every_browse_route_is_routable` fails loudly if a route is added without its tail — that test is the contract, do not weaken it.

- [ ] **Step 1: Write the failing test**

Append to `tests/registry_http.rs`:

```rust
#[tokio::test]
async fn the_browse_api_lists_a_teams_images() {
    // The peer listener is where browse routes live; mirror tests/browse_http.rs's harness.
    let (base, e) = common::serve_peer().await;
    e.store.put_tag("acme", "nginx", "latest", &rustic_git::registry::Digest::of(b"m")).await.unwrap();
    e.store.put_tag("acme", "nginx", "v1", &rustic_git::registry::Digest::of(b"m")).await.unwrap();
    let r = common::peer_get(&base, "/api/acme/images").await;
    assert_eq!(r.status(), StatusCode::OK);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b[0]["name"], "nginx");
    assert_eq!(b[0]["tags"], 2);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test registry_http the_browse_api_lists`
Expected: FAIL — 404; the route does not exist and its tail is not in `BROWSE_TAILS`.

- [ ] **Step 3: Add the browse routes**

In `src/http/browse_api.rs`, following the shape every handler there already uses (open, then answer):

```rust
#[derive(Serialize)]
struct ImageSummary {
    name: String,
    tags: usize,
    public: bool,
}

/// `GET /api/{owner}/images` — the team's images, for the Container Images page.
///
/// Owner-scoped rather than repo-scoped, so it is the one browse route whose second segment is not
/// a repo name. It still routes: `images` is a `BROWSE_TAILS` entry, which sends it to whichever
/// node holds `{owner}/images` — and since it only lists the object store, any node can answer.
async fn images(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path(owner): Path<String>,
) -> Response {
    match crate::registry::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(who)) if who == owner => {}
        Ok(_) => return hidden(),
        Err(r) => return r,
    }
    let names = match crate::registry::routes::image_names(&app, &owner).await {
        Ok(n) => n,
        Err(e) => return internal(e),
    };
    let mut out = vec![];
    for name in names {
        let tags = app.store.tags(&owner, &name).await.unwrap_or_default().len();
        let public = app.store.image_is_public(&owner, &name).await.unwrap_or(false);
        out.push(ImageSummary { name, tags, public });
    }
    Json(out).into_response()
}
```

Factor the prefix listing out of `catalog` (Task 9) into
`pub async fn image_names(app: &App, owner: &str) -> Result<Vec<String>>` in `registry/routes.rs`,
and have `catalog` call it — one lister, two callers.

Add `"images"` and `"imagetags"` to `BROWSE_TAILS`, bump its length annotation, and register both routes. Proxy them in `src/api.rs` exactly as its neighbours are proxied.

- [ ] **Step 4: Build the pages**

`registries/page.tsx` — a server component that fetches `/api/{owner}/images` through the same helper every other page in `(org)/` uses (read `web/apps/web/src/app/(shell)/[owner]/(org)/activity/page.tsx` and copy its data-fetch and empty-state shape). Render `image-list.tsx`: one row per image with its name, tag count, a private/public pill, and a copyable `docker pull {host}/{owner}/{name}:latest`. Empty state: the `docker login` and `docker push` lines a user needs to create their first image, since there is no create button — images appear by being pushed.

`registries/[image]/page.tsx` — the tag table: tag, digest (short), size, pushed. Same fetch helper, `/api/{owner}/{image}/imagetags`.

- [ ] **Step 5: Run everything**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Run: `cd web && bun run lint && bun run build`
Expected: PASS. `every_browse_route_is_routable` must pass — if it fails, a route was added without its tail.

- [ ] **Step 6: Commit**

```bash
git add src/http src/api.rs web/apps/web/src tests/registry_http.rs
git commit -m "Show a team its images where the tab has been pointing"
```

---

### Task 12: A real client, end to end

The conformance the unit tests cannot give: whether `docker` and `podman` agree that this is a registry.

**Files:**
- Create: `tests/registry_e2e.sh`
- Modify: `README.md` (a Container Images section)

- [ ] **Step 1: Write the script**

`tests/registry_e2e.sh`:

```bash
#!/usr/bin/env bash
# Pushes and pulls a real image with a real client. Requires docker (or podman) and a running
# node. Not part of `cargo test`: it needs a daemon and a registry reachable over TLS or listed as
# an insecure registry.
set -euo pipefail

REG="${REG:-localhost:8080}"
OWNER="${OWNER:-acme}"
TOKEN="${TOKEN:?run: cargo run -- admin add-token acme, and export TOKEN}"
CLI="${CLI:-docker}"

echo "==> login"
echo "$TOKEN" | "$CLI" login "$REG" --username "$OWNER" --password-stdin

echo "==> build a tiny image"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
printf 'FROM scratch\nCOPY hello /hello\n' > "$tmp/Dockerfile"
echo hello > "$tmp/hello"
"$CLI" build -t "$REG/$OWNER/e2e:v1" "$tmp"

echo "==> push"
"$CLI" push "$REG/$OWNER/e2e:v1"

echo "==> pull it back from a clean local state"
"$CLI" rmi "$REG/$OWNER/e2e:v1"
"$CLI" pull "$REG/$OWNER/e2e:v1"

echo "==> the catalog and the tag list agree"
curl -fsS -u "$OWNER:$TOKEN" "http://$REG/v2/_catalog" | grep -q "$OWNER/e2e"
curl -fsS -u "$OWNER:$TOKEN" "http://$REG/v2/$OWNER/e2e/tags/list" | grep -q v1

echo "==> a second push of the same layers mounts rather than re-uploads"
"$CLI" tag "$REG/$OWNER/e2e:v1" "$REG/$OWNER/e2e-two:v1"
"$CLI" push "$REG/$OWNER/e2e-two:v1"

echo "OK"
```

`chmod +x tests/registry_e2e.sh`.

- [ ] **Step 2: Run it**

Run: `cargo run -- serve &` then `TOKEN=$(cargo run -- admin add-token acme | tail -1) ./tests/registry_e2e.sh`
Expected: `OK`. A failure here is a real client disagreeing with the handlers — fix the handler, and add the disagreement as a Rust test in the file that owns that endpoint, so it cannot come back.

- [ ] **Step 3: Optional conformance suite**

If the OCI conformance binary is available:
`OCI_ROOT_URL=http://localhost:8080 OCI_NAMESPACE=acme/conformance OCI_USERNAME=acme OCI_PASSWORD=$TOKEN OCI_TEST_PULL=1 OCI_TEST_PUSH=1 OCI_TEST_CONTENT_DISCOVERY=1 OCI_TEST_CONTENT_MANAGEMENT=1 conformance.test`
Record which groups pass in the README section; do not leave a claim of conformance the suite has not backed.

- [ ] **Step 4: Document it**

Add a Container Images section to `README.md`: what the registry serves, `docker login`, push, pull, that images are their own namespace, and the env knobs this plan introduced — `RUSTIC_GIT_EXTERNAL_URL`, `RUSTIC_GIT_MAX_LAYER`, `RUSTIC_GIT_BLOB_GRACE_SECS`, `RUSTIC_GIT_JWT_SECRET`.

- [ ] **Step 5: Commit**

```bash
git add tests/registry_e2e.sh README.md
git commit -m "Prove it with a real client, and say how to use it"
```
