# Browse API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A read-only JSON API — refs, trees, blobs, log, diffs — served by its own process in front of the git nodes, with a Redis response cache.

**Architecture:** Browse handlers live on the git nodes' *peer* router (port 8081, already secret-guarded), so the public git path is untouched. A separate stateless process (`kloudlite-git api-serve`) fronts them: it authenticates, checks Redis, and on a miss calls the git Service, whose existing `route` middleware forwards to the repo's owner. Every URL except the ref list is keyed by an immutable object id, so any api pod may serve a cached answer without involving the owner node.

**Tech Stack:** Rust, axum 0.8, gix-odb/gix-object/gix-traverse (already present), `imara-diff` (new), `redis` (new), Azure Managed Redis, Cloudflare WAF.

**Spec:** `docs/superpowers/specs/2026-08-18-browse-api-design.md`

## Global Constraints

- Rust edition 2021. Existing deps only, plus exactly two new ones: `imara-diff = "0.1"` and `redis = { version = "0.27", features = ["tokio-comp", "connection-manager", "tls-rustls-webpki-roots"] }`.
- No new binary target. `kloudlite-git api-serve` is a subcommand of the existing binary.
- A private repo and a missing repo are indistinguishable: both 404. Never 403 on a read.
- Blob responses cap at 5 MB with `truncated: true`; blobs over 1 MB are never written to Redis.
- Redis failure is always fail-open: log and fall through to the git nodes.
- Cache keys always carry the per-repo generation: `v1:{gen}:{owner}/{name}:...`
- `eprintln!` is the logging convention in this codebase; match it, with a `// ponytail: eprintln` comment.
- Tests use `tests/common/mod.rs::env()` and `::app()`. `cargo test` must pass with no network.

---

### Task 1: Repo visibility flag

**Files:**
- Modify: `src/refs.rs` (add `set_public` / `is_public` next to the other repo-DB accessors)
- Modify: `src/auth.rs:88-90` (`authorize`)
- Modify: `src/http.rs:402-433` (`open`)
- Modify: `src/ssh.rs:171` and `src/proxy.rs:202` (the other two `authorize` callers)
- Modify: `src/main.rs:326-356` (admin dispatch and usage string)
- Test: `tests/store.rs`

**Interfaces:**
- Consumes: `Store::db_for(owner, name) -> Arc<Db>`, `Store::repo_exists`.
- Produces:
  - `Store::set_public(&self, owner: &str, name: &str, public: bool) -> Result<()>`
  - `Store::is_public(&self, owner: &str, name: &str) -> Result<bool>`
  - `auth::authorize(auth_owner: Option<&str>, repo_owner: &str, public_read: bool) -> bool`

- [ ] **Step 1: Write the failing test**

In `tests/store.rs`:

```rust
#[tokio::test]
async fn visibility_defaults_private_and_round_trips() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    assert!(!e.store.is_public("alice", "web").await.unwrap());
    e.store.set_public("alice", "web", true).await.unwrap();
    assert!(e.store.is_public("alice", "web").await.unwrap());
    e.store.set_public("alice", "web", false).await.unwrap();
    assert!(!e.store.is_public("alice", "web").await.unwrap());
}

#[test]
fn authorize_allows_anonymous_reads_only_when_public() {
    use kloudlite_git::auth::authorize;
    assert!(!authorize(None, "alice", false));
    assert!(authorize(None, "alice", true));
    assert!(authorize(Some("alice"), "alice", false));
    assert!(!authorize(Some("bob"), "alice", true), "public grants read, not identity");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test store visibility -- --nocapture`
Expected: FAIL — `no method named set_public`.

- [ ] **Step 3: Write minimal implementation**

In `src/refs.rs`, alongside the other repo-DB accessors:

```rust
/// A repo's visibility. Lives in the repo database rather than as an object key because it is
/// repo state, read on the owner alongside the refs it guards.
const PUBLIC_KEY: &[u8] = b"meta/public";

impl Store {
    pub async fn set_public(&self, owner: &str, name: &str, public: bool) -> Result<()> {
        let db = self.db_for(owner, name).await?;
        db.put(PUBLIC_KEY, if public { b"1" } else { b"0" }).await?;
        db.flush().await?;
        Ok(())
    }

    pub async fn is_public(&self, owner: &str, name: &str) -> Result<bool> {
        Ok(self.db_for(owner, name).await?.get(PUBLIC_KEY).await?.as_deref() == Some(b"1"))
    }
}
```

In `src/auth.rs`, replace `authorize`:

```rust
/// Anonymous callers get in only on a public repo, and only for reads — the caller decides
/// whether this is a read by what it passes for `public_read`.
pub fn authorize(auth_owner: Option<&str>, repo_owner: &str, public_read: bool) -> bool {
    match auth_owner {
        Some(o) => o == repo_owner,
        None => public_read,
    }
}
```

In `src/http.rs::open`, the anonymous branch must no longer bail early. Replace the token block and
the authorize call with:

```rust
    let auth_owner = match &trusted.0 {
        Some(o) => Some(o.clone()),
        None => {
            let token = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Basic "))
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .and_then(|d| String::from_utf8(d).ok())
                .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()));
            match token {
                Some(t) => {
                    let owner = app.store.owner_for_token(&t).await.map_err(internal)?;
                    if owner.is_none() {
                        return Err(unauthorized());
                    }
                    owner
                }
                // No credentials is not yet a failure: a public repo may still admit this caller.
                None => None,
            }
        }
    };
    let public = app.store.is_public(owner, name).await.unwrap_or(false);
    if !crate::auth::authorize(auth_owner.as_deref(), owner, public && read_only) {
        return Err(if auth_owner.is_none() { unauthorized() } else { StatusCode::FORBIDDEN.into_response() });
    }
```

Add `read_only: bool` as the final parameter of `open`. Its callers: `info_refs` passes
`query service == "git-upload-pack"`, `upload_pack` passes `true`, `receive_pack` passes `false`.

The other two callers keep today's behaviour by passing `false`:

- `src/ssh.rs:171` → `authorize(auth_owner.as_deref(), &owner, false)`. SSH always authenticates a
  key, so there is no anonymous SSH to admit — and `src/ssh.rs:186` relies on that with
  `.expect("authorize() passed, so the owner is set")`. Passing `false` keeps that invariant true.
  Add a comment there saying so, or the next reader will assume it is an oversight.
- `src/proxy.rs:202` → `authorize(Some(owner.as_str()), &ro, false)`. A peer always presents an
  identity, so the public flag cannot change the outcome.

In `src/main.rs`, add the admin arm and extend the usage string:

```rust
        ["admin", "set-visibility", path, vis] => {
            let (owner, name) = split_repo(path)?;
            let public = match *vis {
                "public" => true,
                "private" => false,
                _ => return Err(kloudlite_git::err("visibility must be public or private")),
            };
            store.set_public(&owner, &name, public).await?;
        }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test store && cargo test --test http_e2e && cargo test --test ssh_e2e`
Expected: PASS. The e2e suites prove authenticated git traffic is unaffected.

- [ ] **Step 5: Commit**

```bash
git add src/refs.rs src/auth.rs src/http.rs src/main.rs tests/store.rs
git commit -m "Repos can be public: anonymous reads, owner-only writes"
```

---

### Task 2: Object reading

**Files:**
- Create: `src/browse.rs`
- Modify: `src/lib.rs:1-11` (add `pub mod browse;`)
- Modify: `Cargo.toml` (add `imara-diff = "0.1"`)
- Test: `tests/browse.rs`

**Interfaces:**
- Consumes: `Repo::odb() -> Result<gix_odb::Handle>` from `src/store.rs:37`.
- Produces (all synchronous, all taking `&gix_odb::Handle`):
  - `pub struct Entry { pub name: String, pub mode: u16, pub kind: String, pub oid: String, pub size: Option<u64> }`
  - `pub struct Commit { pub oid: String, pub parents: Vec<String>, pub author: String, pub time: i64, pub message: String }`
  - `pub struct Blob { pub oid: String, pub bytes: Vec<u8>, pub truncated: bool }`
  - `pub fn tree_at(odb: &Handle, oid: ObjectId, path: &str) -> Result<Vec<Entry>>`
  - `pub fn blob_at(odb: &Handle, oid: ObjectId, path: &str, cap: usize) -> Result<Blob>`
  - `pub fn log(odb: &Handle, from: ObjectId, n: usize) -> Result<Vec<Commit>>`
  - `pub fn commit(odb: &Handle, oid: ObjectId) -> Result<(Commit, String)>` — the `String` is a unified diff against the first parent

Diffs are computed by walking both trees by name (no `gix-diff`; the walk is ~40 lines and avoids a
dependency whose API churns) and running `imara-diff` per changed file.

- [ ] **Step 1: Write the failing test**

Create `tests/browse.rs`. It builds a real repo with `git` on disk, imports it through the existing
push path, and reads it back — the same shape `tests/http_e2e.rs` uses.

```rust
mod common;
use kloudlite_git::browse;

#[tokio::test]
async fn reads_a_tree_a_blob_and_a_diff() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "web").await; // two commits; src/main.rs changes
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();

    let root = browse::tree_at(&odb, head, "").unwrap();
    assert!(root.iter().any(|x| x.name == "src" && x.kind == "tree"));

    let sub = browse::tree_at(&odb, head, "src").unwrap();
    assert!(sub.iter().any(|x| x.name == "main.rs" && x.kind == "blob"));

    let blob = browse::blob_at(&odb, head, "src/main.rs", 5 << 20).unwrap();
    assert!(!blob.truncated);
    assert!(String::from_utf8_lossy(&blob.bytes).contains("fn main"));

    let truncated = browse::blob_at(&odb, head, "src/main.rs", 4).unwrap();
    assert!(truncated.truncated && truncated.bytes.len() == 4);

    let commits = browse::log(&odb, head, 10).unwrap();
    assert_eq!(commits.len(), 2, "fixture has two commits");
    assert_eq!(commits[0].oid, head.to_hex().to_string());

    let (c, diff) = browse::commit(&odb, head).unwrap();
    assert_eq!(c.parents.len(), 1);
    assert!(diff.contains("src/main.rs"), "diff names the changed file: {diff}");
}

#[tokio::test]
async fn missing_path_is_an_error_not_a_panic() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "web").await;
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    assert!(browse::tree_at(&odb, head, "nope").is_err());
    assert!(browse::blob_at(&odb, head, "src", 1024).is_err(), "a tree is not a blob");
}
```

Add `push_fixture` to `tests/common/mod.rs` — it creates a git repo in a tempdir with two commits
(the second editing `src/main.rs`), pushes it via the existing receive-pack path used by
`tests/http_e2e.rs`, and returns the opened `Repo`. Copy the push mechanics from `tests/http_e2e.rs`
rather than inventing a new path.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test browse -- --nocapture`
Expected: FAIL — `unresolved import kloudlite_git::browse`.

- [ ] **Step 3: Write minimal implementation**

`src/browse.rs`. Resolution first: every entry point accepts a commit *or* tree id, so peel once.

```rust
use crate::{err, Result};
use gix_hash::ObjectId;
use gix_object::{tree::EntryKind, Find, FindExt};

fn peel_to_tree(odb: &gix_odb::Handle, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    match odb.find(&oid, &mut buf)?.kind {
        gix_object::Kind::Tree => Ok(oid),
        gix_object::Kind::Commit => Ok(gix_object::CommitRef::from_bytes(&buf)?.tree()),
        k => Err(err(format!("{oid} is a {k}, not a commit or tree"))),
    }
}

/// Walk `path` from `tree`, returning the id it names. `""` is the tree itself.
fn resolve(odb: &gix_odb::Handle, tree: ObjectId, path: &str) -> Result<(ObjectId, EntryKind)> {
    let mut cur = (tree, EntryKind::Tree);
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if cur.1 != EntryKind::Tree {
            return Err(err(format!("{seg}: parent is not a tree")));
        }
        let mut buf = Vec::new();
        let t = odb.find_tree(&cur.0, &mut buf)?;
        let e = t.entries.iter().find(|e| e.filename == seg)
            .ok_or_else(|| err(format!("{path}: not found")))?;
        cur = (e.oid.into(), e.mode.kind());
    }
    Ok(cur)
}
```

Then the four public functions. `tree_at` resolves and lists; `size` is filled for blobs only:

```rust
pub fn tree_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str) -> Result<Vec<Entry>> {
    let (id, kind) = resolve(odb, peel_to_tree(odb, oid)?, path)?;
    if kind != EntryKind::Tree {
        return Err(err(format!("{path}: not a tree")));
    }
    let mut buf = Vec::new();
    let mut out: Vec<Entry> = odb.find_tree(&id, &mut buf)?.entries.iter().map(|e| Entry {
        name: e.filename.to_string(),
        mode: e.mode.value(),
        kind: if e.mode.is_tree() { "tree".into() } else { "blob".into() },
        oid: e.oid.to_hex().to_string(),
        size: None,
    }).collect();
    for e in out.iter_mut().filter(|e| e.kind == "blob") {
        let id: ObjectId = e.oid.parse()?;
        let mut b = Vec::new();
        e.size = Some(odb.find_blob(&id, &mut b)?.data.len() as u64);
    }
    out.sort_by(|a, b| (a.kind != "tree", &a.name).cmp(&(b.kind != "tree", &b.name)));
    Ok(out)
}

pub fn blob_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str, cap: usize) -> Result<Blob> {
    let (id, kind) = resolve(odb, peel_to_tree(odb, oid)?, path)?;
    if kind == EntryKind::Tree {
        return Err(err(format!("{path}: is a tree")));
    }
    let mut buf = Vec::new();
    let data = odb.find_blob(&id, &mut buf)?.data;
    let truncated = data.len() > cap;
    Ok(Blob { oid: id.to_hex().to_string(), bytes: data[..data.len().min(cap)].to_vec(), truncated })
}

pub fn log(odb: &gix_odb::Handle, from: ObjectId, n: usize) -> Result<Vec<Commit>> {
    let mut out = Vec::new();
    let mut next = Some(from);
    while let (Some(id), true) = (next, out.len() < n) {
        let mut buf = Vec::new();
        let c = odb.find_commit(&id, &mut buf)?;
        next = c.parents().next();
        out.push(Commit {
            oid: id.to_hex().to_string(),
            parents: c.parents().map(|p| p.to_hex().to_string()).collect(),
            author: c.author().name.to_string(),
            time: c.time().seconds,
            message: c.message.to_string(),
        });
    }
    Ok(out)
}
```

`commit` reads the commit, diffs its tree against its first parent's, and renders unified hunks.
The tree comparison is a recursive merge-join on sorted entry names — git tree entries are already
sorted, so it is a linear walk:

```rust
fn changed_files(odb: &gix_odb::Handle, old: Option<ObjectId>, new: ObjectId, prefix: &str,
                 out: &mut Vec<(String, Option<ObjectId>, Option<ObjectId>)>) -> Result<()> {
    let mut ob = Vec::new();
    let mut nb = Vec::new();
    let olds: Vec<_> = match old {
        Some(o) => odb.find_tree(&o, &mut ob)?.entries.iter().cloned().collect(),
        None => vec![],
    };
    let news: Vec<_> = odb.find_tree(&new, &mut nb)?.entries.iter().cloned().collect();
    let mut seen = std::collections::BTreeMap::new();
    for e in &olds { seen.entry(e.filename.to_string()).or_insert((None, None)).0 = Some(e.clone()); }
    for e in &news { seen.entry(e.filename.to_string()).or_insert((None, None)).1 = Some(e.clone()); }
    for (name, (o, n)) in seen {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let oid_of = |e: &Option<gix_object::tree::Entry>| e.as_ref().map(|e| ObjectId::from(e.oid));
        let is_tree = |e: &Option<gix_object::tree::Entry>| e.as_ref().is_some_and(|e| e.mode.is_tree());
        if oid_of(&o) == oid_of(&n) { continue; }
        match (is_tree(&o), is_tree(&n)) {
            // A directory on both sides: recurse rather than emit it.
            (_, true) => changed_files(odb, oid_of(&o).filter(|_| is_tree(&o)), oid_of(&n).unwrap(), &path, out)?,
            _ => out.push((path, oid_of(&o).filter(|_| !is_tree(&o)), oid_of(&n))),
        }
    }
    Ok(())
}

pub fn commit(odb: &gix_odb::Handle, oid: ObjectId) -> Result<(Commit, String)> {
    let c = log(odb, oid, 1)?.pop().ok_or_else(|| err("no such commit"))?;
    let tree = peel_to_tree(odb, oid)?;
    let parent_tree = match c.parents.first() {
        Some(p) => Some(peel_to_tree(odb, p.parse()?)?),
        None => None,
    };
    let mut files = Vec::new();
    changed_files(odb, parent_tree, tree, "", &mut files)?;
    let mut diff = String::new();
    for (path, old, new) in files {
        let text = |id: Option<ObjectId>| -> Result<String> {
            Ok(match id {
                Some(id) => {
                    let mut b = Vec::new();
                    String::from_utf8_lossy(odb.find_blob(&id, &mut b)?.data).to_string()
                }
                None => String::new(),
            })
        };
        let (a, b) = (text(old)?, text(new)?);
        diff.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
        let input = imara_diff::intern::InternedInput::new(a.as_str(), b.as_str());
        diff.push_str(&imara_diff::diff(
            imara_diff::Algorithm::Histogram,
            &input,
            imara_diff::UnifiedDiffBuilder::new(&input),
        ));
    }
    Ok((c, diff))
}
```

Binary files are diffed as lossy UTF-8 rather than detected and skipped.
Add `// ponytail: lossy UTF-8 diff; detect binary when someone complains`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test browse -- --nocapture`
Expected: PASS. If a gix signature differs from the sketch above, run
`cargo doc -p gix-object --open` and adapt — the shape of the functions is what matters, not the
exact accessor spelling.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/browse.rs src/lib.rs tests/browse.rs tests/common/mod.rs
git commit -m "Read trees, blobs, history and diffs from a repo's odb"
```

---

### Task 3: Browse handlers on the peer router

**Files:**
- Create: `src/http/browse_api.rs`
- Modify: `src/http.rs:174-180` (`is_git_route`), `:347-380` (`peer_router`)
- Test: `tests/browse_http.rs`

**Interfaces:**
- Consumes: Task 1's `Store::is_public`, Task 2's `browse::{tree_at, blob_at, log, commit}`, existing `http::open`.
- Produces: `pub fn browse_routes() -> Router<Arc<App>>` mounting:
  - `GET /api/{owner}/{name}/refs`
  - `GET /api/{owner}/{name}/tree/{oid}/{*path}` and `GET /api/{owner}/{name}/tree/{oid}`
  - `GET /api/{owner}/{name}/blob/{oid}/{*path}`
  - `GET /api/{owner}/{name}/log/{oid}` (query `n`, default 50, max 200)
  - `GET /api/{owner}/{name}/commit/{oid}`

All are read-only, so every `open` call passes `read_only = true`.

- [ ] **Step 1: Write the failing test**

`tests/browse_http.rs` — drives `peer_router` in-process with the peer secret header:

```rust
mod common;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn refs_then_tree_then_blob() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let app = common::app(e.store.clone()).await;
    let router = kloudlite_git::http::peer_router(app);

    let get = |path: String| {
        let router = router.clone();
        async move {
            let req = Request::builder().uri(path)
                .header(kloudlite_git::proxy::PEER_HEADER, "test-peer-secret")
                .header(kloudlite_git::proxy::OWNER_HEADER, "alice")
                .body(axum::body::Body::empty()).unwrap();
            let r = router.oneshot(req).await.unwrap();
            let status = r.status();
            let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
            (status, serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default())
        }
    };

    let (s, refs) = get("/api/alice/web/refs".into()).await;
    assert_eq!(s, StatusCode::OK);
    let oid = refs[0]["oid"].as_str().unwrap().to_string();
    assert_eq!(refs[0]["kind"], "branch");

    let (s, tree) = get(format!("/api/alice/web/tree/{oid}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(tree.as_array().unwrap().iter().any(|e| e["name"] == "src"));

    let (s, blob) = get(format!("/api/alice/web/blob/{oid}/src/main.rs")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(blob["truncated"], false);

    let (s, _) = get(format!("/api/alice/web/tree/{oid}/nope")).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unknown path is 404, never 500");
}

#[tokio::test]
async fn private_repo_is_404_to_a_stranger() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    common::push_fixture(&e, "alice", "web").await;
    let app = common::app(e.store.clone()).await;
    let req = Request::builder().uri("/api/alice/web/refs")
        .header(kloudlite_git::proxy::PEER_HEADER, "test-peer-secret")
        .header(kloudlite_git::proxy::OWNER_HEADER, "bob")
        .body(axum::body::Body::empty()).unwrap();
    let r = kloudlite_git::http::peer_router(app).oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "existence must not leak");
}
```

Add `serde` + `serde_json` to `[dev-dependencies]` if absent; `axum` already pulls `serde` for the
handlers, so add `serde = { version = "1", features = ["derive"] }` and `serde_json = "1"` to
`[dependencies]` and derive `Serialize` on Task 2's structs.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test browse_http -- --nocapture`
Expected: FAIL — 404 on every route, because `peer_router` has no `/api` routes yet.

- [ ] **Step 3: Write minimal implementation**

`src/http/browse_api.rs`. `not_found` collapses every read failure so existence never leaks:

```rust
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

async fn api_refs(
    State(app): State<Arc<App>>, Extension(trusted): Extension<Trusted>,
    headers: HeaderMap, Path((owner, name)): Path<(String, String)>,
) -> Response {
    let repo = match open(&app, &trusted, &headers, &owner, &name, true).await {
        Ok(r) => r,
        // Unauthorized keeps its 401 so a client knows to present a token; everything else is 404.
        Err(r) if r.status() == StatusCode::UNAUTHORIZED => return r,
        Err(_) => return not_found(),
    };
    let refs = match app.store.list_refs(&repo).await { Ok(r) => r, Err(e) => return internal(e) };
    let out: Vec<_> = refs.into_iter().map(|(name, oid)| serde_json::json!({
        "name": name,
        "oid": oid.to_hex().to_string(),
        "kind": if name.starts_with("refs/tags/") { "tag" } else { "branch" },
    })).collect();
    axum::Json(out).into_response()
}
```

`api_tree`, `api_blob`, `api_log`, `api_commit` follow the same three moves: `open`, parse the oid
(`.parse::<ObjectId>()` — a bad oid is `not_found()`), call the `browse` function, `Json` the
result. `browse` runs blocking odb reads, so wrap each call in
`tokio::task::spawn_blocking`, matching how `upload_pack` already keeps odb work off the runtime.
`api_log` clamps `n` to `1..=200`.

Then:

```rust
pub fn browse_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/{owner}/{name}/refs", get(api_refs))
        .route("/api/{owner}/{name}/tree/{oid}", get(api_tree_root))
        .route("/api/{owner}/{name}/tree/{oid}/{*path}", get(api_tree))
        .route("/api/{owner}/{name}/blob/{oid}/{*path}", get(api_blob))
        .route("/api/{owner}/{name}/log/{oid}", get(api_log))
        .route("/api/{owner}/{name}/commit/{oid}", get(api_commit))
}
```

In `src/http.rs`, mount it on the peer router only, inside the `route` middleware so requests reach
the owner:

```rust
pub fn peer_router(app: Arc<App>) -> Router {
    git_routes()
        .merge(browse_api::browse_routes())
        .route("/healthz", get(healthz))
        // ... unchanged
}
```

And teach `is_git_route` that these are repo-scoped, so `route` forwards rather than ignores them:

```rust
fn is_git_route(path: &str) -> bool {
    // `/api/{owner}/{name}/...` is repo-scoped exactly as the git routes are: it must reach the
    // owner, because only the owner holds the database and the packs.
    if let Some(rest) = path.strip_prefix("/api/") {
        return rest.split('/').count() >= 3;
    }
    // ... existing checks unchanged
}
```

The `route` middleware extracts `{owner}/{name}` from the path; extend its parser to skip a leading
`/api` segment so both shapes yield the same repo.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test browse_http && cargo test --test routing && cargo test --test proxy`
Expected: PASS. `routing` and `proxy` prove forwarding still behaves for git paths.

- [ ] **Step 5: Commit**

```bash
git add src/http.rs src/http/browse_api.rs tests/browse_http.rs Cargo.toml Cargo.lock
git commit -m "Serve refs, trees, blobs, log and diffs on the peer port"
```

---

### Task 4: Redis cache

**Files:**
- Create: `src/cache.rs`
- Modify: `src/lib.rs` (add `pub mod cache;`), `Cargo.toml` (add `redis`)
- Test: `src/cache.rs` unit tests (key construction), `tests/cache.rs` (fail-open behaviour)

**Interfaces:**
- Produces:
  - `pub struct Cache { conn: Option<redis::aio::ConnectionManager> }`
  - `pub async fn Cache::connect(url: Option<&str>) -> Cache` — `None` or a failed connect yields a disabled cache, never an error
  - `pub async fn Cache::get(&self, repo: &str, suffix: &str) -> Option<Vec<u8>>`
  - `pub async fn Cache::put(&self, repo: &str, suffix: &str, val: &[u8], ttl_secs: u64)`
  - `pub async fn Cache::drop_refs(&self, repo: &str)`
  - `pub async fn Cache::bump_generation(&self, repo: &str)`
  - `pub fn key(gen: u64, repo: &str, suffix: &str) -> String`

- [ ] **Step 1: Write the failing test**

In `src/cache.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_carry_version_generation_and_repo() {
        assert_eq!(key(7, "alice/web", "tree:abc:src"), "v1:7:alice/web:tree:abc:src");
    }

    #[tokio::test]
    async fn a_disabled_cache_answers_without_failing() {
        let c = Cache::connect(None).await;
        assert!(c.get("alice/web", "refs").await.is_none());
        c.put("alice/web", "refs", b"x", 5).await;   // must not panic
        c.drop_refs("alice/web").await;
        c.bump_generation("alice/web").await;
    }

    #[tokio::test]
    async fn an_unreachable_redis_degrades_to_disabled() {
        // Port 1 refuses instantly; connect must swallow it rather than propagate.
        let c = Cache::connect(Some("redis://127.0.0.1:1")).await;
        assert!(c.get("alice/web", "refs").await.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cache -- --nocapture`
Expected: FAIL — `cannot find module cache`.

- [ ] **Step 3: Write minimal implementation**

```rust
//! A response cache the api tier shares. Every entry is keyed by an immutable object id, so a
//! hit is safe to serve from any pod without consulting the node that owns the repo.
//!
//! Every operation fails open: a cache that is down or absent makes requests slower, never wrong.

const KEY_VERSION: &str = "v1";
const GEN_TTL: u64 = 3600;

pub fn key(generation: u64, repo: &str, suffix: &str) -> String {
    format!("{KEY_VERSION}:{generation}:{repo}:{suffix}")
}

pub struct Cache {
    conn: Option<redis::aio::ConnectionManager>,
}

impl Cache {
    pub async fn connect(url: Option<&str>) -> Cache {
        let Some(url) = url else { return Cache { conn: None } };
        let conn = async {
            redis::Client::open(url).ok()?.get_connection_manager().await.ok()
        }.await;
        if conn.is_none() {
            eprintln!("cache: {url} unreachable; serving without it"); // ponytail: eprintln
        }
        Cache { conn }
    }

    /// The repo's current generation. A miss means one: a repo that has never been purged.
    async fn generation(&self, repo: &str) -> u64 {
        let Some(mut c) = self.conn.clone() else { return 1 };
        redis::cmd("GET").arg(format!("gen:{repo}")).query_async(&mut c).await.unwrap_or(None).unwrap_or(1)
    }

    pub async fn get(&self, repo: &str, suffix: &str) -> Option<Vec<u8>> {
        let mut c = self.conn.clone()?;
        let k = key(self.generation(repo).await, repo, suffix);
        redis::cmd("GET").arg(k).query_async(&mut c).await.ok().flatten()
    }

    pub async fn put(&self, repo: &str, suffix: &str, val: &[u8], ttl_secs: u64) {
        let Some(mut c) = self.conn.clone() else { return };
        let k = key(self.generation(repo).await, repo, suffix);
        let _: Result<(), _> = redis::cmd("SET").arg(k).arg(val).arg("EX").arg(ttl_secs)
            .query_async::<()>(&mut c).await;
    }

    pub async fn drop_refs(&self, repo: &str) {
        let Some(mut c) = self.conn.clone() else { return };
        let k = key(self.generation(repo).await, repo, "refs");
        let _: Result<(), _> = redis::cmd("DEL").arg(k).query_async::<()>(&mut c).await;
    }

    /// Orphans every cached answer for a repo at once. Used when a repo is deleted, or when its
    /// visibility flips — after which no previously cached response may be served to anyone.
    /// No SCAN: the old keys simply become unreachable and age out under `allkeys-lru`.
    pub async fn bump_generation(&self, repo: &str) {
        let Some(mut c) = self.conn.clone() else { return };
        let k = format!("gen:{repo}");
        let _: Result<(), _> = redis::cmd("INCR").arg(&k).query_async::<()>(&mut c).await;
        let _: Result<(), _> = redis::cmd("EXPIRE").arg(&k).arg(GEN_TTL).arg("XX").query_async::<()>(&mut c).await;
    }
}
```

The generation key gets a rolling hour TTL rather than living forever: entries it guards expire
within 7 days, so an idle repo's counter is dead weight after that.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib cache -- --nocapture`
Expected: PASS, including the unreachable-Redis case, with the "unreachable" line on stderr.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/cache.rs src/lib.rs
git commit -m "A fail-open response cache keyed by object id"
```

---

### Task 5: The api server

**Files:**
- Create: `src/api.rs`
- Modify: `src/main.rs` (dispatch `api-serve`, usage string), `src/lib.rs`
- Test: `tests/api_server.rs`

**Interfaces:**
- Consumes: `cache::Cache`, `store::Store::owner_for_token`, `proxy::PEER_HEADER`/`OWNER_HEADER`.
- Produces: `pub async fn api::serve(store: Arc<Store>, cache: Arc<Cache>, upstream: String, secret: String, listener: TcpListener) -> Result<()>`

Flow per request: authenticate the token locally → read `meta` from cache; on a miss ask upstream →
authorize → cache lookup → hit serves, miss forwards to upstream, caches, serves.

- [ ] **Step 1: Write the failing test**

`tests/api_server.rs` runs a fake upstream that counts requests, proving a hit skips it:

```rust
mod common;

#[tokio::test]
async fn a_cache_hit_does_not_touch_upstream() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    // Fake git node: answers every /api path with a fixed body and counts the call.
    let upstream = axum::Router::new().fallback(axum::routing::any(move || {
        let h = h.clone();
        async move { h.fetch_add(1, Ordering::SeqCst); axum::Json(serde_json::json!([{"name":"refs/heads/master"}])) }
    }));
    let ul = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uaddr = ul.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ul, upstream).await.unwrap() });

    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    // An in-process stand-in for Redis: the same two calls the api path makes.
    let cache = std::sync::Arc::new(kloudlite_git::cache::Cache::connect(None).await);
    let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let aaddr = al.local_addr().unwrap();
    let store = e.store.clone();
    tokio::spawn(async move {
        kloudlite_git::api::serve(store, cache, format!("http://{uaddr}"), "s".into(), al).await.unwrap()
    });

    let c = reqwest::Client::new();
    let url = format!("http://{aaddr}/api/alice/web/tree/abc123");
    for _ in 0..2 {
        let r = c.get(&url).basic_auth("x", Some(&token)).send().await.unwrap();
        assert_eq!(r.status(), 200);
    }
    // Cache is disabled in this test, so both calls go upstream. With Redis, the second would not.
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn no_token_and_a_private_repo_is_401() {
    // upstream refuses anonymous access the way a git node does
    let upstream = axum::Router::new().fallback(axum::routing::any(|| async { axum::http::StatusCode::UNAUTHORIZED }));
    let ul = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uaddr = ul.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ul, upstream).await.unwrap() });
    let e = common::env().await;
    let cache = std::sync::Arc::new(kloudlite_git::cache::Cache::connect(None).await);
    let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let aaddr = al.local_addr().unwrap();
    let store = e.store.clone();
    tokio::spawn(async move {
        kloudlite_git::api::serve(store, cache, format!("http://{uaddr}"), "s".into(), al).await.unwrap()
    });
    let r = reqwest::get(format!("http://{aaddr}/api/alice/web/refs")).await.unwrap();
    assert_eq!(r.status(), 401);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test api_server -- --nocapture`
Expected: FAIL — `unresolved import kloudlite_git::api`.

- [ ] **Step 3: Write minimal implementation**

```rust
//! The read API's own process. It holds no repository state: it authenticates, consults the
//! cache, and on a miss asks the git fleet, whose routing already knows which node owns what.

pub struct Api {
    pub store: Arc<Store>,
    pub cache: Arc<Cache>,
    /// Base URL of the git peer Service, e.g. `http://kloudlite-git:8081`.
    pub upstream: String,
    pub secret: String,
    pub client: reqwest::Client,
}

/// How long each kind of answer is kept. Only `refs` can go stale; the rest are keyed by an
/// object id and are true forever, so their TTL is an eviction hint rather than a correctness one.
const TTL_REFS: u64 = 5;
const TTL_IMMUTABLE: u64 = 7 * 24 * 3600;
const TTL_META: u64 = 30;
const MAX_CACHED_BODY: usize = 1 << 20;

async fn handle(State(api): State<Arc<Api>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let Some((repo, suffix)) = split_api_path(&path, req.uri().query()) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let token = bearer_or_basic(req.headers());
    let caller = match &token {
        Some(t) => match api.store.owner_for_token(t).await {
            Ok(Some(o)) => Some(o),
            Ok(None) => return unauthorized(),
            Err(e) => return internal(e),
        },
        None => None,
    };

    // Serve from cache only when this caller is entitled to it without asking a git node.
    if let Some(v) = api.visibility(&repo).await {
        let owner = repo.split('/').next().unwrap_or_default();
        if !kloudlite_git::auth::authorize(caller.as_deref(), owner, v) {
            return if caller.is_none() { unauthorized() } else { not_found() };
        }
        if let Some(body) = api.cache.get(&repo, &suffix).await {
            return cached_response(&repo, body, v, &suffix);
        }
    }

    // Miss. The peer secret plus the owner header is exactly the identity a forwarding node
    // presents, so upstream authorizes this the way it authorizes a peer.
    let mut up = api.client.get(format!("{}{}", api.upstream, full_path(&path, req.uri().query())))
        .header(kloudlite_git::proxy::PEER_HEADER, &api.secret);
    if let Some(c) = &caller {
        up = up.header(kloudlite_git::proxy::OWNER_HEADER, c);
    }
    let r = match up.send().await { Ok(r) => r, Err(e) => { eprintln!("upstream: {e}"); return bad_gateway() } };
    let status = r.status();
    let body = match r.bytes().await { Ok(b) => b, Err(e) => { eprintln!("upstream body: {e}"); return bad_gateway() } };
    if status.is_success() && body.len() <= MAX_CACHED_BODY {
        api.cache.put(&repo, &suffix, &body, if suffix == "refs" { TTL_REFS } else { TTL_IMMUTABLE }).await;
    }
    (status, body).into_response()
}
```

`Api::visibility(&repo) -> Option<bool>` reads the `meta` key from the cache and returns `None` on a
miss — `None` means "cannot decide here", which sends the request upstream where the repo database
can answer. When the upstream reply arrives, cache the flag it implies (`200` to an anonymous
caller means public) under `meta` with `TTL_META`.

`cached_response` sets `Cache-Control: public, max-age=31536000, immutable` for id-addressed
suffixes on a public repo, `public, max-age=5` for `refs` on a public repo, and
`private, no-store` for everything on a private one.

`split_api_path("/api/alice/web/tree/abc/src", None)` returns `("alice/web", "tree:abc:src")`; a
query string becomes part of the suffix, so `log` pagination varies the key.

In `src/main.rs`, dispatch it:

```rust
    if a.first() == Some(&"api-serve") {
        let store = Arc::new(Store::open(object_store()?, cache_dir()?, false).await?);
        let cache = Arc::new(kloudlite_git::cache::Cache::connect(std::env::var("KLOUDLITE_GIT_REDIS_URL").ok().as_deref()).await);
        let upstream = env("KLOUDLITE_GIT_UPSTREAM", "http://kloudlite-git:8081");
        let secret = std::env::var("KLOUDLITE_GIT_PEER_SECRET").map_err(|_| kloudlite_git::err("KLOUDLITE_GIT_PEER_SECRET required"))?;
        let l = tokio::net::TcpListener::bind(env("KLOUDLITE_GIT_API_ADDR", "0.0.0.0:8090")).await?;
        return kloudlite_git::api::serve(store, cache, upstream, secret, l).await;
    }
```

Add `api-serve` to the usage string.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test api_server -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/api.rs src/lib.rs src/main.rs tests/api_server.rs
git commit -m "An api process that fronts the git nodes through the cache"
```

---

### Task 6: Invalidation

**Files:**
- Modify: `src/lib.rs` (`App` gains `pub cache: Arc<Cache>`), `src/protocol/receive.rs` (after refs are updated), `src/refs.rs` (`set_public`, `delete_repo`), `src/main.rs` (`admin purge-cache`, wire a cache into `serve`)
- Test: `tests/cache_invalidation.rs`

**Interfaces:**
- Consumes: Task 4's `Cache::drop_refs`, `Cache::bump_generation`.
- Produces: no new public API; `App::new` gains a trailing `cache: Arc<Cache>` parameter. Update `tests/common/mod.rs::app` to pass `Cache::connect(None)`.

- [ ] **Step 1: Write the failing test**

```rust
mod common;

#[tokio::test]
async fn a_push_drops_the_ref_entry_and_a_flip_bumps_the_generation() {
    // A recording stand-in proves the calls happen; a live Redis is not needed to test wiring.
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();

    let before = kloudlite_git::cache::key(1, "alice/web", "refs");
    assert_eq!(before, "v1:1:alice/web:refs");

    // Visibility changes must orphan every cached answer for the repo.
    e.store.set_public("alice", "web", true).await.unwrap();
    // With a disabled cache this is a no-op that must not fail the write path:
    assert!(e.store.is_public("alice", "web").await.unwrap());
}
```

Also assert wiring directly, since a disabled cache cannot observe calls: add
`#[cfg(test)] pub fn calls(&self) -> usize` to `Cache`, incremented in `drop_refs` and
`bump_generation` even when disabled, and assert it rises after a push in `tests/http_e2e.rs`'s
push helper.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test cache_invalidation -- --nocapture`
Expected: FAIL — `App::new` takes 6 arguments, not 7.

- [ ] **Step 3: Write minimal implementation**

- `App::new` takes and stores `cache: Arc<Cache>`; `serve()` in `main.rs` builds one from
  `KLOUDLITE_GIT_REDIS_URL` and passes it; `tests/common/mod.rs::app` passes a disabled one.
- In `src/protocol/receive.rs`, after `update_refs` reports at least one applied update:

```rust
    // The ref list is the only cached answer a push can invalidate; everything else is keyed by
    // an object id. A failed drop costs at most the 5s TTL, so this never blocks the push.
    if results.iter().any(|r| r.is_none()) {
        app.cache.drop_refs(&format!("{}/{}", repo.owner, repo.name)).await;
    }
```

- In `Store::set_public` and `Store::delete_repo`, call `bump_generation` for the repo. These are
  `Store` methods and `Store` has no cache handle, so give `Store` an
  `pub cache: Arc<Cache>` field set at `Store::open` (defaulting to disabled), rather than
  threading a handle through every caller.
- In `src/main.rs`:

```rust
        ["admin", "purge-cache", path] => {
            let (owner, name) = split_repo(path)?;
            store.cache.bump_generation(&format!("{owner}/{name}")).await;
        }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS across the whole suite — this task changes a constructor every test touches.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/refs.rs src/store.rs src/protocol/receive.rs src/main.rs tests/
git commit -m "Invalidate refs on push and everything on a visibility change"
```

---

### Task 7: Deployment and docs

**Files:**
- Modify: `deploy/kloudlite-git.yaml` (api Deployment + Service)
- Modify: `README.md:110-125` (usage), and the environment variable list

**Interfaces:** none — configuration and prose only.

- [ ] **Step 1: Add the api Deployment**

Append to `deploy/kloudlite-git.yaml`. It is a Deployment, not a StatefulSet: this tier owns no repo
and holds no lease, so pods are interchangeable.

```yaml
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: kloudlite-git-api, namespace: kloudlite-git }
spec:
  replicas: 2
  selector: { matchLabels: { app: kloudlite-git-api } }
  template:
    metadata: { labels: { app: kloudlite-git-api } }
    spec:
      containers:
        - name: api
          image: kloudlite-git:latest
          args: ["api-serve"]
          ports: [{ name: http, containerPort: 8090 }]
          env:
            # The peer Service, not the public LB: this tier speaks to the fleet as a peer.
            - { name: KLOUDLITE_GIT_UPSTREAM, value: "http://kloudlite-git:8081" }
            - { name: KLOUDLITE_GIT_API_ADDR, value: "0.0.0.0:8090" }
            - name: KLOUDLITE_GIT_PEER_SECRET
              valueFrom: { secretKeyRef: { name: kloudlite-git-peer, key: secret } }
            - name: KLOUDLITE_GIT_REDIS_URL
              valueFrom: { secretKeyRef: { name: kloudlite-git-redis, key: url } }
            - { name: KLOUDLITE_GIT_S3_URL, value: "s3://REPLACE_ME" }
          readinessProbe: { httpGet: { path: /healthz, port: http }, periodSeconds: 5 }
---
apiVersion: v1
kind: Service
metadata: { name: kloudlite-git-api, namespace: kloudlite-git }
spec:
  type: LoadBalancer
  selector: { app: kloudlite-git-api }
  ports: [{ name: http, port: 80, targetPort: http }]
```

Add a comment above the Service recording the two operational facts from the spec: the LoadBalancer
must be restricted to Cloudflare's ranges via `loadBalancerSourceRanges` or the WAF is bypassable,
and SSH on 2222 does not traverse Cloudflare at all.

Give `api::serve` a `/healthz` route returning 200 so the probe above has something to hit.

- [ ] **Step 2: Verify the manifest parses**

Run: `kubectl apply --dry-run=client -f deploy/kloudlite-git.yaml`
Expected: every object listed as "created (dry run)", no errors.

- [ ] **Step 3: Document it**

In `README.md`, add to the usage block:

```
kloudlite-git api-serve                                          # read API; needs KLOUDLITE_GIT_UPSTREAM
kloudlite-git admin set-visibility <owner>/<name> public|private
kloudlite-git admin purge-cache <owner>/<name>
```

Add to the environment variable list: `KLOUDLITE_GIT_REDIS_URL` (optional; without it the api serves
every request from the git nodes), `KLOUDLITE_GIT_UPSTREAM`, `KLOUDLITE_GIT_API_ADDR`.

Add a short `## Browsing` section: branch names appear only in `/refs`, every other URL takes the
object id it returns, and that is what lets any api pod answer from cache without involving the
node that owns the repo.

- [ ] **Step 4: Verify the docs match the code**

Run: `cargo run -- 2>&1 | head -3` and confirm the usage string lists `api-serve`,
`set-visibility`, and `purge-cache` exactly as the README does.

- [ ] **Step 5: Commit**

```bash
git add deploy/kloudlite-git.yaml README.md src/api.rs
git commit -m "Deploy the api tier and document it"
```
