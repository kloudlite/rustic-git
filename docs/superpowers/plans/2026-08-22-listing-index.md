# Listing Index & Image Metadata Scope Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sub-project 1 of the repo-local data design: path-encoded visibility markers (`index/{public|private}/{repo|img}/{owner}/{name}`) written by owning nodes, image listing served from markers (killing the `manifest_stat` N+1 and the SlateDB-layout coupling), image delete simplified to marker-first, and a reconcile sweep in the GC worker.

**Architecture:** Markers are VIEWS: small object-store objects beside (never inside) the SlateDB prefix, holding cosmetic fields in a line-based body. Truth stays where it is today (repo visibility in the repo DB via `set_public`; image visibility in the image DB; repo description in Mongo until sub-project 2). Fail-closed flip ordering: remove the more permissive marker before writing the other; a reader seeing both treats the entry as private. Only owning-node code paths write markers.

**Tech Stack:** Rust, axum, `slatedb::object_store` (the `os` handle on `Store`), existing test harnesses (`tests/registry_*.rs`, `tests/browse_http.rs`, `mem://` object store).

**Spec:** `docs/superpowers/specs/2026-08-22-repo-local-data-design.md` (§2 View, §3 Listing reads, §6 Consistency model, §9 sub-project 1). Read it before implementing.

## Global Constraints

- `cargo test` green after every task; no NEW clippy warnings in touched files (~13 pre-existing `--all-targets` errors are ignored).
- House style: comments explain WHY; `// ponytail:` markers name ceiling + upgrade path; commit subjects imperative sentence case, no tool attribution.
- **Markers are never consulted for authorization** — only listings. Auth still flows through `registry::auth` / `open`.
- **Fail-closed invariant:** at no point in any flip sequence may a private repo/image be reachable under `index/public/…`. Remove-permissive-first is mandatory; both-markers-present reads as private.
- Marker writes happen AFTER the corresponding truth write, never before; a marker write failure must not fail the user's operation (log + continue; the reconcile sweep repairs).
- Do not touch PR/pull code, Mongo collections, or Redis Streams — those are sub-projects 2 and 3.
- Plan ruling: repo LISTING stays on Mongo this sub-project (truth incl. description is still there); image listing switches to markers with a fallback to the old directory listing for unmarked (pre-backfill) images.
- File-size rule: no task may leave a file it touches longer than it found it without a split; `browse_api.rs` (1118 lines) is split FIRST (Task 0) so every later task lands in a focused module. `api.rs` (2625) and `directory.rs` (1233) are deliberately NOT split here — sub-project 2 deletes their Mongo repo/pull halves, and splitting code about to be deleted is wasted motion; their split is a scheduled sub-project-2 task.

---

### Task 0: Split browse_api.rs into focused modules

**Files:**
- Create: `src/http/browse_api/mod.rs` (shared helpers + router: `hidden`, `open_ro`, `odb_json`, `parse_oid`, `internal`, `browse_routes()`), `src/http/browse_api/images.rs` (`images`, `imagetags`, `imagetagdelete`, `imagedelete`, `declared_size`, `ImageSummary`), `src/http/browse_api/repo.rs` (`api_refs`, `tree`, `api_tree_root`, `api_tree`, `api_blob`, `api_log`, `api_commit`, `api_files`, `api_lastmod`, `api_signature`), `src/http/browse_api/admin.rs` (`api_visibility`, `api_create`, `api_delete`, `api_protect`, `api_protections`), `src/http/browse_api/merge.rs` (`api_compare`, `api_merge`, `api_patch`)
- Delete: `src/http/browse_api.rs` (content moves; the file itself becomes the directory's `mod.rs`)

**Rules:** a PURE MOVE — no logic changes, no signature changes, no renames beyond `pub(super)` where cross-module visibility needs it. The router in `mod.rs` keeps registering every route exactly as before, so `every_browse_route_is_routable` (http.rs) is the seam's regression test. Keep each file's helpers with their only callers; helpers used by two modules stay in `mod.rs`.

- [ ] **Step 1:** Move the code per the mapping above. `cargo build --lib` until clean.
- [ ] **Step 2: Run** `cargo test` — full suite must be green with ZERO test edits (a pure move breaks nothing; if a test needs editing, the move wasn't pure — fix the move).
- [ ] **Step 3:** `cargo clippy --lib` — no new warnings; confirm no module exceeds ~450 lines (`wc -l src/http/browse_api/*.rs`).
- [ ] **Step 4: Commit** — `Split browse_api into focused modules`

---

### Task 1: The marker module

**Files:**
- Create: `src/index.rs`
- Modify: `src/lib.rs` (add `pub mod index;` next to the other module decls)
- Test: inline `#[cfg(test)]` in `src/index.rs` using `object_store::memory::InMemory` (grep how `src/pool.rs` tests build a `mem` store and reuse that helper pattern)

**Interfaces:**
- Produces (all `pub`, all taking `os: &Arc<dyn ObjectStore>` — same trait object as `Store.os`):
  - `pub enum Kind { Repo, Img }` with `fn seg(&self) -> &'static str` returning `"repo"` / `"img"`
  - `pub struct Marker { pub name: String, pub public: bool, pub created_by: String, pub created_ms: i64, pub description: String, pub manifests: u64, pub updated_ms: i64 }` (last two are 0 for code repos)
  - `pub fn path(public: bool, kind: Kind, owner: &str, name: &str) -> object_store::path::Path` → `index/{public|private}/{repo|img}/{owner}/{name}`
  - `pub async fn write(os, kind, owner, m: &Marker) -> crate::Result<()>` — fail-closed flip: if `m.public`, delete the private marker THEN put public; if private, delete the public marker THEN put private. (Delete-then-write in both directions is safe and simpler than ordering per-direction: the permissive marker is never present alongside a fresher other-side marker.)
  - `pub async fn remove(os, kind, owner, name) -> crate::Result<()>` — delete BOTH paths, public first (permissive first), NotFound tolerated
  - `pub async fn list(os, kind, owner, include_private: bool) -> crate::Result<Vec<Marker>>` — list `index/public/{kind}/{owner}/` (plain `list`, the markers are leaves), plus the private prefix when asked; fetch bodies concurrently (`futures::future::join_all`); a name present under BOTH prefixes is returned once, as private (fail closed); sorted by name
- Body encoding, line-based like `ownership::Entry` (`k=v` lines, description last so it may contain `=`):

```text
v=1
public=true
created_by=alice@example.com
created_ms=1755772800000
manifests=3
updated_ms=1755772900000
description=my thing
```

- [ ] **Step 1: Write the failing tests** (in `src/index.rs` `#[cfg(test)]`):

```rust
#[tokio::test]
async fn flip_never_leaves_a_public_marker_beside_private() {
    let os = mem_store();
    let m = marker("web", true);
    write(&os, Kind::Repo, "alice", &m).await.unwrap();
    write(&os, Kind::Repo, "alice", &marker("web", false)).await.unwrap();
    // public path must be gone, private present
    assert!(os.get(&path(true, Kind::Repo, "alice", "web")).await.is_err());
    assert!(os.get(&path(false, Kind::Repo, "alice", "web")).await.is_ok());
}

#[tokio::test]
async fn both_markers_read_as_private() {
    let os = mem_store();
    // simulate a crashed flip: both present
    os.put(&path(true, Kind::Repo, "a", "x"), body(&marker("x", true)).into()).await.unwrap();
    os.put(&path(false, Kind::Repo, "a", "x"), body(&marker("x", false)).into()).await.unwrap();
    let l = list(&os, Kind::Repo, "a", true).await.unwrap();
    assert_eq!(l.len(), 1);
    assert!(!l[0].public);
}

#[tokio::test]
async fn anonymous_listing_never_contains_private_names() {
    let os = mem_store();
    write(&os, Kind::Img, "a", &marker("secret", false)).await.unwrap();
    write(&os, Kind::Img, "a", &marker("open", true)).await.unwrap();
    let l = list(&os, Kind::Img, "a", false).await.unwrap();
    assert_eq!(l.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["open"]);
}

#[tokio::test]
async fn body_roundtrips_including_equals_in_description() {
    let m = Marker { name: "x".into(), public: true, created_by: "b".into(),
        created_ms: 5, description: "a=b=c".into(), manifests: 2, updated_ms: 9 };
    assert_eq!(decode("x", true, &body(&m)).unwrap().description, "a=b=c");
}
```

- [ ] **Step 2: Run** `cargo test --lib index` — expect FAIL (module missing).
- [ ] **Step 3: Implement** `src/index.rs` per the Interfaces block. Encode/decode are `fn body(&Marker) -> Vec<u8>` and `fn decode(name: &str, public: bool, bytes: &[u8]) -> crate::Result<Marker>`; unknown keys ignored (forward compat); missing keys default (`manifests=0`). `write` and `remove` treat `object_store::Error::NotFound` on delete as success. Doc comment on the module states the two policy rules verbatim: markers are views, never authorization; remove-permissive-first.
- [ ] **Step 4: Run** `cargo test --lib index` — PASS. `cargo build --lib` clean.
- [ ] **Step 5: Commit** — `git add src/index.rs src/lib.rs && git commit -m "Add the path-encoded listing index"`

---

### Task 2: Repo handlers write markers

**Files:**
- Modify: `src/http/browse_api/admin.rs` (post-Task-0 home of `api_create`, `api_visibility`, `api_delete`) — create/flip write markers after `set_public` succeeds; delete removes markers FIRST
- Modify: `src/store.rs` — `set_public` needs the repo's `created_by`? It does NOT: markers for repos this sub-project carry empty `description`/`created_by` (truth for those is Mongo until sub-project 2; the reconcile and sub-2 cutover fill them). State this in a comment.
- Test: `tests/browse_http.rs`

**Interfaces:**
- Consumes: `crate::index::{write, remove, Marker, Kind}` from Task 1.

- [ ] **Step 1: Failing test** in `tests/browse_http.rs` (reuse the existing harness helpers used by `protections_require_visibility`):

```rust
#[tokio::test]
async fn repo_lifecycle_maintains_markers() {
    // create private -> private marker exists, public absent
    // flip public   -> public marker exists, private absent
    // delete        -> both absent
    // assert via env's object store: e.store.os.get(&index::path(...))
}
```

Assert each of the three states with real `os.get` calls on both paths.

- [ ] **Step 2: Run** `cargo test --test browse_http repo_lifecycle_maintains_markers` — FAIL.
- [ ] **Step 3: Implement.**
  - `api_create`: after the repo exists (and after the optional `set_public`), `index::write(&app.store.os, Kind::Repo, &owner, &Marker { name, public, created_ms: now_ms, ..empty })`; on error `eprintln!` and continue (marker failure must not fail the create — Global Constraints).
  - `api_visibility`: after `set_public` returns Ok, same `index::write` with the new `public`.
  - `api_delete`: BEFORE deleting repo storage, `index::remove(...)` — gone from listings first; storage cleanup after (mirrors the image-delete design).
- [ ] **Step 4: Run** the test — PASS; full `cargo test` green.
- [ ] **Step 5: Commit** — `Write repo visibility markers from the owning node`

---

### Task 3: Image truth gains `meta`; visibility writes go through it

**Files:**
- Modify: `src/registry/store.rs` — add `image_meta`/`set_image_meta` around the existing `image_is_public` (~208) / `set_image_visibility` (~215); `set_image_visibility` also writes the marker
- Test: `tests/registry_http.rs`

**Interfaces:**
- Produces: `pub async fn set_image_visibility(&self, owner, name, public) -> Result<()>` (existing signature, now also maintains the marker); marker body’s `manifests`/`updated_ms` are refreshed by Task 4, not here.
- Note: heed the existing comment at `src/registry/store.rs:134` — callers use the designated visibility fn, never raw DB writes.

- [ ] **Step 1: Failing test:** flip an image public then private via the existing visibility route (grep `imagevisibility`/`set-image` route in `browse_api.rs`/api tier tests for the entry point used today; if the only path is the admin CLI, test `set_image_visibility` directly on a fixture store): assert marker moves prefix, fail-closed both-states checks as in Task 2.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement:** after the DB visibility write succeeds, `index::write(&self.os, Kind::Img, owner, &Marker { name, public, updated_ms: now, ..existing-or-default })` — read the existing marker first (either path) to preserve `manifests`/`created_*` fields; log-and-continue on marker failure. **Serialize the whole flip** (DB write + marker swap) under `self.keyed_lock(&format!("index/img/{owner}/{name}"))` so two racing flips cannot interleave the remove-then-write sequence (spec §6.5). Repo flips in Task 2's `api_visibility` get the same guard with key `index/repo/{owner}/{name}` — go back and add it there in this task if Task 2 landed without it.
- [ ] **Step 4: Run** — PASS; suite green.
- [ ] **Step 5: Commit** — `Move image visibility flips through the listing index`

---

### Task 4: Push refreshes the image marker

**Files:**
- Modify: `src/registry/manifests.rs` — in `put_manifest` (~49), where `touch_image` is called (~138): after success, refresh the marker
- Modify: `src/registry/store.rs` — add `pub async fn refresh_image_marker(&self, owner, name) -> Result<()>`: read current visibility (`image_is_public`, safe here — put_manifest already runs on the owning node), count manifests via the existing `manifest_stat`, write the marker with fresh `manifests`/`updated_ms`; first push creates the marker (private by default until a visibility flip says otherwise — fail closed)
- Test: `tests/registry_manifests.rs`

- [ ] **Step 1: Failing test:** push a manifest to a new image → marker exists under `index/private/img/...` with `manifests=1`; push a second → `manifests=2`, `updated_ms` advanced.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** per above; call `refresh_image_marker` from `put_manifest` after `touch_image`, log-and-continue on error. **Step 4: Run** — PASS, suite green.
- [ ] **Step 5: Commit** — `Refresh the image marker on every manifest push`

---

### Task 5: Image delete is marker-first

**Files:**
- Modify: `src/http/browse_api/images.rs` `imagedelete`: FIRST `index::remove(&app.store.os, Kind::Img, ...)`, THEN the existing manifest deletion and `delete_image` storage cleanup
- Modify: `src/registry/store.rs` `delete_image` doc comment (~248): the listing no longer answers from directory presence, so the "ghost image" paragraph is superseded — update it to say cleanup is now at leisure and a crash mid-cleanup leaves orphaned bytes for GC, not a visible phantom
- Test: `tests/registry_blobs.rs` or `tests/registry_http.rs`

- [ ] **Step 1: Failing test:** delete an image; assert both marker paths are gone AND (ordering) a delete whose storage-cleanup step is made to fail still leaves no marker — simulate by asserting marker removal happens even when the image has zero manifests and `delete_image`'s prefix listing is empty.
- [ ] **Step 2 – 4:** Run FAIL → implement → PASS, suite green.
- [ ] **Step 5: Commit** — `Remove the image marker before its storage on delete`

---

### Task 6: Image listing reads markers (with unmarked fallback)

**Files:**
- Modify: `src/http/browse_api/images.rs` `images` and `src/registry/routes.rs` `image_names` (~11) / `catalog` (~92)
- Test: `tests/registry_http.rs`

**Interfaces:**
- Consumes: `index::list(os, Kind::Img, owner, include_private)`.
- `ImageSummary` gains `pub public: bool` (it could not carry visibility before — this is goal 4 of the spec).

- [ ] **Step 1: Failing tests:**

```rust
// listed fields come from the marker, not manifest_stat
#[tokio::test] async fn image_listing_serves_marker_fields() { /* push 2 manifests, list, assert manifests==2, public==false */ }
// pre-backfill image: DB dir exists, no marker -> still listed (fallback), public=false
#[tokio::test] async fn unmarked_image_still_lists_via_fallback() { /* write repo/img/... db bytes only */ }
```

- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement:** `images` calls `index::list(.., include_private: true)` (the handler already requires `who == owner`); union with the old `list_dir_names("repo/img/{owner}/")` result — names present only in the directory listing (unmarked, pre-backfill) are appended with `public: false` and stats from `manifest_stat` (the fallback keeps the N+1 ONLY for unmarked images; say so in a comment with a `// ponytail: fallback dies with the backfill` marker). `catalog` (owner-scoped, same auth) switches to the same source. The handler keeps its any-node safety: markers + directory listing are both plain object-store reads.
- [ ] **Step 4: Run** — PASS; suite green; `every_browse_route_is_routable` untouched (no new routes).
- [ ] **Step 5: Commit** — `Serve image listings from the index`

---

### Task 7: Reconcile sweep in the GC worker

**Files:**
- Modify: `src/registry/gc.rs` (new `pub async fn reconcile_owner(store, owner) -> Result<usize>`), `src/bin/worker.rs` gc lane (~198, beside `sweep_stale_uploads`)
- Test: `tests/registry_uploads.rs` (same fixture style as `stale_upload_sessions_are_swept`)

**Scope (spec §6.4, structural half):** the worker must not open image DBs (fencing). This reconcile repairs what object-store reads can prove: (a) an image directory with NO marker → create one under **private** (fail closed) with stats from `manifest_stat`; (b) a marker whose image directory is GONE → remove it; (c) stale `manifests`/`updated_ms` in a marker body → rewrite from `manifest_stat`. Visibility drift is the OWNING NODE's repair duty — Task 7b — not this sweep's. State exactly this split in the fn's doc comment.

- [ ] **Step 1: Failing tests:** (a) unmarked image gains a private marker after reconcile; (b) marker for a deleted image is removed; (c) marker with `manifests=0` is corrected after a push happened.
- [ ] **Step 2 – 4:** FAIL → implement (keep-biased like `sweep_owner`: skip entries on read errors, never remove a marker on uncertainty) → PASS, suite green. Wire `reconcile_owner` into the worker's per-owner loop, log the repaired count.
- [ ] **Step 5: Commit** — `Reconcile listing markers in the GC sweep`

---

### Task 7b: Owner-side visibility reconcile

**Files:**
- Modify: `src/store.rs` or `src/registry/store.rs` (whichever holds `set_public`/`image_is_public` — one `pub async fn reconcile_marker(&self, owner, name, kind: index::Kind) -> Result<bool>` reading THIS node's own DB), `src/main.rs` `spawn_lease_tasks` (the renewal loop, ~line 247)
- Test: `tests/browse_http.rs` or `tests/registry_http.rs`

**Why (spec §6.4, visibility half):** a crash between a DB visibility write and its marker swap leaves DB and marker disagreeing, and the GC worker cannot see DB truth. The OWNING node can — reading its own repos is exactly what the single-writer invariant permits. This closes the only drift the structural sweep cannot, making §6's convergence claim true.

**Interfaces:**
- Produces: `reconcile_marker(owner, name, kind) -> Result<bool>` (true = a repair was written): read the DB's visibility (`is_public` for repos via the repo DB / `image_is_public` for images), read the marker (either path), rewrite via `index::write` when the sides disagree or the marker is missing; preserve existing body fields.
- Call sites: (1) on repo/image OPEN in the pool's open path — lazily heals any repo the moment it is touched; (2) a low-frequency lane in `spawn_lease_tasks`' renewal loop: every ~10 renewal beats, for each repo in `store.pool.warm_repos()`, call `reconcile_marker` — heals warm repos even if nobody flips them again. Both run only on the node that owns the repo (the only place these code paths execute).

- [ ] **Step 1: Failing test:** write a repo's DB visibility as public but plant a PRIVATE marker (simulating the crashed flip), call `reconcile_marker`, assert the marker moved to the public prefix; and the inverse direction.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** per Interfaces; log-and-continue on marker write failure. **Step 4: Run** — PASS, full suite green.
- [ ] **Step 5: Commit** — `Reconcile visibility markers from the owning node`

---

### Task 8: Drift ceiling documented

**Files:**
- Modify: `deploy/rustic-git.yaml` — comment on the worker Deployment naming the sweep cadence and the drift ceiling per spec §6: structural drift heals within one sweep period; visibility drift heals when the owning node next opens the repo or on its warm-repo reconcile lane
- Modify: `CLAUDE.md` — one line under the load-bearing rules: markers under `index/` are views, never authorization; owning nodes write them and reconcile their visibility, the GC worker reconciles their structure
- [ ] **Step 1:** Make both edits. **Step 2:** `kubectl apply --dry-run=client -f deploy/rustic-git.yaml` OK; `cargo test` green (no code change). **Step 3: Commit** — `Document the listing index and its drift ceiling`

---

### Task 9: Retire the superseded paths

**Files:**
- Modify: `src/http/browse_api/images.rs`, `src/registry/routes.rs`, `src/registry/store.rs`
- Test: existing suites (this task deletes, its tests are the ones that must still pass)

**What is dead after Task 6, and what is not:** `manifest_stat` survives (Task 4's marker refresh and Task 6's unmarked-image fallback both use it), and `list_dir_names` survives (same fallback + Task 7's reconcile walks directories). What CAN go: any now-unreferenced helper the listing switch orphaned — found by evidence, not memory.

- [ ] **Step 1:** `cargo build --lib 2>&1 | grep dead_code` and `grep -rn "image_names\|manifest_stat\|list_dir_names" src/` — for each hit, list its remaining callers. Delete every fn/struct/field with none. Do NOT delete the fallback or reconcile dependencies (they die with the backfill — confirm each survivor carries the `// ponytail: fallback dies with the backfill` marker from Task 6, adding it where missing).
- [ ] **Step 2:** Full `cargo test` green; `cargo clippy --lib` shows no dead-code warnings in registry/browse_api modules.
- [ ] **Step 3:** Append to the spec's §9 sub-project-2 list: "split `api.rs` and `directory.rs` as their Mongo repo/pull halves are deleted" — the deferred refactor stays tracked, not forgotten.
- [ ] **Step 4: Commit** — `Retire listing paths superseded by the index`

---

## Final verification

- [ ] `cargo test` — full suite green
- [ ] `cargo clippy --lib` — no new warnings in touched files
- [ ] The leak test (`anonymous_listing_never_contains_private_names`) and both-markers test pass — the fail-closed invariant holds
- [ ] Re-read spec §2/§3/§6: every sub-project-1 claim maps to a landed task (markers, flip ordering, marker-first delete, listing switch with fallback, reconcile, drift ceiling doc)
