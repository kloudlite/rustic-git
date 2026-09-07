# Package Versions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `spec.packages` accepts `attr@version` (devbox's grammar), the api resolves and locks it at write time, the agent installs the locked binary from cache.nixos.org without a source build, and the web and probe show and measure it.

**Architecture:** `packages.rs` grows an entry grammar; a new `packages/resolve.rs` resolves through a 24 h object-store cache → Nixhub (`search.devbox.sh`) → a mirrored nixpkgs-multiverse index, and writes `spec.locks` beside `spec.packages` (api is the only writer). The agent hashes locks into the profile, `nix copy`s each locked store path from cache.nixos.org, and evaluates one `buildEnv` with bare entries from the region pin plus the substituted paths, under `--max-jobs 0` so nothing compiles. Locks move only through `POST /v1/workspaces/{id}/packages/update`.

**Tech Stack:** Rust (axum, kube-rs, reqwest, serde), Nix CLI, Next.js web, the SLO probe.

**Spec:** `docs/superpowers/specs/2026-09-08-package-versions-design.md`

## Global Constraints

- Every command runs in the `dev` pod at `/work/src`; nothing runs on the laptop. Gate before each commit: `cargo clippy --workspace --all-targets -- -D warnings` plus the tests the task names; `CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml` regenerates `deploy/k3s/crds.yaml`, never hand-edit it.
- Grammar: entry is `attr` or `attr@version`; `version` ∈ `latest` | `N` | `N.N` | `N.N.N`, digits only; duplicates keyed on `attr`; `MAX_PACKAGES` 100, `MAX_ATTR_LEN` 64 over the whole entry. Bare `attr` stays on the region pin.
- The api is the only writer of `spec.locks`; it never writes a `@` entry without a lock and never writes a guessed lock (422 unknown with the three nearest versions, 503 index unavailable).
- Locks move only on `POST /v1/workspaces/{id}/packages/update`; edits re-resolve only entries whose string changed; clone/restore copy locks unchanged.
- Agent: `nix copy --from https://cache.nixos.org <storePath>` per lock; a miss is `PackagesReady=False/NotCached` in seconds, no source build (`--max-jobs 0`); `Unresolved` for a `@` entry without a lock.
- Only `x86_64-linux` is resolved. Comments explain WHY. Commit subjects imperative sentence case, no attribution.

---

### Task 1: Entry grammar

**Files:**
- Modify: `crates/workspaces/src/packages.rs`
- Test: same file, `mod tests`

**Interfaces:**
- Produces: `pub struct Entry { pub attr: String, pub version: Option<VersionReq> }`, `pub enum VersionReq { Latest, Prefix(String) }` (`Prefix("20")`, `Prefix("20.1")`, `Prefix("20.1.0")`), `pub fn parse_entry(s: &str) -> Result<Entry, PackageError>`, `PackageError::Version(String)`, `validate_list` unchanged in signature but now parses each entry and dedupes by `attr`; `pub fn bare(list: &[String]) -> Vec<String>` (entries without `@`), `pub fn pinned(list: &[String]) -> Vec<(String, Entry)>` (`(entry string, parsed)`).

- [ ] **Step 1: Failing tests**

```rust
    #[test]
    fn an_entry_is_an_attr_or_an_attr_at_a_version() {
        assert_eq!(parse_entry("jq").unwrap(), Entry { attr: "jq".into(), version: None });
        assert_eq!(parse_entry("nodejs@latest").unwrap().version, Some(VersionReq::Latest));
        assert_eq!(parse_entry("nodejs@20").unwrap().version, Some(VersionReq::Prefix("20".into())));
        assert_eq!(parse_entry("python3@3.11.4").unwrap().version, Some(VersionReq::Prefix("3.11.4".into())));
        for bad in ["nodejs@", "nodejs@^20", "nodejs@20.x", "nodejs@20-rc1", "nodejs@>=20", "nodejs@1.2.3.4", "@20", "a@b@c"] {
            assert!(matches!(parse_entry(bad), Err(PackageError::Version(_)) | Err(PackageError::Attr(_))), "{bad:?} must be refused");
        }
    }

    #[test]
    fn duplicates_are_keyed_on_the_attr_not_the_entry() {
        assert!(matches!(validate_list(&["nodejs".into(), "nodejs@20".into()]), Err(PackageError::Duplicate(a)) if a == "nodejs"));
        assert!(validate_list(&["nodejs@20".into(), "jq".into()]).is_ok());
    }

    #[test]
    fn bare_and_pinned_split_a_list() {
        let l = ["jq".to_string(), "nodejs@20".to_string(), "python3@latest".to_string()];
        assert_eq!(bare(&l), vec!["jq"]);
        assert_eq!(pinned(&l).iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), vec!["nodejs@20", "python3@latest"]);
    }
```

- [ ] **Step 2: Run** `cargo test -p kloudlite-workspaces --lib packages` → FAIL (missing items).

- [ ] **Step 3: Implement**

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionReq { Latest, Prefix(String) }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry { pub attr: String, pub version: Option<VersionReq> }

/// `attr` or `attr@version`. The version grammar is devbox's: `latest`, or one to three dotted
/// numbers, digits only — a prefix, resolved to the newest release under it. Anything with an
/// operator or a pre-release tag is refused: there is exactly one way to write a pin, so a list is
/// never ambiguous about what it asked for.
pub fn parse_entry(s: &str) -> Result<Entry, PackageError> {
    if s.len() > MAX_ATTR_LEN { return Err(PackageError::Attr(s.to_string())); }
    let Some((attr, version)) = s.split_once('@') else {
        validate_attr(s)?;
        return Ok(Entry { attr: s.to_string(), version: None });
    };
    validate_attr(attr)?;
    let req = if version == "latest" {
        VersionReq::Latest
    } else {
        let parts: Vec<&str> = version.split('.').collect();
        let ok = (1..=3).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !ok { return Err(PackageError::Version(s.to_string())); }
        VersionReq::Prefix(version.to_string())
    };
    Ok(Entry { attr: attr.to_string(), version: Some(req) })
}
```

`validate_list`: parse each, dedupe on `entry.attr`; keep `TooMany`. `PackageError::Version(e)` displays as `"{e:?} is not a version: use latest, N, N.N or N.N.N"`. `bare`/`pinned` as declared. Update the existing grammar test so `"a@b"`-style strings are judged by `parse_entry`.

- [ ] **Step 4: Run** the crate's tests → PASS. **Step 5: Commit** `git commit -m "Packages: an entry may name a version"`.

---

### Task 2: The lock in the CRD

**Files:**
- Modify: `crates/workspaces/src/crd/mod.rs`, `deploy/k3s/crds.yaml` (regenerated)
- Test: `crates/workspaces/tests/crd_yaml.rs` (existing drift test)

**Interfaces:**
- Produces: `pub struct Lock { pub entry: String, pub version: String, pub attr_path: String, pub rev: String, #[serde(default)] pub store_path: String, pub resolved_at: String, pub source: LockSource }`, `pub enum LockSource { Nixhub, Mirror }` (serde lowercase); `WorkspaceSpec.locks: Vec<Lock>` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`); `SnapshotState::Workspace { .., locks: Vec<Lock> }` (`#[serde(default)]`); `PackagesStatus.locked: Vec<LockedStatus>` with `pub struct LockedStatus { pub entry: String, pub version: String, pub rev: String }`; `pub const PKG_NOT_CACHED: &str = "NotCached"; pub const PKG_UNRESOLVED: &str = "Unresolved";`.

- [ ] Add the types with WHY docs (the lock is desired state because only the api may write spec and the agent has no internet; `store_path` empty means "mirror-resolved, evaluate the revision"). Add `locks: w.spec.locks.clone()` where `SnapshotState::Workspace` is built (~line 374). Regenerate `crds.yaml`; run `cargo test -p kloudlite-workspaces`; commit `"CRD: a workspace carries the locks its pinned packages resolved to"`.

---

### Task 3: The resolver

**Files:**
- Create: `crates/workspaces/src/packages/resolve.rs` (and turn `packages.rs` into `packages/mod.rs` with `pub mod resolve;`)
- Test: `resolve.rs` `mod tests` with a fake index

**Interfaces:**
- Produces:
  ```rust
  #[async_trait] pub trait Index: Send + Sync {
      /// One resolution, or Ok(None) for "no such package/version", Err for "unavailable".
      async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String>;
      /// Every known version string for `attr`, newest first (for the 422's "nearest").
      async fn versions(&self, attr: &str) -> Result<Vec<String>, String>;
  }
  pub struct Nixhub { pub client: reqwest::Client, pub base: String }   // base "https://search.devbox.sh"
  pub struct Mirror { pub os: Arc<dyn ObjectStore> }                      // reads index/pkgs/versions.json
  pub struct Resolver { pub cache: Arc<dyn ObjectStore>, pub nixhub: Arc<dyn Index>, pub mirror: Arc<dyn Index>, pub now: fn() -> DateTime<Utc> }
  pub enum Refusal { Unknown { entry: String, nearest: Vec<String> }, Unavailable }
  impl Resolver {
      pub async fn lock_one(&self, entry: &str) -> Result<Lock, Refusal>;
      /// Locks for `packages`: keep `prev` for entries whose string is unchanged, resolve the rest;
      /// `refresh_all` re-resolves every @ entry (the update route).
      pub async fn lock_all(&self, packages: &[String], prev: &[Lock], refresh_all: bool) -> Result<Vec<Lock>, Refusal>;
  }
  pub const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
  pub fn cache_key(attr: &str, version: &VersionReq) -> String  // "pkgs/x86_64-linux/{attr}/{version}"
  ```
- Consumes: Task 1's `parse_entry`/`pinned`, Task 2's `Lock`.

- [ ] **Step 1: Failing tests** (a `FakeIndex` implementing `Index` from a `HashMap<(attr, req), Option<Lock>>` and an `InMemory` object store):

```rust
    #[tokio::test]
    async fn a_hit_in_the_cache_asks_nobody() { /* put a fresh Lock JSON under cache_key; nixhub = FakeIndex that panics; lock_one returns the cached lock */ }
    #[tokio::test]
    async fn nixhub_answers_first_and_the_answer_is_cached() { /* nixhub Some(lock) → lock; second call served from cache (fake counts calls == 1) */ }
    #[tokio::test]
    async fn the_mirror_answers_when_nixhub_is_unavailable() { /* nixhub Err → mirror Some(lock with store_path "") → lock.source == Mirror */ }
    #[tokio::test]
    async fn unknown_everywhere_is_a_refusal_naming_the_nearest_versions() { /* both None; versions ["20.20.2","20.19.5","20.18.3","18.1.0"] → Refusal::Unknown{nearest: first three} */ }
    #[tokio::test]
    async fn unavailable_everywhere_with_no_cache_is_unavailable() { /* both Err → Refusal::Unavailable */ }
    #[tokio::test]
    async fn a_stale_cache_entry_is_ignored_but_a_fresh_one_wins() { /* resolved_at 25 h ago → re-resolve */ }
    #[tokio::test]
    async fn lock_all_keeps_untouched_locks_and_resolves_only_new_entries() { /* prev has nodejs@20; packages ["nodejs@20","jq","python3@3.11"] → nodejs lock unchanged (fake not called for it), python3 resolved; refresh_all=true → both resolved */ }
    #[test]
    fn mirror_picks_the_newest_release_under_the_prefix() { /* Mirror::pick(&versions_json, "nodejs", Prefix("20")) → "20.20.2" with rev/attr from the row */ }
```

- [ ] **Step 2: Run** `cargo test -p kloudlite-workspaces --lib packages::resolve` → FAIL.

- [ ] **Step 3: Implement**

`Nixhub::resolve`: `GET {base}/v2/resolve?name={attr}&version={v}` (`v` = `latest` or the prefix), 10 s timeout; 404 → `Ok(None)`; other non-2xx/timeout → `Err`; parse `{"version", "systems": {"x86_64-linux": {"flake_installable": {"ref": {"rev"}, "attr_path"}, "outputs": [{"path", "default"}]}}}` into a `Lock { source: Nixhub, store_path: default output path }`; a body without `x86_64-linux` is `Ok(None)`. `Nixhub::versions`: `GET {base}/v2/pkg?name={attr}` → `releases[].version` in order.

`Mirror`: reads `index/pkgs/versions.json` from the object store (Task 5 writes it): the nixpkgs-multiverse shape `{ "<attr>": [ { "version": "20.20.2", "rev": "<sha>", "attr_path": "nodejs_20" }, ... ] }` (normalise on write in Task 5, so the reader is one shape). `pick`: rows whose `version` equals the prefix or starts with `prefix + "."`, newest by numeric tuple; `Latest` = newest row. Missing file → `Err` (unavailable); attr absent → `Ok(None)`.

`Resolver::lock_one`: parse → if no version, error at the call site (callers only pass pinned entries) → cache get (`resolved_at` within TTL → return) → nixhub → mirror → refusal. On success, write the cache. `nearest`: from whichever index's `versions()` answers, sorted by shared-prefix length with the request then numeric distance, first three.

Comments explain the order (cache saves Nixhub a call per person per day; mirror second because it has no store path and costs an evaluation on the node).

- [ ] **Step 4: Run** the tests → PASS. **Step 5: Commit** `"Packages: resolve a version through the cache, Nixhub, then the mirror"`.

---

### Task 4: The api locks before it writes, and the update route

**Files:**
- Modify: `crates/workspaces/src/api/workspaces.rs` (`create_ws`, `patch_ws_packages`, `clone_ws`, `restore_ws`, new `update_ws_packages`), `crates/workspaces/src/api/mod.rs` (route, `ApiState.resolver: Option<Arc<Resolver>>`), `bins/api/src/main.rs` (construct the resolver from `KLOUDLITE_NIXHUB_URL` default `https://search.devbox.sh` and the object store)
- Test: `crates/workspaces/tests/api_packages.rs` (new; mirror `api_user.rs`'s harness with a `FakeIndex`)

**Interfaces:**
- Produces: `POST /v1/workspaces/{id}/packages/update` → 200 with the workspace doc; 422 body `{"error": "<entry> is not a version anyone published; nearest: a, b, c"}`; 503 `{"error": "the package index is unavailable; try again"}`. `ws_doc` gains `locks: [{entry, version, rev, source}]`.
- Consumes: Task 3's `Resolver`, Task 2's `Lock`.

- [ ] **Step 1: Failing HTTP tests**: create with `["jq", "nodejs@20"]` → 202 and the CR's `spec.locks` has one lock for `nodejs@20`; patch to `["jq", "nodejs@20", "python3@3.11"]` → only python3 resolved (fake call count); patch with `nodejs@0.0.99` → 422 with `nearest`; fake index unavailable + no cache → 503 and no CR write; `POST …/packages/update` re-resolves `nodejs@20` (fake returns a newer version) and the doc shows it; clone and restore carry `locks`; a `@` entry with `resolver: None` (dev without index) → 503.
- [ ] **Step 2:** run → FAIL. **Step 3:** implement: a helper `async fn lock_for(s: &ApiState, packages: &[String], prev: &[Lock], refresh: bool) -> Result<Vec<Lock>, Response>` used by all five handlers before the CR write; restore reads `frozen.locks`; clone copies the source's locks; `update_ws_packages` = `my_ws` + `lock_for(.., refresh=true)` + merge-patch `spec.locks`. The 422/503 mapping lives in one `fn refuse(r: Refusal) -> Response`.
- [ ] **Step 4:** `cargo test -p kloudlite-workspaces --test api_packages && cargo test -p kloudlite-workspaces && cargo build -p kloudlite-api-bin` → PASS. **Step 5:** commit `"Api: pinned packages are locked before the workspace is written"`.

---

### Task 5: The mirror beat

**Files:**
- Create: `crates/workspaces/src/packages/mirror_beat.rs`
- Modify: `bins/api/src/main.rs` (spawn in the `user` role next to `keys::run_beat`)
- Test: `mirror_beat.rs` `mod tests` (the normaliser, from a fixture excerpt)

**Interfaces:**
- Produces: `pub async fn run_beat(s: Arc<ApiState>)` (every 24 h, first tick immediately; fetches `https://raw.githubusercontent.com/fzakaria/nixpkgs-multiverse/main/index/versions.json`, normalises to the Task 3 shape, writes `index/pkgs/versions.json`; a fetch/parse failure logs `packages.mirror.failed` and keeps the previous file), `pub fn normalise(raw: &serde_json::Value) -> serde_json::Value`.

- [ ] Write the normaliser test from a 3-package excerpt of the real file (fetch it once in the pod to learn the exact shape; put the excerpt in the test), implement, spawn, gate, commit `"Packages: mirror the nixpkgs-multiverse index daily"`.

---

### Task 6: The agent installs locks from the cache

**Files:**
- Modify: `crates/workspaces/src/packages/mod.rs` (`hash`, `expression` take locks), `bins/agent/src/nix.rs` (`Nix::copy_from_cache`, `--max-jobs 0`), `bins/agent/src/controller/workspace.rs` (locks into the build, `NotCached`/`Unresolved`, `PackagesStatus.locked`)
- Test: `packages/mod.rs` tests, `bins/agent/src/controller/workspace.rs` tests (fake `Nix`)

**Interfaces:**
- Produces: `pub fn hash(pin: &str, bare: &[String], locks: &[Lock]) -> String` (covers each lock's `store_path`, or `rev#attr_path` when empty), `pub fn expression(pin: &str, bare: &[String], locks: &[Lock]) -> String` rendering `builtins.storePath "<path>"` for a cached lock and `(import (builtins.getFlake "github:NixOS/nixpkgs/<rev>") { }).<attr_path>` for a mirror lock; `Nix::copy_from_cache(&self, store_path: &str, timeout) -> Result<(), String>` running `nix copy --from https://cache.nixos.org <path>`, `Err` containing `"NotCached"` when the substituter has no such path; `RealNix::build` adds `--option substituters https://cache.nixos.org --max-jobs 0`.

- [ ] **Step 1: Failing tests**: `expression` renders both lock shapes and quotes nothing unvalidated (a `store_path` must match `^/nix/store/[a-z0-9]{32}-[A-Za-z0-9+._?=-]+$`, else `expression` returns `Err`); `hash` changes when a lock's `store_path` changes and not when bare entries reorder; controller: a `@` entry with no lock → `PackagesReady=False/Unresolved` and no build; a lock whose `copy_from_cache` fails `NotCached` → `PackagesReady=False/NotCached` naming the entry and version, no build; success → `status.packages.locked` lists `{entry, version, rev}`.
- [ ] **Step 2:** run → FAIL. **Step 3:** implement (the copies run before `build`, sequential, each under the build timeout; the profile hash gates everything as today; the existing `by-inputs` index keys on the new hash). **Step 4:** `cargo test -p kloudlite-agent-bin && cargo test -p kloudlite-workspaces` → PASS. **Step 5:** commit `"Agent: pinned packages come from cache.nixos.org, never from a source build"`.

---

### Task 7: The web

**Files:**
- Modify: `web/apps/web/src/lib/api.ts` (`locks` on the workspace doc; `updatePackages(token, id)`), `web/apps/web/src/components/app/workspace-list.tsx` (grammar check, resolved version chip `nodejs@20 → 20.20.2`, the two new reasons rendered like existing package errors, "Update pinned packages" action), `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/actions.ts` (or wherever `set_packages` lives — grep `packages` in the actions files)
- Test: bun test for the client-side entry regex (same grammar as Task 1); tsc, lint, bun test clean

- [ ] Implement; the 422 message renders verbatim; commit `"Web: pinned packages show what they resolved to, and can be updated"`.

---

### Task 8: The probe

**Files:**
- Modify: `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, `web/apps/web/src/lib/fixtures/superadmin.ts`, `bins/slo/src/stages/experience_ws.rs` (steps), `bins/slo/src/stages/experience.rs` (ids)
- Test: `cargo test -p kloudlite-workspaces slo`, `cargo test -p kloudlite-slo-bin`, `bun run test`

**Catalogue rows** (all `Suite::Hourly`, stage `"14 · Experience"`, feature `"Workspaces"`):

| id | sli | target |
| --- | --- | --- |
| `ws.packages.pin` | A workspace created with `jq@1.7` locks a 1.7.x, and `jq --version` in the pod says so | `p95(180_000)` |
| `ws.packages.pin.unknown` | `jq@0.0.99` is refused with the nearest versions named | `avail(99.9)` |
| `ws.packages.update` | `POST …/packages/update` answers with the lock unchanged for an exact pin | `avail(99.9)` |
| `ws.packages.pin.lockshape` | The lock names a nixpkgs revision and a store path | `avail(99.9)` |

Steps in `experience_ws.rs`, after `ws.packages.remove`: `pin` creates `<prefix>-pin` with `["jq@1.7"]`, polls ready, asserts `doc.locks[0].version` starts with `1.7.` and `rev` is 40 hex, execs `jq --version` in the pod via `crate::kube::exec` and asserts it contains the locked version; `pin.unknown` PATCHes `["jq@0.0.99"]` and asserts 422 with `nearest:` in the body; `update` POSTs the update route on the pin workspace and asserts 200 and the same `version`; `lockshape` is asserted from the same doc (its own id so a regression names itself). The pin workspace is parked (`lifecycle::park`) at the end, as `team.workspace` does. `NotCached` has no probe step: no version can be made reliably uncached without a source build, which the design forbids.

- [ ] Add rows to the three catalogues, the ids to `experience.rs`'s `IDS`, the steps; gates green; commit `"Probe: a pinned package resolves, locks, installs and updates"`.

---

### Task 9: Docs

**Files:** `CLAUDE.md` (one paragraph under "Workspaces and environments" after the profile paragraph: the grammar, where the lock lives, the resolver order, fail-closed rules, `NotCached`, the update route), `deploy/k3s/README.md` (nothing to apply beyond `crds.yaml`), `docs/migrations/2026-09-08-package-locks.md` (no migration: existing lists have no `@` entries; note `KLOUDLITE_NIXHUB_URL` and the mirror path).

- [ ] Write; commit `"Docs: package versions"`.

---

## Self-review

- Spec §1 → Task 1; §2 → Tasks 3, 4, 5; §3 → Task 6; §4 → Tasks 6 (status), 7; §5 failure table → Tasks 3 (cache/mirror/unknown/unavailable), 6 (`NotCached`, `Unresolved`, gcroot unchanged), 4 (no write on refusal); §6 out of scope honoured; the SLO section → Task 8 (four ids; `NotCached` explicitly not probed and why).
- Names consistent across tasks: `Entry`, `VersionReq`, `parse_entry`, `bare`, `pinned`, `Lock`, `LockSource`, `LockedStatus`, `Index`, `Nixhub`, `Mirror`, `Resolver`, `Refusal`, `lock_all`, `lock_for`, `copy_from_cache`, `PKG_NOT_CACHED`, `PKG_UNRESOLVED`, `updatePackages`.
