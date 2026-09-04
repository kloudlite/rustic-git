# Performance Review — 2026-08-24

Scope: full codebase — Rust server (`src/`), OCI registry (`src/registry/`), git core
(`src/protocol/`, `src/objects.rs`, `src/store.rs`), pulls/merge worker, Next.js app
(`web/apps/web`), deploy manifests. Axes: CPU, memory, IO/network. Findings verified against
the code; already-`ponytail:`-marked debt is listed separately and not re-counted.

Recurring themes across subsystems:
1. **HEAD-then-GET on the same object** — the `get`'s `meta` already carries size/mtime.
   Appears in blob pulls, upload PATCH/complete, and the imagetags API.
2. **Sequential awaits that are trivially parallel** — per-item loops over object-store
   calls with no `join_all`/`buffer_unordered`.
3. **Redundant re-derivation on hot paths** — repo opened per PR, existence checked 3×
   per open, auth header decoded twice.
4. **cgroup-blind runtime sizing** — tokio and V8 both size themselves from the node,
   not the pod.

---

## P0 — fix soon (high impact, small-to-medium diffs)

### 1. Background lanes can starve lease renewal past the TTL
`src/main.rs:315-381`, `src/lib.rs:520-560`. `renew_once` shares one task with
`reconcile_owned_markers`, `check_owned_pulls`, and `announce_stranded_merges`, and the first
two sleep `RECONCILE_GAP` (200ms) per warm repo. At `max_warm` = 64 that is ~13s per lane —
longer than `LEASE_TTL` (10s). A node with enough warm repos skips renewals, the leader drops
its entries, and `renew_once` then evicts live databases: repo churn, forwarding storms, 503s.
The checkpoint already got exactly this treatment (bounded, for this reason); the lanes did not.
**Fix:** spawn each lane as its own task (or `tokio::spawn` the pass detached) so nothing can
delay `renew_once`. Verified by hand — this is real. *(IO/latency + availability)*

### 2. Registry pull path: HEAD + GET per blob
`src/registry/blobs.rs:63,83`. Every blob pull does a HEAD then a GET of the same key. Drop the
HEAD for `with_body`; take `size` from `GetResult.meta`. Halves round trips on the hottest
registry path. *(IO)*

### 3. Manifest push: serial per-blob existence HEADs
`src/registry/manifests.rs:139-140`. Up to 2 sequential HEADs per referenced digest — a
40-layer manifest is ~80 serial round trips before the write. `try_join_all` over the digests,
blob path first with manifest-path fallback on miss. *(IO/latency)*

### 4. Repo open costs 4-6 redundant store round trips — on every git and browse request
`src/http.rs:673-674` → `src/store.rs:269-284` → `src/refs.rs:323-337`. `open()` checks
`repo_exists` then `is_public` sequentially; `open_repo` re-checks `repo_exists`;
`reconcile_marker` re-checks `is_public` and `index::read` does two *sequential* GETs for a
private repo; plus two `create_dir_all`. **Fix:** open once, read visibility from the handle
(`db_for` + one `PUBLIC_KEY` get); `tokio::join!` the two index paths. *(IO)*

### 5. Mergeability check computes and discards a full diff, and re-opens the repo per PR
`src/pulls.rs:422-426` calls `browse::compare` for `merge_base`/`fast_forward` only, but
`compare` unconditionally builds a whole unified diff. And `src/pulls.rs:399` re-runs
`store.open_repo()` (marker reconcile, pack scan, dir stats) inside the per-PR loop —
×25 per sweep per repo. **Fix:** `merge_base`-only path in `compare` (or call
`browse::merge_base` directly); hoist `open_repo` into `check_repo`. Two small diffs that
remove most of the background sweep's cost. *(CPU + IO)*

### 6. GC sweep reads every manifest of the owner twice, serially
`src/registry/gc.rs:296` calls `referenced()` twice per sweep; each call GETs and parses every
manifest of every image sequentially (`gc.rs:45-72`), and `sweep_owner` lists `blobs/{owner}`
fully more than once. 500 manifests ⇒ 1000 serial GETs per tick. **Fix:** compute the
referenced set once and reuse it; `buffer_unordered(16)` the GETs. *(IO + CPU)*

### 7. Whole pack buffered in memory on upload
`src/store.rs:512-514`. `tokio::fs::read` loads the entire `.pack` (RSS spike = pack size per
concurrent push) — the download path at `:391-394` already streams for exactly this reason.
**Fix:** `put_multipart` fed from a file stream. *(memory)*

### 8. `git clone` walks the commit graph twice
`src/protocol/upload.rs:343-356`. With `include-tag` (clients send it by default),
`commit_range` runs the full traversal, then `write_pack` runs the same traversal again.
**Fix:** compute the commit list once and share it. *(CPU + object reads)*

### 9. Tokio/V8 sized from the node, not the cgroup
`src/main.rs:710`, `src/bin/{api,worker}.rs`; `deploy/kloudlite-git-web.yaml`. Bare
`#[tokio::main]` on a 64-core node spawns 64 worker threads inside a 100m-CPU / 512Mi pod;
Node's V8 heap ceiling is sized from host memory, so the web pod OOMKills instead of GC'ing.
**Fix:** `TOKIO_WORKER_THREADS=4` env on the Rust pods; `NODE_OPTIONS=--max-old-space-size=384`
on web. Two env vars. *(memory + CPU)*

### 10. No HTTP compression anywhere
No `CompressionLayer`, no ingress gzip. Browse-API JSON (trees, logs, whole diffs) crosses
web↔api and api↔browser uncompressed — easily 5-10× on diff/tree payloads. **Fix:**
`tower_http::compression::CompressionLayer` on the browse/api routers (exclude git pack routes;
packs are already compressed). *(network)*

### 11. Web: immutable browse responses fetched with `cache: "no-store"`
`web/apps/web/src/lib/browse.ts:43`. `tree/blob/commit/files/lastmod/log` are oid-keyed and
immutable (the file's own comment says so), yet every fetch forbids caching. **Fix:**
`immutable` flag on `get()` → `next: { revalidate: false }` for everything except `refs`.
*(network + api CPU)*

### 12. Web shell fetches every repo of every owner on every page
`web/apps/web/src/components/app/app-shell.tsx:56-58`. N `listRepos` calls per hard load and
the full `ApiRepo[]` serialized into every page's RSC payload for ⌘K search. **Fix:** ship only
`{owner,name,public}` for the current owner; lazy-load the rest when the palette opens.
*(network + api load)*

---

## P1 — worth doing (medium impact)

### Server / registry
- **Manifest pull is uncached** (`src/registry/manifests.rs:264-283`): object GET + image-DB
  open + point read for media type, per pull, for content-addressed immutable data. Small
  in-process LRU keyed by digest, or store media type as object metadata.
- **`image_exists()` tax on every read** (`src/registry/store.rs:191-254`): pool check + DB get
  on top of every `tag`/`image_is_public` call; anonymous pulls pay it again via `auth::allow`.
  Single-probe rewrite or short-TTL memo.
- **Upload PATCH/complete HEAD+GET** (`src/registry/uploads.rs:195,292,432`): same
  meta-from-get fix as blobs.
- **`_catalog`/images listing serial `manifest_stat`** (`src/registry/routes.rs:26-40`,
  `gc.rs:159,178`): `join_all` / `buffer_unordered` the stats.
- **Delete paths: full listing then serial deletes** (`src/registry/store.rs:373-380`,
  `uploads.rs:485-492`, `src/http/browse_api/images.rs:200-212`): use `delete_stream`.
- **Tag delete-by-digest re-gets each tag ×3 DB ops** (`src/registry/manifests.rs:328-338`):
  one `scan_prefix`, compare in place.
- **`imagetags` HEAD + GET per tag** (`src/http/browse_api/images.rs:117-125`): 100-tag image =
  100 avoidable HEADs; take size/mtime from the GET's meta.
- **Forwarded requests clone the full HeaderMap+Extensions up front** (`src/http.rs:392-401`):
  the replay is only needed on connect failure — build it lazily in the `Err` arm.
- **Claim/stranded sweeps deserialize every PR ever** (`src/pulls.rs:579,659` via
  `list(db)`): runs on the 15s beat, grows without bound. Filter raw bytes first or keep a
  `merge/queued/{n}` index key.

### Git core / worker
- **`reachable_set_hiding` clones the whole id vec and materializes O(repo) sets**
  (`src/protocol/upload.rs:729-737`; per-ref in `receive.rs:262-284`): kill the clones, hoist
  loop-invariant tips.
- **Pack writing pinned to one thread** (`src/protocol/upload.rs:917` `thread_limit: Some(1)`):
  lift to `None`/small cap — gix clones the odb handle per worker.
- **Filtered-clone blob dedup after header lookup** (`src/protocol/upload.rs:493`): swap to
  `seen.insert(child) && keep_blob(...)` so a blob in K trees pays 1 lookup, not K.
- **Worker mirror-fetches all branches per job** (`src/merge_worker.rs:227-239`): fetch only
  the base and head refspecs the job names.
- **5-8 git spawns per merge** (`src/merge_worker.rs:326-380`): batch the rev-parses into one
  invocation.
- **Merge object write: compress → write → re-read → re-verify → re-read**
  (`src/objects.rs:137-222` + `store.rs:512`): keep `Mode::Verify` but feed the in-memory pack
  via `Cursor` instead of a temp file.
- **`prune_stale_packs` on every repo open, O(entries × packs)** (`src/store.rs:126-154`):
  HashSet the names; gate behind a per-repo timestamp.

### Web
- **About rail fetched (full recursive walk + 50 commits) for every file view, even when
  `hidden`** (`lib/repo-rail.ts:19-22`, `components/repo/file-view.tsx:43-46`): drop from
  FileView or Suspense it.
- **Every blob path shipped to the client for "go to file"** (`code.tsx:109,136`): 10k-file
  repo ⇒ 10k-entry RSC payload; server-side search or cap.
- **`guardRepo` lists the whole namespace to check one repo** (`lib/guard.ts:34-43`,
  `api.ts:57`): add `GET /v1/repos/{owner}/{name}`.
- **Pull list fetches full comment arrays to render a count** (`components/repo/pulls.tsx`):
  return `commentCount`, accept `?state=&limit=`.
- **README waterfall on repo home** (`code.tsx:99-121`): 3 sequential RTTs; speculative
  `README.md` fetch alongside the tree.
- **One `ScrollArea` per diff file + per code block** (`diff-files.tsx:87`,
  `code-block.tsx:10`): 100-file PR hydrates 100 ResizeObservers; plain `overflow-x-auto`.
- **cmdk + radix Dialog in the entry bundle of every page** (`global-search.tsx`):
  `next/dynamic` the dialog.
- **Marketing page 150ms `setInterval` forever** (`marketing/environment-panel.tsx:159-161`):
  pause when hidden, or CSS animation.

### Ops
- **Server requests 96Mi / limit 512Mi**: 5.3× ratio = Burstable + eviction-prone; pack
  indexing spikes risk OOMKill. Measure steady RSS, request ~256-384Mi.
- **`panic = "abort"` in `[profile.release]`**: drops unwind tables; no `catch_unwind` in
  `src/`. Leave test profiles unwinding.

---

## P2 — low, batch opportunistically
- `route_inner` per-request `to_string` + double `format!` (`src/http.rs:259-313`).
- Basic-auth decoded twice per request (`src/auth.rs:198-232`); auth-cache overflow does an
  O(4096) retain under the global mutex on the miss path.
- `keyed_lock` O(n) retain per acquisition (`src/store.rs:39-45` and registry twin) — retain
  only past a threshold, like `neg_cache_miss` already does.
- `events::fields` allocates 8 static keys per publish; `from_fields` linear scan.
- `api_compare`/`merge::perform` sequential `get_ref` pairs → `tokio::join!`.
- GC `get_bytes` extra `to_vec` copy; key Vecs collected then deleted serially.
- `put_tag` double handle resolve + unconditional `touch_image` put; `tags()` redundant sort.
- sha512 digest probe on by-tag manifest push (skip unless the image has sha512 manifests).
- `shallow_walk` per-parent buffer alloc + duplicate decode (`upload.rs:605-615`);
  `tips.contains` O(refs) per want; tags peeled twice with `include-tag`.
- `content.clone()` per upserted file in the patch API (`src/objects.rs:295`) — take changes
  by value.
- Web: per-character `<span>`s in file-search highlighting; `optimizePackageImports` for
  `radix-ui`; web readiness probe 5s → 10s.
- `revoke_tokens_for` serial GETs (by-hand command; fine).

---

## Already-tracked debt (`ponytail:` markers — known ceilings, listed for completeness)

**The biggest one:** the gix#2935 workaround (`src/protocol/upload.rs:825-830`) — merge
commits get a second pass with *whole-tree* expansion, plus a redundant re-decode of every
commit in the range to find merges. This is the dominant cost of fetching a merge-heavy
branch. The correct fix is upstream; the cheap interim wins are (a) capture parent counts
during the first traversal instead of re-decoding, (b) fold into finding P0-8's single
traversal.

Others: chunked upload PATCH re-streams the whole staging object per chunk (O(N×chunks) —
the single biggest push IO amplifier, deliberately traded for stateless sessions);
`complete` re-streams to hash; whole git request/response buffered (`http.rs:822`);
`reachable_set` full enumeration per call; `PackCopyAndBaseObjects` computes no new deltas;
rebase costs a full worktree; worker cache pruned by age not size; `CHECK_LIMIT` flat cap;
auth cache sweep-on-overflow; in-process `keyed_lock`s; `neg_cache`/`recovery_asked` growth
bounds; `image_listing` backfill fallback; various `eprintln` markers.

---

## Suggested order (effort vs payoff)

| # | Item | Effort | Payoff |
|---|------|--------|--------|
| 1 | P0-1 lease-starvation (spawn lanes) | S | availability bug, not just perf |
| 2 | P0-9 tokio/V8 env vars | S | two env vars, fleet-wide |
| 3 | P0-2/3 + P1 HEAD+GET family | S | halves registry round trips |
| 4 | P0-5 mergeability diff + open hoist | S | background sweep cost ~gone |
| 5 | P0-11 web immutable caching | S | every navigation faster |
| 6 | P0-10 compression | S-M | 5-10× on browse payloads |
| 7 | P0-4 repo-open dedup | M | every git/browse request |
| 8 | P0-8 single traversal for clone | M | clone CPU halved |
| 9 | P0-6 GC single-pass | M | sweep IO |
| 10 | P0-7 streamed pack upload | M | push RSS |
| 11 | P0-12 + web P1 batch | M | shell payload, rail, guard |
