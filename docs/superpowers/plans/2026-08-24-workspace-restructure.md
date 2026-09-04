# Workspace Restructure — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut the single `kloudlite_git` library into a Cargo workspace (strategy A: bottom-up, crate-at-a-time behind a shrinking `kloudlite_git` facade) so each of the three binaries links only what it uses, and the eight files over 700 lines become focused module trees. Zero behavior change; binary names, image contents, deploy yamls, env vars unchanged.

**Architecture:** Extraction order is forced by two item-level facts the spec's module-level map could not see: (1) `App` is named by `registry` and `git` handler signatures (`State<Arc<App>>`), so it cannot dissolve into `bins/server` — it becomes a small `crates/app` crate; (2) `App` holds `dir: pulls::Source`, so `pulls` must extract before `app`, which must extract before `git` and `registry`. Final crate DAG:

```
core ── storage ── gitbase ── pulls ── app ── git ─┐
  │        │          │         │       │  ├─ registry ─┼─ bins/server
  │        │          │         │       │  │            ├─ bins/worker (core,storage,pulls,registry)
  │        └──────────┴─────────┴── api ┴──────────────── bins/api    (core,storage,pulls,api)
```

Two spec deviations, decided here on contact with the code (both recorded in their tasks):
- `objects.rs`/`refs.rs` go to a **`crates/gitbase`** crate (spec's named decision point): `objects` drags gix-pack/gix-object/gix-actor/flate2 and `refs` drags gix-traverse into any host crate's build; putting them in `storage` would hand gix-pack back to the worker. `storage` keeps only the gix the `Store` API already leaks (`gix-odb`, `gix-hash` via `Repo::odb()` / `get_ref`). `browse::merge_base` moves to `gitbase` too, which is what unknots `pulls → browse`.
- `gpg.rs` goes to **`crates/api`**, not `crates/git`: its only consumers are `api/credentials.rs` and `api/signatures.rs`. Consequence: the spec's "api binary drops pgp" is wrong — the api tier verifies GPG signatures; pgp stays in the api binary. Every other expected win holds.

**Tech Stack:** Rust 2021, Cargo workspace with `[workspace.dependencies]`, tokio, axum 0.8, SlateDB, gix-*, russh, pgp.

**Spec:** `docs/superpowers/specs/2026-08-24-workspace-restructure-design.md`

## Global Constraints

- **Behavior-frozen migration. NO functional changes.** Code moves and import rewrites only; the one sanctioned shape change is turning three `App` lane methods into free functions with identical bodies (Task 6).
- **The facade keeps every consumer compiling mid-migration:** after each extraction, `src/lib.rs` re-exports the moved modules (`pub use kloudlite_git_<crate>::…`) so `kloudlite_git::…` paths in `main.rs`, `src/bin/*`, and `tests/*.rs` are untouched until Task 11.
- **Every commit green:** foreground `cargo test` (full suite) and `cargo clippy --lib -- -D warnings` before every commit. As crates appear, additionally `cargo clippy -p <new-crate> -- -D warnings`; from Task 11 the invocation is `cargo clippy --workspace -- -D warnings` (lib + bin targets — the equivalent of today's gate; `--all-targets` keeps its pre-existing test-target lints, the bar there stays "no NEW warnings in files you touch").
- **Single-opener invariant untouched:** the routing middleware (`repo_of` → `route_inner`), `BROWSE_TAILS`, and `every_browse_route_is_routable` move only in Task 10, verbatim, and that test must be green in the same commit.
- **Binary names unchanged:** `kloudlite-git`, `kloudlite-git-api`, `kloudlite-git-worker` — `[[bin]] name` pins them regardless of package names.
- **Only two things delete a blob; manifest bytes verbatim; `Digest::parse` is the only path→key gate** — Task 8 moves that code without editing it.
- Preserve every `// ponytail:` marker; comments explain WHY at `src/http.rs` density; moved code keeps its comments byte-for-byte.
- Each crate whose handlers return `Result<T, Response>` keeps `#![allow(clippy::result_large_err)]` at its root (currently on `src/lib.rs`; needed in `git`, `registry`, `api`, `app`, `bins/server`).
- Commit subjects imperative sentence case, no tool attribution, no Claude reference.
- `Cargo.lock` must not change dependency versions at any step (`git diff Cargo.lock` shows only member additions).

---

### Task 1: Workspace shell around the existing package

**Files:**
- Modify: `Cargo.toml` (root)

**Steps:**

- [ ] **Step 1:** Prepend a `[workspace]` section to the root `Cargo.toml`, above `[package]` (the root package stays a member of its own workspace — Cargo's "root package" layout):

```toml
[workspace]
members = ["."]
resolver = "2"

# Every shared dependency pinned once. Members say `{ workspace = true }`; a member may add
# features but never a different version. Feature sets live here so two members cannot drift.
[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["io", "io-util"] }
slatedb = { version = "0.15", features = ["aws", "azure"] }
axum = { version = "0.8", features = ["http1", "tokio", "query"] }
russh = "0.62"
gix-odb = { version = "0.83", features = ["parallel"] }
gix-pack = "0.73"
gix-object = "0.63"
gix-hash = { version = "0.26", features = ["sha1"] }
gix-traverse = "0.60"
gix-features = { version = "0.49", features = ["progress"] }
gix-actor = "0.41"
flate2 = "1"
futures = "0.3"
rand = "0.8"
base64 = "0.22"
rustls = { version = "0.23", default-features = false, features = ["ring"] }
reqwest = { version = "0.13", default-features = false, features = ["stream", "query", "json", "gzip"] }
form_urlencoded = "1"
imara-diff = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
redis = { version = "0.27", features = ["tokio-comp", "connection-manager", "tls-rustls-webpki-roots", "tokio-rustls-comp"] }
mongodb = { version = "3", default-features = false, features = ["rustls-tls", "compat-3-0-0"] }
jsonwebtoken = { version = "11", default-features = false, features = ["rust_crypto"] }
chrono = "0.4"
pgp = { version = "0.20", default-features = false }
tower-http = { version = "0.6", default-features = false, features = ["compression-gzip"] }
# dev-only
slatedb-common = { version = "0.15", features = ["test-util"] }
async-trait = "0.1"
tower = { version = "0.5", features = ["util"] }
tempfile = "3"
```

  Keep the existing `[profile.release]` block where it is — profiles are read from the workspace root, so it is already in the right file. Move the WHY comments that sit on individual dependency lines (rand's three-versions note, rustls' crypto-provider note, jsonwebtoken's backend note, etc.) up into `[workspace.dependencies]` alongside their pins as each line moves.
- [ ] **Step 2:** Rewrite the root `[dependencies]`/`[dev-dependencies]` entries to `{ workspace = true }` form, e.g. `tokio = { workspace = true }`, `pgp = { workspace = true }`. No feature or version may change; `cargo tree > /tmp/before.txt` before and `diff` against after must be empty.
- [ ] **Step 3:** Verify:

```sh
cargo tree -e normal > /tmp/tree-after.txt && diff /tmp/tree-before.txt /tmp/tree-after.txt   # empty
cargo test            # full suite, foreground — all green
cargo clippy --lib -- -D warnings
git diff Cargo.lock   # no version changes
```

- [ ] **Step 4:** Commit: `Wrap the package in a workspace with pinned shared dependencies`

---

### Task 2: Extract crates/core (err, hex, jwt, pktline, httpx, peer)

This is the riskiest seam because it splits two files (`src/proxy.rs`, `src/http.rs`) along item boundaries. The exact item lists below were read from the working tree.

**Files:**
- Create: `crates/core/Cargo.toml`, `crates/core/src/lib.rs`, `crates/core/src/err.rs`, `crates/core/src/jwt.rs` (moved), `crates/core/src/pktline.rs` (moved), `crates/core/src/httpx.rs`, `crates/core/src/peer.rs`
- Modify: root `Cargo.toml` (members += `crates/core`; dependency on it), `src/lib.rs`, `src/proxy.rs`, `src/http.rs`, `src/protocol/receive.rs`

**Steps:**

- [ ] **Step 1:** `crates/core/Cargo.toml`:

```toml
[package]
name = "kloudlite-git-core"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[lib]
name = "kloudlite_git_core"

[dependencies]
axum = { workspace = true }
reqwest = { workspace = true }
jsonwebtoken = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
base64 = { workspace = true }
tokio = { workspace = true }
futures = { workspace = true }
```

- [ ] **Step 2:** `crates/core/src/err.rs` — move from `src/lib.rs`, verbatim: `pub type Error`, `pub type Result<T>`, `pub fn err`, `pub fn hex` (with its doc comment), `pub fn require_jwt_secret`, `pub fn require_jwt_secret_from_env`, and the lib.rs unit tests that cover them (`hex_is_lowercase_and_two_chars_per_byte`, the require_jwt_secret tests). `crates/core/src/lib.rs`:

```rust
#![allow(clippy::result_large_err)]
pub mod err;
pub mod httpx;
pub mod jwt;
pub mod peer;
pub mod pktline;
pub use err::{err, hex, require_jwt_secret, require_jwt_secret_from_env, Error, Result};
```

- [ ] **Step 3:** Move `src/jwt.rs` → `crates/core/src/jwt.rs` and `src/pktline.rs` → `crates/core/src/pktline.rs` unchanged except `crate::Result`-style paths now resolve inside core (they already only use `crate::{err, Result}`).
- [ ] **Step 4:** `crates/core/src/httpx.rs` — the shared HTTP bits `registry` and `protocol` import from `http.rs`. Exactly two items move (everything else in `http.rs` stays put until Task 10):
  - `pub struct Trusted(pub Option<String>)` with its `FromRequestParts`/extension plumbing and doc comment (from `src/http.rs:165`) — consumers: all of `src/registry/*.rs`, all of `src/http/browse_api/*.rs`.
  - `pub fn max_body() -> usize` (from `src/http.rs:22`; was `pub(crate)`, becomes `pub`) — consumer: `src/protocol/receive.rs:397`.
  `max_decompressed()` does NOT move — its only caller stays in `http.rs`.
- [ ] **Step 5:** `crates/core/src/peer.rs` — the client half of `src/proxy.rs`, verbatim, in this exact item list (no `App`, no russh, in any of them):
  - consts: `OWNER_HEADER`, `HOPS_HEADER`, `PEER_HEADER`, `MAX_HOPS`, `CONNECT_TIMEOUT`, `LEADER_TIMEOUT`, `CLAIM_ATTEMPTS`, `CLAIM_BACKOFF`, `RECOVER_ATTEMPTS`, `RECOVER_BACKOFF`, `RELEASE_ATTEMPTS`, `RELEASE_BACKOFF`, `HOP_BY_HOP`
  - fns: `secret_eq`, `is_connect_error`, `stream_addr` (keep its `// ponytail:` marker), `stream_to_peer`
  - `pub struct Forwarder` + `impl Forwarder { new, forward }` — change `pub(crate) client/secret` to `pub` (the server half in the facade now reads `forwarder.secret` cross-crate; peer stream check in `serve_peer_stream` and worker's header both do too)
  - `#[cfg(test)] mod is_connect_error_tests` and `mod tests` (secret_eq test)
  What STAYS in `src/proxy.rs` (server half; moves to `git` in Task 7): `HEADER_MAX`, `HEADER_TIMEOUT`, `serve_peer_streams`, `serve_peer_stream` — these use `App`, `ssh::serve_git`, `pool::is_fenced`, `pktline::write_err`.
- [ ] **Step 6:** Facade wiring in the root package:
  - root `Cargo.toml`: `members = [".", "crates/core"]`; `[dependencies] kloudlite-git-core = { path = "crates/core" }`.
  - `src/lib.rs`: delete the moved items and `pub mod jwt; pub mod pktline;`; add
    ```rust
    pub use kloudlite_git_core::{err, hex, require_jwt_secret, require_jwt_secret_from_env, Error, Result};
    pub use kloudlite_git_core::{jwt, pktline};
    ```
  - `src/proxy.rs`: delete the moved items; add at top `pub use kloudlite_git_core::peer::*;` (keeps every `crate::proxy::X` and `kloudlite_git::proxy::X` path alive — tests import `kloudlite_git::proxy::{Forwarder, HOPS_HEADER, OWNER_HEADER, PEER_HEADER}`).
  - `src/http.rs`: delete `Trusted` and `max_body`; add `pub use kloudlite_git_core::httpx::{max_body, Trusted};` and fix the two `use crate::http::` sites if paths shifted (they should not).
- [ ] **Step 7:** Verify:

```sh
cargo test
cargo clippy --lib -- -D warnings
cargo clippy -p kloudlite-git-core -- -D warnings
! cargo tree -p kloudlite-git-core -e normal | grep -qE 'russh|gix-|pgp|slatedb|redis|mongodb'   # core is thin
```

- [ ] **Step 8:** Commit: `Extract the core crate: errors, jwt, pktline, http shared bits, peer client`

---

### Task 3: Extract crates/storage (store, pool, ownership, index, cache, events, auth, config) with the pool and cache splits

**Decision point (objects/refs), resolved:** `objects.rs` and `refs.rs` do NOT join `storage`. `Staging::add` takes `gix_object::Kind`, `apply_changes` writes packs via `gix-pack`, and `refs.rs` walks with `gix-traverse` — hosting them here would put gix-pack/gix-traverse into every `storage` consumer, including the worker the spec promises drops them. They go to `crates/gitbase` (Task 4). `storage` accepts the gix its API already exposes — `Repo::odb() -> gix_odb::Handle` (`src/store.rs:118`) and `get_ref -> Option<gix_hash::ObjectId>` — so its gix footprint is exactly `gix-odb` + `gix-hash`.

**Files:**
- Create: `crates/storage/Cargo.toml`, `crates/storage/src/lib.rs`; move `src/store.rs`, `src/ownership.rs` + `src/ownership/tests.rs`, `src/index.rs`, `src/events.rs`, `src/auth.rs`, `src/config.rs`; split-move `src/pool.rs` → `crates/storage/src/pool/{mod,lease,evict}.rs` and `src/cache.rs` → `crates/storage/src/cache/{mod,disk}.rs`
- Modify: root `Cargo.toml`, `src/lib.rs`

**Steps:**

- [ ] **Step 1:** `crates/storage/Cargo.toml`:

```toml
[package]
name = "kloudlite-git-storage"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[lib]
name = "kloudlite_git_storage"

[dependencies]
kloudlite-git-core = { path = "../core" }
tokio = { workspace = true }
tokio-util = { workspace = true }
slatedb = { workspace = true }
gix-odb = { workspace = true }
gix-hash = { workspace = true }
futures = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
redis = { workspace = true }
rand = { workspace = true }
base64 = { workspace = true }
chrono = { workspace = true }

[dev-dependencies]
slatedb-common = { workspace = true }
tempfile = { workspace = true }
```

  (If `cargo check -p kloudlite-git-storage` flags an unused entry, delete it; if it flags a missing one — e.g. `axum` should NOT be needed here; if it is, the offending item belongs in a higher crate, stop and re-read the seam.)
- [ ] **Step 2:** Move `store.rs`, `ownership.rs` (+ `ownership/tests.rs`), `index.rs`, `events.rs`, `auth.rs`, `config.rs` verbatim. Inside the crate, rewrite `crate::err`/`crate::Result`/`crate::hex` to `kloudlite_git_core::…` (or re-export them from `crates/storage/src/lib.rs` as `use kloudlite_git_core::{err, hex, Error, Result};` so `crate::err` keeps resolving — do the re-export, it is the smaller diff and every later crate repeats the pattern):

```rust
#![allow(clippy::result_large_err)]
pub(crate) use kloudlite_git_core::{err, hex, Error, Result};
pub mod auth;
pub mod cache;
pub mod config;
pub mod events;
pub mod index;
pub mod ownership;
pub mod pool;
pub mod store;
```

- [ ] **Step 3 (spec file split — pool.rs, 891 lines):** land it as `pool/mod.rs` (Pool struct, `path`, `env_u64`, `FencedError`, `is_fenced`, the `ReleaseHook` trait, construction/`exists`), `pool/lease.rs` (open/lease/handle-lifecycle: everything from the lease-taking path through fencing detection), `pool/evict.rs` (eviction, `max_warm` pressure, release-on-close). Cut at existing `impl` block boundaries — no function body changes; `pub(crate)` between the submodules, public surface identical (`pub use` from `mod.rs` for every item that was `pub` before: `Pool`, `FencedError`, `is_fenced`, `path`, `ReleaseHook`). Keep the `// ponytail:` markers at `pool.rs:267` and near line 28's Weak-hook comment with their code.
- [ ] **Step 4 (spec file split — cache.rs, 766 lines):** `cache/mod.rs` (Cache struct, `key`, memory layer `mem_get`/`Mem`, redis `run`/`run_within`), `cache/disk.rs` (the on-disk cache half: disk read/write/prune paths). Same rules as Step 3.
- [ ] **Step 5:** Facade: root `Cargo.toml` gains `kloudlite-git-storage = { path = "crates/storage" }` and the member entry. `src/lib.rs` replaces the eight `pub mod` lines with:

```rust
pub use kloudlite_git_storage::{auth, cache, config, events, index, ownership, pool, store};
```

- [ ] **Step 6:** Verify:

```sh
cargo test
cargo clippy --lib -- -D warnings && cargo clippy -p kloudlite-git-storage -- -D warnings
! cargo tree -p kloudlite-git-storage -e normal | grep -qE 'russh|pgp|gix-pack|gix-traverse|gix-object|axum|imara-diff'
```

- [ ] **Step 7:** Commit: `Extract the storage crate with pool and cache split into modules`

---

### Task 4: Extract crates/gitbase (objects, refs, merge_base)

**Files:**
- Create: `crates/gitbase/Cargo.toml`, `crates/gitbase/src/lib.rs`; move `src/objects.rs`, `src/refs.rs`; move `pub fn merge_base` out of `src/browse.rs:442`
- Modify: root `Cargo.toml`, `src/lib.rs`, `src/browse.rs`, `src/pulls.rs`

**Steps:**

- [ ] **Step 1:** `crates/gitbase/Cargo.toml` — deps: `kloudlite-git-core`, `kloudlite-git-storage`, `gix-odb`, `gix-hash`, `gix-object`, `gix-pack`, `gix-traverse`, `gix-features`, `gix-actor`, `flate2`, `tokio`, `serde` (all `{ workspace = true }` except the two path deps). Lib name `kloudlite_git_gitbase`.
- [ ] **Step 2:** Move `objects.rs` and `refs.rs` verbatim (`crate::store` → `kloudlite_git_storage::store`, or the same `pub(crate) use` re-export trick as Task 3). `crates/gitbase/src/lib.rs`:

```rust
pub(crate) use kloudlite_git_core::{err, Error, Result};
pub(crate) use kloudlite_git_storage::store;
pub mod objects;
pub mod refs;
mod merge_base;
pub use merge_base::merge_base;
```

- [ ] **Step 3:** Move `merge_base` (the whole function, its doc comment, and any private helper only it uses — check `src/browse.rs` around line 442) into `crates/gitbase/src/merge_base.rs`. In `src/browse.rs` add `pub use kloudlite_git_gitbase::merge_base;` so `crate::browse::merge_base` (callers: `src/pulls.rs:462`, `src/http/browse_api/merge.rs:136`) and test imports keep resolving.
- [ ] **Step 4:** Facade: `src/lib.rs` → `pub use kloudlite_git_gitbase::{objects, refs};` replacing the two `pub mod` lines; root Cargo.toml member + dep.
- [ ] **Step 5:** Verify:

```sh
cargo test
cargo clippy --lib -- -D warnings && cargo clippy -p kloudlite-git-gitbase -- -D warnings
! cargo tree -p kloudlite-git-gitbase -e normal | grep -qE 'russh|pgp|axum|redis|mongodb'
```

- [ ] **Step 6:** Commit: `Extract the gitbase crate for objects, refs and merge_base`

---

### Task 5: Extract crates/pulls (model/check/jobs split, directory split, merge_worker)

**Decision point (pulls seam), resolved by reading `src/pulls.rs`:**
- **`model.rs`** — everything the worker imports, zero gix: `pull_key`, `PullRequest`, `Mergeability`, `MergeJob`, `PullState`, `Comment`, the `ms`/`ms_opt` deserializers, `get`, `put`, `list`, `with_merge_jobs`, `open_only`, `next_number`, `Source`, `ensure_migrated`, `migrate_from`, `is_migrated`, and **`Deep`** (moved out of the check section — `src/bin/worker.rs` names `kloudlite_git::pulls::Deep` at lines 254/355, so it must not sit behind the gix gate).
- **`check.rs`** — gix, server-only, **feature-gated** (`#[cfg(feature = "check")] pub mod check;` with `pub use check::*` under the same cfg): `CHECK_LIMIT`, `check`, `check_with`, `check_repo`, `Checked` (its `Deep` payload comes from `model`). This is the only module that touches `gix_hash::ObjectId`, `store::Repo::odb()`, and `gitbase::merge_base` — the feature gates optional deps `gix-hash` and `kloudlite-git-gitbase`.
- **`jobs.rs`** — merge-claim lifecycle, no gix: `modify`, `claim_merge`, `takeable`, `claim_merge_number`, `ANNOUNCE_EVERY`, `stranded_merges`, `mark_announced`, `finish_merge`, `clear_merge`, `set_state`.
- The worker links `kloudlite-git-pulls` with default features (no `check`); the facade — and later `bins/server` — enables `features = ["check"]`. The worker_merges suite (37 tests) is the guard that no code moved across an await boundary.

**Files:**
- Create: `crates/pulls/Cargo.toml`, `crates/pulls/src/lib.rs`, `crates/pulls/src/pulls/{mod,model,check,jobs}.rs`, `crates/pulls/src/directory/{mod,teams}.rs` (split of `src/directory.rs`, 775 lines), `crates/pulls/src/merge_worker.rs` (moved)
- Modify: root `Cargo.toml`, `src/lib.rs`

**Steps:**

- [ ] **Step 1:** `crates/pulls/Cargo.toml`:

```toml
[package]
name = "kloudlite-git-pulls"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[lib]
name = "kloudlite_git_pulls"

[features]
# The mergeability check is a gix graph walk; the worker never runs it and must not link gix
# to have the pull-request model. The server and the facade turn it on.
check = ["dep:gix-hash", "dep:kloudlite-git-gitbase"]

[dependencies]
kloudlite-git-core = { path = "../core" }
kloudlite-git-storage = { path = "../storage" }
kloudlite-git-gitbase = { path = "../gitbase", optional = true }
gix-hash = { workspace = true, optional = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
futures = { workspace = true }
mongodb = { workspace = true }
reqwest = { workspace = true }
chrono = { workspace = true }
```

- [ ] **Step 2:** Split `src/pulls.rs` into `pulls/{model,check,jobs}.rs` per the resolved seam above; `pulls/mod.rs` is re-exports only, so every existing path (`pulls::check`, `pulls::Deep`, `pulls::claim_merge`, …) is unchanged:

```rust
pub mod model;
pub use model::*;
mod jobs;
pub use jobs::*;
#[cfg(feature = "check")]
mod check;
#[cfg(feature = "check")]
pub use check::*;
pub use crate::directory::{MergeState, MergeableState};
```

- [ ] **Step 3 (spec file split — directory.rs):** `directory/mod.rs` (types `Member`, `User`, `Handle`, `Repo`, `Credential`, `Passkey`, `check_handle`, `Directory` struct + its repo/user/credential methods, `is_duplicate_key`, the `pub use crate::pulls::{Comment, …}` re-export) and `directory/teams.rs` (`Team` and the team-membership methods of `Directory` as a second `impl Directory` block). Cut at impl-block boundaries; public surface identical via `pub use teams::*` if anything is free-standing.
- [ ] **Step 4:** Move `src/merge_worker.rs` verbatim (`crate::Result` → core re-export; `crate::directory::MergeableState` → `crate::directory::…` still works inside this crate). The `local()`/`networked()` split moves untouched — never format a networked argv into anything.
- [ ] **Step 5:** Facade: `src/lib.rs` → `pub use kloudlite_git_pulls::{directory, merge_worker, pulls};` (root dep declared with `features = ["check"]`).
- [ ] **Step 6:** Verify:

```sh
cargo test        # includes the 37-test worker_merges guard inside tests/pulls.rs
cargo clippy --lib -- -D warnings && cargo clippy -p kloudlite-git-pulls --all-features -- -D warnings
! cargo tree -p kloudlite-git-pulls -e normal | grep -qE 'gix-|russh|pgp'        # default features: no gix
cargo tree -p kloudlite-git-pulls -e normal --features check | grep -q gix-hash  # check pulls it in
```

- [ ] **Step 7:** Commit: `Extract the pulls crate split into model, check and jobs`

---

### Task 6: Extract crates/app (App minus its three lanes)

`App` cannot dissolve into `bins/server` (spec assumption): `src/registry/routes.rs`, `src/registry/auth.rs`, `src/ssh.rs`, and the proxy server half all name it. It becomes a small crate above `pulls` (it holds `dir: pulls::Source`). The three lane methods that would drag `pulls/check` into `app` — `reconcile_owned_markers`, `check_owned_pulls`, `announce_stranded_merges` — become free functions `pub async fn …(app: &App)` with **identical bodies** (`self` → `app`), parked in the facade `src/lib.rs` until Task 10 moves them to `bins/server/src/lanes.rs`, which is where the spec wants lanes anyway.

**Files:**
- Create: `crates/app/Cargo.toml`, `crates/app/src/lib.rs`
- Modify: root `Cargo.toml`, `src/lib.rs`, `src/main.rs` (three call sites in `spawn_lease_tasks`)

**Steps:**

- [ ] **Step 1:** `crates/app/Cargo.toml` — deps: `kloudlite-git-core`, `kloudlite-git-storage`, `kloudlite-git-pulls` (NO `check` feature), `tokio`, `futures`, `rand`, `serde_json`, `reqwest`. Lib name `kloudlite_git_app`.
- [ ] **Step 2:** Move from `src/lib.rs` into `crates/app/src/lib.rs`, verbatim: `AddrOf`, `Patience`, `App` (struct, all fields including `dir: pulls::Source`, `neg_cache`, `recovery_asked`, `skew_ms` — keep both `// ponytail:` markers in the field docs), `RECOVERY_ASK_EVERY`, `RECONCILE_GAP` (becomes `pub` — the lane free fns in the facade need it), `NEG_TTL`, `impl pool::ReleaseHook for App`, and `impl App` **minus** the three lane methods: keep `new`, `with_directory`, `neg_cache_hit`, `neg_cache_miss`, `owner`, `with_topology`, `is_leader`, `route`, `now_ms`, `advance_clock`, `may_ask_to_recover`, `claim`, `claim_to_recover`, `force_claim`, `renew_all`, `renew_once`, `prune_once`, `release`, `announce_draining`, `grant_claim`, `grant_renew`, `grant_release`, `on_fenced`, `open_repo_after_fence`. `Forwarder` comes from `kloudlite_git_core::peer`.
- [ ] **Step 3:** In the facade `src/lib.rs`: add `pub use kloudlite_git_app::{App, AddrOf, Patience, RECOVERY_ASK_EVERY};` and rewrite the three lanes as free functions with unchanged bodies:

```rust
pub async fn reconcile_owned_markers(app: &App) { /* body of the old method, self -> app */ }
pub async fn check_owned_pulls(app: &App) { /* uses pulls::check_repo — facade has the check feature */ }
pub async fn announce_stranded_merges(app: &App) { /* uses pulls::stranded_merges + events */ }
```

  In `src/main.rs` `spawn_lease_tasks`, change the three call sites `a.reconcile_owned_markers().await` → `kloudlite_git::reconcile_owned_markers(&a).await` (same for the other two). This is the plan's one sanctioned call-shape change; nothing about ordering, intervals, or bodies moves.
- [ ] **Step 4:** Verify:

```sh
cargo test
cargo clippy --lib -- -D warnings && cargo clippy -p kloudlite-git-app -- -D warnings
! cargo tree -p kloudlite-git-app -e normal | grep -qE 'gix-|russh|pgp|axum'   # app stays gix-free
```

- [ ] **Step 5:** Commit: `Extract the app crate and turn its lanes into free functions`

---

### Task 7: Extract crates/git (protocol with the upload split, browse, ssh, gc, proxy server half)

**Files:**
- Create: `crates/git/Cargo.toml`, `crates/git/src/lib.rs`; move `src/protocol/{mod,receive}.rs`; split-move `src/protocol/upload.rs` (974 lines) → `crates/git/src/protocol/upload/{mod,refs,walk,pack}.rs`; move `src/browse.rs`, `src/ssh.rs`, `src/gc.rs`; move the remainder of `src/proxy.rs`
- Modify: root `Cargo.toml`, `src/lib.rs`; delete `src/proxy.rs`

**Steps:**

- [ ] **Step 1:** `crates/git/Cargo.toml` — package `kloudlite-git-git`, lib `kloudlite_git_git`; deps: `kloudlite-git-core`, `kloudlite-git-storage`, `kloudlite-git-gitbase`, `kloudlite-git-app`, and workspace deps `tokio`, `tokio-util`, `axum`, `russh`, `gix-odb`, `gix-pack`, `gix-object`, `gix-hash`, `gix-traverse`, `gix-features`, `flate2`, `futures`, `imara-diff`, `serde`, `serde_json`, `base64`. (`imara-diff` is browse's; confirm with `cargo check -p` and prune unused.)
- [ ] **Step 2 (spec file split — upload.rs):** `upload/mod.rs` keeps the entry points and re-exports (`advertise`, `serve`, `read_args`); `upload/refs.rs` gets ref advertisement (`ls_refs`, `head_target`, `peel_to_object`); `upload/walk.rs` gets negotiation and walking (`fetch`'s want/have walk helpers: `parse_size`, `filtered_objects`, `keep_blob`, `shallow_walk`, `ours`, `peel_wants`, `commit_range`, `counts_with_leaves`); `upload/pack.rs` gets pack emission (`write_pack_range`, `count_objects`, `pack_from_ids`, `write_counts`) — including the gitoxide#2935 merge-commit workaround, which moves with its comment intact (delete only when upstream fixes, per CLAUDE.md). `fetch` itself stays in `mod.rs` if it is the orchestrator; cut helpers at function boundaries, `pub(super)` visibility, public surface unchanged (`advertise`, `serve` re-exported from `protocol::upload`).
- [ ] **Step 3:** Move `protocol/mod.rs`, `protocol/receive.rs`, `browse.rs` (minus `merge_base`, already gone; keep its `pub use kloudlite_git_gitbase::merge_base;`), `ssh.rs`, `gc.rs` verbatim. `receive.rs`'s `crate::http::max_body` becomes `kloudlite_git_core::httpx::max_body`.
- [ ] **Step 4:** Move the server half of `src/proxy.rs` (`HEADER_MAX`, `HEADER_TIMEOUT`, `serve_peer_streams`, `serve_peer_stream`) to `crates/git/src/proxy.rs`, whose top reads `pub use kloudlite_git_core::peer::*;` so `proxy::Forwarder` etc. keep one canonical path. Delete `src/proxy.rs`.
- [ ] **Step 5:** `crates/git/src/lib.rs`:

```rust
#![allow(clippy::result_large_err)]
pub(crate) use kloudlite_git_core::{err, Error, Result};
pub(crate) use kloudlite_git_storage::{auth, ownership, pool, store};
pub(crate) use kloudlite_git_gitbase::refs;
pub(crate) use kloudlite_git_app::App;
pub mod browse;
pub mod gc;
pub mod protocol;
pub mod proxy;
pub mod ssh;
```

  Facade `src/lib.rs`: `pub use kloudlite_git_git::{browse, gc, protocol, proxy, ssh};`
- [ ] **Step 6:** Verify:

```sh
cargo test        # protocol.rs, ssh_e2e.rs, pack_cap.rs, proxy.rs suites all still through the facade
cargo clippy --lib -- -D warnings && cargo clippy -p kloudlite-git-git -- -D warnings
! cargo tree -p kloudlite-git-git -e normal | grep -qE 'pgp|mongodb'
```

- [ ] **Step 7:** Commit: `Extract the git crate with upload split into refs, walk and pack`

---

### Task 8: Extract crates/registry

**Files:**
- Create: `crates/registry/Cargo.toml`; move `src/registry/{mod,auth,blobs,gc,manifests,referrers,routes,store,uploads}.rs` → `crates/registry/src/`
- Modify: root `Cargo.toml`, `src/lib.rs`

**Steps:**

- [ ] **Step 1:** `crates/registry/Cargo.toml` — package `kloudlite-git-registry`, lib `kloudlite_git_registry`; deps: `kloudlite-git-core`, `kloudlite-git-storage`, `kloudlite-git-app`, and `tokio`, `axum`, `futures`, `serde`, `serde_json`, `base64`, `chrono`, `form_urlencoded`, `reqwest`. No gix, no russh, no pgp.
- [ ] **Step 2:** Move all nine files byte-for-byte; `src/registry/mod.rs` becomes `crates/registry/src/lib.rs` (prepend `#![allow(clippy::result_large_err)]` and the cross-crate `pub(crate) use` block: `kloudlite_git_core::{err, hex, jwt, httpx::Trusted}`, `kloudlite_git_storage::{auth, index, ownership, pool, store}`, `kloudlite_git_app::App`). The blob-deletion rule, verbatim manifests, `Digest::parse`, `oci_err` envelope: moved, not edited.
- [ ] **Step 3:** Facade: `src/lib.rs` → `pub use kloudlite_git_registry as registry;` (keeps `kloudlite_git::registry::…` and, inside the facade, `crate::registry::…`).
- [ ] **Step 4:** Verify:

```sh
cargo test        # registry_{blobs,http,gc,limits,manifests,store,uploads}.rs suites
cargo clippy --lib -- -D warnings && cargo clippy -p kloudlite-git-registry -- -D warnings
! cargo tree -p kloudlite-git-registry -e normal | grep -qE 'gix-|russh|pgp|mongodb|imara-diff'
```

- [ ] **Step 5:** Commit: `Extract the registry crate`

---

### Task 9: Extract crates/api (api handlers plus gpg)

**Files:**
- Create: `crates/api/Cargo.toml`; move `src/api/{mod,browse,credentials,feed,forward,images,passkeys,pulls,repos,signatures,teams}.rs` → `crates/api/src/`; move `src/gpg.rs` → `crates/api/src/gpg.rs`
- Modify: root `Cargo.toml`, `src/lib.rs`

**Steps:**

- [ ] **Step 1:** `crates/api/Cargo.toml` — package `kloudlite-git-api`, lib `kloudlite_git_api`; deps: `kloudlite-git-core`, `kloudlite-git-storage`, `kloudlite-git-pulls` (default features — the api tier reads the model and the directory, it never runs the gix check), and `tokio`, `axum`, `tower-http`, `pgp`, `serde`, `serde_json`, `base64`, `reqwest`, `futures`, `chrono`, `rand`. **Recorded deviation:** `gpg` lives here, not in `git` — its only consumers are `credentials.rs` and `signatures.rs`, and moving it here is what actually strips pgp from the git-serving path; the spec's "api binary drops pgp" is dropped as infeasible (the api tier verifies commit signatures).
- [ ] **Step 2:** Move the files; `api/mod.rs` becomes `crates/api/src/lib.rs` (add `#![allow(clippy::result_large_err)]`, `pub mod gpg;`, and `pub(crate) use kloudlite_git_core::{err, jwt, Result}; pub(crate) use kloudlite_git_storage::{auth, cache, events, store}; pub(crate) use kloudlite_git_pulls::directory;`). `crate::gpg::…` in credentials/signatures now resolves inside this crate; `gpg`'s `#[cfg(test)] pub mod tests` helpers (`gen`, `reforge_subkey`, `subkey_signature`) stay visible to `signatures.rs` tests since both are one crate.
- [ ] **Step 3:** Facade: `src/lib.rs` → `pub use kloudlite_git_api as api; pub use kloudlite_git_api::gpg;`.
- [ ] **Step 4:** Verify:

```sh
cargo test        # api_server.rs suite
cargo clippy --lib -- -D warnings && cargo clippy -p kloudlite-git-api -- -D warnings
! cargo tree -p kloudlite-git-api -e normal | grep -qE 'gix-|russh|slatedb'   # note: slatedb SHOULD be absent
```

  If that last assertion fails because `storage` carries slatedb (it does), drop the `slatedb` term — the real assertions are `gix-|russh`. The binary-level wins are measured in Task 10.
- [ ] **Step 5:** Commit: `Extract the api crate and move gpg beside its only consumers`

---

### Task 10: Create the three bin packages; split main.rs and http.rs; move browse_api

**Files:**
- Create: `bins/server/Cargo.toml`, `bins/server/src/main.rs`, `bins/server/src/{boot,lanes,listeners}.rs` (split of `src/main.rs`, 922 lines), `bins/server/src/router/{mod,route,git,limits}.rs` (split of `src/http.rs`, 1040 lines), `bins/server/src/browse_api/` (moved from `src/http/browse_api/`), `bins/server/src/lib.rs` (thin, for the test host)
- Create: `bins/api/Cargo.toml`, `bins/api/src/main.rs` (moved from `src/bin/api.rs`)
- Create: `bins/worker/Cargo.toml`, `bins/worker/src/main.rs` (moved from `src/bin/worker.rs`)
- Modify: root `Cargo.toml` (drop `[[bin]]` sections, keep `[lib]` facade one more task), `src/lib.rs`
- Delete: `src/main.rs`, `src/bin/`, `src/http.rs`, `src/http/`

**Steps:**

- [ ] **Step 1:** Bin manifests — minimal dependency lists, enumerated:

```toml
# bins/server/Cargo.toml
[package]
name = "kloudlite-git-server"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[lib]                      # thin lib so the workspace test host (Task 11) can drive the router
name = "kloudlite_git_server"
path = "src/lib.rs"

[[bin]]
name = "kloudlite-git"        # binary name unchanged
path = "src/main.rs"

[dependencies]
kloudlite-git-core = { path = "../../crates/core" }
kloudlite-git-storage = { path = "../../crates/storage" }
kloudlite-git-gitbase = { path = "../../crates/gitbase" }
kloudlite-git-pulls = { path = "../../crates/pulls", features = ["check"] }
kloudlite-git-app = { path = "../../crates/app" }
kloudlite-git-git = { path = "../../crates/git" }
kloudlite-git-registry = { path = "../../crates/registry" }
tokio = { workspace = true }
axum = { workspace = true }
tower-http = { workspace = true }
russh = { workspace = true }
rustls = { workspace = true }          # main() installs the ring provider
flate2 = { workspace = true }          # gzip request bodies in the router
futures = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
rand = { workspace = true }
base64 = { workspace = true }
reqwest = { workspace = true }
gix-hash = { workspace = true }        # only if the router names ObjectId; prune if not
```

```toml
# bins/api/Cargo.toml
[package]
name = "kloudlite-git-api-bin"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[[bin]]
name = "kloudlite-git-api"
path = "src/main.rs"

[dependencies]
kloudlite-git-core = { path = "../../crates/core" }
kloudlite-git-storage = { path = "../../crates/storage" }
kloudlite-git-pulls = { path = "../../crates/pulls" }     # directory; no check feature
kloudlite-git-api = { path = "../../crates/api" }
tokio = { workspace = true }
axum = { workspace = true }
rustls = { workspace = true }
```

```toml
# bins/worker/Cargo.toml
[package]
name = "kloudlite-git-worker-bin"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[[bin]]
name = "kloudlite-git-worker"
path = "src/main.rs"

[dependencies]
kloudlite-git-core = { path = "../../crates/core" }
kloudlite-git-storage = { path = "../../crates/storage" }
kloudlite-git-pulls = { path = "../../crates/pulls" }     # model + jobs + merge_worker; NO check
kloudlite-git-registry = { path = "../../crates/registry" }
tokio = { workspace = true }
rustls = { workspace = true }
reqwest = { workspace = true }
redis = { workspace = true }
serde_json = { workspace = true }
futures = { workspace = true }
```

  In each moved `main.rs`, rewrite `kloudlite_git::X` to the owning crate path (mechanical: `kloudlite_git::config` → `kloudlite_git_storage::config`, `kloudlite_git::merge_worker` → `kloudlite_git_pulls::merge_worker`, `kloudlite_git::proxy::PEER_HEADER` → `kloudlite_git_core::peer::PEER_HEADER`, `kloudlite_git::registry::gc` → `kloudlite_git_registry::gc`, …). Bins are the first consumers to leave the facade; tests stay on it until Task 11.
- [ ] **Step 2 (spec file split — main.rs → bins/server):** `boot.rs` (`host_key`, `fleet_guard` + its tests, `backfill_repo_markers` + `backfill_tests`, `run`/CLI admin plumbing, `post_to_owner`, the `mod tests` store helper), `lanes.rs` (`spawn_lease_tasks` plus the three free functions from the facade — `reconcile_owned_markers`, `check_owned_pulls`, `announce_stranded_merges` — deleted from `src/lib.rs` in the same commit), `listeners.rs` (the listener-binding parts of `serve()`: public, peer, peer-stream, ssh), `main.rs` (`main`, `serve()` orchestration, crypto-provider install). Cut at existing function boundaries only.
- [ ] **Step 3 (spec file split — http.rs → bins/server/src/router/):** `router/route.rs` — the routing middleware and ownership plumbing: `repo_of`, `api_prefixed`, `git_shape`, `api_route`, `is_git_route`, `route_public`, `route_peer`, `route_inner`, `trust_peer`, `trust_nobody`, `healthz`, `own_claim`, `own_renew`, `own_release`, `own_draining`, `two_lines`, `leader_only`, `BROWSE_TAILS`, and the `every_browse_route_is_routable` test beside it. `router/git.rs` — the git handlers: `open`, `body_reader`, `reopen_after_fence`, `info_refs`, `upload_pack`, `receive_pack`, `respond_first`, `is_client_fault`, `success`, `git_routes`. `router/limits.rs` — `max_decompressed`, `internal`, `client_err`, `bad_request`, `fenced_elsewhere` (plus a `pub use kloudlite_git_core::httpx::{max_body, Trusted};`). `router/mod.rs` — `router()`, `peer_router()`, re-exports. Move `src/http/browse_api/` to `bins/server/src/browse_api/` unchanged (its `crate::…` paths point at the re-export block in `bins/server/src/lib.rs`).
- [ ] **Step 4:** `bins/server/src/lib.rs` exposes what main.rs and the test host need:

```rust
#![allow(clippy::result_large_err)]
pub(crate) use kloudlite_git_core::{err, Error, Result};
pub(crate) use kloudlite_git_storage::{auth, cache, events, index, ownership, pool, store};
pub(crate) use kloudlite_git_gitbase::{objects, refs};
pub(crate) use kloudlite_git_pulls::{directory, merge_worker, pulls};
pub(crate) use kloudlite_git_app::App;
pub(crate) use kloudlite_git_git::{browse, protocol, proxy, ssh};
pub(crate) use kloudlite_git_registry as registry;
pub mod boot;
pub mod browse_api;
pub mod lanes;
pub mod listeners;
pub mod router;
```

  Root package: delete the three `[[bin]]` sections and `src/main.rs`, `src/bin/`, `src/http.rs`, `src/http/`; facade `src/lib.rs` drops its `pub mod http`/lane functions but keeps every `pub use` (tests still compile against it). Root `Cargo.toml` members += the three bin packages.
- [ ] **Step 5:** Verify — this is where the spec's binary wins are measured:

```sh
cargo build --release --locked
ls target/release/kloudlite-git target/release/kloudlite-git-api target/release/kloudlite-git-worker  # all three, same names
cargo test          # includes every_browse_route_is_routable in its new home
cargo clippy --workspace -- -D warnings
! cargo tree -p kloudlite-git-api-bin -e normal    | grep -qE 'russh|gix-pack|gix-odb|gix-traverse|imara-diff'
! cargo tree -p kloudlite-git-worker-bin -e normal | grep -qE 'russh|pgp|gix-pack|gix-traverse|imara-diff'
cargo tree -p kloudlite-git-worker-bin -e normal | grep -q slatedb    # worker keeps its store
```

- [ ] **Step 6:** Commit: `Move the three binaries into bin packages and split main and the router`

---

### Task 11: Delete the facade, re-home the tests, fix Dockerfile/CI/docs

**Tests decision, resolved by reading `tests/*.rs`:** the suites exercise the composed server (`kloudlite_git::App`, router, registry, protocol — imports span seven crates), so they stay together at the **workspace root**: the root package stops being a facade and becomes the integration-test host `kloudlite-git-tests` (near-empty lib, all dev-dependencies). This — rather than attaching to `bins/server` — keeps `./tests/registry_e2e.sh`, `./tests/compat-matrix.sh`, and `cargo test --test registry_blobs` working from the repo root exactly as CLAUDE.md documents them.

**Files:**
- Modify: root `Cargo.toml`, `src/lib.rs` (gutted), every `tests/*.rs`, `Dockerfile`, `.github/workflows/image.yml`, `CLAUDE.md`
- Delete: all remaining facade re-exports

**Steps:**

- [ ] **Step 1:** Root `Cargo.toml`: rename the package to `kloudlite-git-tests`, drop `[lib] name = "kloudlite_git"` (plain default lib), move every runtime dependency to `[dev-dependencies]` as path deps on all nine workspace crates (pulls with `features = ["check"]`, plus `kloudlite-git-server` for the router/boot helpers) plus the existing dev deps (`slatedb-common`, `tower`, `tempfile`, `async-trait`, `serde_json`, `mongodb`, `tokio`, `axum`, `futures`, `reqwest`, `russh` — prune to what the tests actually name). `src/lib.rs` shrinks to `//! Integration-test host; the code lives in crates/ and bins/.`
- [ ] **Step 2:** Rewrite test imports once, mechanically:

```sh
grep -rl 'kloudlite_git::' tests/ | xargs sed -i '' \
  -e 's/kloudlite_git::App/kloudlite_git_app::App/g' \
  -e 's/kloudlite_git::\(store\|pool\|ownership\|index\|cache\|events\|auth\|config\)/kloudlite_git_storage::\1/g' \
  -e 's/kloudlite_git::\(objects\|refs\)/kloudlite_git_gitbase::\1/g' \
  -e 's/kloudlite_git::\(pulls\|directory\|merge_worker\)/kloudlite_git_pulls::\1/g' \
  -e 's/kloudlite_git::\(protocol\|browse\|ssh\|gc\)/kloudlite_git_git::\1/g' \
  -e 's/kloudlite_git::proxy/kloudlite_git_core::peer/g' \
  -e 's/kloudlite_git::registry/kloudlite_git_registry::/g;s/registry::::/registry::/g' \
  -e 's/kloudlite_git::\(pktline\|jwt\|err\|hex\)/kloudlite_git_core::\1/g'
```

  then `cargo test --no-run` and fix the residue by hand (router/`http` references → `kloudlite_git_server::router`, `Trusted` → `kloudlite_git_core::httpx::Trusted`). `tests/common/` gets the same treatment.
- [ ] **Step 3:** Dockerfile: the two `COPY Cargo.toml Cargo.lock ./` + `COPY src ./src` pairs (planner and build stages, lines 17-18 and 26-27) become `COPY Cargo.toml Cargo.lock ./` + `COPY src ./src` + `COPY crates ./crates` + `COPY bins ./bins` + `COPY tests ./tests` (cargo-chef and `cargo build --release --locked` need every member manifest; `tests` is a member). The build command and the three `COPY --from=build /src/target/release/…` lines are unchanged — verify the built image stage still finds all three binaries.
- [ ] **Step 4:** CI `.github/workflows/image.yml`: `cargo clippy --lib -- -D warnings` → `cargo clippy --workspace -- -D warnings` (same bar: every lib and bin, test targets excluded; the pre-existing `--all-targets` lints stay grandfathered). `cargo test` line unchanged — at the workspace root it now runs every member's tests plus `tests/`.
- [ ] **Step 5:** CLAUDE.md Commands section: replace the clippy line with `cargo clippy --workspace -- -D warnings` and its parenthetical; note that `cargo test --test registry_blobs` still works from the root (tests host member); add one line naming the workspace layout (`crates/{core,storage,gitbase,pulls,app,git,registry,api}`, `bins/{server,api,worker}`).
- [ ] **Step 6:** Verify:

```sh
cargo test
cargo clippy --workspace -- -D warnings
cargo build --release --locked && ls target/release/kloudlite-git{,-api,-worker}
./tests/registry_e2e.sh || test $? -eq 77          # 77 = docker half skipped, not a pass — note which
grep -rn 'kloudlite_git::' tests/ src/ | wc -l        # 0 — the facade name is gone
docker build . -t kloudlite-git-workspace-check       # if a daemon is available; otherwise flag for CI
```

- [ ] **Step 7:** Commit: `Delete the facade, re-home the tests and update the build plumbing`

---

## Self-review notes (plan time)

- Spec coverage: all six lib crates land (plus the two decided additions `gitbase`, `app`); all eight >700-line splits are folded into their moving task (http.rs T10, upload.rs T7, lib.rs T6/T10, main.rs T10, pool.rs T3, directory.rs T5, pulls.rs T5, cache.rs T3).
- Extraction order deviates from spec prose (pulls before git/registry) because `App` sits between them; the facade strategy, gates, and end state are the spec's.
- The `cargo tree` assertions use `! … | grep -q` so "absent" is exit 0; run them exactly as written.
- If any step surfaces an item-level cycle the map missed, follow the spec's default: move the item to the lower crate or add a narrow trait in `core`; if a seam refuses both, stop and escalate rather than improvise.
