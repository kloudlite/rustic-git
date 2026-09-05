# Git Server and Storage Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every git-server / storage / core finding of the 2026-09-02 review: the anonymous 2 GiB upload-pack negotiation buffer (H1 / summary High #4), marker-field forgery through a repo description (M1), unauthenticated claim amplification against the elected map writer (M2), `/metrics` disclosure on the peer listener (M3), and the six Lows including the three named test gaps.

**Architecture:** No structural change. Routing still runs before authentication and still refuses anything it cannot route; the lease/fence contract (one elected map writer, `writing_epoch` under `leader_lock`, SlateDB's writer fence as backstop) is untouched. Two of the fixes act on the review's structural note — "everything expensive happens before authentication" — by making the pre-auth work smaller, never by moving work after authentication: the negotiation body gets a route-sized cap instead of `max_body`, and `App::route` gains an existence gate that turns an invented name into a routed 404 instead of a leader write. `Route` gains one variant (`Missing`) so "nothing to route, nothing to claim" is a distinct answer from "nobody may serve this" — a repo that does not exist keeps answering 404, not 503.

**Tech Stack:** Rust 2021 workspace, axum 0.8, SlateDB + `object_store`, `metrics`/`metrics-exporter-prometheus`, tokio; integration tests in the root `kloudlite-tests` package under `tests/`, unit tests in-crate.

**Spec:** docs/superpowers/reviews/2026-09-02-codebase-review.md (details: docs/superpowers/reviews/2026-09-02-details/core-storage-server.md)

## Global Constraints
- Upload-pack negotiation cap: `8 * 1024 * 1024` bytes (8 MiB), a `const` in `bins/server/src/router/git.rs`, no env override — a `want`/`have` pkt-line is ~50 bytes, so 8 MiB is over 150 000 lines, far past any real negotiation, and one fewer knob to misconfigure than `KLOUDLITE_MAX_BODY` was.
- `max_body()` (2 GiB default, 512 MiB in `deploy/kloudlite.yaml:145`) stays exactly as it is on the receive-pack streamed path (`live_body`) — it is already correct there and its test at `tests/git_http_limits.rs:81` must keep passing unchanged.
- M2 design: **existence-gated claim**, not a token bucket (justification in Task 3). Exempt set is exactly: any request whose path is `/api/{owner}/{name}/create`, and any `/v2/` image-route request whose method is not `GET` and not `HEAD`. Everything else claims only when `pool.exists` says the prefix has an object; an `exists` error falls back to claiming, matching `force_claim` at `crates/app/src/lib.rs:519-523`.
- M3 option chosen: **its own listener/port** — `KLOUDLITE_METRICS_ADDR=0.0.0.0:9464` via the already-written `kloudlite_core::metrics::serve_if_configured()`, and `/metrics` removed from the peer router. Reason in Task 4.
- L3 single copy lives in `crates/core/src/sshkeys.rs` behind an optional `ssh` feature (optional `russh` dependency). `crates/storage` depends on `kloudlite-core` with no features, so its Cargo.toml keeps no ssh dependency.
- Control characters rejected in a description: any `char` where `c.is_control()` is true (covers `\n`, `\r`, `\t`, NUL and the C1 range) — one predicate, no allow-list to drift.
- Comments explain WHY only; deliberate ceilings keep or gain a `// ponytail:` marker naming the ceiling and upgrade path.
- Every commit message: imperative sentence case subject, no attribution trailers of any kind.

---

### Task 1: Cap the upload-pack negotiation body at 8 MiB

**Files:**
- Modify `bins/server/src/router/git.rs:120-131` (the `read_body` doc comment and body) and its call site at `git.rs:415`
- Test `tests/git_http_limits.rs` (append after the existing push-cap test that ends at line 100)

**Interfaces:**
- Consumes: `axum::body::to_bytes`, `kloudlite_core::httpx::max_body` (unchanged, still used by `live_body`)
- Produces: `const MAX_NEGOTIATION: usize`; `async fn read_body(body: Body) -> Result<Bytes, Response>` (signature unchanged)

- [ ] **Step 1: Write the failing test** — append to `tests/git_http_limits.rs`:

```rust
/// The negotiation is kilobytes; the cap that used to apply here was `max_body` (2 GiB), buffered
/// in memory by `to_bytes` on a route an anonymous client reaches on any public repo. A handful of
/// those OOMs the pod, and an OOM moves repo ownership on the attacker's schedule.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_upload_pack_negotiation_is_refused_anonymously() {
    let _serial = SERIAL.lock().await;
    let (base, e) = common::serve_public().await;
    e.store.create_repo("alice", "web").await.unwrap();
    // Public, so this reaches `read_body` with no credentials at all — the amplifier the cap
    // exists for.
    e.store.set_public("alice", "web", true).await.unwrap();
    let r = reqwest::Client::new()
        .post(format!("{base}/alice/web.git/git-upload-pack"))
        .header("content-type", "application/x-git-upload-pack-request")
        .header("git-protocol", "version=2")
        .body(vec![b'0'; 9 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    // And a real-sized negotiation still gets through to the protocol, so the cap is not simply
    // refusing everything: a v2 command with no flush is a client error, never a 413.
    let mut body = Vec::new();
    kloudlite_core::pktline::write_text(&mut body, "command=ls-refs").unwrap();
    let r = reqwest::Client::new()
        .post(format!("{base}/alice/web.git/git-upload-pack"))
        .header("content-type", "application/x-git-upload-pack-request")
        .header("git-protocol", "version=2")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_ne!(r.status(), 413, "a kilobyte negotiation must not hit the cap");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test git_http_limits an_oversized_upload_pack_negotiation_is_refused_anonymously`. Expected failure: `assertion `left == right` failed\n  left: 200\n right: 413` (the 9 MiB body is buffered and the protocol answers normally), or a 400 from the protocol — anything but 413.

- [ ] **Step 3: Implement** — in `bins/server/src/router/git.rs`, replace the `read_body` doc comment and body at lines 120-131 with:

```rust
/// Cap on an upload-pack negotiation body. A `want`/`have` pkt-line is about 50 bytes, so this is
/// over 150 000 of them — past any real negotiation, and small enough that the concurrent-request
/// count that OOMs the pod is unreachable. NOT `max_body`: this body is BUFFERED, and an OOM here
/// moves repo ownership on an attacker's schedule.
const MAX_NEGOTIATION: usize = 8 * 1024 * 1024;

/// Read the whole body only AFTER `open()` has authenticated the caller. `Bytes` as an extractor
/// runs before the handler, so an anonymous client could make the pod buffer the whole cap and,
/// with a few of those in flight, OOM it. The `DefaultBodyLimit` layer only governs extractors, so
/// the cap is applied here by hand. Upload-pack only: its request is the negotiation, kilobytes —
/// so the cap is `MAX_NEGOTIATION`, not the 2 GiB `max_body` that governs receive-pack's STREAMED
/// body (`live_body`), which never sits in memory.
async fn read_body(body: Body) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, MAX_NEGOTIATION)
        .await
        .map_err(|_| (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response())
}
```

`use kloudlite_core::httpx::max_body;` at `git.rs:2` stays — `live_body` at `git.rs:139` still uses it.

- [ ] **Step 4: Run tests and clippy** — `cargo test --test git_http_limits && cargo test -p kloudlite-server && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add bins/server/src/router/git.rs tests/git_http_limits.rs && git commit -m "Cap the upload-pack negotiation body at 8 MiB"`

---

### Task 2: Reject control characters in a repo description, and refuse them in marker bodies

**Files:**
- Modify `crates/api/src/repos.rs:60-68` (`check_description`) and its unit test block at `repos.rs:548-554`
- Modify `crates/storage/src/index.rs:59-66` (`body`) and its test module (starts at the `#[cfg(test)]` line, helpers `mem_store`/`marker`)
- Callers unchanged: `repos.rs:136` (create) and `repos.rs:347` (description edit) both already route through `check_description`

**Interfaces:**
- Consumes: `char::is_control`
- Produces: `check_description(d: &str) -> std::result::Result<(), Response>` (signature unchanged, one more refusal); `fn body(m: &Marker) -> Vec<u8>` (signature unchanged, strips control characters)

- [ ] **Step 1: Write the failing test** — in `crates/api/src/repos.rs`, inside the existing `#[cfg(test)] mod tests`, beside the length assertions at lines 550-553:

```rust
    /// The marker body is `k=v` lines parsed per line, so a newline in a description writes real
    /// fields — `created_by=someone.else` renders as the forged creator in every listing.
    #[test]
    fn a_description_with_control_characters_is_refused() {
        assert!(check_description("hi\ncreated_by=someone.else").is_err());
        assert!(check_description("hi\rthere").is_err());
        assert!(check_description("hi\tthere").is_err());
        assert!(check_description("hi \u{0000} there").is_err());
        assert!(check_description("perfectly ordinary — with an em dash").is_ok());
    }
```

and in `crates/storage/src/index.rs`, inside `#[cfg(test)] pub(crate) mod tests`:

```rust
    /// Belt and braces behind `check_description`: whatever reaches `body`, one marker is one
    /// line per field, so no writer can ever inject a second field.
    #[tokio::test]
    async fn a_newline_in_a_description_cannot_forge_a_marker_field() {
        let s = mem_store().await;
        let mut m = marker("web", false);
        m.description = "hi\ncreated_by=someone.else\ncreated_ms=0".to_string();
        write(&s, Kind::Repo, "alice", &m).await.unwrap();
        let l = list(&s, Kind::Repo, "alice", true).await.unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].created_by, "alice@example.com", "the real creator survives");
        assert_eq!(l[0].created_ms, 1755772800000);
        assert!(!l[0].description.contains('\n'));
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-api a_description_with_control_characters_is_refused` fails with `assertion failed: check_description("hi\ncreated_by=someone.else").is_err()`; `cargo test -p kloudlite-storage a_newline_in_a_description_cannot_forge_a_marker_field` fails with `assertion `left == right` failed\n  left: "someone.else"\n right: "alice@example.com"`.

- [ ] **Step 3: Implement** — in `crates/api/src/repos.rs`, replace `check_description` (lines 60-68) with:

```rust
pub(crate) fn check_description(d: &str) -> std::result::Result<(), Response> {
    if d.chars().count() > MAX_DESCRIPTION {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("description must be {MAX_DESCRIPTION} characters or fewer"),
        )
            .into_response());
    }
    // The listing marker is `k=v` lines parsed per line and the description is written last, so a
    // newline in it writes real fields — a forged `created_by=` renders as the creator in every
    // listing. Refused here because both `create` and the description edit route through this one
    // function; every other control character goes with it, since none of them belongs on a line
    // under a repo name.
    if d.chars().any(char::is_control) {
        return Err((
            StatusCode::BAD_REQUEST,
            "description may not contain control characters",
        )
            .into_response());
    }
    Ok(())
}
```

and in `crates/storage/src/index.rs`, replace `body` (lines 59-66) with:

```rust
/// `k=v` lines, `description` last so it may itself contain `=`. Control characters are dropped
/// from the description on the way in: `decode` parses per line, so a newline that reached here
/// would write fields of its own, and `check_description` refusing them at the API is one caller,
/// not a guarantee about every future writer of a marker.
fn body(m: &Marker) -> Vec<u8> {
    let description: String = m.description.chars().filter(|c| !c.is_control()).collect();
    format!(
        "v=1\npublic={}\ncreated_by={}\ncreated_ms={}\nmanifests={}\nupdated_ms={}\ndescription={}",
        m.public, m.created_by, m.created_ms, m.manifests, m.updated_ms, description
    )
    .into_bytes()
}
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-api -p kloudlite-storage && cargo test --test browse && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/api/src/repos.rs crates/storage/src/index.rs && git commit -m "Refuse control characters in a repo description"`

---

### Task 3: Claim a repo key only when it exists, or the route can create it

**Design decision (M2).** Existence-gated claim, not a token bucket. A bucket only rate-limits the amplifier — a spray still costs the elected writer its whole budget every TTL, and a legitimate first request for a real cold repo can be starved by invented names sharing the bucket; the existence gate costs invented names *zero* leader writes and adds no state to keep, drain or tune. The `ponytail:` note at `crates/app/src/lib.rs:411-422` names the bucket as the upgrade because the note's own paragraph rejects existence gating for a real reason — "the first write to a new repo, image or volume opened it here unleased" — and that reason is fully answered by exempting the routes that can create a database, which the bucket never had to identify; with the exempt set explicit, the gate is strictly stronger than the bucket and smaller.

**Exact behaviour for the create routes.** `may_create(method, path)` is true for exactly two shapes and nothing else: `/api/{owner}/{name}/create` (the git-repo create, any method — it is the only browse tail that creates a database), and any `/v2/` path that `registry::image_route` resolves whose method is not `GET` and not `HEAD` (upload start `POST .../blobs/uploads/`, `PATCH`, `PUT` manifest or blob, `DELETE`: the paths that can bring an image database into being). Those claim unconditionally, exactly as today, so the create/first-write path keeps its lease and the two-writer window the note describes never reopens. Everything else — every git route, every browse read, every volume route, every registry `GET`/`HEAD` — routes to the holder if one is named, and answers `Route::Missing` (404) when the map names nobody and the object-store prefix is empty. The one behaviour change a client can see: a `HEAD /v2/{o}/{n}/blobs/{digest}` against an image that does not exist yet now answers 404 `NAME_UNKNOWN` instead of 404 `BLOB_UNKNOWN`; both are 404 and `docker push` proceeds to `POST .../blobs/uploads/`, which is exempt.

**Files:**
- Modify `crates/storage/src/ownership/mod.rs:49-57` (`enum Route`)
- Modify `crates/app/src/lib.rs:381-461` (`App::route`) and `crates/app/src/lib.rs:819`
- Modify `bins/server/src/router/route.rs:398-425` (the `app.route` call and its match arms) and add `may_create` beside `repo_of`
- Modify `crates/git/src/ssh.rs:180-190` and `crates/git/src/proxy.rs:91-105` (one new arm each)
- Test `tests/routing.rs` (append after `a_claim_on_an_unowned_repo_is_granted_and_only_the_claimant_warms`, which ends at line 721)

**Interfaces:**
- Consumes: `kloudlite_storage::pool::Pool::exists(&self, owner: &str, name: &str) -> Result<bool>` (`crates/storage/src/pool/mod.rs:296`)
- Produces: `Route::Missing`; `App::route_for(&self, repo: &str, may_create: bool) -> Route`; `App::route(&self, repo: &str) -> Route` (unchanged signature, delegates with `false`); `route::may_create(method: &axum::http::Method, path: &str) -> bool`

- [ ] **Step 1: Write the failing test** — append to `tests/routing.rs`:

```rust
/// An invented repo name costs the elected writer nothing: `route` gates the claim on the prefix
/// existing, so a spray of distinct bad names writes no map entries at all. The create route is
/// the exemption, and it still claims — that is what keeps the first write to a new repo leased.
#[tokio::test(flavor = "multi_thread")]
async fn an_invented_repo_name_is_404_and_claims_nothing() {
    let e = common::env().await;
    let token = e.store.create_token("alice").await.unwrap();
    let f = fleet(2);
    let a = node(e.store.os.clone(), LEADER, &f).await;
    let b = node(e.store.os.clone(), "kloudlite-1", &f).await;
    for i in 0..5 {
        let res = client().await
            .get(format!("http://{}/alice/nope{i}/info/refs?service=git-upload-pack", b.public))
            .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
            .send().await.unwrap();
        assert_eq!(res.status(), 404, "an invented name is not found, not 503");
        assert_eq!(a.app.owner(&format!("alice/nope{i}")).await.unwrap(), None, "nothing claimed");
    }
    assert_eq!(b.store.pool.warm_count(), 0, "and nothing was opened");
    // A real repo still routes and claims exactly as before.
    e.store.create_repo("alice", "web").await.unwrap();
    let res = client().await
        .get(format!("http://{}/alice/web/info/refs?service=git-upload-pack", b.public))
        .basic_auth("x", Some(&token)).header("git-protocol", "version=2")
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(a.app.owner("alice/web").await.unwrap().unwrap().node, "kloudlite-1");
}

/// `may_create` is the whole exempt set: the create route and registry writes claim an
/// empty-prefix name, everything else does not.
#[test]
fn only_the_create_routes_may_claim_a_name_that_does_not_exist() {
    use axum::http::Method;
    use kloudlite_server::router_test::may_create;
    assert!(may_create(&Method::POST, "/api/alice/web/create"));
    assert!(may_create(&Method::POST, "/v2/alice/web/blobs/uploads/"));
    assert!(may_create(&Method::PUT, "/v2/alice/web/manifests/v1"));
    assert!(!may_create(&Method::GET, "/v2/alice/web/manifests/v1"));
    assert!(!may_create(&Method::HEAD, "/v2/alice/web/blobs/sha256:abc"));
    assert!(!may_create(&Method::POST, "/alice/web/git-receive-pack"));
    assert!(!may_create(&Method::GET, "/api/alice/web/refs"));
    assert!(!may_create(&Method::DELETE, "/api/alice/web/volumedelete"));
}
```

Add the test-only re-export this needs at the end of `bins/server/src/lib.rs`:

```rust
/// `may_create` decides, before authentication, which routes may claim a name that does not exist
/// yet. Exported for the routing integration test — the exempt set is the security property, so it
/// is asserted from outside rather than only in the module that writes it.
pub mod router_test {
    pub use crate::router::route::may_create;
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test routing an_invented_repo_name_is_404_and_claims_nothing only_the_create_routes_may_claim_a_name_that_does_not_exist`. Expected: the second test fails to compile with ``error[E0432]: unresolved import `kloudlite_server::router_test` ``; after the module exists, the first fails with `assertion `left == right` failed\n  left: 404\n right: 404` passing but `assert_eq!(a.app.owner(...), None)` failing — `Some(Entry { node: "kloudlite-1", .. })`, the claim the fix removes.

- [ ] **Step 3: Implement** —

In `crates/storage/src/ownership/mod.rs`, extend the enum (lines 49-57):

```rust
pub enum Route {
    /// This node owns it and serves it.
    Local,
    /// Another node owns it. Forward there.
    Peer(Peer),
    /// Nobody may safely serve it right now — 503, and let the client retry. The leader being
    /// unreachable lands here, deliberately: an unclaimable repo is not served by whoever asked.
    Unavailable,
    /// The map names nobody and there is nothing in the object store under this key — so there is
    /// nothing to route and nothing worth claiming. 404. Distinct from `Unavailable` because a
    /// name that does not exist must not tell a client to retry.
    Missing,
}
```

In `crates/app/src/lib.rs`, rename the existing `pub async fn route(&self, repo: &str) -> Route` (line 381) to `route_for` with the extra argument, add the delegating `route`, and replace the claim block at lines 411-424:

```rust
    /// Where this request belongs. `may_create` says whether the route being served can bring this
    /// database into being (see `router::route::may_create`); only such a route may claim a key
    /// with nothing under it.
    pub async fn route_for(&self, repo: &str, may_create: bool) -> Route {
```

```rust
                // A repo the map does not name is CLAIMED before anyone opens it. Routing on
                // "does the prefix exist" was a two-writer window: the first write to a new repo,
                // image or volume opened it here unleased, and until its manifest landed every
                // other node saw the same empty prefix and opened it too. That is why the routes
                // which can CREATE claim unconditionally — the window is theirs and they keep the
                // lease. Every other route gates on the prefix, because `route()` runs before
                // authentication: without the gate a spray of invented names is one leader map
                // write per name per LEASE_TTL, from an anonymous client, against the one node the
                // whole fleet's routing depends on. An `exists` that errs falls back to claiming,
                // exactly as `force_claim` does — an unreadable store must not turn into a 404.
                if !may_create {
                    if let Some((o, n)) = repo.split_once('/') {
                        if !self.store.pool.exists(o, n).await.unwrap_or(true) {
                            return Route::Missing;
                        }
                    }
                }
                match self.claim(repo).await {
```

and add, immediately after `route_for`'s closing brace:

```rust
    /// `route_for` for the paths that can never create a database — every git route, and the peer
    /// stream. The default is the safe one on purpose: a new caller that forgets to think about it
    /// gets the gated behaviour, not the amplifier.
    pub async fn route(&self, repo: &str) -> Route {
        self.route_for(repo, false).await
    }
```

At `crates/app/src/lib.rs:819` (`on_fenced`), use the ungated form — the database demonstrably exists, it just fenced us:

```rust
        if !matches!(self.route_for(&format!("{owner}/{name}"), true).await, Route::Local) {
```

In `bins/server/src/router/route.rs`, add beside `repo_of` (after line 263):

```rust
/// Which routes may claim a repo key whose object-store prefix is still empty — the exemption
/// `App::route_for` gates the claim on. Exactly the paths that can CREATE a database: the git-repo
/// create, and a registry write, which brings an image's database into being on its first upload.
/// A `GET`/`HEAD` never creates one, so it never claims a name that does not exist.
pub(crate) fn may_create(method: &axum::http::Method, path: &str) -> bool {
    if let Some((_, name, tail)) = api_route(path.trim_start_matches('/')) {
        if !name.is_empty() && tail == "create" {
            return true;
        }
    }
    crate::registry::image_route(path.trim_start_matches('/')).is_some()
        && !matches!(*method, axum::http::Method::GET | axum::http::Method::HEAD)
}
```

Make the module and function reachable from `bins/server/src/lib.rs`'s `router_test` re-export: `bins/server/src/router/mod.rs:3` already declares `pub(crate) mod route;` — change it to `pub mod route;` and `may_create` to `pub fn may_create`.

Then at `route.rs:398`, pass the flag and handle the new variant (the `let route = ...` line and the match at 404-425):

```rust
    let route = app.route_for(&repo, may_create(req.method(), &path)).await;
```

and add a `Missing` arm to BOTH matches in `route_inner` (the out-of-hops one at line 404 and the main one at 413), before the `Unavailable` arm:

```rust
        // Nothing under this key anywhere: routing has nothing to send it to and nothing worth
        // claiming. Answered in the shape the caller speaks, so a registry client still gets an
        // OCI envelope.
        crate::ownership::Route::Missing => {
            if crate::registry::is_v2_path(&path) {
                crate::registry::oci_err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such image")
            } else {
                (StatusCode::NOT_FOUND, "not found").into_response()
            }
        }
```

In `crates/git/src/ssh.rs`, after the `Unavailable` arm at line 182:

```rust
        crate::ownership::Route::Missing => return Err(crate::err("no such repository")),
```

In `crates/git/src/proxy.rs`, after the `Unavailable` arm at line 98:

```rust
            crate::ownership::Route::Missing => return refuse(reader, "no such repository").await,
```

- [ ] **Step 4: Run tests and clippy** — `cargo test --test routing && cargo test --test http_e2e --test registry_http --test registry_uploads --test browse_http --test ssh_e2e && cargo test --workspace && cargo clippy --workspace --all-targets --locked -- -D warnings`. Then `./tests/registry_e2e.sh` — a real `docker push` to a brand-new image is the flow the exempt set has to cover (exit 77 means the docker half was skipped, which is not a pass; run it where a daemon exists before merging).

- [ ] **Step 5: Commit** — `git add crates/storage/src/ownership/mod.rs crates/app/src/lib.rs crates/git/src/ssh.rs crates/git/src/proxy.rs bins/server/src/lib.rs bins/server/src/router/mod.rs bins/server/src/router/route.rs tests/routing.rs && git commit -m "Claim a repo key only when it exists or the route creates it"`

---

### Task 4: Serve `/metrics` on its own listener instead of the peer port

**Option chosen (M3): its own listener/port.** The other four options in the tree already do exactly this — `kloudlite_core::metrics::serve_if_configured()` and `KLOUDLITE_METRICS_ADDR=0.0.0.0:9464` are written, deployed and scraped for the api, worker, agent and gateway (`deploy/kloudlite.yaml:410`, `:570`, `deploy/k3s/agent-daemonset.yaml:130`). Reusing it costs one call in `main` and deletes the exemption instead of adding a credential to the scrape path, and unlike "accept the secret on `/metrics`" it needs no Prometheus-side secret to keep working; unlike "document it", it actually closes the hole, because a port of its own is something a NetworkPolicy can name.

**Files:**
- Modify `bins/server/src/router/mod.rs:39-40` (drop the `/metrics` merge from `peer_router`)
- Modify `bins/server/src/router/route.rs:554-562` (drop the `trust_peer` early return)
- Modify `bins/server/src/main.rs` (call `serve_if_configured` beside the existing `metrics::init()` at line 238)
- Modify `deploy/kloudlite.yaml:31-33` (scrape port), `:145-150` (env + containerPort), `:308-341` (NetworkPolicy ingress)
- Modify `deploy/alerts.md:3-4`
- Test `tests/metrics.rs` (rewrite of the single test there)

**Interfaces:**
- Consumes: `kloudlite_core::metrics::serve_if_configured()`, `kloudlite_core::metrics::routes()`
- Produces: no new symbols; `peer_router` no longer serves `/metrics`

- [ ] **Step 1: Write the failing test** — replace the body of `tests/metrics.rs` with:

```rust
//! Metrics count, and are NOT reachable on the peer listener: the peer port runs with
//! `networkPolicy: none`, so anything served there without the secret is readable by any pod, and
//! the scrape text lists every repository key this node has touched.
mod common;

#[tokio::test]
async fn the_peer_listener_no_longer_serves_metrics() {
    kloudlite_core::metrics::init();
    let (base, _e) = common::serve_peer().await;
    let res = reqwest::get(format!("{base}/metrics")).await.unwrap();
    assert_eq!(res.status(), 404, "metrics moved to their own listener");
}

#[tokio::test]
async fn the_metrics_listener_serves_prometheus_text_and_counts() {
    kloudlite_core::metrics::init();
    let (base, _e) = common::serve_peer().await;
    // One request through the middleware so the series exists before the scrape.
    assert_eq!(common::peer_get(&base, "/healthz").await.status(), 200);

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, kloudlite_core::metrics::routes::<()>().with_state(())).await.unwrap();
    });
    let res = reqwest::get(format!("http://{addr}/metrics")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(
        body.contains(r#"http_requests_total{listener="peer",class="probe",status="2xx"}"#),
        "no request series in:\n{body}"
    );
    assert!(body.contains("http_request_duration_seconds_bucket{"), "durations are histograms:\n{body}");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test metrics`. Expected: `the_peer_listener_no_longer_serves_metrics` fails with `assertion `left == right` failed\n  left: 200\n right: 404`.

- [ ] **Step 3: Implement** —

`bins/server/src/router/mod.rs`: delete lines 39-40 (the comment and `.merge(kloudlite_core::metrics::routes())`) from `peer_router`, and amend the `peer_router` doc comment's last sentence to read:

```rust
/// `/metrics` is NOT here: it is a listener of its own (`KLOUDLITE_METRICS_ADDR`), because a
/// scrape route inside the secret check cannot be scraped, and one outside it is an enumeration
/// oracle for any pod on a cluster running `networkPolicy: none`.
```

`bins/server/src/router/route.rs`: delete the early return at lines 554-562 (the comment plus `if req.uri().path() == "/metrics" { return next.run(req).await; }`), so `trust_peer` checks the secret on everything.

`bins/server/src/main.rs`: after `kloudlite_core::metrics::init();` at line 238, add:

```rust
    // Its own listener, like every other binary's: the peer port is secret-gated, and metrics
    // text names every repository key this node has touched.
    kloudlite_core::metrics::serve_if_configured().await;
```

`deploy/kloudlite.yaml`: at lines 31-33 change `prometheus.io/port: "8081"` to `"9464"`; in the server container's env block beside `KLOUDLITE_MAX_BODY` (line 145) add:

```yaml
            # Metrics on a port of their own: 8081 requires the peer secret, which Prometheus
            # cannot present, and the scrape text enumerates every repo this node has touched.
            - name: KLOUDLITE_METRICS_ADDR
              value: 0.0.0.0:9464
```

after line 150 add `            - { name: metrics, containerPort: 9464 }`; and in the `kloudlite-peers-only` NetworkPolicy add a final ingress rule (after the `8080`/`2222` block ending at line 341):

```yaml
    # Prometheus presents no secret, so metrics have their own port — open it to the whole
    # cluster network exactly as 8080 is, and nothing else.
    - ports:
        - { protocol: TCP, port: 9464 }
```

`deploy/alerts.md:3-4`: replace "the server tier serves `/metrics` on the peer port (8081)" with "every binary, the server tier included, serves `/metrics` on `KLOUDLITE_METRICS_ADDR` (9464)".

- [ ] **Step 4: Run tests and clippy** — `cargo test --test metrics --test routing && cargo test --workspace && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add bins/server/src/router/mod.rs bins/server/src/router/route.rs bins/server/src/main.rs deploy/kloudlite.yaml deploy/alerts.md tests/metrics.rs && git commit -m "Move server metrics off the peer listener onto their own port"`

---

### Task 5: Refuse `mem://` as a fleet object store unless it is opted into

**Files:**
- Modify `crates/storage/src/config.rs:107-129` (`fleet_store_ok`) and its unit test at `config.rs:158-166`
- Caller unchanged: `bins/server/src/main.rs:58`

**Interfaces:**
- Consumes: `std::env::var`
- Produces: `fleet_store_ok(url: &str) -> Result<()>` (signature unchanged); env `KLOUDLITE_ALLOW_MEM_FLEET`

- [ ] **Step 1: Write the failing test** — in `crates/storage/src/config.rs`'s test module, beside the existing `fleet_store_ok` assertions:

```rust
    /// `InMemory` is per-process: every pod would take epoch 1 of its own lease, every pod would
    /// be leader, and every pod would open every database — the exact two-writer bug the guard
    /// exists to prevent. Allowed only when something says out loud that it is a test fleet.
    #[test]
    fn mem_is_not_a_fleet_store_unless_opted_into() {
        std::env::remove_var("KLOUDLITE_ALLOW_MEM_FLEET");
        assert!(super::fleet_store_ok("mem://").is_err());
        std::env::set_var("KLOUDLITE_ALLOW_MEM_FLEET", "1");
        assert!(super::fleet_store_ok("mem://").is_ok());
        std::env::remove_var("KLOUDLITE_ALLOW_MEM_FLEET");
    }
```

If `mem://` appears in the `for ok in [...]` loop at `config.rs:163-165`, remove it from that list.

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-storage mem_is_not_a_fleet_store_unless_opted_into` fails with `assertion failed: super::fleet_store_ok("mem://").is_err()`.

- [ ] **Step 3: Implement** — in `crates/storage/src/config.rs`, insert into `fleet_store_ok` after the `file://` branch:

```rust
    // `InMemory` is per-process. In a real multi-pod fleet every pod takes epoch 1 of its own
    // lease, every pod is leader and every pod opens every database — the same two-writer bug the
    // `file://` refusal above exists for, with no URL scheme to give it away. The in-process test
    // fleet is the legitimate case, and it says so.
    if url == "mem://" && std::env::var("KLOUDLITE_ALLOW_MEM_FLEET").is_err() {
        return Err(crate::err(
            "KLOUDLITE_S3_URL=mem:// cannot host a fleet: InMemory is per-process, so every pod \
             would be its own leader and open every database; set KLOUDLITE_ALLOW_MEM_FLEET=1 \
             only for an in-process test fleet",
        ));
    }
```

and drop the "use mem:// for a local fleet" clause from the `file://` error message so the two do not contradict each other, replacing it with "use s3:// / az://".

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-storage && cargo test --test routing && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/storage/src/config.rs && git commit -m "Refuse mem:// as a fleet object store unless opted into"`

---

### Task 6: Delete the `forget_pack_public` alias

**Files:**
- Modify `crates/storage/src/store.rs:620-622` (delete the wrapper) and `store.rs:629` (`async fn forget_pack` → `pub async fn`)
- Modify `crates/git/src/gc.rs:135` (the sole caller)

**Interfaces:**
- Consumes: nothing new
- Produces: `Store::forget_pack(&self, owner: &str, name: &str, fname: &str) -> Result<()>` becomes `pub`; `forget_pack_public` is gone

- [ ] **Step 1: Write the failing test** — the compiler is the test here; no behaviour changes. Assert the alias is gone by making the caller name the real method: edit `crates/git/src/gc.rs:135` first, from `forget_pack_public(` to `forget_pack(`, and let the build fail.

- [ ] **Step 2: Run it, expect failure** — `cargo build -p kloudlite` fails with ``error[E0624]: method `forget_pack` is private``.

- [ ] **Step 3: Implement** — in `crates/storage/src/store.rs`, delete lines 620-622:

```rust
    pub async fn forget_pack_public(&self, owner: &str, name: &str, fname: &str) -> Result<()> {
        self.forget_pack(owner, name, fname).await
    }
```

and change the declaration at line 629 from `async fn forget_pack(` to `pub async fn forget_pack(`.

- [ ] **Step 4: Run tests and clippy** — `cargo test --workspace && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add crates/storage/src/store.rs crates/git/src/gc.rs && git commit -m "Drop the forget_pack_public alias"`

---

### Task 7: Keep one copy of `ssh_fingerprint`, in core behind an `ssh` feature

**Where the single copy lives:** `crates/core/src/sshkeys.rs`, gated by a new optional `ssh` feature that pulls in `russh`. `crates/storage` already depends on `kloudlite-core` with no features (`crates/storage/Cargo.toml:12`), so its own manifest keeps no ssh dependency — the property CLAUDE.md asks for. `crates/git` would also do, but `crates/api` does not depend on it (`crates/api/Cargo.toml:12-15`), and both consumers already depend on core.

**Files:**
- Create `crates/core/src/sshkeys.rs`
- Modify `crates/core/Cargo.toml` (optional `russh`, `[features] ssh`), `crates/core/src/lib.rs:1-9` (module declaration)
- Modify `bins/server/Cargo.toml:18` and `bins/server/src/boot.rs:31-42` (delete the copy, re-export)
- Modify `crates/api/Cargo.toml:12` and `crates/api/src/credentials.rs:243-251` (delete the copy) plus its callers at `credentials.rs:256` and `:878`

**Interfaces:**
- Consumes: `russh::keys::PublicKey::from_openssh`, `russh::keys::HashAlg::Sha256`
- Produces: `kloudlite_core::sshkeys::ssh_fingerprint(line: &str) -> kloudlite_core::Result<String>`

- [ ] **Step 1: Write the failing test** — create `crates/core/src/sshkeys.rs` with its test module and nothing else yet:

```rust
#[cfg(test)]
mod tests {
    /// The one copy: the fingerprint an ssh key is indexed by, and the refusal a non-key gets.
    /// Two hand-mirrored copies of a security-relevant parse is what this module removed.
    #[test]
    fn a_public_key_line_fingerprints_and_anything_else_is_refused() {
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGb9ECWmEzf6FQbrBZ9w7lshQhqowDY5hZYd/Q9K+2sw \
                    alice@example.com";
        let f = super::ssh_fingerprint(line).unwrap();
        assert!(f.starts_with("SHA256:"), "{f}");
        assert_eq!(super::ssh_fingerprint(&format!("  {line}  ")).unwrap(), f, "trimmed");
        assert!(super::ssh_fingerprint("not a key").is_err());
        assert!(super::ssh_fingerprint("").is_err());
    }
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-core --features ssh a_public_key_line_fingerprints_and_anything_else_is_refused` fails to compile: ``error[E0425]: cannot find function `ssh_fingerprint` in module `super` ``.

- [ ] **Step 3: Implement** —

`crates/core/Cargo.toml`: add under `[dependencies]` and a new `[features]` section:

```toml
# Only for `sshkeys`: two crates (the server binary and the api) both need the fingerprint of an
# OpenSSH public key, and a hand-mirrored copy of a security-relevant parse drifts. Optional so
# `storage`, which depends on core and must stay free of ssh parsing, does not pull it in.
russh = { workspace = true, optional = true }

[features]
ssh = ["dep:russh"]
```

`crates/core/src/lib.rs`: add after `pub mod peer;`:

```rust
#[cfg(feature = "ssh")]
pub mod sshkeys;
```

Prepend to `crates/core/src/sshkeys.rs` (above the test module):

```rust
//! OpenSSH public-key helpers. Here rather than in either consumer because the server binary and
//! the api tier both need the same parse, and `storage` — which must stay free of the ssh
//! dependency — takes core with no features.

/// The fingerprint of an OpenSSH public key line, or an error naming what is wrong with it. Used
/// to validate and identify a key before it is stored.
pub fn ssh_fingerprint(line: &str) -> crate::Result<String> {
    let key = russh::keys::PublicKey::from_openssh(line.trim())
        .map_err(|_| crate::err("that does not look like an OpenSSH public key"))?;
    Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
}
```

`bins/server/Cargo.toml:18`: `kloudlite-core = { path = "../../crates/core", features = ["ssh"] }`.
`crates/api/Cargo.toml:12`: `kloudlite-core = { path = "../core", features = ["ssh"] }`.

`bins/server/src/boot.rs`: delete lines 31-42 (the doc comment and `pub(crate) fn ssh_fingerprint`) and put in their place:

```rust
/// One copy, in core: `crates/api` needs the same parse and `storage` must not carry the ssh
/// dependency, so neither of those two is a home for it.
pub(crate) use kloudlite_core::sshkeys::ssh_fingerprint;
```

`crates/api/src/credentials.rs`: delete lines 243-251 (the doc comment and `fn ssh_fingerprint`) and put in their place:

```rust
// One copy, in core — this crate and the server binary both index keys by it.
use kloudlite_core::sshkeys::ssh_fingerprint;
```

Callers at `credentials.rs:256` and `:878` need no change; if a call site's error type complains, map with `.map_err(|e| crate::err(e.to_string()))` at that site only.

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-core --features ssh && cargo test --workspace && cargo clippy --workspace --all-targets --locked -- -D warnings`, plus `cargo tree -p kloudlite-storage -i russh` to confirm storage still pulls no ssh parse of its own (it may appear transitively through a sibling; the manifest is what the rule is about).

- [ ] **Step 5: Commit** — `git add crates/core/Cargo.toml crates/core/src/lib.rs crates/core/src/sshkeys.rs bins/server/Cargo.toml bins/server/src/boot.rs crates/api/Cargo.toml crates/api/src/credentials.rs Cargo.lock && git commit -m "Keep one copy of ssh_fingerprint in core"`

---

### Task 8: Pin which `io::ErrorKind::Other` messages may reach a client

**Files:**
- Modify `bins/server/src/router/git.rs:498-514` (the `is_client_fault` doc comment) and its test module at `git.rs:527-552`

**Interfaces:**
- Consumes: `is_client_fault(e: &crate::Error) -> bool` (unchanged)
- Produces: no new symbols; one new test and a comment naming the rule that keeps `Other` safe

- [ ] **Step 1: Write the failing test** — in `bins/server/src/router/git.rs`'s `mod tests`, after `os_io_errors_are_server_faults_and_protocol_ones_are_the_clients`:

```rust
    /// `Other` is answered 400 WITH ITS MESSAGE, so every producer of one on these paths must be
    /// a literal the client may read. Today that is pkt-line's own `io::Error::other` and
    /// `live_body`'s cap. This pins the contract: a new `Other` carrying an internal detail (a
    /// store URL, a peer address, a secret) must fail here rather than ship.
    #[test]
    fn the_only_other_kind_errors_answered_400_are_the_two_literals() {
        let fault = |e: Error| is_client_fault(&(Box::new(e) as crate::Error));
        // The two producers, by their exact strings.
        assert!(fault(Error::other("request body too large")));
        assert!(fault(Error::other("bad pkt len")));
        // The one `Other` the response path itself makes is a broken pipe, which is not `Other`
        // and so never reaches a client as text.
        let pipe = Error::new(ErrorKind::BrokenPipe, "client went away");
        assert!(!fault(pipe));
        // A source-level guard: nothing under the git router may construct an `io::Error::other`
        // whose message is built from a peer address or a store URL.
        let src = include_str!("git.rs");
        for line in src.lines().filter(|l| l.contains("Error::other(")) {
            assert!(
                !line.contains("format!") || line.contains("too large"),
                "an `Other` with a formatted message is echoed to the client verbatim: {line}"
            );
        }
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-server the_only_other_kind_errors_answered_400_are_the_two_literals`. Expected failure: ``error: couldn't read `git.rs`: No such file or directory`` is NOT expected (the path is relative to this file); the expected first failure is `assertion failed: !fault(pipe)` if `BrokenPipe` were ever added to the matched set — otherwise the test passes on the first run, which is the acceptable outcome for a pinning test: keep it, and record in the commit message that it pins existing behaviour.

- [ ] **Step 3: Implement** — extend the `is_client_fault` doc comment at `git.rs:498-505` with the rule the test pins:

```rust
/// `Other` is the one kind whose MESSAGE is echoed to the client in the 400 body, so every
/// producer of one on these paths must be a literal: pkt-line's own `io::Error::other`, and
/// `live_body`'s "request body too large". A future `Other` built with `format!` would put an
/// internal string on the wire — `the_only_other_kind_errors_answered_400_are_the_two_literals`
/// is what refuses one.
```

No behaviour change: the matched set at `git.rs:512-514` stays as it is.

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-server && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add bins/server/src/router/git.rs && git commit -m "Pin which io error messages may reach a client"`

---

### Task 9: Test that a valid token under the wrong username is refused

**Files:**
- Test `tests/http_e2e.rs` (a new helper beside `raw_get_with` at lines 25-44, and a new test)

**Interfaces:**
- Consumes: `open()` at `bins/server/src/router/git.rs:44-46` and `kloudlite_core::httpx::user_names` (no code change — this is the missing test in L6)
- Produces: `fn raw_get_as(port: u16, path: &str, user: &str, token: &str) -> String`

- [ ] **Step 1: Write the failing test** — append to `tests/http_e2e.rs`:

```rust
/// Like `raw_get`, but with a username of the test's choosing — every other helper here sends
/// git's `x` placeholder, which is exactly the half this test has to vary.
fn raw_get_as(port: u16, path: &str, user: &str, token: &str) -> String {
    use base64::Engine;
    use std::io::{Read, Write};
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
    write!(
        c,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Authorization: Basic {cred}\r\nGit-Protocol: version=2\r\n\r\n"
    )
    .unwrap();
    let mut s = Vec::new();
    c.read_to_end(&mut s).unwrap();
    String::from_utf8_lossy(&s).to_string()
}

/// The token is the secret, but the username must name the owner it belongs to (or be git's `x`).
/// Halves that disagree did not verify: the answer is 401, never a silent fall-through to
/// anonymous — which on a PUBLIC repo would look like a success and hide a wrong credential.
#[tokio::test(flavor = "multi_thread")]
async fn a_valid_token_under_the_wrong_username_is_refused() {
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let refs = "/alice/proj.git/info/refs?service=git-upload-pack";

    // The matching halves work, so the refusals below are about the mismatch and nothing else.
    assert!(raw_get_as(port, refs, "x", &token).starts_with("HTTP/1.1 200"));
    assert!(raw_get_as(port, refs, "alice", &token).starts_with("HTTP/1.1 200"));

    // A real token, a username that is neither `x` nor its owner: 401.
    let r = raw_get_as(port, refs, "bob", &token);
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");

    // And on a PUBLIC repo it is still 401, not a fall-through to the anonymous read.
    s.set_public("alice", "proj", true).await.unwrap();
    let r = raw_get_as(port, refs, "bob", &token);
    assert!(r.starts_with("HTTP/1.1 401"), "a wrong username must not read anonymously: {r}");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test --test http_e2e a_valid_token_under_the_wrong_username_is_refused`. This pins existing behaviour, so it is expected to PASS on the first run; if any assertion fails, that is the L6 gap having hidden a real defect — stop and fix `open()` at `bins/server/src/router/git.rs:38-49` before continuing, rather than relaxing the assertion.

- [ ] **Step 3: Implement** — no production change; the test is the deliverable. If Step 2 failed, the change is in `open()`: the `Some(o) if crate::auth::user_names(&user, &o, true) => Some(o)` arm must keep `_ => return Err(unauthorized())` as its only alternative, with no anonymous fall-through.

- [ ] **Step 4: Run tests and clippy** — `cargo test --test http_e2e && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 5: Commit** — `git add tests/http_e2e.rs && git commit -m "Test that a token under the wrong username is refused"`

---

## Self-review

- H1 (upload-pack negotiation cap) → Task 1
- Summary High #4 → Task 1 (the same finding; its "add the missing test beside `tests/git_http_limits.rs:81`" is Task 1 Step 1)
- M1 (newline injection into marker bodies) → Task 2
- Summary Medium `index.rs:60-90` / `repos.rs:60` → Task 2 (same finding)
- M2 (claim amplification) → Task 3 — existence-gated claim, exempt set fixed to `/api/{o}/{n}/create` and non-`GET`/`HEAD` `/v2` image routes
- Summary Medium `crates/app/src/lib.rs:411-422` → Task 3 (same finding)
- M3 (`/metrics` without the peer secret) → Task 4 — own listener/port, reusing `serve_if_configured`
- Summary Medium `route.rs:557-562` → Task 4 (same finding)
- L1 (`mem://` as a fleet store) → Task 5
- L2 (`forget_pack_public`) → Task 6
- L3 (`ssh_fingerprint` duplication) → Task 7 — single copy in `crates/core/src/sshkeys.rs` behind the optional `ssh` feature
- L4 (`revoke_tokens_for` full LIST) → deferred: the report itself says it is only worth an index "if `admin` ever runs it on a schedule"; nothing schedules it, so an index would be speculative work on a by-hand command.
- L5 (`io::ErrorKind::Other` echoed to clients) → Task 8
- L6 test gap 1 (negotiation cap) → Task 1
- L6 test gap 2 (newline forgery) → Task 2
- L6 test gap 3 (credential-half mismatch) → Task 9
- Architecture note "a `peer_write()` marker fn" → deferred: it is a suggestion, not a finding, and it adds a function with one job and no behaviour; revisit when a new peer-only write route is actually added.
