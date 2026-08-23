# Perf Fixes — Git Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the git-core findings of the 2026-08-24 perf review — one traversal per clone, streamed pack uploads, fewer clones/spawns/temp files on the fetch, push and merge paths — without changing any observable behavior.

**Architecture:** Every task is a perf refactor inside the file the finding names: `src/protocol/upload.rs` (traversal sharing, pack-writer threading, micro-allocations), `src/store.rs` (upload streaming, prune gating), `src/objects.rs` (in-memory pack indexing), `src/merge_worker.rs` (narrow fetch, batched rev-parses), with one signature ripple into `src/protocol/receive.rs` and `src/http/browse_api/merge.rs`. No new modules, no new dependencies. Existing tests are the safety net; new assertions are added only where a fix has observable behavior (the prune gate).

**Tech Stack:** Rust, tokio, gix-pack 0.73 / gix-object / gix-traverse 0.60 (vendored at `~/.cargo/registry/src/*/`), `object_store` via `slatedb::object_store` (`WriteMultipart`), the real `git` binary in the worker.

**Spec:** `docs/perf-review-2026-08-24.md` — P0-7, P0-8, the "Git core / worker" P1 block, and the git-core P2 batch rows.

## Global Constraints

- **Single-opener invariant**: one SlateDB per repo, only the routed node opens it. Nothing here may add a DB open on a new path.
- **The gix#2935 workaround stays** (`src/protocol/upload.rs` — merge commits get a whole-tree second pass) until GitoxideLabs/gitoxide#2935 is fixed upstream. Its `// ponytail:` comment is kept and updated when touched, never dropped.
- **`local()` vs `networked()` in `merge_worker.rs`**: a networked argv carries the peer secret; it must never be formatted into any error, log or panic. Only `local` argvs may be named in messages.
- Keep every `// ponytail:` marker edited near; when a fix removes a ceiling, remove/adjust its marker; when a fix adds a deliberate shortcut, add one.
- Comments explain WHY, never what; match the density of the file being edited.
- `cargo clippy --lib -- -D warnings` green; no NEW warnings in touched test files.
- `cargo test` green before every commit (iterate with the named `--test` file, full suite before committing).
- Integration tests that shell out to git start with `if !common::have_git() { … return; }` and use `#[tokio::test(flavor = "multi_thread")]`.
- Commit subjects imperative sentence case, no tool attribution, no Claude reference.

**What covers pack correctness** (verified — do not claim coverage beyond this):
- `tests/protocol.rs::incremental_fetch_sends_the_delta_not_the_snapshot` — builds full + incremental packs via `fetch_pack_bytes`, `git index-pack`s and `fsck --connectivity-only`s both.
- `tests/protocol.rs::a_wanted_tree_still_carries_its_contents_under_a_filter` — filtered pack completeness.
- `tests/protocol.rs::receive_then_fetch`, `gappy_pack_is_rejected`, `squash_and_merge_commit_land_the_right_shape` — push/connectivity/merge-object paths.
- `tests/http_e2e.rs::tags_peeling_and_atomic_push` — a REAL `git clone` (include-tag on by default) followed by `git fsck`; this is the include-tag regression net for Task 1.
- `tests/pulls.rs::worker_merges` — `merge_worker::run`/`check` end-to-end over the peer listener (Tasks 8–9).
- `tests/store.rs::open_repo_prunes_packs_the_index_no_longer_names`, `a_corrupt_pack_index_row_falls_back_to_the_listing`, and the `upload_pack_files` round trips at `tests/store.rs:99,134,676` (Tasks 5–6).

**Spec-sketch corrections found while reading the code:**
- P0-8: the double traversal is not only the `(None, None)` arm — a **filtered** fetch with `include-tag` also runs `commit_range` twice (`upload.rs:345` and `:374`). The shared range fixes both.
- P1 "hoist old_tips out of per-ref loop": `old_tips` is already hoisted (`receive.rs:254`); what is per-ref is the `old_tips.clone()` at `:265` forced by `reachable_set_hiding` taking `Vec`. The fix is slice parameters, not hoisting.
- P1 thread_limit: verified in vendored `gix-pack-0.73.0/src/data/output/entry/iter_from_counts.rs` — line 82 does `let db = db.clone();` per parallel chunk and the bound is `Find: crate::Find + Send + Clone + 'static` (line 60); the crate's own `Options::default()` is `thread_limit: None` (line 401). `None` is safe.
- P2 "double tag peel with include-tag" needs no separate change: it falls out of Task 1 (extra tag ids are appended to the shared range instead of being re-peeled by `peel_wants`).

---

### Task 1: One commit traversal per fetch (P0-8), merges captured from the walk

**Files:**
- Modify: `src/protocol/upload.rs` — `fetch` (`:341-392`), `commit_range` (`:745-756`), `write_pack` (`:803-841`), delete `merge_commits` (`:845-858`)
- (`src/gc.rs:145` keeps calling `write_pack(&odb, tips, Vec::new(), …)` unchanged — `write_pack` stays as a thin wrapper.)

**Context:** With `include-tag` (every plain clone), `fetch` runs `commit_range` to decide which tags ride along (`:345`), then the `(None, None)` arm calls `write_pack`, which re-runs the identical peel + traversal (`:815-819`). The filtered arm re-runs it too (`:374`). Separately, the gix#2935 workaround calls `merge_commits()` (`:832`), which re-decodes **every** commit in the range just to count parents — but `gix_traverse::commit::Simple` already yields `Info { id, parent_ids, .. }` (verified in vendored `gix-traverse-0.60.0/src/commit/mod.rs:59-67`), so parent counts are free during the first walk. The workaround itself and its ponytail comment stay — only the re-decode goes.

**Interfaces:**
- Produces: `struct Range { ids: Vec<ObjectId>, leaves: Vec<ObjectId>, merges: Vec<ObjectId> }`; `fn commit_range(odb, wants: Vec<ObjectId>, haves: Vec<ObjectId>) -> Result<Range>`; `fn write_pack_range(odb: &gix_odb::Handle, range: Range, out: &mut dyn Write, interrupt: &AtomicBool) -> Result<()>`.
- `write_pack` keeps its exact public signature (gc.rs depends on it) and becomes `commit_range` + `write_pack_range`.

- [ ] **Step 1: Reshape `commit_range`.** Replace the current function and its doc comment's tuple language with:

```rust
/// What one traversal of `wants`-minus-`haves` yields — computed once per fetch and shared
/// between the include-tag decision and the pack itself, because the walk is the expensive
/// half of serving a clone and used to run twice.
struct Range {
    /// Tags passed through on the way to the commits, then every commit in the range.
    ids: Vec<ObjectId>,
    /// Trees or blobs wanted directly (a promisor fetch) — kept apart because they are
    /// not filtered: the client asked for those exact objects.
    leaves: Vec<ObjectId>,
    /// The merge commits in the range, captured from the traversal's own parent list so the
    /// gix#2935 second pass (see `write_pack_range`) costs no re-decode of every commit.
    merges: Vec<ObjectId>,
}

/// The commits a fetch would send: reachable from `wants`, not from `haves`.
fn commit_range(
    odb: &gix_odb::Handle,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
) -> Result<Range> {
    let Peeled { commits, tags, leaves } = peel_wants(odb, &wants)?;
    let mut ids = tags;
    let mut merges = Vec::new();
    for info in gix_traverse::commit::Simple::new(commits, odb.clone()).hide(haves)? {
        let info = info?;
        if info.parent_ids.len() > 1 {
            merges.push(info.id);
        }
        ids.push(info.id);
    }
    Ok(Range { ids, leaves, merges })
}
```

- [ ] **Step 2: Split `write_pack`.** Its body after the traversal becomes `write_pack_range`; the traversal is `commit_range`. Move the existing block comment ("Commits carry only what they ADD…") and the `// ponytail:` gix#2935 comment onto `write_pack_range`, deleting only the sentence about the redundant re-decode (`merge_commits` is gone) and keeping the "Drop this when gix-pack is fixed" line:

```rust
/// Stream a pack containing everything reachable from `wants` and not from `haves`.
pub(crate) fn write_pack(
    odb: &gix_odb::Handle,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    let range = commit_range(odb, wants, haves)?;
    write_pack_range(odb, range, out, interrupt)
}

/// The pack for an already-computed [`Range`] — `fetch` computes the range once and shares it
/// with the include-tag decision; `write_pack` wraps the two for callers with plain wants.
fn write_pack_range(
    odb: &gix_odb::Handle,
    range: Range,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    // pack entries are copied straight out of mapped packs, which must not be unloaded meanwhile
    let mut odb = odb.clone();
    odb.prevent_pack_unload();
    let odb = &odb;
    // [existing "Commits carry only what they ADD…" comment]
    // [existing ponytail gix#2935 comment, minus the re-decode sentence]
    let Range { ids, mut leaves, merges } = range;
    leaves.extend(merges);
    let counts = counts_with_leaves(
        odb,
        ids,
        ObjectExpansion::TreeAdditionsComparedToAncestor,
        leaves,
        interrupt,
    )?;
    write_counts(odb, counts, out, interrupt)
}
```

Delete `merge_commits` entirely.

- [ ] **Step 3: Compute the range once in `fetch`.** After the `wanted-refs` section (`:332`) and before the include-tag block, add:

```rust
    // The traversal is the expensive half of a fetch, and with include-tag it used to run
    // twice — once to decide which tags ride along, once to build the pack. A shallow fetch
    // already has its commit list (the shallow walk decided it), so it computes no range.
    let range = match &shallow {
        None => Some(commit_range(&odb, wants.clone(), common.clone())?),
        Some(_) => None,
    };
```

Rewrite the include-tag `sending` set to read from it (replacing the `commit_range` call at `:343-346`):

```rust
        let sending: std::collections::HashSet<ObjectId> = match (&shallow, &range) {
            (Some(s), _) => s.commits.iter().copied().collect(),
            (None, Some(r)) => r.ids.iter().copied().collect(),
            (None, None) => unreachable!("range exists whenever the fetch is not shallow"),
        };
```

Rewrite the pack-arm match (`:364-392`) to consume the range (the arm comments stay where they are):

```rust
    let res = match (&shallow, filter, range) {
        (shallow, Some(f), range) => {
            let (commits, leaves) = match (shallow, range) {
                (Some(s), _) => (s.commits.clone(), Vec::new()),
                (None, Some(r)) => (r.ids, r.leaves),
                (None, None) => unreachable!("range exists whenever the fetch is not shallow"),
            };
            let mut ids = filtered_objects(&odb, &commits, f)?;
            ids.extend(extra_tags);
            let have: std::collections::HashSet<ObjectId> = common.into_iter().collect();
            ids.retain(|id| !have.contains(id));
            counts_with_leaves(&odb, ids, ObjectExpansion::AsIs, leaves, interrupt)
                .and_then(|c| write_counts(&odb, c, &mut band, interrupt))
        }
        (Some(s), None, _) => {
            let mut ids = s.commits.clone();
            ids.extend(extra_tags);
            write_pack_of(&odb, ids, common, &mut band, interrupt)
        }
        (None, None, Some(mut r)) => {
            r.ids.extend(extra_tags);
            write_pack_range(&odb, r, &mut band, interrupt)
        }
        (None, None, None) => unreachable!("range exists whenever the fetch is not shallow"),
    };
```

Note the equivalence being preserved: previously `extra_tags` were appended to `wants` and re-peeled by `write_pack`'s `peel_wants` (tag ids landed in the `tags` bucket, targets re-walked into an identical set); now the tag ids are appended to `r.ids` directly and counted under the same expansion. Same objects, one fewer peel — this is also the P2 "tags peeled twice with include-tag" fix. Pack entry ORDER shifts (tags appended instead of prepended); order is not part of the pack contract and the index-pack/fsck tests prove it.

- [ ] **Step 4: No new test — pure perf refactor.** The net: `tests/protocol.rs` (both pack-content tests index-pack + fsck the produced packs) and `tests/http_e2e.rs::tags_peeling_and_atomic_push` (a real clone exercises include-tag + the shared range, then fscks). Run:

```sh
cargo test --test protocol
cargo test --test http_e2e tags_peeling_and_atomic_push
```

- [ ] **Step 5: Full suite + clippy, then commit:**

```sh
cargo clippy --lib -- -D warnings && cargo test
git add src/protocol/upload.rs
git commit -m "Walk the commit graph once per fetch and capture merges during it"
```

---

### Task 2: Slice parameters for the reachability walks — no per-ref clones

**Files:**
- Modify: `src/protocol/upload.rs` — `reachable_set` (`:666-671`), `reachable_set_hiding` (`:711-739`), `ours` (`:645-654`)
- Modify: `src/protocol/receive.rs:262-288`

**Context:** `reachable_set_hiding` takes `tips: Vec` and `hide: Vec`, and at `:731` feeds the counter `ids.clone()` — a full copy of every tag+commit id in the push, per call. `receive.rs` then clones `old_tips` once **per updated ref** (`:265`) and once more at `:286`. `gix_traverse`'s `hide` takes `impl IntoIterator<Item = ObjectId>` (vendored `gix-traverse-0.60.0/src/commit/simple.rs:335`), so slices work all the way down.

**Interfaces:**
- Changes: `reachable_set(odb, tips: &[ObjectId])`, `reachable_set_hiding(odb, tips: &[ObjectId], hide: &[ObjectId], interrupt)`. Callers: `receive.rs:262,284`, `upload.rs::ours:651`, `upload.rs::reachable_set:670`.

- [ ] **Step 1: Change the signatures and kill the clone.** In `reachable_set_hiding`: `tips: &[ObjectId], hide: &[ObjectId]`; `peel_wants(odb, tips)?` already takes a slice; `.hide(hide.iter().copied())?`; and replace

```rust
        &mut ids.clone().into_iter().map(Ok),
```

with

```rust
        &mut ids.iter().copied().map(Ok),
```

(the `ids` Vec then moves into the result set as before). `reachable_set` becomes `reachable_set_hiding(odb, tips, &[], &AtomicBool::new(false))`; `ours` drops its `tips.to_vec()`.

- [ ] **Step 2: Update `receive.rs`:** `:262-267` passes `&[n]` — bind `let n_tip = [n];` and pass `&n_tip` (or `std::slice::from_ref(&n)`) and `&old_tips`; `:284-287` passes `&old_tips`. Both `.clone()` calls go.

- [ ] **Step 3: Tests + commit.** Behavior-identical; net is `tests/protocol.rs` (push connectivity: `gappy_pack_is_rejected`, `cannot_claim_sibling_object_as_tip`, `an_object_no_ref_reaches_is_refused`).

```sh
cargo test --test protocol
cargo clippy --lib -- -D warnings && cargo test
git add src/protocol/upload.rs src/protocol/receive.rs
git commit -m "Take reachability tips by slice instead of cloning per ref"
```

---

### Task 3: Unpin the pack writer's thread limit

**Files:**
- Modify: `src/protocol/upload.rs` — `write_counts` (`thread_limit: Some(1)` in the `iter_from_counts` options)

**Context:** Verified in vendored `gix-pack-0.73.0/src/data/output/entry/iter_from_counts.rs`: the parallel path clones the odb handle per chunk worker (`:82 let db = db.clone();`), the trait bound is `Find + Send + Clone + 'static` (`:60`), and the crate's own default is `thread_limit: None` (`:401`). The handle passed in already has `prevent_pack_unload()` set before cloning, which the clones inherit — so multi-threaded entry generation is safe here.

- [ ] **Step 1:** Change `thread_limit: Some(1)` to `thread_limit: None,` with the why:

```rust
            // gix clones the odb handle per worker (verified: iter_from_counts spawns with
            // `db.clone()`), and the prevent_pack_unload set above rides along on each clone —
            // so entry generation may use the machine. Some(1) was caution, not correctness.
            thread_limit: None,
```

Keep the `// ponytail: PackCopyAndBaseObjects…` marker line untouched.

- [ ] **Step 2:** Net: every pack-producing test.

```sh
cargo test --test protocol
cargo clippy --lib -- -D warnings && cargo test
git add src/protocol/upload.rs
git commit -m "Let pack entry generation use more than one thread"
```

---

### Task 4: upload.rs micro-batch — blob dedup order, tip set, shallow buffer

**Files:**
- Modify: `src/protocol/upload.rs` — `filtered_objects` (`:490-496`), `fetch` (`:296`), `shallow_walk` (`:600-615`)

Three same-shape one-liners, one commit.

- [ ] **Step 1: Dedup before the header lookup** (P1). In `filtered_objects`, a blob appearing in K trees pays K `try_header` calls because `seen.insert` runs second. Swap:

```rust
            } else if seen.insert(child) && keep_blob(odb, child, filter) {
```

A rejected blob now enters `seen` — deterministic (the filter's verdict for an id never changes within one fetch), so no blob that should travel is lost, and repeats of a rejected blob now skip the header lookup too.

- [ ] **Step 2: Tip membership as a set** (P2). `:296` runs `tips.contains(w)` per want — O(refs × wants). Above the `unknown` computation insert:

```rust
    let tip_set: std::collections::HashSet<&ObjectId> = tips.iter().collect();
    let unknown: Vec<ObjectId> = wants.iter().copied().filter(|w| !tip_set.contains(w)).collect();
```

(Leave `wants.contains(oid)` in the include-tag loop alone — `wants` is client-bounded and small.)

- [ ] **Step 3: Hoist the `since` probe buffer** (P2). In `shallow_walk`, `:606` allocates `&mut Vec::new()` per parent per commit. Declare `let mut pbuf = Vec::new();` next to the existing `let mut buf = Vec::new();` (`:574`) and use `&mut pbuf` in the `too_old` closure. (Two buffers, not one: `commit` still borrows `buf` while the closure runs.) The duplicate decode of the parent when it is later popped stays — removing it means threading decoded times through the queue, and `deepen-since` is a rare request; not worth the shape change.

- [ ] **Step 4: Tests + commit.**

```sh
cargo test --test protocol
cargo clippy --lib -- -D warnings && cargo test
git add src/protocol/upload.rs
git commit -m "Trim per-object costs in filtered, shallow and want handling"
```

---

### Task 5: Stream pack uploads instead of buffering them (P0-7)

**Files:**
- Modify: `src/store.rs` — `upload_pack_files` (`:504-520`)

**Context:** `tokio::fs::read` loads the whole `.pack` into RSS per concurrent push; the download path (`fetch_pack_file`, `:391-394`) already streams "never buffered" for exactly this reason. The registry's `pour` (`src/registry/uploads.rs:80-124`) is the house pattern: `os.put_multipart` + `WriteMultipart`, bounded in-flight parts, abort on error. `WriteMultipart` is already importable from `slatedb::object_store` (uploads.rs does), and the `SizedStore` wrapper in `src/lib.rs:873` forwards `put_multipart_opts`, so multipart works on every configured backend (file/mem included).

**Interfaces:** signature unchanged: `pub async fn upload_pack_files(&self, repo: &Repo, pack: &Path, idx: &Path) -> Result<()>`.

- [ ] **Step 1: Replace the body's read+put with a stream.**

```rust
    pub async fn upload_pack_files(&self, repo: &Repo, pack: &Path, idx: &Path) -> Result<()> {
        use slatedb::object_store::WriteMultipart;
        use tokio::io::AsyncReadExt;
        // pack first, idx last: a concurrent reader must never see an idx without its pack.
        for p in [pack, idx] {
            let fname = p
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| err("bad pack path"))?;
            let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), fname));
            // Streamed, never buffered — the download path (`fetch_pack_file`) streams for the
            // same reason: a whole pack in memory per concurrent push is RSS equal to the push.
            let size = tokio::fs::metadata(p).await?.len();
            let mut f = tokio::fs::File::open(p).await?;
            let mut w = WriteMultipart::new(self.os.put_multipart(&key).await?);
            // 5 MiB parts, at most 4 in flight: the same memory bound the registry's `pour` uses.
            let mut buf = vec![0u8; 5 * 1024 * 1024];
            let streamed = async {
                loop {
                    let n = f.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    w.wait_for_capacity(4).await.map_err(std::io::Error::other)?;
                    w.put(bytes::Bytes::copy_from_slice(&buf[..n]));
                }
                Ok::<_, std::io::Error>(())
            }
            .await;
            // A failed part must not leave the multipart dangling with the handle gone — same
            // rule as the registry's pour; leaked halves are the bucket's lifecycle rule's job.
            if let Err(e) = streamed {
                let _ = w.abort().await;
                return Err(e.into());
            }
            w.finish().await?;
            // record after the upload, so the index never names a file that is not there yet
            self.record_pack(&repo.owner, &repo.name, fname, size).await?;
        }
        Ok(())
    }
```

(`bytes` is already a transitive/first-party dep via the registry; if it is not in `[dependencies]`, use `slatedb::bytes::Bytes` or the `Bytes` re-export the registry imports — check `src/registry/uploads.rs`'s import line and copy it. The `PutPayload` import becomes unused in this file if nothing else uses it — remove it if so.)

- [ ] **Step 2: Tests.** Net: `tests/store.rs` upload round trips (`:99,134,676` — one of which asserts the recorded size against the listing), `tests/protocol.rs` push tests, and the `mem://`/`file://` stores both exercise multipart.

```sh
cargo test --test store
cargo test --test protocol receive_then_fetch
cargo clippy --lib -- -D warnings && cargo test
git add src/store.rs
git commit -m "Stream pack uploads through multipart instead of buffering whole files"
```

---

### Task 6: Prune stale packs with a set and an hourly gate

**Files:**
- Modify: `src/store.rs` — `prune_stale_packs` (`:126-154`)
- Modify: `tests/store.rs::open_repo_prunes_packs_the_index_no_longer_names` (`:704-738`)

**Context:** Runs on **every** `open_repo` and is O(dir entries × indexed packs) via `indexed.iter().any(...)`. Two fixes: a `HashSet` for the membership test, and a per-repo mtime gate so the directory scan runs at most once an hour — nothing it reclaims is younger than `STALE_AFTER` anyway, so a fresher scan can never find more.

- [ ] **Step 1: Update the test first** (this fix has observable behavior — the gate). The existing test opens the repo twice; the first open now writes the gate marker, so the second would skip the scan. Add, just before the second `open_repo` call at `:727`:

```rust
    // The prune is gated to once an hour per repo; clear the gate so this open scans.
    std::fs::remove_file(repo.pack_dir.join(".pruned")).unwrap();
```

And append a new assertion block at the end of the test proving the gate holds:

```rust
    // And with a fresh gate marker, a stale file survives the next open: the scan is skipped.
    let p = repo.pack_dir.join("pack-stale2.pack");
    std::fs::write(&p, b"x").unwrap();
    std::fs::File::options().write(true).open(&p).unwrap().set_modified(old).unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(repo.pack_dir.join("pack-stale2.pack").exists(), "a fresh .pruned gate skips the scan");
```

Run `cargo test --test store open_repo_prunes` — the new assertion fails (the file is pruned) until Step 2 lands.

- [ ] **Step 2: Implement.** At the top of `prune_stale_packs`, after `let now = …`:

```rust
    // At most one scan per STALE_AFTER per repo: open_repo is on every request's path, and a
    // fresher scan can never reclaim more — nothing it deletes is younger than STALE_AFTER.
    // The marker never matches the pack/temp shapes below, so it is never pruned itself.
    let marker = pack_dir.join(".pruned");
    let fresh = marker
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .is_some_and(|age| age < STALE_AFTER);
    if fresh {
        return Ok(());
    }
    std::fs::write(&marker, b"")?;
```

And replace the linear membership test:

```rust
    let indexed: std::collections::HashSet<&str> =
        indexed.iter().map(|(f, _)| f.as_str()).collect();
```

with the loop condition becoming `if !(is_pack || is_temp) || indexed.contains(name.as_str())`. The existing `// ponytail: an mtime guard, not a lock…` comment stays as is.

- [ ] **Step 3: Tests + commit.**

```sh
cargo test --test store
cargo clippy --lib -- -D warnings && cargo test
git add src/store.rs tests/store.rs
git commit -m "Gate the stale-pack scan hourly and test membership with a set"
```

---

### Task 7: Index merge objects from memory, and take patch content by value

**Files:**
- Modify: `src/objects.rs` — `index_objects` (`:137-171`), `write_object_pack` (`:180-223`), delete `incoming_pack_path` (`:130-134`) and `temp_name_tests` (`:450-460`); `apply_changes` (`:247-319`)
- Modify: `src/http/browse_api/merge.rs:333` (the one `apply_changes` caller)
- (Leave `store.rs`'s `incoming-` prune clause: it still reclaims leftovers from before this change; soften its doc reference from "removed by the code that writes them" to past tense for the merge shape.)

**Context (Cursor):** `index_objects` writes the in-memory pack to a temp file, re-opens it through a `BufReader`, and deletes it after — write, read, unlink for bytes that never left memory. `Bundle::write_to_directory` takes any `BufRead`; `std::io::Cursor<Vec<u8>>` is one. **Keep `Mode::Verify`** — the whole point of this path is that invented objects are validated like pushed ones.

**Context (by-value):** `apply_changes` clones every upserted file's content at `:295` (`staging.add(…, content.clone())`) because it borrows the map. Its only caller (`merge.rs:333`) already moves `changes` into a `spawn_blocking` and never touches it again — take the map by value and the clone disappears.

- [ ] **Step 1: `write_object_pack` returns the bytes.** Change its signature to `fn write_object_pack(objects: &[(gix_object::Kind, Vec<u8>)]) -> Result<Vec<u8>>`, drop the `path` parameter and the final `std::fs::write`, `Ok(out)` instead. Adjust its doc comment's last sentence ("…written directly rather than by putting the object somewhere first just to read it back" still holds — now it is not even put on disk).

- [ ] **Step 2: `index_objects` reads from a Cursor.** Replace the temp-file dance:

```rust
fn index_objects(
    repo: &Repo,
    objects: &[(gix_object::Kind, Vec<u8>)],
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    std::fs::create_dir_all(&repo.pack_dir)?;
    // The pack exists only in memory until Bundle writes the validated result: no temp file to
    // write, re-read and unlink, and nothing for a killed process to leave behind.
    let pack = write_object_pack(objects)?;
    let odb = repo.odb()?;
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut std::io::Cursor::new(pack),
        Some(&repo.pack_dir),
        &mut gix_features::progress::Discard,
        &AtomicBool::new(false),
        Some(odb),
        gix_pack::bundle::write::Options {
            thread_limit: None,
            iteration_mode: gix_pack::data::input::Mode::Verify,
            index_version: gix_pack::index::Version::V2,
            object_hash: gix_hash::Kind::Sha1,
            alloc_limit_bytes: Some(1024 * 1024 * 1024),
            compression: Default::default(),
        },
    )?;
    if let Some(k) = outcome.keep_path {
        let _ = std::fs::remove_file(k);
    }
    match (outcome.data_path, outcome.index_path) {
        (Some(data), Some(index)) => Ok((data, index)),
        _ => Err(err("the new objects produced no pack")),
    }
}
```

Delete `incoming_pack_path` and the `temp_name_tests` module (their reason to exist — colliding temp paths — is gone with the temp file). Trim the module doc's "one-object pack …" language only if it now misleads; the design story is unchanged.

- [ ] **Step 3: `apply_changes` by value.** Signature: `changes: std::collections::BTreeMap<String, Change>`; iterate `for (path, change) in changes` (by value), match `Change::Upsert { content, executable }` moving `content` into `staging.add(gix_object::Kind::Blob, content)?` — note `executable` and `path` uses adjust to owned values (`&path` in messages, `existing_kind` call unchanged). Caller `merge.rs:333`: drop the `&` on `&changes`. Doc comment untouched.

- [ ] **Step 4: Tests + commit.** Net: `tests/protocol.rs::squash_and_merge_commit_land_the_right_shape` (the merge-object path end to end), `tests/store.rs::open_repo_prunes…` (still passes — the prune clause remains), and the browse-api patch tests for `apply_changes` callers.

```sh
cargo test --test protocol squash_and_merge_commit_land_the_right_shape
cargo clippy --lib -- -D warnings && cargo test
git add src/objects.rs src/http/browse_api/merge.rs src/store.rs
git commit -m "Index invented objects from memory and move patch content instead of cloning"
```

---

### Task 8: Worker fetches only the branches the job names

**Files:**
- Modify: `src/merge_worker.rs` — `sync` (`:212-225`), `fetch` (`:227-239`)

**Context:** Every job mirror-fetches **all** branches (`+refs/heads/*:refs/heads/*`). What the code actually touches, verified by reading every consumer of the cache: `run` resolves `job.base`/`job.head`, `merge-base`/`merge-tree`/`commit-tree`/`rebase` all operate on those two tips and their history, `commit_tree` reads the head's log, and the push names only the base. `check` resolves and `merge-tree`s the same two. Nothing reads any other ref. So fetch just those two refspecs. `--prune --force` keep their meaning per-refspec (a rewritten base/head is still mirrored, a deleted one pruned); other cached branches go stale, which is harmless because nothing ever reads a ref a job did not name, and a later job naming it force-updates it. Branch names cannot smuggle refspec syntax: git refuses `*`, `:` and space in branch names, and these came in through ref creation.

**Interfaces:** `fn fetch(dir, url, secret, owner, base: &str, head: &str)`; `sync` passes `&job.base, &job.head`.

- [ ] **Step 1: Implement.**

```rust
fn fetch(dir: &Path, url: &str, secret: &str, owner: &str, base: &str, head: &str) -> Result<()> {
    // Only the two branches this job names: every consumer of the cache (`run`, `check`, the
    // rebase worktree, `commit_tree`'s log read) operates on the base and head tips and their
    // history, so mirroring every branch was pure transfer. Forced and pruned per refspec, the
    // cache still never keeps a rewritten history of THESE refs; other cached branches go stale
    // harmlessly — nothing reads a ref a job did not name, and the next job naming one forces it.
    let o = networked(
        dir,
        secret,
        owner,
        &[
            "fetch",
            "--quiet",
            "--prune",
            "--force",
            url,
            &format!("+refs/heads/{base}:refs/heads/{base}"),
            &format!("+refs/heads/{head}:refs/heads/{head}"),
        ],
    )?;
    if !o.status.success() {
        // The URL is safe to name — it is the caller's own configuration; the argv is not.
        return Err(crate::err(format!("fetching {url}: {}", stderr_tail(&o))));
    }
    Ok(())
}
```

In `sync`: `fetch(&dir, &url, secret, &job.owner, &job.base, &job.head)?;`. One wrinkle: a fetch of a **deleted** branch fails the whole command where the mirror refspec silently pruned it — and both `run` and `check` answer "one of the branches is gone" for that case anyway; the fetch error surfaces as `Err`, which re-announces instead of refusing. To keep the current outcome, the plan keeps it simple: a failed fetch whose stderr names a missing ref is still an `Err` (lease lapses, job re-announced, fails the same way next time — a loop). Avoid that: run the two refspecs as ONE fetch and, on failure, retry once with each refspec alone, ignoring a single-ref failure — no. **Simpler and correct:** keep one fetch; if it fails, fall back to the old mirror refspec once:

```rust
    if !o.status.success() {
        // A branch deleted upstream fails a named refspec where the mirror silently pruned it —
        // and `run`/`check` want to SEE the missing ref to refuse cleanly. One mirror fetch as
        // the fallback keeps that path; the fast path never pays for it.
        let o = networked(dir, secret, owner,
            &["fetch", "--quiet", "--prune", "--force", url, "+refs/heads/*:refs/heads/*"])?;
        if !o.status.success() {
            return Err(crate::err(format!("fetching {url}: {}", stderr_tail(&o))));
        }
    }
```

- [ ] **Step 2: Tests + commit.** Net: `tests/pulls.rs::worker_merges` (merge, check, and the branch-gone refusal all go through `sync`).

```sh
cargo test --test pulls
cargo clippy --lib -- -D warnings && cargo test
git add src/merge_worker.rs
git commit -m "Fetch only the job's base and head branches in the merge worker"
```

---

### Task 9: Batch the merge worker's rev-parses

**Files:**
- Modify: `src/merge_worker.rs` — `run` (`:326-376`), `check` (`:511-517`)

**Context:** `run` spawns git three times just to resolve ids: `oid(base)`, `oid(head)` (`:326-330`), and later `rev-parse {base}^{{tree}}` (`:370`). One `git rev-parse` resolves all three: it prints one line per rev and exits non-zero if any fails — and "which branch is gone" is not distinguished by the current message either. `check` spawns twice for existence (`:513-514`); one invocation answers both.

- [ ] **Step 1: `run`.** Replace the `oid` closure and its two calls with:

```rust
    // One spawn resolves all three ids a merge can need; rev-parse exits non-zero if any rev
    // is unresolvable, which is exactly the "a branch is gone" answer.
    let resolved = local(
        &dir,
        &[
            "rev-parse",
            &format!("refs/heads/{}^{{commit}}", job.base),
            &format!("refs/heads/{}^{{commit}}", job.head),
            &format!("refs/heads/{}^{{tree}}", job.base),
        ],
    )?;
    if !resolved.status.success() {
        return Ok(Outcome::refused("one of the branches is gone"));
    }
    let ids: Vec<String> = String::from_utf8_lossy(&resolved.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect();
    let [base_oid, head_oid, base_tree] = ids.as_slice() else {
        return Err(crate::err("rev-parse did not answer three ids"));
    };
    let (base_oid, head_oid) = (base_oid.clone(), head_oid.clone());
```

and in the squash/merge arm replace `must(&dir, &["rev-parse", &format!("{base_oid}^{{tree}}")])?` with `base_tree.clone()` (or compare against `*base_tree`). The base's tree is a pure function of `base_oid`, so resolving it up front changes nothing.

- [ ] **Step 2: `check`.** Replace the two `must(… "--verify", "--quiet" …)` calls with:

```rust
    if !local(&dir, &["rev-parse", &format!("{refs}^{{commit}}"), &format!("{head_ref}^{{commit}}")])?
        .status
        .success()
    {
        return Ok(unknown("one of the branches is gone".to_string()));
    }
```

(All argvs here are `local` — safe to exist; nothing formats a `networked` argv.)

- [ ] **Step 3: Tests + commit.** Net: `tests/pulls.rs::worker_merges` — including its branch-gone and already-merged cases.

```sh
cargo test --test pulls
cargo clippy --lib -- -D warnings && cargo test
git add src/merge_worker.rs
git commit -m "Resolve the merge worker's revs in one git invocation"
```

---

## Task order

1 → 2 → 3 → 4 (all `upload.rs`-centric, in dependency order), then 5 → 6 (`store.rs`), 7 (`objects.rs`), 8 → 9 (`merge_worker.rs`). Each task is independently committable; only 3 and 4 assume Task 1's reshaped `write_pack_range`/`fetch` (line positions, not interfaces).
