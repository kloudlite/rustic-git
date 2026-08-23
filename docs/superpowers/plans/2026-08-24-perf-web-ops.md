# Web & Ops Performance Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the web-app and ops findings from the 2026-08-24 performance review — runtime sizing, HTTP compression, immutable browse caching, shell payload slimming, and the P1/P2 web batch — each as its own commit.

**Architecture:** Ops fixes are env vars and one Cargo profile line (yaml-only changes are safe to roll without an image repin). Compression is a `tower-http` `CompressionLayer` mounted on exactly two routers: `browse_routes()` (peer listener, api↔fleet hop) and the api tier's `/v1` router (web↔api hop) — never on git pack or registry blob routes, which serve already-compressed bytes. Two small new Rust endpoints back the web fixes: `GET /v1/repos/{owner}/{name}` for `guardRepo`, and `?state=&limit=` + `commentCount` on the pulls list. All web source is under `web/apps/web/src/`; fixes copy existing siblings and change no visible behavior except where noted.

**Tech Stack:** axum 0.8, tower 0.5, tower-http 0.6 (NEW dep, `compression-gzip` only), reqwest 0.13 (+`gzip` feature), Next.js 16 app router (read `node_modules/next/dist/docs/` when unsure — it differs from training data), React 19, bun 1.3.

**Spec:** `docs/perf-review-2026-08-24.md` — findings P0-9, P0-10, P0-11, P0-12, the Web P1 list, the Ops P1 list, and the web/ops P2 bullets.

## Global Constraints

- **Single-opener invariant** (`CLAUDE.md`): any route touching a per-repo/per-image database must route — `BROWSE_TAILS` in `src/http.rs` is the contract. Neither new endpoint in this plan opens a per-repo DB (`get_repo` reads an `index/` marker from the shared object store; the pulls change edits an already-routed handler), so no `BROWSE_TAILS` change is needed — and none may be made.
- **Markers under `index/` are views for listings, never authorization.** `get_repo` may read a marker only AFTER `settings_caller` has established membership, exactly as `list_repos` does.
- Rust gates: `cargo test` green and `cargo clippy --lib -- -D warnings` green before every Rust commit.
- Web gates: from `web/`, `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` after every web task. Editor TS diagnostics are stale; trust `tsc`. Tests run with `bun test`.
- House style: comments explain WHY; preserve existing `// ponytail:` markers except where the fix removes the ceiling the marker names; tokens over raw Tailwind colors; `--radius: 0`; copy existing siblings.
- Commit subjects: imperative sentence case, no tool attribution, no "claude" anywhere.
- Deploy coupling: env-var/probe-only yaml edits do NOT require an image repin. The compression and endpoint changes DO ship in a new image — deploying them is outside this plan (see `CLAUDE.md` "Deploying").
- Perf fixes must not change behavior; where a regression test is impractical (pure config, RSC payload shape) the task says so and relies on existing tests plus lint/tsc.

**Deliberately excluded spec findings** (say why once, here):
- *Ops P1 "server memory requests → 256Mi"*: stale. `deploy/rustic-git.yaml` already requests 384Mi on `rustic-git-srv` with a measured rationale in the comment above it, 96Mi on the leader (measured 8Mi steady), and 256Mi on api/worker. Nothing to change; tell the user to re-validate against live RSS if they still want to.

---

## Ops

### Task 1: Size tokio and V8 from the pod, not the node (P0-9 + P2 probe)

**Files:**
- Modify: `deploy/rustic-git.yaml` — the `env:` blocks of all four Rust workloads: `rustic-git-leader` (env starts ~line 56), `rustic-git-srv` (~line 288), `rustic-git-api` (~line 608), `rustic-git-worker` (~line 715). (Spec says "three workloads"; there are four — leader, srv, api, worker — all get the var.)
- Modify: `deploy/rustic-git-web.yaml` — web `env:` block (~line 34) and the readinessProbe `periodSeconds: 5` at line 90.

**Context:** Bare `#[tokio::main]` sizes worker threads from the node's cores; tokio reads `TOKIO_WORKER_THREADS` from the environment (verified in vendored `tokio-1.53.1/src/runtime/builder.rs:473` — the builder default reads it). Node's V8 old-space ceiling is sized from host memory, so the 512Mi web pod OOMKills instead of GC'ing; `--max-old-space-size=384` leaves headroom under the limit.

**Interfaces:** none — pure yaml. No test possible; `kubectl apply` validation is the check.

- [ ] **Step 1:** In `deploy/rustic-git.yaml`, add to each of the four containers' `env:` lists (comment once per workload, WHY-style):

```yaml
            # Tokio sizes its pool from the NODE's cores — 64 threads inside a
            # 100m-CPU pod. Four matches what the request can actually run.
            - name: TOKIO_WORKER_THREADS
              value: "4"
```

- [ ] **Step 2:** In `deploy/rustic-git-web.yaml`, add to the web container's `env:`:

```yaml
            # V8 sizes its heap from HOST memory, so the pod OOMKills instead of
            # GC'ing. 384 leaves the rest of the 512Mi limit for buffers and stack.
            - name: NODE_OPTIONS
              value: "--max-old-space-size=384"
```

- [ ] **Step 3:** Same file, readinessProbe `periodSeconds: 5` → `10` (line 90). The startup probe already owns cold start; 5s readiness polling on a Next server buys nothing.
- [ ] **Step 4:** `kubectl apply --dry-run=client -f deploy/rustic-git.yaml -f deploy/rustic-git-web.yaml` to validate syntax (or `kubectl create --dry-run=client` if not connected — any yaml parse is enough).
- [ ] **Step 5:** Commit: `git add deploy/ && git commit -m "Size tokio and V8 from the pod, not the node"`

### Task 2: `panic = "abort"` in the release profile (Ops P1)

**Files:**
- Modify: `Cargo.toml:95-98` (`[profile.release]` currently `lto = "thin"`, `codegen-units = 1`, `strip = true`)

**Context:** No `catch_unwind` anywhere in `src/` (verified by grep). Test profiles are untouched — they keep unwinding.

- [ ] **Step 1:** Add `panic = "abort"` to `[profile.release]`.
- [ ] **Step 2:** `cargo build --release` compiles (debug `cargo test` is unaffected by the release profile but run it anyway: `cargo test`).
- [ ] **Step 3:** Commit: `git add Cargo.toml && git commit -m "Abort on panic in release builds"`

---

## Rust endpoints and compression

### Task 3: Gzip the browse and api JSON hops (P0-10)

**Files:**
- Modify: `Cargo.toml` — new dep next to `tower` (line 88), and `reqwest` features (line 52)
- Modify: `src/http/browse_api/mod.rs` — `browse_routes()` (starts ~line 110)
- Modify: `src/api/mod.rs` — the api router (ends `.fallback(axum::routing::get(handle)).with_state(api)` ~line 186)
- Modify: `tests/browse_http.rs` — one new test

**Context:** No compression exists anywhere; browse JSON (trees, logs, whole diffs, base64 blobs) crosses api↔fleet and web↔api uncompressed. The layer mounts on exactly the two JSON routers. `peer_router` and `router` in `src/http.rs` are NOT touched: they merge `git_routes()` and `v2_routes()`, whose payloads (packs, blobs) are already compressed. The api tier's reqwest client only benefits on the api↔fleet hop if it sends `Accept-Encoding: gzip` and transparently decompresses — that is reqwest's `gzip` feature (currently absent because `default-features = false`). The web app's Node fetch (undici) already sends `accept-encoding` and decompresses, so the web↔api hop works with no web change.

**Justification for the new dep:** tower-http 0.6 is the axum-team-maintained middleware crate for the tower 0.5 / axum 0.8 already in the tree; `default-features = false, features = ["compression-gzip"]` pulls only the gzip codec. Hand-rolling streaming gzip negotiation is more code than the dependency.

**Interfaces:**
- Produces: `Content-Encoding: gzip` on browse/api responses over ~32 bytes (tower-http's default `SizeAbove` predicate) when the caller sends `Accept-Encoding: gzip`. No route or body-shape change.

- [ ] **Step 1: Write the failing test.** In `tests/browse_http.rs`, add (a `post_json` helper is needed — model it on the existing `post_as` at line 25, adding `.header("content-type", "application/json")` and a JSON body string):

```rust
/// Compression mounts on the browse router alone: git packs and registry blobs
/// are already compressed and must not pay for a second pass. The pull's long
/// body pushes the response past the compressor's minimum-size predicate.
#[tokio::test(flavor = "multi_thread")]
async fn browse_json_is_gzipped_when_asked_for() {
    let e = common::env().await;
    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    let body = format!(
        r#"{{"title":"{}","body":"","base":"refs/heads/main","head":"refs/heads/topic","author":"a@example.com"}}"#,
        "long enough to clear the size-above predicate ".repeat(4)
    );
    let status = post_json_as(&router, "alice", "/api/alice/widget/pulls", &body).await;
    assert!(status.is_success(), "opening the pull: {status}");

    let req = Request::builder()
        .uri("/api/alice/widget/pulls")
        .header(rustic_git::proxy::PEER_HEADER, "test-peer-secret")
        .header(rustic_git::proxy::OWNER_HEADER, "alice")
        .header("accept-encoding", "gzip")
        .body(axum::body::Body::empty())
        .unwrap();
    let r = router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers().get("content-encoding").and_then(|v| v.to_str().ok()),
        Some("gzip"),
    );
}
```

Before finalizing the helper, read how `api_pull_open` deserializes `NewPull` (`src/http/browse_api/pulls.rs`, struct at ~line 133) and match its field names — the sketch above assumes `title`/`body`/`base`/`head`/`author`; fix the JSON to whatever the struct actually requires and use whatever 2xx it actually returns.

- [ ] **Step 2:** `cargo test --test browse_http browse_json_is_gzipped` — fails (no `content-encoding`).
- [ ] **Step 3:** `Cargo.toml`:

```toml
reqwest = { version = "0.13", default-features = false, features = ["stream", "query", "json", "gzip"] }
```

and next to `tower`:

```toml
# Only the gzip compressor: mounted on the two JSON routers (browse, /v1) and
# nowhere near packs or registry blobs, which are already compressed.
tower-http = { version = "0.6", default-features = false, features = ["compression-gzip"] }
```

- [ ] **Step 4:** In `src/http/browse_api/mod.rs`, at the end of the `browse_routes()` chain (after the last `.route(...)`), add:

```rust
        // Browse answers are JSON — trees, logs, whole diffs, base64 blobs — and
        // 5-10x smaller gzipped. This router alone: packs and registry blobs are
        // already compressed, and their routers never merge this one.
        .layer(tower_http::compression::CompressionLayer::new())
```

- [ ] **Step 5:** In `src/api/mod.rs`, insert the same layer between `.fallback(axum::routing::get(handle))` and `.with_state(api)` (everything the /v1 router serves is JSON; the fallback's proxied browse bodies are decompressed by reqwest's gzip support before they get here, so this recompresses them for the web hop):

```rust
        .layer(tower_http::compression::CompressionLayer::new())
```

- [ ] **Step 6:** `cargo test --test browse_http && cargo test --test api_server && cargo clippy --lib -- -D warnings`, then the full `cargo test`.
- [ ] **Step 7:** Commit: `git add Cargo.toml Cargo.lock src/http/browse_api/mod.rs src/api/mod.rs tests/browse_http.rs && git commit -m "Gzip browse and api JSON responses"`

### Task 4: `GET /v1/repos/{owner}/{name}` (Web P1: guardRepo, Rust half)

**Files:**
- Modify: `src/api/repos.rs` — new `get_repo` after `list_repos` (~line 246)
- Modify: `src/api/mod.rs:145-148` — add `.get(get_repo)` to the existing `/v1/repos/{owner}/{name}` route; import in the `use` that pulls repo handlers
- Modify: `tests/api_server.rs` — two tests

**Context:** `guardRepo` in the web app lists the whole namespace to check one repo. The listing comes from `index/` markers in the shared object store (`repo_listing` → `crate::index::list`), and `crate::index::read(os, Kind::Repo, owner, name) -> Option<Marker>` (`src/index.rs:136`) already reads exactly one. No per-repo DB is touched, so this route needs no `BROWSE_TAILS` entry — the spec's worry about routing does not apply here, and adding one would be wrong (this is api-tier, not the fleet's browse router). Authorization is `settings_caller` (validates both path segments, then membership), the exact gate `update_repo`/`delete_repo` on the same path already use; a non-member and a missing repo both answer 404, preserving the enumeration-proofing `list_repos` documents.

**Interfaces:**
- Produces: `GET /v1/repos/{owner}/{name}` → 200 with one `RepoOut` (same JSON shape as one element of the list: `_id`, `owner`, `name`, `public`, `description`, `createdBy`, `createdAt`), 404 for non-member/missing, 401 anonymous, 503 with no directory.
- Consumes: `settings_caller` (`src/api/repos.rs:260`), `crate::index::read`, `RepoOut`.

- [ ] **Step 1: Write the failing tests** in `tests/api_server.rs`, copying the shapes of `creating_a_repo_refuses_an_anonymous_caller` and `creating_a_repo_asks_the_directory_before_it_asks_the_fleet` (lines ~489-548):

```rust
/// Reading one repo is scoped exactly like listing them: identity first, then
/// the directory's membership answer, and the fleet is never asked at all.
#[tokio::test(flavor = "multi_thread")]
async fn getting_one_repo_refuses_an_anonymous_caller() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let r = reqwest::Client::new().get(format!("{base}/v1/repos/alice/web")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(up.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn getting_one_repo_asks_the_directory_before_anything_else() {
    let e = common::env().await;
    let up = upstream(axum::http::StatusCode::OK).await;
    let base = api_with_jwt(&e, &up, KEY).await;
    let token = rustic_git::jwt::Jwt::new(KEY).unwrap().mint("k@example.com", "K", Some("k")).unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/v1/repos/alice/web"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "only the absent database should stop it");
    assert_eq!(up.hits.load(Ordering::SeqCst), 0, "a marker read never touches the fleet");
}
```

The membership-satisfied happy path needs a live directory (Mongo), which no test in this file has; the marker→`RepoOut` mapping is the same seven lines `repo_listing` is already tested through. Say so in the commit if asked — this is the accepted ceiling.

- [ ] **Step 2:** `cargo test --test api_server getting_one_repo` — both fail (405: the route only takes PATCH/DELETE today).
- [ ] **Step 3:** In `src/api/repos.rs`, after `list_repos`:

```rust
/// One repo, for the page guard that today lists the whole namespace to check a
/// single name. Same gate as the settings routes on this path, same 404 for
/// missing and not-yours; the marker under `index/` is only a view — membership
/// was decided above it, never by it.
pub(crate) async fn get_repo(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    match crate::index::read(&api.store.os, crate::index::Kind::Repo, &owner, &name).await {
        Some(m) => axum::Json(RepoOut {
            id: format!("{owner}/{}", m.name),
            owner: owner.clone(),
            name: m.name,
            public: m.public,
            description: m.description,
            created_by: m.created_by,
            created_at: m.created_ms,
        })
        .into_response(),
        None => (StatusCode::NOT_FOUND, "no such repository").into_response(),
    }
}
```

(Read `RepoOut`'s actual field list at the top of `repos.rs` first and mirror the construction in `create_repo`/`repo_listing` exactly — the snippet above copies `repo_listing`'s mapping.)

- [ ] **Step 4:** In `src/api/mod.rs`, change the route to `axum::routing::patch(update_repo).delete(delete_repo).get(get_repo)` and add `get_repo` to the repos import.
- [ ] **Step 5:** `cargo test --test api_server && cargo clippy --lib -- -D warnings`, full `cargo test`.
- [ ] **Step 6:** Commit: `git add src/api/repos.rs src/api/mod.rs tests/api_server.rs && git commit -m "Serve a single repo for the page guard"`

### Task 5: Pulls list — `commentCount`, `?state=`, `?limit=` (Web P1, Rust half)

**Files:**
- Modify: `src/http/browse_api/pulls.rs` — `api_pulls` (~line 88)
- Modify: `src/api/pulls.rs` — `list_pulls` (~line 31): forward the two query params
- Modify: `tests/browse_http.rs` — one new test

**Context:** The list serializes every PR with its full comment array; the page renders only a count. The owning node's handler is already routed (`pulls` is in `BROWSE_TAILS`), so this is a change inside an existing routed route — no routing work. The DETAIL route (`api_pull`) keeps full comments; only the list slims. Filtering/truncating happens after `pulls::list` reads everything — the "deserialize every PR" cost is the spec's separate P1 server finding (`merge/queued/{n}` index), not this task's.

**Interfaces:**
- Produces: `GET /api/{owner}/{name}/pulls?state=open&limit=50` — each element is the PR's JSON minus `comments`, plus `"commentCount": <n>`. No params = all PRs, same order as today (newest first).
- `GET /v1/repos/{owner}/{name}/pulls` forwards `state`/`limit` through to the owning node.

- [ ] **Step 1: Write the failing test** in `tests/browse_http.rs` (reuse the `post_json_as` helper from Task 3):

```rust
/// The list is for a page that renders a COUNT — shipping every comment body to
/// draw a number is what this asserts away. The detail route keeps them.
#[tokio::test(flavor = "multi_thread")]
async fn the_pull_list_carries_a_comment_count_not_the_comments() {
    let e = common::env().await;
    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    let open = r#"{"title":"t","body":"","base":"refs/heads/main","head":"refs/heads/topic","author":"a@example.com"}"#;
    assert!(post_json_as(&router, "alice", "/api/alice/widget/pulls", open).await.is_success());
    assert!(post_json_as(
        &router, "alice", "/api/alice/widget/pulls/1/comments",
        r#"{"body":"looks fine","author":"b@example.com"}"#,
    ).await.is_success());

    let (status, list) = get_as(&router, "alice", "/api/alice/widget/pulls?state=open&limit=10").await;
    assert_eq!(status, StatusCode::OK);
    let row = &list.as_array().unwrap()[0];
    assert_eq!(row["commentCount"], 1);
    assert!(row.get("comments").is_none(), "the array must not travel with the list");

    let (_, closed) = get_as(&router, "alice", "/api/alice/widget/pulls?state=merged").await;
    assert_eq!(closed.as_array().unwrap().len(), 0, "state filters");

    let (_, detail) = get_as(&router, "alice", "/api/alice/widget/pulls/1").await;
    assert!(detail.get("comments").is_some(), "the detail keeps full comments");
}
```

As in Task 3, read `NewPull` and the comment route's body struct first and correct the JSON field names to match.

- [ ] **Step 2:** `cargo test --test browse_http the_pull_list` — fails.
- [ ] **Step 3:** In `src/http/browse_api/pulls.rs`, change `api_pulls` to take `Query(q): Query<HashMap<String, String>>` (the imports at the top already have `Query` and `HashMap`) and replace the `Ok(mut v)` arm:

```rust
        Ok(mut v) => {
            v.reverse();
            // The page renders a count, so the list carries a count: a 25-PR page
            // was shipping every comment body ever written just to say "3 comments".
            // Filtering happens on the serialized value so the state names here are
            // exactly the ones the wire already speaks.
            let mut out: Vec<serde_json::Value> = v
                .into_iter()
                .map(|p| {
                    let n = p.comments.len();
                    let mut j = serde_json::to_value(&p).unwrap_or_default();
                    if let Some(o) = j.as_object_mut() {
                        o.remove("comments");
                        o.insert("commentCount".into(), n.into());
                    }
                    j
                })
                .collect();
            if let Some(want) = q.get("state") {
                out.retain(|j| j["state"] == serde_json::Value::String(want.clone()));
            }
            if let Some(n) = q.get("limit").and_then(|s| s.parse::<usize>().ok()) {
                out.truncate(n);
            }
            Json(out).into_response()
        }
```

- [ ] **Step 4:** In `src/api/pulls.rs`, add to `list_pulls` a `axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>` extractor and build the forwarded path:

```rust
    let mut path = format!("/api/{}/{}/pulls", encode(&owner), encode(&name));
    let mut sep = '?';
    for k in ["state", "limit"] {
        if let Some(v) = q.get(k) {
            path.push(sep);
            sep = '&';
            path.push_str(k);
            path.push('=');
            path.push_str(&encode(v));
        }
    }
    read_from_owner(&api, &owner, path).await
```

(The forward itself cannot be integration-tested here — `list_pulls` stops at the absent directory in `tests/api_server.rs`, per `pull_routes_ask_the_directory_before_the_fleet`. The owning-node test above covers the observable behavior; the three forwarding lines ride on review.)

- [ ] **Step 5:** `cargo test --test browse_http --test pulls --test api_server && cargo clippy --lib -- -D warnings`, full `cargo test`.
- [ ] **Step 6:** Commit: `git add src/http/browse_api/pulls.rs src/api/pulls.rs tests/browse_http.rs && git commit -m "Slim the pull list to a comment count and accept state and limit"`

---

## Web

### Task 6: Cache immutable browse responses (P0-11)

**Files:**
- Modify: `web/apps/web/src/lib/browse.ts:34-51` (`get()`), and the callers `tree`, `blob`, `log`, `commit`, `files`, `lastChanges`

**Context:** Every browse fetch is `cache: "no-store"`, yet everything keyed by an oid is immutable (the file's own header comment says so: "every answer is immutable except `refs`"). `refs`, `images`, `imageTags`, and `post()` stay `no-store`.

**SECURITY GATE before writing code:** private-repo bodies must never be served from cache to a caller with a different token. Read the installed Next's data-cache docs (`web/node_modules/next/dist/docs/`, the `fetch` caching pages) and confirm whether the request **headers participate in the data-cache key**. If they do (expected), proceed as below. If they do not, append the token to the cache identity instead — e.g. pass `next: { revalidate: false, tags: [...] }` is NOT sufficient; in that case key by URL alone is unsafe and the fix is to keep `no-store` for token-bearing requests and cache only anonymous ones (`immutable && !token`). Record which branch was taken in the commit body.

**Interfaces:**
- `get<T>(path, token?, immutable = false)` — internal only; exported signatures unchanged.

- [ ] **Step 1:** Do the docs check above.
- [ ] **Step 2:** In `get()`:

```ts
async function get<T>(path: string, token?: string, immutable = false): Promise<ApiResult<T>> {
  ...
    // Oid-keyed answers never change (see the header comment), so they are the
    // one thing worth keeping across requests. `refs` moves and stays no-store.
    res = await fetch(`${BASE}${path}`, {
      headers,
      ...(immutable ? { cache: "force-cache" } : { cache: "no-store" }),
    });
```

(adjusted per Step 1 if the key check failed), and pass `true` from `tree`, `blob`, `log`, `commit`, and the `get` calls inside `files` and `lastChanges`. `refs` untouched.

- [ ] **Step 3:** `bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json` (from `web/`). Pure config — no behavior test is practical; existing pages are the safety net.
- [ ] **Step 4:** Manual spot check if a dev server is handy: `bun run dev`, load a repo page twice, confirm the second render's tree/blob fetches are cache hits (Next logs them).
- [ ] **Step 5:** Commit: `git add web/apps/web/src/lib/browse.ts && git commit -m "Cache immutable browse responses"`

### Task 7: Lazy-load the ⌘K repo list and dialog (P0-12 + P1 dynamic CommandDialog)

**Files:**
- Create: `web/apps/web/src/app/api/repos/route.ts`
- Create: `web/apps/web/src/components/app/search-dialog.tsx`
- Modify: `web/apps/web/src/components/app/global-search.tsx`
- Modify: `web/apps/web/src/components/app/app-shell.tsx:53-58` (drop `listRepos`, `apiToken`, the `lists`/`repos` computation and the `repos` prop)

**Context:** The shell runs N `listRepos` calls per hard load and serializes every repo of every owner into every page's RSC payload — only so ⌘K can filter to `r.owner === owner` anyway. The dialog is also cmdk+radix in every page's entry bundle. One change fixes all three: the dialog (and its data) load when ⌘K first opens. The spec suggests shipping the current owner's `{owner,name,public}` eagerly; since the dialog is invisible until opened, shipping nothing eagerly is strictly simpler and loses no visible UX — the palette shows a fetch-in-flight empty state for the first ~50ms.

**Interfaces:**
- Produces: `GET /api/repos?owner={slug}` (Next route handler, session-authenticated via `apiToken()`) → `{ owner, name, public, description }[]`; 400 no owner, 401 no session, 404/502 passthrough.
- `GlobalSearch({ me, owners })` — `repos` prop deleted.
- `SearchDialog({ owner, owners, open, onOpenChange, go })` — client component holding everything that was inside `<CommandDialog>`.

- [ ] **Step 1:** Create the route handler (this is the "simplest: a route handler the client fetches on open" the spec asks for; `apiToken` works in route handlers — it only needs `headers()`):

```ts
import { NextResponse } from "next/server";
import { apiToken } from "@/lib/api-token";
import { listRepos } from "@/lib/api";

/** The ⌘K palette's data, fetched when it OPENS. This used to ride in every
 *  page's RSC payload — every repo of every owner, on every hard load. */
export async function GET(req: Request) {
  const owner = new URL(req.url).searchParams.get("owner");
  if (!owner) return NextResponse.json({ error: "owner is required" }, { status: 400 });
  const token = await apiToken();
  if (!token) return NextResponse.json([], { status: 401 });
  const list = await listRepos(token, owner);
  if (!list.ok) return NextResponse.json([], { status: list.kind === "notFound" ? 404 : 502 });
  // Only what the palette draws — never the whole ApiRepo.
  return NextResponse.json(
    list.value.map((r) => ({ owner: r.owner, name: r.name, public: r.public, description: r.description })),
  );
}
```

- [ ] **Step 2:** Create `search-dialog.tsx`: move the `<CommandDialog>...</CommandDialog>` block and its imports (`CommandDialog` family, `sections`/`settingsSection`, the icons, `ApiRepo`-shaped type) out of `global-search.tsx` into a `"use client"` component. It owns the repo fetch:

```tsx
type PaletteRepo = { owner: string; name: string; public: boolean; description: string };

export function SearchDialog({ owner, owners, open, onOpenChange, go }: { ... }) {
  const [repos, setRepos] = useState<PaletteRepo[] | null>(null);
  useEffect(() => {
    if (!open) return;
    let stale = false;
    fetch(`/api/repos?owner=${encodeURIComponent(owner)}`)
      .then((r) => (r.ok ? r.json() : []))
      .then((v) => { if (!stale) setRepos(v); })
      .catch(() => { if (!stale) setRepos([]); });
    return () => { stale = true; };
  }, [open, owner]);
  const mine = repos ?? [];
  ...
```

The JSX inside is the existing dialog body verbatim, with `r._id` keys replaced by `r.name` (the slim payload has no `_id`; names are unique per owner).

- [ ] **Step 3:** In `global-search.tsx`: drop the `repos` prop and the `mine` filter; load the dialog lazily and mount it only after first open so its chunk and its fetch cost nothing until then:

```tsx
import dynamic from "next/dynamic";
const SearchDialog = dynamic(() => import("./search-dialog").then((m) => m.SearchDialog), { ssr: false });
...
const [opened, setOpened] = useState(false);   // once true, stays mounted so reopening is instant
// setOpen(true) sites also setOpened(true)
...
{opened && <SearchDialog owner={owner} owners={owners} open={open} onOpenChange={setOpen} go={go} />}
```

- [ ] **Step 4:** In `app-shell.tsx`, delete lines 53-58 (`apiToken`/`listRepos` imports, the `token`/`lists`/`repos` block — including the `// ponytail: N calls per hard load...` marker, whose ceiling this task removes) and the `repos={repos}` prop.
- [ ] **Step 5:** `bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json`. Manual check with `bun run dev`: ⌘K opens, lists the current owner's repos, jumps.
- [ ] **Step 6:** Commit: `git add web/apps/web/src && git commit -m "Load the command palette and its repo list on demand"`

### Task 8: `guardRepo` asks for one repo (Web P1, web half — needs Task 4)

**Files:**
- Modify: `web/apps/web/src/lib/api.ts` — new `getRepo` next to `listRepos` (~line 120)
- Modify: `web/apps/web/src/app/(shell)/[owner]/[repo]/guard.ts:34-42`

**Interfaces:**
- Produces: `getRepo(token, owner, name): Promise<ApiResult<ApiRepo>>` — `cache()`-wrapped like `listRepos`, since the layout and its page both call the guard.

- [ ] **Step 1:** In `lib/api.ts`:

```ts
/** One repo, for the page guard — the guard used to list the whole namespace to
 *  check a single name. Cached per render for the same reason `listRepos` is. */
export const getRepo = cache(function getRepo(token: string, owner: string, name: string) {
  return call<ApiRepo>(`/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, {
    method: "GET",
    token,
  });
});
```

- [ ] **Step 2:** In `guard.ts`, replace the `listRepos` call and `find`:

```ts
  const one = await getRepo(token, owner, repo);
  if (!one.ok) {
    // `unauthorized` is the api refusing our token, not a missing repo. Treating
    // it as 404 made an expired session look like every repo had been deleted.
    if (one.kind === "unauthorized") redirect("/login?from=expired");
    if (one.kind === "notFound") notFound();
    throw new Error(one.message);
  }
  return { session, owner, repo, meta: one.value, token };
```

(keep the surrounding session/token lines; swap the import).

- [ ] **Step 3:** `bun run lint && bunx tsc && bun run dev` spot check: a repo page renders, a bogus repo 404s. Note: this ships only after the Task 4 image is deployed; until then the old api answers 405 → `unavailable` → error page. Coordinate the deploy order (server first), as `CLAUDE.md`'s repin rule already requires.
- [ ] **Step 4:** Commit: `git add web/apps/web/src/lib/api.ts "web/apps/web/src/app/(shell)/[owner]/[repo]/guard.ts" && git commit -m "Guard a repo page with one lookup instead of a listing"`

### Task 9: Pull list renders the count the wire now carries (needs Task 5)

**Files:**
- Modify: `web/apps/web/src/lib/api.ts` — `ApiPull` type (~line 380) and `listPulls`
- Modify: `web/apps/web/src/components/repo/pulls.tsx:52-56`

**Interfaces:**
- `ApiPull.comments` becomes optional; new `commentCount?: number` (the list omits `comments`; the detail route still sends them and omits `commentCount`).
- `listPulls(token, owner, name)` → asks for `?limit=100` (the page draws one screenful; 100 is far past it — note the ceiling).

- [ ] **Step 1:** In `lib/api.ts`:

```ts
export type ApiPull = {
  ...
  /** Full bodies on the detail route only; the LIST sends `commentCount` instead. */
  comments?: ApiComment[];
  commentCount?: number;
  ...
};

export function listPulls(token: string, owner: string, name: string) {
  // ponytail: flat 100 cap, no paging; add ?page= when a repo outgrows it
  return call<ApiPull[]>(`${repoPath(owner, name)}/pulls?limit=100`, { method: "GET", token });
}
```

- [ ] **Step 2:** Fix the two consumers of `comments`: in `pulls.tsx` replace `p.comments.length` (both uses) with `const n = p.commentCount ?? p.comments?.length ?? 0` — the fallback keeps the page correct against a not-yet-redeployed api. Grep for other `\.comments` readers (`grep -rn "\.comments" web/apps/web/src`) — the pull detail page reads them from `getPull`, which still sends them; add `?? []` there only if `tsc` demands it.
- [ ] **Step 3:** `bun run lint && bunx tsc`. Existing rendering is the safety net; no new test (display-only change).
- [ ] **Step 4:** Commit: `git add web/apps/web/src && git commit -m "Render pull comment counts without fetching the comments"`

### Task 10: Take the About rail off the file view's critical path (Web P1)

**Files:**
- Modify: `web/apps/web/src/components/repo/file-view.tsx:41-46` and the `<aside>` (~line 128)

**Context:** Every file view awaits `repoRail` — a full recursive file walk plus 50 commits — before any byte of the file renders, and the `<aside>` is `hidden xl:block`, so narrow viewports pay for pixels they never draw (the server cannot know the viewport; that part stays — noted, not fixed). After Task 6 the rail's fetches are data-cache hits on repeat visits, so the remaining win is first-visit latency: Suspense keeps the UX (the rail still appears) while the file stops waiting for it. Dropping the rail entirely would change the page; this doesn't.

Incidental observation for the user, NOT fixed here (perf plan, behavior must not change): `repoRail` calls `log(token, owner, repo, oid, 50)` but `log`'s fifth parameter is `page`, not `limit` (`lib/browse.ts:74`) — that requests page 50, which looks like a real bug. Report it; do not fix it in this plan.

- [ ] **Step 1:** In `file-view.tsx`, remove `repoRail` from the `Promise.all` (await `blob` alone) and move the rail into a co-located async component rendered under Suspense:

```tsx
import { Suspense } from "react";
...
  const b = await blob(token, owner, repo, head.oid, path);
...
      <aside className="hidden xl:block">
        {/* The rail is a walk of the whole tree plus 50 commits — the file must
            not wait for it. Suspense streams it in after the bytes are on screen. */}
        <Suspense fallback={null}>
          <FileRail token={token} owner={owner} repo={repo} meta={meta} base={base} all={all.value} oid={head.oid} />
        </Suspense>
      </aside>

async function FileRail({ token, owner, repo, meta, base, all, oid }: { ... }) {
  const rail = await repoRail(token, owner, repo, oid);
  return (
    <RepoAbout
      base={base}
      description={meta.description}
      branches={all.filter((r) => r.kind === "branch").length}
      tags={all.filter((r) => r.kind === "tag").length}
      isPrivate={!meta.public}
      languages={rail.languages}
      contributors={rail.contributors}
    />
  );
}
```

- [ ] **Step 2:** `bun run lint && bunx tsc`; `bun run dev` spot check: a file page paints the code, then the rail fills in.
- [ ] **Step 3:** Commit: `git add web/apps/web/src/components/repo/file-view.tsx && git commit -m "Stream the About rail in after the file renders"`

### Task 11: Cap the go-to-file list and prefetch the README (Web P1, both in code.tsx)

**Files:**
- Modify: `web/apps/web/src/components/repo/code.tsx:109` (the `paths` map) and `99-121` (the README block relative to the `Promise.all`)

- [ ] **Step 1: cap the shipped list.** Where `paths` is built:

```ts
  // ponytail: go-to-file ships at most 5000 paths to the client; server-side
  // search when a repo outgrows that. 10k-file repos were paying a 10k-entry
  // RSC payload on every page.
  const paths = rail.blobs.slice(0, 5000).map((b) => ({ path: b.path, kind: "file" as const }));
```

- [ ] **Step 2: speculative README.** Add a fourth member to the existing `Promise.all`, and use it when the directory's README turns out to be the common spelling:

```ts
  const [entries, rail, touched, readmeGuess] = await Promise.all([
    tree(token, owner, repo, head.oid, dir),
    repoRail(token, owner, repo, head.oid),
    lastChanges(token, owner, repo, head.oid, dir),
    // Speculative: most directories that have a README spell it README.md, and
    // fetching it in parallel removes a whole round trip from the repo home.
    // A miss is a cheap 404; any other spelling falls back to the exact fetch.
    blob(token, owner, repo, head.oid, `${dir ? `${dir}/` : ""}README.md`),
  ]);
  ...
  const readme = readmeEntry
    ? readmeEntry.name === "README.md" && readmeGuess.ok
      ? readmeGuess
      : await blob(token, owner, repo, head.oid, `${dir ? `${dir}/` : ""}${readmeEntry.name}`)
    : undefined;
```

(With Task 6 the speculative miss is also never re-paid: it is oid-keyed and cached. Keep the existing `readmeEntry` regex and the rest of the block unchanged.)

- [ ] **Step 3:** `bun run lint && bunx tsc`; dev spot check on a repo with a README.
- [ ] **Step 4:** Commit: `git add web/apps/web/src/components/repo/code.tsx && git commit -m "Prefetch the README and cap the go-to-file payload"`

### Task 12: Plain overflow scroll for diffs and code blocks (Web P1)

**Files:**
- Modify: `web/apps/web/src/components/repo/code-block.tsx` (whole file is 16 lines)
- Modify: `web/apps/web/src/components/repo/diff-files.tsx` — `FileHunks` (~line 87)

**Context:** Every code block and every diff file mounts a Radix `ScrollArea` (a client component with a ResizeObserver); a 100-file PR hydrates 100 of them to do what CSS does. House style already demands wide content scroll in its own `overflow-x-auto` container.

- [ ] **Step 1:** `code-block.tsx`:

```tsx
import { highlight, langFor } from "@/lib/highlight";
import type { BundledLanguage } from "shiki";

/** A highlighted source block. Async server component: shiki runs once per render
 *  on the server and the browser receives coloured spans, nothing to hydrate —
 *  including the scrollbar, which is plain CSS overflow rather than a mounted
 *  ScrollArea per block. */
export async function CodeBlock({ code, path, lang }: { code: string; path?: string; lang?: BundledLanguage | "text" }) {
  const html = await highlight(code, lang ?? (path ? langFor(path) : "text"));
  return (
    <div className="code-block w-full overflow-x-auto">
      <div dangerouslySetInnerHTML={{ __html: html }} />
    </div>
  );
}
```

- [ ] **Step 2:** In `diff-files.tsx` `FileHunks`, replace the `ScrollArea`/`ScrollBar` pair with `<div className="w-full overflow-x-auto">…</div>` (keep the `w-max min-w-full` table comment — it is why the table doesn't stretch the page). Remove the now-unused imports in both files.
- [ ] **Step 3:** `bun run lint && bunx tsc`; dev spot check: a long line in a file view and in a commit diff scrolls horizontally inside its box, the page body does not.
- [ ] **Step 4:** Commit: `git add web/apps/web/src/components/repo && git commit -m "Scroll code and diffs with CSS instead of a ScrollArea each"`

### Task 13: P2 batch — highlight runs, radix import optimization, idle marketing tick

**Files:**
- Modify: `web/apps/web/src/components/repo/file-search.tsx:30-39` (`Highlighted`)
- Modify: `web/apps/web/next.config.ts`
- Modify: `web/apps/web/src/components/marketing/environment-panel.tsx` (`setInterval` effect, ~line 159)

- [ ] **Step 1: runs, not per-char spans.** Replace `Highlighted`:

```tsx
/** Consecutive hits render as one span: a 60-char path was 60 elements. */
function Highlighted({ text, hits }: { text: string; hits: number[] }) {
  const set = new Set(hits);
  const runs: { s: string; hit: boolean }[] = [];
  for (let i = 0; i < text.length; i++) {
    const hit = set.has(i);
    const last = runs[runs.length - 1];
    if (last && last.hit === hit) last.s += text[i];
    else runs.push({ s: text[i], hit });
  }
  return (
    <>
      {runs.map((r, i) =>
        r.hit ? <span key={i} className="font-semibold text-foreground">{r.s}</span> : <span key={i}>{r.s}</span>,
      )}
    </>
  );
}
```

- [ ] **Step 2:** `next.config.ts` (the dep is the `radix-ui` monopackage, `package.json` line 23):

```ts
const nextConfig: NextConfig = {
  output: "standalone",
  experimental: {
    // The radix-ui monopackage re-exports everything; without this, one import
    // pulls the whole barrel into every chunk that touches a UI primitive.
    optimizePackageImports: ["radix-ui"],
  },
};
```

Verify the option's exact spelling against `node_modules/next/dist/docs/` before committing — the installed Next differs from training data.

- [ ] **Step 3: pause the story when nobody is watching.** In `environment-panel.tsx`, guard the tick:

```ts
  useEffect(() => {
    // A background tab was re-rendering this 6-7x a second forever; browsers
    // throttle the timer but not to zero. Hidden means paused.
    const id = setInterval(() => {
      if (!document.hidden) setT((v) => (v + 1) % T);
    }, 150);
    return () => clearInterval(id);
  }, []);
```

- [ ] **Step 4:** `bun run lint && bunx tsc && bun run build` (the config change only bites at build), plus `bun test` for the suite.
- [ ] **Step 5:** Commit: `git add web/apps/web && git commit -m "Batch the small web wins: highlight runs, radix imports, idle marketing tick"`
