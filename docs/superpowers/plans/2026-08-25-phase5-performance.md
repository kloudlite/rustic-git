# Phase 5: Performance — Implementation Plan

> **For agentic workers:** execute this with `superpowers:subagent-driven-development` — one
> subagent per `### Task`, in the order given. Each task is independently shippable and ends in
> its own commit; do not batch two tasks into one commit.

**Goal.** Remove the seven measured hot spots from the 2026-08-25 audit's section 3 (plus the
small medium-perf list) without changing a single observable behaviour. Every fix is either a
configuration value, a call-count reduction, or a rewrite of an inner loop that keeps its
existing tests green.

**Architecture.** Nothing moves between crates. The changes sit in five places:

- `crates/storage` — SlateDB open-time settings: a shared block cache and an on-disk object-store
  cache, both sized from env against the pod memory/disk limits.
- `crates/registry` — the blob upload path becomes a real resumable S3 multipart upload with a
  sidecar holding the upload id and part list; `manifest_stat` stops LISTing; the GC sweep's
  second `referenced()` pass becomes conditional.
- `crates/git` — `upload-pack` stops enumerating every object in the repo to answer a `have`;
  `receive-pack` batches its closure walk and validates before it uploads.
- `crates/pulls` + `bins/worker` — network sync is hoisted out of `check`/`run` so a repo-wide
  re-check fetches once instead of 25 times.
- `bins/server` + `crates/workspaces` + `crates/app` — the agent long-poll stops writing a
  heartbeat per second and stops fetching every agent to find one; `regions()` gets a TTL cache;
  `grant_renew` drops the global lock; four serial per-team reads become concurrent.

**Tech Stack.** Rust 2021, tokio, axum, `slatedb 0.15` (default features ⇒ the `foyer` feature IS
on, so `slatedb::db_cache::foyer::FoyerCache` is available), `object_store 0.14.1`
(re-exported as `slatedb::object_store`), `futures`, `gix-*`, Azure Cosmos SDK.

**Audit findings covered.** P1, P2, P3, P4, P5, M7(perf), and the medium-perf batch:
`receive.rs:228,263`, `manifests.rs:210`, `gc.rs:307`, `app/lib.rs:589`,
`workspaces/api.rs:499,581,764,784`. **Explicitly out of scope:** P6 (leader guard on the requeue
sweep) and P7 (agent 204 sleep floor) — a separate plan owns those. `repack` never re-deltaing is
also out of scope: it is a storage-size finding, not a latency one.

---

## Global Constraints

- `cargo clippy --workspace -- -D warnings` is clean after every task. No new warnings in files
  you touch, test targets included.
- `cargo test` passes after every task. **No behaviour change:** each perf fix keeps its existing
  tests green *unmodified*. If an existing test has to change, you have changed behaviour — stop
  and re-read the task.
- **Measure before and after.** Every task below names the instrument and the exact command. A
  task is not done until the after-number is recorded in the commit message body.
- Comments explain WHY, never what. Match the density of `bins/server/src/router/route.rs`.
- Deliberate shortcuts get `// ponytail: <ceiling and upgrade path>`. Keep every marker you edit
  near; several tasks below *remove* an existing marker because they close it — say so.
- Commit subjects: imperative sentence case, no tool attribution.

---

## File Structure

| File | Responsibility in this plan |
|---|---|
| `tests/common/counting.rs` | **New.** An `ObjectStore` decorator counting ops and bytes per method. The measurement instrument for Tasks 1 and 6. Nothing in `crates/` depends on it. |
| `tests/common/mod.rs` | Add `pub mod counting;`. |
| `crates/storage/src/pool/mod.rs` | `Pool::new` gains the disk-cache root, the byte budget, and one shared `Arc<dyn DbCache>` field. |
| `crates/storage/src/pool/lease.rs` | `Pool::open` passes the shared cache to `Db::builder`. |
| `crates/storage/src/ownership/mod.rs` | `leader_settings` gets the same disk cache under its own subdir. |
| `crates/storage/src/store.rs` | `Store::open` hands `cache_dir` to `Pool::new`. |
| `bins/server/src/vol_agent.rs` | `work()`: one heartbeat per poll window; point read instead of `agents_in().find()`. |
| `crates/workspaces/src/store.rs` | `MetaStore` gains `get_agent(region, id)`. |
| `crates/workspaces/src/cosmos.rs` | `get_agent` point read; `regions()` TTL cache. |
| `crates/workspaces/src/api.rs` | Four `try_join_all` conversions. |
| `crates/git/src/protocol/upload/mod.rs` | `reachable_commits` next to `reachable_set`. |
| `crates/git/src/protocol/upload/walk.rs` | `ours()` splits into `our_commits()` (haves) and `ours()` (non-tip wants). |
| `crates/git/src/protocol/receive.rs` | One batched closure walk; validate-then-upload. |
| `crates/pulls/src/merge_worker.rs` | `sync_many` + `check_in`/`run_in` that take an already-synced dir. |
| `bins/worker/src/main.rs` | Fetch once per repo-wide re-check, then N local checks. |
| `crates/registry/src/uploads.rs` | Sidecar-backed multipart PATCH/PUT. |
| `crates/registry/src/store.rs` | `manifest_stat` reads DB counters. |
| `crates/registry/src/manifests.rs` | Bump the counters on push/delete. |
| `crates/registry/src/gc.rs` | Early return when nothing is doomed. |
| `crates/app/src/lib.rs` | `grant_renew` per-repo locking. |

---

### Task 1: P2 — configure SlateDB's disk and block caches

Biggest latency lever in the plan and it is configuration only. Today `Pool::new` builds
`Settings` with `object_store_cache_options` left at `Default`, whose `root_folder` is `None` —
**the disk cache is off entirely**, so every SST block miss under a tag read, a visibility check
or a ref read is an S3 GET. Separately, `Db::builder` installs `default_db_cache()` **per
database** (verified in `~/.cargo/registry/.../slatedb-0.15.0/src/db/builder.rs:206,1892`): a
512 MiB block cache plus a 128 MiB meta cache *each*, and this pool holds up to
`KLOUDLITE_MAX_WARM=64` databases open. Sharing one cache is therefore a memory-safety fix as
much as a hit-rate fix.

Verified API (do not substitute names):

- `slatedb::config::Settings::object_store_cache_options: ObjectStoreCacheOptions`
  (`src/config.rs:1647`) with `root_folder: Option<PathBuf>`, `max_cache_size_bytes:
  Option<usize>`, `part_size_bytes: usize` (default 4 MiB), `cache_on_flush: bool`,
  `cache_on_compaction: bool`.
- `slatedb::DbBuilder::with_db_cache(Arc<dyn DbCache>)` (`src/db/builder.rs:275`) — the cache is
  wrapped `UnownedDbCache`, so **SlateDB will not close it**; each database gets its own scope id
  inside `DbCacheWrapper`, so one instance shared across 64 repos cannot clobber keys.
- `slatedb::db_cache::{DbCache, SplitCache}` (`SplitCache::new().with_block_cache(Option<..>)
  .with_meta_cache(Option<..>).build()`, `src/db_cache/mod.rs:450`) and
  `slatedb::db_cache::foyer::{FoyerCache, FoyerCacheOptions { max_capacity: u64, shards: usize }}`
  via `FoyerCache::new_with_opts`.

**Files:** `crates/storage/src/pool/mod.rs`, `crates/storage/src/pool/lease.rs`,
`crates/storage/src/store.rs`, `crates/storage/src/ownership/mod.rs`,
`tests/common/counting.rs` (new), `tests/common/mod.rs`, `tests/throughput.rs`.

**Interfaces:**

```rust
// crates/storage/src/pool/mod.rs
use slatedb::db_cache::{foyer::{FoyerCache, FoyerCacheOptions}, DbCache, SplitCache};

/// One cache for every repo database on this node, not one per database.
///
/// `Db::builder` installs its own 512 MiB block + 128 MiB meta cache when none is given, and this
/// pool holds `KLOUDLITE_MAX_WARM` (64) databases open — 40 GiB of nominal cache against a pod
/// limit measured in single-digit GiB. SlateDB scopes each database's keys inside the wrapper, so
/// sharing is safe; it also never closes a cache passed this way, which is what we want when the
/// pool outlives every database in it.
fn shared_db_cache(block_bytes: u64, meta_bytes: u64) -> Arc<dyn DbCache> {
    let mk = |cap| Some(Arc::new(FoyerCache::new_with_opts(FoyerCacheOptions {
        max_capacity: cap,
        ..Default::default()
    })) as Arc<dyn DbCache>);
    Arc::new(SplitCache::new().with_block_cache(mk(block_bytes)).with_meta_cache(mk(meta_bytes)).build())
}

pub struct Pool { /* … */ db_cache: Arc<dyn DbCache> }

impl Pool {
    pub fn new(os: Arc<dyn ObjectStore>, background: bool) -> Pool { /* … */ }
}
```

Inside `Pool::new`, after the existing `wal_gc` block:

```rust
// A block miss is an S3 GET on the request path — under every tag read, visibility check and
// ref read. The disk cache is what turns the second one into a local file read; it is OFF by
// default (`root_folder: None`), which is how it has been running. Sized by env because the
// budget is the pod's ephemeral disk, not anything this code can see: default 4 GiB, and
// `KLOUDLITE_SLATEDB_DISK_CACHE_MB=0` turns it back off for a node with no scratch space.
// `cache_on_flush`/`cache_on_compaction` are left OFF: this pool runs neither by default
// (see `background`), and a leader that does would be writing SSTs it is not about to re-read.
let cache_mb = env_u64("KLOUDLITE_SLATEDB_DISK_CACHE_MB", 4096);
let root = std::path::PathBuf::from(std::env::var("KLOUDLITE_CACHE_DIR")
    .unwrap_or_else(|_| "./.local/cache".into())).join("slatedb");
let object_store_cache_options = slatedb::config::ObjectStoreCacheOptions {
    root_folder: (cache_mb > 0).then(|| root.clone()),
    max_cache_size_bytes: Some((cache_mb * 1024 * 1024) as usize),
    ..Default::default()
};
```

and thread it into the `Settings { .. }` literal. Then, in
`crates/storage/src/pool/lease.rs:102`:

```rust
Db::builder(path(owner, name), self.os.clone())
    .with_settings(self.settings.clone())
    .with_db_cache(self.db_cache.clone())
```

`ownership/mod.rs`'s `leader_settings` gets the same `object_store_cache_options` but rooted at
`…/slatedb-ownership` — one database, read on every route decision, and it must not share an
eviction budget with the repo pool.

Budgets: `KLOUDLITE_SLATEDB_BLOCK_CACHE_MB` (default 256) and
`KLOUDLITE_SLATEDB_META_CACHE_MB` (default 64). Both are node-wide totals now, so the defaults
are *below* SlateDB's per-database defaults on purpose — document that in the comment.

**Expected win + how measured.** Warm-path metadata reads (tag read, `is_public`, ref list) drop
from one-or-more S3 GETs to a local file read: expect a ≥10× reduction in object-store GET count
on the second and later reads of the same repo, and node RSS with 64 warm repos bounded by
`block+meta` (320 MiB) instead of unbounded. Measure with the new counting store:

```
cargo test --release --test throughput -- --ignored --nocapture slatedb_cache_cuts_object_store_gets
```

The test opens a `Store` over `CountingStore::new(InMemory)` in a `tempdir`, writes 200 refs
across 8 repos, evicts, re-reads them all, and prints `gets_before` / `gets_after` for the
re-read phase. Record both numbers. **Do not** assert a hard ratio — assert only "the second pass
issues strictly fewer GETs than the first", which is the property, and print the rest.

- [ ] **Step 1:** Write `tests/common/counting.rs`: `pub struct CountingStore { inner: Arc<dyn
      ObjectStore>, pub gets: AtomicU64, pub puts: AtomicU64, pub get_bytes: AtomicU64, pub
      put_bytes: AtomicU64 }` implementing `ObjectStore` by delegation. Add `pub mod counting;`
      to `tests/common/mod.rs`.
- [ ] **Step 2:** Add the ignored `slatedb_cache_cuts_object_store_gets` measurement to
      `tests/throughput.rs`, run it against today's code, record the baseline numbers.
- [ ] **Step 3:** Add `db_cache` + `object_store_cache_options` to `Pool::new` and
      `with_db_cache` to `Pool::open`. Nothing else changes.
- [ ] **Step 4:** Give `leader_settings` its own `object_store_cache_options` under
      `…/slatedb-ownership`.
- [ ] **Step 5:** Re-run the measurement; record the after-numbers.
- [ ] **Step 6:** `cargo clippy --workspace -- -D warnings && cargo test`.
- [ ] **Step 7:** Commit: `git commit -m "Give SlateDB a disk cache and one shared block cache"`
      (before/after GET counts in the body).

> Deploy note for whoever rolls this: Rust pods run as uid 1001 with a read-only root, so
> `KLOUDLITE_CACHE_DIR` must already be a writable mount — it is (the pack cache lives there) —
> and the mount's size request has to cover `KLOUDLITE_SLATEDB_DISK_CACHE_MB`. Repin the image
> before the yaml, per CLAUDE.md.

---

### Task 2: P5 — stop the agent long-poll writing 90 Cosmos ops per 30s

`work()` (`bins/server/src/vol_agent.rs:385`) loops every `poll_interval` for `poll_window`, and
each iteration does: `agents_in(region)` (a cross-partition-key *query* returning every agent in
the region) → `upsert_agent(me)` (a write) → `queued_jobs(region)` (a query). At a 1s interval
over a 30s window that is ~90 ops per agent per idle poll, two thirds of which exist only to
refresh a heartbeat that nobody reads more than once per window.

**Files:** `bins/server/src/vol_agent.rs`, `crates/workspaces/src/store.rs`,
`crates/workspaces/src/cosmos.rs`.

**Interfaces:**

```rust
// crates/workspaces/src/store.rs — MetaStore
/// One agent by id. `agents_in` + `find` was the only way to do this, and it read every agent in
/// the region on every poll iteration; the agents container is partitioned by region with the
/// agent id as the document id, so this is a point read.
async fn get_agent(&self, region: &str, id: &str) -> Result<Option<AgentDoc>, StoreErr>;

// crates/workspaces/src/cosmos.rs
async fn get_agent(&self, region: &str, id: &str) -> Result<Option<AgentDoc>, StoreErr> {
    read_item(&self.agents, region, id).await
}
```

`store::MemStore` gets the obvious in-memory equivalent.

In `work()`, hoist the heartbeat out of the loop:

```rust
// The heartbeat is a liveness signal with a lease-scale timeout, not a clock: writing it once per
// poll window is exactly as fresh as the window, and it was costing one Cosmos write per second
// per agent. The job scan stays per-iteration — that is what the long poll is FOR.
let mut me = store.get_agent(&region.id, &q.agent).await.map_err(job_store_err)?.ok_or_else(job_not_found)?;
me.heartbeat_at = chrono::Utc::now();
me.status = "alive".into();
me.used = Capacity { cpu: q.used_cpu, mem_mb: q.used_mem_mb, disk_gb: q.used_disk_gb };
store.upsert_agent(&me).await.map_err(job_store_err)?;
loop { /* queued_jobs + CAS lease + deadline check, unchanged */ }
```

Note the ordering: the heartbeat is written *before* the first job scan, so an agent that leases
a job on its first iteration still refreshed its liveness — that is the behaviour today and it
must stay.

**Expected win + how measured.** ~90 Cosmos ops per agent per idle window → 2 + one query per
iteration (~32), and the two per-iteration *queries* become one point read + one query. Cosmos RU
per idle agent-hour drops by roughly 3×. Measure with the existing store-op counter in
`tests/vol_agent.rs`: add `long_poll_costs_one_heartbeat_per_window`, which drives `work()`
against `MemStore` wrapped in a counting shim, with `poll_interval` at 10ms and `poll_window` at
100ms, and asserts `upsert_agent` was called exactly once while the poll ran ≥5 iterations.

```
cargo test --test vol_agent long_poll_costs_one_heartbeat_per_window -- --nocapture
```

- [ ] **Step 1:** Write the characterization test above against today's code; it must FAIL,
      showing ~10 heartbeat writes. Record the number.
- [ ] **Step 2:** Add `get_agent` to `MetaStore`, `CosmosStore`, `MemStore`.
- [ ] **Step 3:** Hoist the heartbeat; swap `agents_in().find()` for `get_agent`.
- [ ] **Step 4:** Test passes; record after-count.
- [ ] **Step 5:** `cargo clippy --workspace -- -D warnings && cargo test`.
- [ ] **Step 6:** Commit: `git commit -m "Heartbeat once per agent poll window, not per iteration"`.

---

### Task 3: M7(perf) — TTL-cache `regions()`

`region_by_token` runs on **every** agent request and calls `regions()`
(`crates/workspaces/src/cosmos.rs:145`), which is `SELECT * FROM c` with `PartitionKey::EMPTY` —
a cross-partition query. Regions change when an operator adds one, i.e. approximately never.

**Files:** `crates/workspaces/src/cosmos.rs`.

**Interfaces:** model it on `App::neg_cache` (`crates/app/src/lib.rs:181`) — a `Mutex<Option<…>>`
with an `Instant`, swept on insert. One entry, so no sweep is needed; that is the whole
difference.

```rust
/// Regions change when an operator adds one. This query is cross-partition and sits on every
/// agent request via `region_by_token`, so it is cached for `REGION_TTL` — the cost of the
/// staleness is that a freshly-added region is unusable for up to a minute, which is shorter
/// than the time to bring up an agent in it. `put_region` clears it so a test (and an operator
/// on the same node) sees its own write.
const REGION_TTL: Duration = Duration::from_secs(45);
regions_cache: Mutex<Option<(Instant, Vec<Region>)>>,
```

`regions()` reads it under the lock, returns the clone on a hit, queries and fills on a miss.
`put_region()` takes the lock and sets `None`. Use the poison-tolerant `lock()` form that
`storage/src/auth.rs` uses — a panic while holding this must not 500 every subsequent agent poll.

**Expected win + how measured.** One cross-partition query per agent request → one per 45s
node-wide. With N agents polling on a 30s window that is N×2 queries/minute → ~1.3. Measure by
extending Task 2's counting shim assertion: `regions_are_queried_once_per_ttl` drives two
back-to-back `work()` calls and asserts exactly one `regions` query.

```
cargo test --test vol_agent regions_are_queried_once_per_ttl -- --nocapture
```

- [ ] **Step 1:** Write the test; it fails at 2 queries.
- [ ] **Step 2:** Add the cache field, the read path, and the `put_region` invalidation.
- [ ] **Step 3:** Test passes; `cargo clippy --workspace -- -D warnings && cargo test`.
- [ ] **Step 4:** Commit: `git commit -m "Cache the region list for the agent token lookup"`.

---

### Task 4: P3 — answer a `have` from the commit graph, not the whole repo

`upload/mod.rs:183` calls `walk::ours()` → `reachable_set(odb, tips)`, which peels the tips, walks
every commit, and then runs `output::count::objects_unthreaded(.., ObjectExpansion::TreeContents)`
over all of them — a full enumeration of every tree and blob in the repository, on **every
incremental fetch**, just to test whether a handful of `have` oids are ours.

A `have` is always a commit (git only ever sends commit ids in `have` lines; anything else is a
malformed request, and treating an unknown id as "not common" is the correct answer either way).
So the have-path only needs the commit set. The full object set stays for the *other* caller: the
non-tip `want` check at `mod.rs:216`, where a promisor fetch legitimately wants a tree or a blob.

**Files:** `crates/git/src/protocol/upload/mod.rs`, `crates/git/src/protocol/upload/walk.rs`.

**Interfaces:**

```rust
// crates/git/src/protocol/upload/mod.rs, beside reachable_set
/// Every COMMIT reachable from `tips`. What a `have` check needs and all it needs: git only ever
/// sends commit ids in a `have` line, and an id that is not a commit reachable from our refs is
/// "not common" whichever way we compute it. The full-object `reachable_set` costs an enumeration
/// of every tree and blob in the repo, which is what this exists to avoid on the fetch path.
pub(crate) fn reachable_commits(odb: &gix_odb::Handle, tips: &[ObjectId])
    -> Result<std::collections::HashSet<ObjectId>>
{
    let mut odb = odb.clone();
    odb.prevent_pack_unload();
    let Peeled { commits, tags, .. } = walk::peel_wants(&odb, tips)?;
    // Tags stay in the set: a client that fetched an annotated tag has it, and it peels to a
    // commit we are about to walk anyway.
    let mut set: std::collections::HashSet<ObjectId> = tags.into_iter().collect();
    for info in gix_traverse::commit::Simple::new(commits, odb.clone()) {
        set.insert(info?.id);
    }
    Ok(set)
}

// crates/git/src/protocol/upload/walk.rs — `ours` splits in two, same memoize-in-a-slot shape
pub(super) fn our_commits<'a>(slot: &'a mut Option<HashSet<ObjectId>>, odb: &gix_odb::Handle, tips: &[ObjectId])
    -> Result<&'a HashSet<ObjectId>>;
pub(super) fn ours<'a>(slot: &'a mut Option<HashSet<ObjectId>>, odb: &gix_odb::Handle, tips: &[ObjectId])
    -> Result<&'a HashSet<ObjectId>>;   // unchanged, now only reached by the non-tip want check
```

Two separate slots in `mod.rs` (`have_set` becomes `commit_set` + `object_set`) — a fetch that
does both pays for both, which is strictly no worse than today.

**Expected win + how measured.** An incremental fetch on a repo with 5k commits and 200k objects
goes from ~200k object visits to ~5k commit visits: O(objects) → O(commits). Wall clock on the
`have` phase should fall by more than an order of magnitude on any repo with real trees.

```
cargo test --release --test protocol -- --nocapture incremental_fetch_walks_commits_not_objects
```

Build the fixture inside that test: a repo with 300 commits each touching a distinct new file
(so objects ≫ commits), then time a fetch declaring the tip-1 commit as `have`, printing the
elapsed. Assert only the existing correctness (the pack contains exactly the one new commit's
objects) and print the timing — a wall-clock assertion in CI is a flake.

- [ ] **Step 1:** Add the test above; record the baseline elapsed and the object-visit count.
- [ ] **Step 2:** Add `reachable_commits` + `our_commits`; point the `have` filter at it. Leave
      the non-tip `want` check on `ours`.
- [ ] **Step 3:** Re-run; record the after-elapsed. Then run the full protocol suite —
      `cargo test --test protocol` — every existing negotiation test must pass untouched.
- [ ] **Step 4:** `cargo clippy --workspace -- -D warnings && cargo test`.
- [ ] **Step 5:** Commit: `git commit -m "Answer a have from the commit graph, not every object"`.

---

### Task 5: P4 — fetch once per repo-wide re-check, not once per change

`bins/worker/src/main.rs:271` fans a `HeadMoved` out into up to `CHECK_LIMIT = 25`
(`crates/pulls/src/pulls/check.rs:14`) `check_one` calls. Each one calls
`merge_worker::check` → `sync()` (`merge_worker.rs:243`), which does a full `git fetch` over the
peer listener for that change's base and head — 25 serial network fetches of the same base
branch, all under the repo's merge lock taken at `main.rs:242`.

The fix is to split sync out of `check`/`run`: the caller syncs once with a union refspec, then
the 25 checks are pure local `merge-tree` calls.

**Files:** `crates/pulls/src/merge_worker.rs`, `bins/worker/src/main.rs`.

**Interfaces:**

```rust
// crates/pulls/src/merge_worker.rs

/// Fetch every ref a batch of checks will need, in ONE network round trip.
///
/// `sync` fetches per-Job, which is right for a single merge but wrong for a repo-wide re-check:
/// a base branch that moved fans out to every open change against it, and each of those was
/// re-fetching the same base. Refs are deduped here because a union refspec naming the same ref
/// twice makes git fail the whole fetch, not skip the duplicate.
pub fn sync_many(cache: &Path, upstream: &str, secret: &str, owner: &str, name: &str, refs: &[&str])
    -> Result<(PathBuf, String)>;

/// `check`, against a repo directory the caller has already synced. `check` keeps its signature
/// and becomes `sync` + this, so the single-check path is unchanged.
pub fn check_in(dir: &Path, job: &Job) -> Result<Verdict>;

/// Same split for the merge itself: `run` is `sync` + `run_in`, and the batch path never uses
/// `run_in` — a merge PUSHES, so it needs its own fresh fetch for the lease to mean anything.
pub fn run_in(dir: &Path, url: &str, job: &Job, secret: &str) -> Result<Outcome>;
```

`sync_many` reuses the existing `cache_of` + init + `USED` touch, then calls the existing `fetch`
helper with the full ref list. Keep `fetch` `networked()` — the `local()`/`networked()` split is
what keeps the peer secret out of error messages, and `sync_many` must not format its argv into
anything (CLAUDE.md).

In `bins/worker/src/main.rs`, the `deep` loop becomes:

```rust
// One fetch for the whole fan-out. A base branch that moved re-checks every open change against
// it, and each of those used to pay its own network round trip for the same base — 25 of them,
// serially, with the repo's merge lock held the whole time.
let refs: Vec<&str> = deep.iter().flat_map(|d| [d.base.as_str(), d.head.as_str()]).collect();
match tokio::task::spawn_blocking({ /* sync_many(…, &refs) */ }).await { … }
for d in &deep { check_one_in(w, &dir, owner, name, d).await; }
```

`check_one` keeps its shape (it still builds the `Job`, still `spawn_blocking`s, still writes the
verdict back the same way) — only the sync moves out. A `sync_many` failure falls back to the
per-change path, so a partially-gone ref degrades to today's behaviour instead of failing all 25.

**Expected win + how measured.** 25 network fetches → 1. On a repo with 25 open changes and a
50ms-RTT peer listener, the fan-out's git time drops from ~25×(RTT + pack negotiation) to one,
and the merge lock is held for a fraction of the time. Measure with the existing pulls suite:

```
cargo test --release --test pulls -- --nocapture head_moved_fans_out_with_one_fetch
```

Instrument by counting `upload-pack` requests server-side: the test's harness already serves the
peer listener, so add a request counter to it and assert `== 1` for the fan-out. Print the
wall-clock of the whole fan-out before and after.

- [ ] **Step 1:** Add `head_moved_fans_out_with_one_fetch` to `tests/pulls.rs` with the
      upload-pack counter; it fails at N. Record N and the elapsed.
- [ ] **Step 2:** Add `sync_many`, `check_in`, `run_in`; re-express `check`/`run` in terms of
      them so the single-job callers are byte-for-byte equivalent.
- [ ] **Step 3:** Switch the worker's fan-out to `sync_many` + `check_in`, with the per-change
      fallback.
- [ ] **Step 4:** Test passes at 1; record the elapsed. Run `cargo test --test pulls` whole.
- [ ] **Step 5:** `cargo clippy --workspace -- -D warnings && cargo test`.
- [ ] **Step 6:** Commit: `git commit -m "Fetch once for a repo-wide mergeability re-check"`.

---

### Task 6: P1 — real resumable multipart for chunked blob uploads

The biggest win and the biggest refactor. Today `patch` reads the whole staging object and
re-streams it through `pour` ahead of the new chunk (`uploads.rs:280` — the existing `ponytail:`
marker says exactly this), so a chunked push of an N-byte layer in K chunks moves O(N·K) bytes
each way; `put_from_session` then re-reads the whole staging object again to hash it
(`uploads.rs:434`). A 10 GiB layer in 100 MiB chunks moves ~1 TiB.

**Verified `object_store 0.14.1` API.** `MultipartUpload` (`src/upload.rs:43`) is a *live handle*
and cannot survive a request, let alone a node move — it is not what this needs. The resumable
one is `object_store::multipart::MultipartStore` (`src/multipart.rs:45`):

```rust
async fn create_multipart(&self, path: &Path) -> Result<MultipartId>;             // MultipartId = String
async fn put_part(&self, path: &Path, id: &MultipartId, part_idx: usize, data: PutPayload) -> Result<PartId>;
async fn complete_multipart(&self, path: &Path, id: &MultipartId, parts: Vec<PartId>) -> Result<PutResult>;
async fn abort_multipart(&self, path: &Path, id: &MultipartId) -> Result<()>;
```

`PartId { content_id: String }` is a plain serializable struct. `MultipartStore` is implemented by
`AmazonS3`, `MicrosoftAzure`, `GoogleCloudStorage` and `InMemory` — **not** by `LocalFileSystem`,
which is `KLOUDLITE_S3_URL=file://` (dev). So this is an *optional* fast path with the current
code as the fallback, not a replacement.

Two hard constraints from S3 that shape the design:

1. Every part except the last must be ≥5 MiB. A client chunking below that (some do) cannot use
   the multipart path — fall back to append for that session.
2. The digest is not known until the last byte, and `sha2` state is not serializable, so
   completion still costs **one** read of the blob to hash. That is O(N), not O(N·K), and it is
   the whole point.

**Files:** `crates/registry/src/uploads.rs`, `crates/storage/src/config.rs`,
`crates/storage/src/store.rs`, `tests/registry_uploads.rs`, `tests/common/counting.rs`.

**Interfaces:**

```rust
// crates/storage/src/config.rs — build the concrete store once, expose both views
pub fn object_store() -> Result<(Arc<dyn ObjectStore>, Option<Arc<dyn MultipartStore>>)>

// s3://   -> let a = Arc::new(b.build()?); (a.clone(), Some(a))
// mem://  -> let m = Arc::new(InMemory::new()); (m.clone(), Some(m))
// file:// -> (Arc::new(LocalFileSystem…), None)   // LocalFileSystem has no MultipartStore impl

// crates/storage/src/store.rs
pub struct Store { pub os: Arc<dyn ObjectStore>, pub mp: Option<Arc<dyn MultipartStore>>, /* … */ }
// `Store::open` gains an `mp` parameter; every existing caller passes `None` unless it wants
// the fast path, so tests are a one-word change.

// crates/registry/src/uploads.rs
/// What a chunked session needs to resume on any node that owns the image: the S3 upload id and
/// the parts accepted so far. Sits beside the staging object as `uploads/{o}/{n}/{uuid}.parts`,
/// so it is swept by the same `sweep_stale_uploads` prefix walk and needs no database row — the
/// property that lets a session survive the image moving nodes stays intact.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Sidecar { id: String, parts: Vec<String>, len: u64 }   // parts = PartId.content_id, in order
```

**Flow.**

- `POST` (session start) writes nothing new; the sidecar is created lazily by the first PATCH
  that qualifies.
- `PATCH`: take the same session lock. Read the sidecar. If `store.mp` is `Some`, the sidecar's
  `len` equals the declared start offset, and this chunk is ≥5 MiB, then: `create_multipart` if
  there is no sidecar yet, stream the body into ≥5 MiB `PutPayload` buffers, `put_part` each with
  `part_idx = parts.len()`, push the `content_id`s, add to `len`, and write the sidecar back with
  a single `put`. Otherwise fall through to today's `pour` append, byte for byte. `have` for the
  Range header is `sidecar.len` when a sidecar exists, else the staging object's size — one
  helper, `received()`, so no caller can read the wrong one.
- `PUT` (completion): if a sidecar exists, `put_part` the final body (any size), then
  `complete_multipart` into the **staging** path. Now stream the staging object once through the
  hasher; on match, `os.copy(staging, blob_path)` — a server-side S3 CopyObject, no bytes over
  the wire — then `discard`. On mismatch, delete the staged object and keep the session open,
  exactly as today.
- Any refusal aborts: `abort_multipart` then delete the sidecar. A crashed session leaves parts,
  which the bucket's existing incomplete-multipart lifecycle rule reaps — the same ceiling the
  current `pour` marker already names.

Replace the old marker; do not delete the concept:

```rust
// ponytail: completion still streams the assembled blob once to hash it (sha2 has no
// serializable state to carry across requests) and `copy` is a single CopyObject, which S3 caps
// at 5 GiB — above that it silently costs a re-upload via the fallback. Upgrade path: multipart
// copy, or hash client-side per part if a registry client ever offers one.
```

**Expected win + how measured.** Bytes moved for a K-chunk push: O(N·K) → O(N) up + O(N) down
(the hash pass) + a server-side copy. For a 1 GiB layer in 10 chunks that is ~5.5 GiB up + 5.5
GiB down → 1 GiB + 1 GiB. Measure with `CountingStore` from Task 1:

```
cargo test --release --test registry_uploads -- --nocapture chunked_push_moves_the_layer_once
```

The test pushes a 40 MiB layer in 8×5 MiB chunks through a `CountingStore` over `InMemory`
(which *does* implement `MultipartStore`) and prints `put_bytes` / `get_bytes`. Assert
`put_bytes < 2 * layer_len` — today it is ~4.5×. Also run the `file://` variant to prove the
fallback path is unchanged, and:

```
./tests/registry_e2e.sh    # exit 77 = docker half skipped, which is NOT a pass
```

- [ ] **Step 1:** Add `chunked_push_moves_the_layer_once` to `tests/registry_uploads.rs` using
      `CountingStore`. It fails; record `put_bytes`/`get_bytes`.
- [ ] **Step 2:** Thread `Option<Arc<dyn MultipartStore>>` through `config::object_store` and
      `Store::open`. No behaviour change yet — commit-able on its own if the diff gets large.
- [ ] **Step 3:** Add `Sidecar`, `received()`, and the sidecar-aware `PATCH` fast path with the
      `<5 MiB` and `mp.is_none()` fallbacks.
- [ ] **Step 4:** Add the `PUT` completion path: final `put_part` → `complete_multipart` → hash
      pass → `copy` → `discard`. Wire `abort_multipart` into every refusal branch.
- [ ] **Step 5:** Confirm `sweep_stale_uploads` reaps `.parts` sidecars (same prefix) and that a
      swept session still answers 404, not 500.
- [ ] **Step 6:** Re-run the measurement; record after-numbers. Run `cargo test --test
      registry_uploads --test registry_blobs --test registry_limits` — all green, unmodified
      except for the new test.
- [ ] **Step 7:** `./tests/registry_e2e.sh` (a real docker push/pull round trip; 77 means the
      docker half did not run, so say so rather than claiming a pass).
- [ ] **Step 8:** `cargo clippy --workspace -- -D warnings && cargo test`.
- [ ] **Step 9:** Commit: `git commit -m "Upload blob chunks as multipart parts instead of
      re-streaming"`.

---

### Task 7: the small medium-perf batch

Five independent one-to-thirty-line fixes. One commit each — they touch four different crates and
a bisect wants them apart.

**Files:** `crates/git/src/protocol/receive.rs`, `crates/registry/src/store.rs`,
`crates/registry/src/manifests.rs`, `crates/registry/src/gc.rs`, `crates/app/src/lib.rs`,
`crates/workspaces/src/api.rs`.

**7a — one batched closure walk in receive-pack** (`receive.rs:263`). The loop calls
`reachable_set_hiding(&odb, &[n], &old_tips, ..)` once per pushed ref; a push of 50 branches off
one base walks the shared history 50 times. Hoist to a single call with every new tip at once,
then attribute per-ref. The per-ref *rejection* granularity is load-bearing (`results[i]`), so
keep it: walk each tip against `old_tips` **plus every tip already accepted in this push**, in
input order — which is what git itself does and gives the same rejections with the shared history
walked once. *Measure:* `cargo test --release --test protocol -- --nocapture
push_of_many_branches_walks_shared_history_once`, printing elapsed for a 50-branch push off one
base; expect near-linear → near-constant in the shared part.
Commit: `Walk a push's object closure once, not once per ref`.

**7b — validate before uploading the pack** (`receive.rs:228`). `write_pack` → `upload_pack_files`
→ *then* connectivity. A push that fails validation has already paid a full S3 upload, and the
cleanup path then deletes it. Move `upload_pack_files` to after the per-ref loop, gated on "at
least one update survived". Keep the existing local-file cleanup for the fully-rejected case —
now it is only local files. Watch the ordering comment: the reason the upload came first was that
the tip check must not accept a ref whose objects exist only on this node; that still holds, so
the upload must complete *before* `store.update_refs`, which it does. *Measure:*
`cargo test --test protocol -- --nocapture rejected_push_uploads_nothing` with `CountingStore`;
assert `put_bytes == 0` for a push with a holed pack.
Commit: `Validate a push before uploading its pack`.

**7c — `manifest_stat` from DB counters** (`store.rs:134`, called from `manifests.rs:210` via
`refresh_image_marker` on every push). It LISTs the whole `manifests/{owner}/{name}` prefix to
produce `(count, newest_ms)`. Both live in the image's own database, which is single-writer, so
they can just be kept: `image/manifests/count` and `image/manifests/newest_ms`, written in the
same `put_manifest`/`delete_manifest` paths that already write the tag. `manifest_stat` reads
them and **falls back to the LIST when the keys are absent**, which is what makes this safe for
existing images — no migration, the first push populates them. *Measure:*
`cargo test --test registry_manifests -- --nocapture manifest_push_does_not_list` with
`CountingStore`; assert zero `list` calls on the second push of an image.
Commit: `Count manifests from the image database, not a prefix list`.

**7d — GC's second `referenced()` pass** (`gc.rs:307`). The re-read exists to close a race for
blobs about to be deleted; when `doomed.is_empty()` there is nothing to protect. Add
`if doomed.is_empty() { return Ok(0); }` above it, with a comment saying the second pass is a
*confirmation*, not a scan — deleting it entirely would reopen the race. *Measure:*
`cargo test --test registry_gc -- --nocapture sweep_with_nothing_doomed_reads_once` with
`CountingStore`; assert the manifest prefix is read once.
Commit: `Skip the GC confirmation pass when nothing is doomed`.

**7e — `grant_renew` per-repo locking** (`app/lib.rs:589`). It holds `leader_lock` across N
serial `ownership.get` + `ownership.put` round trips, so one node renewing 64 repos blocks every
claim on the leader for the whole batch. The lock is protecting a compare-and-set *per repo*, not
across repos — `decide_renew` reads and writes one key. Swap the single global guard for a
per-repo keyed lock (`Store::keyed_lock` already exists and is the same shape) and take it inside
the loop. `grant_claim`/`grant_release` must move to the same keyed lock in the same commit, or
they no longer exclude a concurrent renew of that repo — this is the load-bearing part of the
change, do not ship half of it. *Measure:* `cargo test --test ownership -- --nocapture
renew_does_not_block_a_claim_for_another_repo`; a claim for repo B issued mid-renew of A..A63
must complete before the renew batch does.
Commit: `Lock ownership renewals per repo instead of globally`.

**7f — concurrent per-team reads** (`workspaces/api.rs:499, 581, 764, 784`). Four `for team in
teams_for(..)` loops doing serial point reads. Replace each with
`futures::future::try_join_all(teams.iter().map(|t| s.store.get_env(t, id)))` and pick the first
`Some` **in team order**, so the answer is the same document today's loop returns — a set of
teams where two own an env with the same id must not become order-dependent. The `find_env`
ponytail marker ("N+1 across the caller's teams") stays but gets its ceiling updated: still N
reads, now one round trip. *Measure:* `cargo test --test api_server -- --nocapture
team_lookup_is_concurrent`; assert wall clock for a 10-team caller against a `MemStore` with a
20ms artificial delay is < 100ms (serial would be 200ms+).
Commit: `Read a caller's teams concurrently`.

- [ ] **Step 1:** 7a — characterization test, implement, re-measure, `cargo test --test
      protocol`, commit.
- [ ] **Step 2:** 7b — same cycle, commit.
- [ ] **Step 3:** 7c — same cycle, `cargo test --test registry_manifests --test registry_store`,
      commit.
- [ ] **Step 4:** 7d — same cycle, `cargo test --test registry_gc`, commit.
- [ ] **Step 5:** 7e — same cycle, `cargo test --test ownership --test routing`, commit.
- [ ] **Step 6:** 7f — same cycle, `cargo test --test api_server`, commit.
- [ ] **Step 7:** `cargo clippy --workspace -- -D warnings && cargo test` over the whole batch.

---

## Done when

- All seven tasks committed, each with its before/after number in the commit body.
- `cargo clippy --workspace -- -D warnings` clean; `cargo test` green.
- `./tests/registry_e2e.sh` run for Task 6 (report exit 77 honestly if the docker half skipped).
- No existing test file modified except to ADD a measurement.
