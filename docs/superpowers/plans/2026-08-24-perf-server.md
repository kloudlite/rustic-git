# Perf Fixes — Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the server-side findings of the 2026-08-24 perf review — lease-lane starvation, repo-open round-trip dedup, the mergeability sweep's wasted diff and per-PR opens, the merge-job scans that deserialize every PR ever, the imagetags/imagedelete IO shapes, and a batch of P2 micro-costs — without changing any observable behavior.

**Architecture:** Every task is an independent refactor inside the file the finding names; no new modules, no new dependencies, no new routes (nothing here touches `BROWSE_TAILS`). The only cross-task interface is Task 4's `pulls::with_merge_jobs`, consumed inside the same task. Pure perf refactors lean on the existing suite as the safety net; a task adds a test only where the change has its own observable behavior, and says so where it doesn't.

**Tech Stack:** Rust, tokio, axum 0.8, SlateDB (`slatedb::object_store`), gix, futures, serde_json.

**Spec:** `docs/perf-review-2026-08-24.md` — P0-1, P0-4, P0-5; P1 rows: merge-job scans, imagetags HEAD+GET, imagedelete delete_stream, `api_compare`/`perform` join; P2 batch. The forwarded-replay P1 row is deliberately excluded (see the end of this plan).

## Global Constraints

- **Single-opener invariant:** one node per repo/image database; nothing here may open a database outside the existing `open`/`open_repo`/`db_for` paths, and the routing middleware's shape is untouched.
- **Manifest bytes verbatim** (parse to read, never re-emit); **`Digest::parse` is the only path-segment→key gate**; **only the client DELETE and the GC sweep delete blobs** — Task 7 deletes *manifests* of an image being deleted, which was already this handler's job.
- The `local()`/`networked()` split in `merge_worker.rs` and the peer secret are out of scope and must stay out of any error text.
- Preserve every `// ponytail:` marker near edits; adjust one only where a task removes the ceiling it names; add one where a task cuts a corner with a known ceiling.
- Comments explain WHY, matched to `src/http.rs` density.
- `cargo clippy --lib -- -D warnings` green after every task; `cargo test` green before every commit (run the named `--test` file while iterating).
- Commit subjects imperative sentence case, no tool attribution, no Claude reference.
- Line numbers below were verified against the working tree at plan time — re-read the quoted anchor before editing; if it moved, follow the quote, not the number.

---

## P0

### Task 1: Lease renewal gets the loop to itself — lanes become their own tasks

**Files:**
- Modify: `src/main.rs` (`spawn_lease_tasks`, ~line 305-380)

**Context:** `renew_once` shares one loop with `reconcile_owned_markers` (every 10th beat), `check_owned_pulls` (every 20th), `announce_stranded_merges` (every 5th) and the checkpoint. The lanes sleep `RECONCILE_GAP` (200ms) per warm repo, so at `max_warm` = 64 one lane pass is ~13s — longer than `LEASE_TTL` (10s) — and renewal is skipped past the TTL: the leader drops the entries and `renew_once` evicts live databases. The checkpoint already got a deadline for exactly this failure; the lanes never did. The fix is one long-lived task per lane with its own `tokio::time::interval` — a lane is a sequential loop, so it can never overlap itself, and nothing can delay `renew_once` anymore. Beat-counting dies with the shared loop; the intervals keep today's effective periods: reconcile every 30s, pull checks every 60s, stranded merges every 15s.

**Interfaces:** none — `App::{reconcile_owned_markers, check_owned_pulls, announce_stranded_merges, renew_once}` keep their signatures.

- [ ] **Step 1: Rewrite `spawn_lease_tasks`.** Replace the single spawned loop (the block from `let a = app.clone();` through the checkpoint match) with:

```rust
fn spawn_lease_tasks(app: Arc<kloudlite_git::App>) {
    use kloudlite_git::ownership::{LEASE_TTL, RENEW_EVERY};
    /// How often the leader moves the ownership map's flush pointer. Matched to the collector's
    /// `min_age` so the WAL settles at about two of these rather than growing without bound.
    const CHECKPOINT_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
    /// Ceiling on one checkpoint. Generous for the work (a healthy one takes ~14ms) and short
    /// against the lease TTL it must never eat into.
    const CHECKPOINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    // Renewal runs ALONE. It used to share this loop with the reconcile/check/announce lanes,
    // and each lane sleeps RECONCILE_GAP per warm repo — at max_warm that is longer than
    // LEASE_TTL, so a node with enough warm repos skipped renewals and then evicted its own
    // live databases when the leader dropped them. The checkpoint got a deadline for exactly
    // this failure mode; the lanes get their own tasks below, so nothing can delay a beat.
    let a = app.clone();
    tokio::spawn(async move {
        let mut last_checkpoint = std::time::Instant::now();
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            // A renewal that cannot reach the leader is not fatal: the lease runs to its TTL and
            // the next beat is three seconds away. Missing every beat for a whole TTL is what lets
            // another node claim, which is the intended outcome.
            if let Err(e) = a.renew_once().await {
                eprintln!("renewing leases: {e}"); // ponytail: eprintln
            }
            // Move the ownership map's flush pointer so the WAL behind it can be reclaimed.
            // Timed off the CLOCK, and BOUNDED: an unbounded flush hung here once and the leader
            // stopped renewing leases entirely. Missing a checkpoint costs a few hundred
            // reclaimable objects; missing every renewal costs the fleet its routing.
            if last_checkpoint.elapsed() >= CHECKPOINT_EVERY {
                last_checkpoint = std::time::Instant::now();
                match tokio::time::timeout(CHECKPOINT_TIMEOUT, a.ownership.checkpoint()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => eprintln!("ownership checkpoint: {e}"), // ponytail: eprintln
                    Err(_) => eprintln!(
                        "ownership checkpoint: timed out after {}s; leases keep renewing", // ponytail: eprintln
                        CHECKPOINT_TIMEOUT.as_secs()
                    ),
                }
            }
        }
    });

    // The three backstop lanes, one task each so a slow pass delays only its own next pass —
    // a lane is a sequential loop and cannot overlap itself. Periods match what the old beat
    // arithmetic produced (10th/20th/5th beat at RENEW_EVERY = 3s); the per-lane rationale
    // lives on each lane's method in lib.rs.
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            a.reconcile_owned_markers().await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            a.check_owned_pulls().await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            a.announce_stranded_merges().await;
        }
    });

    if !app.is_leader() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LEASE_TTL).await;
            if let Err(e) = app.prune_once().await {
                eprintln!("pruning ownership: {e}"); // ponytail: eprintln
            }
        }
    });
}
```

  Move the drift-ceiling prose from the old per-beat comments onto the three spawns in one short comment each (e.g. "30s + 200ms/repo drift ceiling — see `reconcile_owned_markers`"), and delete the now-false "counting beats drifted" paragraph. `beat` and the `is_multiple_of` arms go away entirely.
- [ ] **Step 2: Testing note.** No new test: `spawn_lease_tasks` is main-binary wiring with no handle to assert on, and the lanes themselves are already exercised directly (`tests/browse_http.rs:451` drives `reconcile_owned_markers`; `tests/routing.rs:1148` drives `renew_once`). The test clock (`App::advance_clock`) skews lease *timestamps*, not tokio's timers, so it cannot drive these sleeps — an assertion-free spawn test would prove nothing. Relying on the existing suite is the plan.
- [ ] Run `cargo clippy --lib -- -D warnings && cargo clippy --bin kloudlite-git -- -D warnings` (main.rs is the bin) and `cargo test`.
- [ ] Commit: `git commit -am "Give lease renewal its own task so lanes cannot starve it"`

### Task 2: `index::read` fetches both visibility paths concurrently

**Files:**
- Modify: `src/index.rs:136-143` (`read`)

**Context:** A private repo/image pays two *sequential* object-store GETs on every marker read (`reconcile_marker` calls this on every `open_repo`):

```rust
pub async fn read(os: &Arc<dyn ObjectStore>, kind: Kind, owner: &str, name: &str) -> Option<Marker> {
    for public in [true, false] {
        if let Some(r) = fetch_one(os, path(public, kind, owner, name), public).await {
            return r.ok();
        }
    }
    None
}
```

**Interfaces:** signature unchanged; public-path preference unchanged (a marker can only legally exist on one path, but keep the tie-break anyway — behavior identical).

- [ ] **Step 1: Join the two fetches.**

```rust
pub async fn read(os: &Arc<dyn ObjectStore>, kind: Kind, owner: &str, name: &str) -> Option<Marker> {
    // Both paths at once: a private repo used to pay the public-path miss as a full round trip
    // before even asking for the path it lives on. Public still wins a (never-legal) tie.
    let (pu, pr) = tokio::join!(
        fetch_one(os, path(true, kind, owner, name), true),
        fetch_one(os, path(false, kind, owner, name), false),
    );
    pu.or(pr)?.ok()
}
```

  Check `fetch_one`'s exact return type (`Option<Result<Marker>>` per the current `r.ok()` usage) and keep the same None/Err semantics: found-but-unparseable on the public path must still answer `None` overall exactly as before — with the old loop, a `Some(Err)` on public returned `None` without trying private; `pu.or(pr)?` changes that to *try private*. That is a behavior improvement, not a regression, but if you want byte-identical behavior use `match pu { Some(r) => r.ok(), None => pr?.ok() }`. Use the byte-identical form — this is a perf task.
- [ ] Run `cargo test --test browse_http` (marker reconcile coverage) then the full suite.
- [ ] Commit: `git commit -am "Fetch both marker paths concurrently in index::read"`

### Task 3: Git-request visibility check opens the database once

**Files:**
- Modify: `src/refs.rs` (add `repo_public`, next to `repo_exists` at :327-337)
- Modify: `src/http.rs:672-673` (`open`, the `let public = ...` block)
- Modify: `src/store.rs:286-288` (`open_repo`, the two `create_dir_all`s)

**Context:** Every git request's `open` does:

```rust
let public = app.store.repo_exists(&owner, &name).await.unwrap_or(false)
    && app.store.is_public(&owner, &name).await.unwrap_or(false);
```

`repo_exists` = object-store `exists` probe + `db_for` + one DB get; `is_public` = `db_for` again + another get, all sequential — four awaits where one probe and two joined gets on one handle suffice. The spec's full sketch ("open once, read visibility from the handle" all the way through `open_repo`) would require removing `open_repo`'s own `repo_exists` re-check, which other callers (`perform`, `check`, browse `open_ro` path) rely on for their `Ok(None)`; reordering authentication around the fence to thread one handle through is not worth the risk. This task takes the dedup where it is free and leaves `open_repo`'s guard in place — that halves the round trips on the hot path (4-5 → 2-3) without touching auth ordering.

**Interfaces:**
- Produces: `Store::repo_public(&self, owner: &str, name: &str) -> Result<bool>` in `src/refs.rs` — `true` iff the repo exists AND is public; the exists probe still runs first so a mistyped path never conjures a database (the comment at the call site in `http.rs` says why and must survive).

- [ ] **Step 1: Add `repo_public` beside `repo_exists` in `src/refs.rs`:**

```rust
    /// Exists AND public, in one database open: the git front door asks both questions on every
    /// request, and asking them through `repo_exists` + `is_public` paid two `db_for` resolutions
    /// and three sequential gets. The object-store probe still runs first — `db_for` CREATES a
    /// database for whatever name it is handed, and this is reachable anonymously.
    pub async fn repo_public(&self, owner: &str, name: &str) -> Result<bool> {
        if !self.repo_db_exists(owner, name).await? {
            return Ok(false);
        }
        let db = self.db_for(owner, name).await?;
        let (exists, public) = tokio::join!(db.get(repo_key(owner, name)), db.get(PUBLIC_KEY));
        Ok(exists?.is_some() && public?.as_deref() == Some(b"1"))
    }
```

  (Verify `db.get` is `Send`-joinable on one `Arc<Db>` — it takes `&self`, so `tokio::join!` on two borrows of the same handle is fine.)
- [ ] **Step 2: Use it in `src/http.rs` `open`.** Replace the two-call `let public = ...` with:

```rust
    let public = app.store.repo_public(&owner, &name).await.unwrap_or(false);
```

  Keep the existing comment block above it ("Gated on `repo_exists`, which asks the object store rather than the pool...") but reword its first line to name `repo_public` — the WHY (don't conjure databases for mistyped paths) is unchanged and now lives inside the helper too.
- [ ] **Step 3: Join the two `create_dir_all`s in `open_repo`** (`src/store.rs:286-288`):

```rust
        let (a, b) = tokio::join!(
            tokio::fs::create_dir_all(&pack_dir),
            tokio::fs::create_dir_all(objects_dir.join("info")) // gix-odb wants a normal objects dir
        );
        a?;
        b?;
```

- [ ] **Step 4: Testing note.** No new test: `repo_public` is `repo_exists && is_public` by construction and every existing git-HTTP test (`tests/http_e2e.rs`, `tests/browse_http.rs` public/private matrix) exercises both truth-table halves. Run `cargo test --test http_e2e` then the full suite.
- [ ] Commit: `git commit -am "Answer exists-and-public with one database open per git request"`

### Task 4: Mergeability check drops the discarded diff and the per-PR repo open

**Files:**
- Modify: `src/pulls.rs:393-482` (`check`), `:511-521` (`check_repo`)

**Context:** Two wastes in the background sweep. (1) `check` calls `browse::compare(&odb, b, h, 1)` but reads only `merge_base`/`fast_forward`/`head` from the result — `compare` also walks up to `max_commits` of history *and unconditionally builds a full unified diff from the merge base to head* (`src/browse.rs:487-524`), all discarded. `browse::merge_base(odb, base, head, budget)` answers everything `check` actually uses. (2) `check` runs `store.open_repo()` — marker reconcile, pack sync, dir stats — and `check_repo` calls `check` once per open PR, so a 25-PR repo re-opens itself 25 times per sweep.

**Interfaces:**
- Produces: `async fn check_with(store, owner, name, repo: &crate::store::Repo, number: i64) -> Result<Checked>` (private to the module or `pub(crate)`); `pub async fn check(...)` keeps its signature and becomes open-then-delegate, so the routed `pulls/{n}/check` endpoint is untouched.
- Consumes: `browse::merge_base(odb: &gix_odb::Handle, a: ObjectId, b: ObjectId, budget: usize) -> Option<ObjectId>` (`src/browse.rs:442` — read its exact signature first; if it returns `Result<Option<_>>`, adapt the match accordingly).

- [ ] **Step 1: Split `check`.** New shape:

```rust
pub async fn check(store: &Store, owner: &str, name: &str, number: i64) -> Result<Checked> {
    let Some(repo) = store.open_repo(owner, name).await? else { return Ok(Checked::Unchanged) };
    check_with(store, owner, name, &repo, number).await
}

/// The check itself, against a repo the caller already opened — `check_repo` sweeps many
/// changes and must not pay `open_repo` (marker reconcile, pack sync) once per change.
async fn check_with(store: &Store, owner: &str, name: &str, repo: &crate::store::Repo, number: i64) -> Result<Checked> {
    let db = store.db_for(owner, name).await?;
    let Some(pr) = get(&db, number).await? else { return Ok(Checked::Unchanged) };
    if pr.state != PullState::Open {
        return Ok(Checked::Unchanged);
    }
    // ... body unchanged from here, except the compare block below ...
```

  Note the get/open order flips (the old code fetched the PR before opening the repo). For the routed single-check that means a closed/missing PR now pays one `open_repo` it used to skip — negligible against the sweep's ×N savings, and `check_repo` already filtered to open PRs. Move the existing `let Some(repo) = ...` line out; `repo` is now a parameter (clone it for the `spawn_blocking` move: `let repo = repo.clone();`).
- [ ] **Step 2: Replace the `compare` call.** Current block:

```rust
            let cmp = tokio::task::spawn_blocking(move || {
                repo.odb().and_then(|odb| crate::browse::compare(&odb, b, h, 1))
            })
```

  becomes a merge-base-only walk (adjust to `merge_base`'s real signature — read `src/browse.rs:442` first; `compare` calls it as `merge_base(odb, base, head, BUDGET)` with `BUDGET = 50_000`, returning the `Option<ObjectId>` it stores as `mb`):

```rust
            // `merge_base` alone: the old `compare(_, _, _, 1)` also built a full unified diff
            // from the merge base and walked commit history, all of it discarded — the sweep
            // needs the ancestry verdict, nothing else.
            const BUDGET: usize = 50_000;
            let repo2 = repo.clone();
            let mb = tokio::task::spawn_blocking(move || {
                repo2.odb().map(|odb| crate::browse::merge_base(&odb, b, h, BUDGET))
            })
            .await
            .map_err(|e| crate::err(format!("comparing: {e}")))??;
            let fast_forward = mb == Some(b);
            let (state, ff, detail) = match (&mb, fast_forward) {
                (Some(_), true) => (MergeableState::Clean, true, None),
                (Some(m), _) if *m == h => (
                    MergeableState::Behind,
                    false,
                    Some(format!("this branch is already in {}", pr.base)),
                ),
                (None, _) => (
                    MergeableState::Dirty,
                    false,
                    Some("these branches share no history".to_string()),
                ),
                (Some(_), false) => {
                    deep = true;
                    (MergeableState::Unknown, false, Some("checking…".to_string()))
                }
            };
            Mergeability {
                state,
                base_oid: now_base.clone(),
                head_oid: now_head.clone(),
                ...
```

  The old code compared `mb == &cmp.head` on *hex strings* and stored `cmp.base`/`cmp.head` (hex of the same oids as `now_base`/`now_head`) — comparing `ObjectId`s directly and reusing `now_base`/`now_head` is the same values without the re-hexing. If `merge_base` propagates errors (`Result`), map them exactly as `compare`'s were.
- [ ] **Step 3: Hoist the open in `check_repo`:**

```rust
pub async fn check_repo(store: &Store, owner: &str, name: &str) -> Result<Vec<Deep>> {
    let db = store.db_for(owner, name).await?;
    // One `open_repo` for the whole sweep, not one per change: it does marker reconcile and a
    // pack sync, and paying that per PR was most of the background lane's cost.
    let Some(repo) = store.open_repo(owner, name).await? else { return Ok(Vec::new()) };
    let mut deep = Vec::new();
    for pr in open_only(&db, CHECK_LIMIT).await? {
        if let Checked::Deep(d) = check_with(store, owner, name, &repo, pr.number).await? {
            deep.push(d);
        }
    }
    Ok(deep)
}
```

- [ ] **Step 4: Tests.** Behavior is unchanged and richly covered: `tests/pulls.rs` asserts the Clean/Behind/Dirty/Unknown verdicts end to end (the `worker_merges` suite drives `check_repo` via the lanes). No new test — the existing verdict assertions ARE the regression net for the merge-base-only rewrite. Run `cargo test --test pulls`, then the full suite.
- [ ] Commit: `git commit -am "Compute mergeability from the merge base alone and open the repo once per sweep"`

---

## P1

### Task 5: Merge-job scans stop deserializing every PR ever

**Files:**
- Modify: `src/pulls.rs:235-242` (beside `list`), `:567-591` (`claim_merge`), `:649-673` (`stranded_merges`)

**Context:** `claim_merge` and `stranded_merges` both run `list(db)` — full scan + `serde_json` deserialize of every PR the repo ever had — on the 15s/claim beats, and both then keep only PRs *with a merge job*. `PullRequest.merge` is `#[serde(skip_serializing_if = "Option::is_none")]`, so a row without a job contains no `"merge":` key at all: a raw-bytes substring prefilter skips the deserialize for the (vastly dominant) jobless rows. A comment body could contain the literal `"merge":` — that is only a false *positive*, which deserializes one extra row and is then filtered by `takeable` exactly as today. The spec's alternative (a maintained `merge/queued/{n}` index key) touches every writer of merge state for the same win — the prefilter is the simpler correct shape.

**Interfaces:**
- Produces: `pub async fn with_merge_jobs(db: &Db) -> Result<Vec<PullRequest>>`.

- [ ] **Step 1: Write the test** (in `src/pulls.rs`'s existing `#[cfg(test)] mod tests` if one exists — check; otherwise in `tests/pulls.rs` using its fixture pattern — copy how a sibling test there builds a `Store`/db and `put`s PRs):

```rust
#[tokio::test]
async fn with_merge_jobs_skips_jobless_rows_and_survives_a_decoy() {
    // fixture: an in-memory store/db exactly as the sibling tests build one
    // pr 1: no merge job; pr 2: merge job; pr 3: no job but a comment whose body
    // contains the literal "merge": — a false positive the prefilter must tolerate.
    // assert: with_merge_jobs returns exactly [pr 2] (pr 3 deserializes fine and is
    // dropped by the is_some filter).
}
```

  Fill the fixture in from a neighboring test; the assertions above are the contract. Run it, see it fail (function absent).
- [ ] **Step 2: Implement**, beside `list`:

```rust
/// The changes that carry a merge job, without deserializing the ones that don't.
///
/// `merge` is `skip_serializing_if = Option::is_none`, so a jobless row has no `"merge":` key
/// in its bytes at all — and jobless (closed, merged, never-asked) rows are the unbounded
/// majority on the 15s announce beat. A comment body containing the literal is only a false
/// positive: it deserializes one extra row, which the `is_some` filter then drops.
pub async fn with_merge_jobs(db: &Db) -> Result<Vec<PullRequest>> {
    let mut it = db.scan_prefix(PULL_PREFIX.as_bytes(), ..).await?;
    let mut out = Vec::new();
    while let Some(kv) = it.next().await? {
        if !kv.value.windows(8).any(|w| w == b"\"merge\":") {
            continue;
        }
        let pr: PullRequest = serde_json::from_slice(&kv.value)?;
        if pr.merge.is_some() {
            out.push(pr);
        }
    }
    Ok(out)
}
```

  (`windows` is stdlib and fine at row sizes; no memchr dependency.)
- [ ] **Step 3: Switch both scanners.** In `claim_merge` replace `for mut pr in list(&db).await?` with `for mut pr in with_merge_jobs(&db).await?` — `takeable` already returns false for `merge: None`. In `stranded_merges` replace `list(&db)` the same way; its filter already requires `pr.merge.as_ref()`.
- [ ] Run `cargo test --test pulls` and the new test, then the full suite.
- [ ] Commit: `git commit -am "Prefilter merge-job scans on raw bytes before deserializing"`

### Task 6: `imagetags` reads each manifest once instead of HEAD + GET

**Files:**
- Modify: `src/http/browse_api/images.rs:114-127` (the per-tag closure)

**Context:** Per tag: `os.head(&path)` for size/mtime, then `os.get(&path)` for the declared-size sum — but `GetResult.meta` is the same `ObjectMeta` the HEAD returns. 100 tags = 100 avoidable round trips (the loop is already `buffered(8)` — keep that).

- [ ] **Step 1: Fold HEAD into GET.** Replace:

```rust
                let path = crate::registry::store::manifest_path(&owner, &name, &d);
                let meta = app.store.os.head(&path).await.ok();
                let size = meta.as_ref().map(|m| m.size).unwrap_or(0);
                let pushed_ms = meta.as_ref().map(|m| m.last_modified.timestamp_millis());
                // Reading the manifest to ADD UP its declared sizes — never to re-emit it. ...
                let bytes = match app.store.os.get(&path).await {
                    Ok(r) => r.bytes().await.map(|b| declared_size(&b)).unwrap_or(0),
                    Err(_) => 0,
                };
```

  with:

```rust
                let path = crate::registry::store::manifest_path(&owner, &name, &d);
                // One GET: its `meta` is the same ObjectMeta a HEAD returns, and this ran
                // HEAD + GET on the same key per tag. Reading the manifest to ADD UP its
                // declared sizes — never to re-emit it. The digest is over the exact bytes,
                // so nothing here may write a manifest back.
                let (size, pushed_ms, bytes) = match app.store.os.get(&path).await {
                    Ok(r) => {
                        let (size, pushed) = (r.meta.size, r.meta.last_modified.timestamp_millis());
                        (size, Some(pushed), r.bytes().await.map(|b| declared_size(&b)).unwrap_or(0))
                    }
                    Err(_) => (0, None, 0),
                };
```

  Behavior nuance: today a HEAD that succeeds but a GET that fails gives `size` with `bytes = 0`; now both come from the one GET, so a failing GET zeroes both — the same "best effort per tag" contract the handler already had.
- [ ] Run `cargo test --test browse_http` (or whichever `tests/*.rs` covers `imagetags` — `grep -rl imagetags tests/`), then the full suite.
- [ ] Commit: `git commit -am "Read each manifest once in the imagetags listing"`

### Task 7: `imagedelete` deletes manifests through `delete_stream`

**Files:**
- Modify: `src/http/browse_api/images.rs:200-215` (the list-collect-then-serial-delete block)

**Context:** The handler lists the whole `manifests/{owner}/{name}` prefix into a Vec, then deletes one object per await. `ObjectStore::delete_stream` takes the listing stream directly and the backend batches/parallelizes. This deletes *manifests of an image being torn down* — squarely inside this handler's existing charter; the blob rule is untouched.

- [ ] **Step 1: Replace the loop.**

```rust
    use slatedb::object_store::ObjectStore;
    use futures::{StreamExt, TryStreamExt};
    let prefix = slatedb::object_store::path::Path::from(format!("manifests/{owner}/{name}"));
    // `delete_stream` feeds deletes straight off the listing — the collect-then-delete loop
    // paid one round trip per manifest. NotFound per object is tolerated: another delete of
    // the same image racing this one changes nothing about the end state.
    let doomed = app.store.os.list(Some(&prefix)).map_ok(|m| m.location).boxed();
    let mut results = app.store.os.delete_stream(doomed);
    while let Some(r) = results.next().await {
        match r {
            Ok(_) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
            Err(e) => return internal(e.into()),
        }
    }
```

  The old code failed on the first list or delete error before touching the DB — the new shape preserves that (DB-side `delete_image` still runs only after the stream drains clean). `delete_stream` exists on this `object_store` version (`src/lib.rs:885` already delegates it in the test wrapper).
- [ ] **Step 2:** Spec also names `src/registry/store.rs:373-380` and `uploads.rs:485-492` for the same shape — those belong to the registry plan; do NOT touch them here (single-writer of that file's plan). This task is the browse-api site only.
- [ ] Run the images tests (`grep -rl imagedelete tests/`), then the full suite.
- [ ] Commit: `git commit -am "Delete image manifests through delete_stream"`

### Task 8: `api_compare` and `perform` resolve their two refs concurrently

**Files:**
- Modify: `src/http/browse_api/merge.rs:39-43` (`api_compare`) and `:112-116` (`perform`)

**Context:** Both sites await two `get_ref`s sequentially inside a tuple *pattern*, which does not parallelize:

```rust
    let (base_oid, head_oid) = match (
        app.store.get_ref(&repo, &base_ref).await,
        app.store.get_ref(&repo, &head_ref).await,
    ) {
```

- [ ] **Step 1:** At both sites, wrap in `tokio::join!` — the match arms stay identical:

```rust
    let (base_oid, head_oid) = match tokio::join!(
        app.store.get_ref(&repo, &base_ref),
        app.store.get_ref(&repo, &head_ref),
    ) {
```

  (`tokio::join!` yields the same `(Result, Result)` tuple; no arm changes.)
- [ ] Run `cargo test --test pulls` and the merge-API tests, then the full suite. No new test — two awaits becoming concurrent has no observable behavior.
- [ ] Commit: `git commit -am "Resolve base and head refs concurrently in compare and merge"`

---

## P2

### Task 9: Micro-cost batch — pair parse, single Basic decode, keyed-lock threshold, static event keys, GC copy

One commit; each edit is a few lines and behavior-neutral. Run the full suite once at the end.

**Files:**
- Modify: `src/protocol/mod.rs:16` (beside `parse_repo_path`), `src/http.rs` (`repo_of` ~:246-268, `open` ~:648-663), `src/auth.rs:198-232`, `src/store.rs:39-45`, `src/events.rs:71-105`, `src/cache.rs:253` (`xadd`), `src/registry/gc.rs:20-22`

- [ ] **Step 1: Kill the format-then-reparse double.** `repo_of` (and `open`) build `format!("{owner}/{name}")` only so `parse_repo_path` can split it again. Read `parse_repo_path` (`src/protocol/mod.rs:16`) and add beside it:

```rust
/// The pair form of `parse_repo_path`, for callers that already hold the two segments —
/// they were formatting them into one string just to split it again.
pub fn parse_repo_pair(owner: &str, name: &str) -> Option<(String, String)> { ... }
```

  Implement by extracting the split-free tail of `parse_repo_path` (the `.git` strip + `valid_segment` checks — copy its exact validation, whatever it is) and have `parse_repo_path` delegate: `let (o, n) = p.split_once('/')?; parse_repo_pair(o, n)` — but ONLY if `parse_repo_path` today rejects extra slashes the same way `split_once` does; read it first and preserve its exact accept/reject set (there are tests on it — they are the net). Then switch the three `parse_repo_path(&format!("{owner}/{name}"))` call sites in `src/http.rs` and the one in `src/http/browse_api/merge.rs:98` to `parse_repo_pair(owner, name)`. Leave `route_inner`'s `path.to_string()` alone — the borrow of `req` must end before `next.run(req)`, so the owned copy is load-bearing; add nothing.
- [ ] **Step 2: Decode Basic auth once in the git front door.** `src/http.rs` `open` calls `auth::basic_token` then `auth::basic_user_names` — two full base64 decodes of the same header. In `src/auth.rs` make `basic_creds` `pub(crate)`, and refactor `basic_user_names` so its judgement is reusable:

```rust
pub(crate) fn user_names(user: &str, owner: &str, git_placeholder: bool) -> bool {
    user == owner || (git_placeholder && user == GIT_PLACEHOLDER)
}
```

  (`basic_user_names` keeps its signature for the registry caller and becomes `basic_creds(headers).map_or(true, |(u, _)| user_names(&u, owner, git_placeholder))`.) Then in `http.rs` `open`:

```rust
            match crate::auth::basic_creds(headers) {
                Some((user, t)) => {
                    match app.store.owner_for_token(&t).await.map_err(internal)? {
                        Some(o) if crate::auth::user_names(&user, &o, true) => Some(o),
                        _ => return Err(unauthorized()),
                    }
                }
                None => None,
            }
```

  Keep the existing halves-that-disagree comment. Check whether the registry's `auth::allow` has the same token-then-user-names double — if it does, apply the same one-decode shape there; if its plan owns that file, leave it and note it.
- [ ] **Step 3: `keyed_lock` retains only past a threshold.** `src/store.rs:39-45` runs an O(n) `retain` on every acquisition. Gate it like `neg_cache_miss` (`src/lib.rs:253`):

```rust
    pub fn keyed_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.keyed_locks.lock().unwrap();
        // Swept only past a size no honest in-flight set reaches — an every-acquisition retain
        // was O(live keys) on every ref write. Entries with one strong count are held by nobody
        // but this map, so dropping them can never break a caller (it holds a clone).
        const SWEEP_AT: usize = 512;
        if m.len() >= SWEEP_AT {
            m.retain(|_, v| Arc::strong_count(v) > 1);
        }
        m.entry(key.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
```

  Update the method's existing "Cheap enough to run on every acquisition" comment — it is now false; the `// ponytail: in-process lock` marker on the struct field stays. The spec says "both twins": `grep -rn "fn keyed_lock" src/` — at plan time there is exactly one impl (registry code calls the same `Store::keyed_lock`); if a second impl exists when you look, apply the same gate there.
- [ ] **Step 4: Static event field keys.** `src/events.rs:71-82` allocates eight key `String`s per publish. Change `fields` to `Vec<(&'static str, String)>` and `Cache::xadd` (`src/cache.rs:253`) to `fields: &[(&'static str, String)]`; follow the compiler through `xadd`'s mem-stream and Redis bodies (Redis arg building accepts `&str` fine; the mem-stream's stored type may need `(String, String)` — convert at the storage boundary, not per publish). `from_fields` keeps `&[(String, String)]` — its input comes back owned from Redis. Fix the `fields_round_trip` test to convert.
- [ ] **Step 5: GC `get_bytes` drops the copy.** `src/registry/gc.rs:20-22` does `.bytes().await?.to_vec()` — return `slatedb::object_store::Bytes` (or `bytes::Bytes`, whichever the crate re-exports) instead of `Vec<u8>` and delete the `to_vec`; callers use `&bytes` slices and compile unchanged or with `&bytes[..]`.
- [ ] Run `cargo clippy --lib -- -D warnings` and `cargo test`.
- [ ] Commit: `git commit -am "Trim per-request micro-costs in routing, auth, locks, events and gc"`

---

## Deliberately excluded

- **P1 "forwarded requests build the replay lazily" (`src/http.rs:392-401`):** the spec's sketch is wrong once you read the code — `forward(...)` consumes `req`, so method/uri/headers/extensions *cannot* be captured in the `Err` arm; the up-front clone is the only place they still exist. Gating the clone on a non-recording peek of the recovery throttle saves nothing either: `may_ask_to_recover` only records when a recovery is actually attempted, so the window is open (and the clone taken) in the common forward-succeeds case regardless. Making the forwarder return the request on connect failure would be the real fix and is not worth its blast radius for one HeaderMap clone per forwarded GET.
- **`delete_stream` at `src/registry/store.rs:373-380` and `src/registry/uploads.rs:485-492`:** same shape as Task 7 but those files belong to the registry perf plan — one plan per file.
- **`route_inner`'s `path.to_string()`:** load-bearing for the borrow checker (`req` moves into `next.run`); noted inside Task 9 Step 1.
