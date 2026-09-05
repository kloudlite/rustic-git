# Performance and scalability audit — kloudlite

Date: 2026-08-27. Scope: git server hot paths, SlateDB storage, OCI registry, browse API and listings, routing/ownership, Redis events, merge worker, workspaces control plane (agent controller, `/v1`, engine), gateway, api tier (Mongo), memory/concurrency, web app, deploy manifests. Every claim is from the code as read (library claims checked against vendored `slatedb-0.15.0` and `kube-runtime-4.2.0`). No files modified.

Relation to `docs/superpowers/audit-2026-08-25.md`: its P1 (chunked upload O(N·K)) is fixed for chunks ≥ 5 MiB but the fallback still bites (P-19); P2 (no SlateDB cache) is fixed (shared 256 MiB block + 64 MiB meta cache, `pool/mod.rs:135-159`); P3 (`reachable_set`) is still open (P-1); P5–P7 (agent long-poll) are obsolete — the agent is a controller now.

Summary: **17 high, 24 medium, 12 low** (53 findings).

---

## High

### [P-1] Every incremental fetch and every push enumerates every object in the repo
Impact: high — O(objects) CPU + odb reads per `git fetch` (any fetch with a `have`) and per `git push` (any push whose new tree references an existing blob, i.e. all of them). A 1M-object monorepo pays a full walk per fetch, on a blocking thread, per client.
Location: `/Users/karthik/kloudlite/crates/git/src/protocol/upload/mod.rs:183` (`ours` on every non-empty `haves`), `:340-378` (`reachable_set` = `TreeContents` count of the whole repo), `/Users/karthik/kloudlite/crates/git/src/protocol/receive.rs:273-286` (`unexplained` → `reachable_set(old_tips)`).
What: `have` filtering and non-tip `want` checks answer "is X reachable from our refs" by materialising the full closure into a `HashSet`. A `have` is a commit in practice, so a commit-only walk (`gix_traverse::commit::Simple` over tips, O(commits)) answers it. On push, `unexplained` is always non-empty for an incremental push (unchanged subtrees), so `reachable_set` runs on every push; a cheaper proof is "every unexplained object is reachable from `old_tips` without leaving trees the pack does not contain" — or simply trust objects that live in a pack listed in the repo's own `pack_index` (objects are per-repo now, `store.rs:139-145`; the fork-network sharing that motivated the isolation check is gone).
Fix: fetch — commit-set membership for `have`s (`Simple::new(tips)` collected once per fetch), keep the full closure only for non-commit wants. Push — accept any unexplained object present in this repo's own indexed packs (`pack_object_ids` over `pack_index`) and fall back to the full walk only when that fails. Both are ponytail-marked.
Effort: M
Measure: `time git fetch` on an up-to-date clone of a repo with 500k objects, before/after; CPU seconds per fetch in the srv pod.

### [P-2] Git HTTP buffers the entire request and the entire response pack in memory, with no concurrency cap
Impact: high — a clone of a 2 GiB repo builds a 2 GiB `Vec<u8>` before the first byte leaves; three concurrent 500 MB pushes (each `Bytes` body + `KLOUDLITE_MAX_BODY` 512 MiB × measured 3× peak) exceed the 4 Gi srv limit and OOM-kill every repo on that pod.
Location: `/Users/karthik/kloudlite/bins/server/src/router/git.rs:219-249` (`upload_pack`: `body: Bytes`, `let mut out = Vec::new()`), `:251-282` (`receive_pack`), `:339-348` (`success` returns the `Vec`); `/Users/karthik/kloudlite/deploy/kloudlite.yaml:149-156` (limit sized for ONE push); no `Semaphore` in `bins/server`, `crates/git`, `crates/app`.
What: ponytail-marked "stream when repos get big". The SSH path already streams (`ssh.rs:220-262`, `SyncIoBridge`), so the protocol code takes `&mut dyn Write` — only the HTTP adapter buffers. Upload-pack response: pipe the blocking writer into a `tokio::io::duplex`/`mpsc` and return `Body::from_stream`. Receive-pack request: keep `Bytes` for the retry-after-fence path but cap concurrency.
Fix: (1) `tokio::sync::Semaphore(2)` around receive-pack per pod, 503 + `Retry-After` when full (S); (2) stream the upload-pack response through a `SyncIoBridge` over a duplex pipe, exactly as SSH does — the fence retry only applies before the `packfile` line, so buffer up to that point only (M).
Effort: S + M
Measure: `container_memory_working_set_bytes{pod=~"kloudlite-srv.*"}` during 3 parallel 500 MB pushes; TTFB of `git clone` on a 1 GiB repo.

### [P-3] Packs accumulate one per push forever; repack is offline-only
Impact: high with age — a repo with 5k pushes has 5k packs: every odb lookup probes O(packs) indices, `open_repo` stats 5k files (and downloads all of them on a node move or pod restart, emptyDir cache), `gix_odb::at` is re-run per request, and `PackCopyAndBaseObjects` never computes deltas so clones stay fat.
Location: `/Users/karthik/kloudlite/crates/git/src/gc.rs:1-8` ("runs from `admin`, i.e. with the server stopped"), `/Users/karthik/kloudlite/crates/storage/src/store.rs:146-148` (`odb()` = `gix_odb::at` per call), `:386-398` (`open_repo` fetches every pack, 8 at a time), `/Users/karthik/kloudlite/crates/git/src/protocol/upload/pack.rs:130` (no new deltas).
What: No automatic consolidation exists. The pack index is a DB scan + a `metadata()` per pack on every request; a pod restart re-downloads the whole repo before the first request is answered (cold start O(repo size)).
Fix: a per-repo lane on the owning node (it already holds the `lock/repack` key and single-writer guarantee): when `pack_index().len() > 32`, run `repack_locked` under `spawn_blocking` off-peak; switch `build_pack` to `Mode::PackCopyAndBaseObjects` + `thread_limit: None` delta generation once. Keep the `admin` entry point.
Effort: M
Measure: pack count per repo (`pack_index().len()`, log it); `git clone` wall time and pack size for a 1k-push repo before/after; `open_repo` latency after a pod restart.

### [P-4] `docker pull` by tag does a durable SlateDB write under a per-tag lock on the GET path
Impact: high — every manifest GET by tag; concurrent pulls of one tag serialise behind one lock and each waits a WAL flush (≥100 ms tick + object-store PUT).
Location: `/Users/karthik/kloudlite/crates/registry/src/manifests.rs:281-283` (`bump_pulls` awaited before answering), `/Users/karthik/kloudlite/crates/registry/src/store.rs:290-305`; `Db::put` → `await_durable: true` verified in vendored `slatedb-0.15.0/src/config.rs:482-486`.
What: a display-only counter costs the hottest read path a durable write and a lock.
Fix: `tokio::spawn` the bump, or an `AtomicU64` per `(owner,name,tag)` flushed by the 30 s marker lane with `WriteOptions { await_durable: false }`.
Effort: S
Measure: p50/p99 of `GET /v2/{o}/{n}/manifests/{tag}`; `docker pull` wall time with 20 concurrent pullers of one tag.

### [P-5] Manifest PUT is ~6 sequential durable DB writes plus 4 object-store round trips
Impact: high — 500 ms–1 s per manifest; a multi-arch push is N× that.
Location: `/Users/karthik/kloudlite/crates/registry/src/manifests.rs:169-223`, `/Users/karthik/kloudlite/crates/registry/src/store.rs:217-224,353-366,387-406`, `/Users/karthik/kloudlite/crates/storage/src/index.rs:136-149` (marker read = 2 GETs) and write (delete + put).
What: media-type put, referrer put, 2 tag puts, count + newest puts, each a separate WAL flush; then `refresh_image_marker` hits the object store four times.
Fix: one `WriteBatch` for all DB rows (one flush); `await_durable: false` for the marker counters.
Effort: M
Measure: `elapsed` around `put_manifest`; `wal/` objects created per manifest push.

### [P-6] Lease renewal is O(warm repos) sequential durable puts on the leader under one global lock
Impact: high — N nodes × 64 warm repos renew every 3 s; 320 serialised durable puts per beat at 30–50 ms each exceeds `RENEW_EVERY`, starving cold claims that are on client request paths and share `leader_lock`.
Location: `/Users/karthik/kloudlite/crates/app/src/lib.rs:589-606` (`grant_renew`), `:553-587` (`grant_claim` does `ownership.all()` when the leader asks), `:443-452` (`prune_once` full scan every 10 s).
Fix: one `WriteBatch` per renew message, or `await_durable: false` for renewals (advisory, TTL-bounded); keep durable for grant/release.
Effort: S–M
Measure: `grant_renew` duration per beat (log with count); `cluster/ownership/wal/` object rate; lease-lost evictions under load.

### [P-7] `open_repo` does two object-store GETs and three redundant DB reads on every request
Impact: high — every git and browse request on the owner (a page load is 5–10 browse calls). On a warm repo the pack fetch is a stat, so the two `index/` GETs dominate.
Location: `/Users/karthik/kloudlite/crates/storage/src/store.rs:345-405` (`repo_exists`, `is_public`, `reconcile_marker` → `index::read` = 2 GETs, `pack_index` scan, per-pack `metadata()`), `/Users/karthik/kloudlite/bins/server/src/router/git.rs:54,64` (`repo_public` already did `exists` + gets).
Fix: throttle `reconcile_marker` per repo (an `Instant` map on `Store`, once per few minutes — the 30 s lane already covers warm repos); pass `public` from `open` into `open_repo`; cache the `pack_index` result per open pool handle and invalidate in `record_pack`/`forget_pack`.
Effort: S
Measure: object-store GETs on `index/` per browse request (wrap `os` with the `CountingStore` from `app/src/lib.rs:662`); browse `refs` p50.

### [P-8] `index::list` fetches every marker body with unbounded `join_all`, no pagination, no cache
Impact: high — `/v2/_catalog`, `/api/{owner}/images`, `/v1/repos`, `/v1/activity`, anonymous `/v1/teams/{slug}/profile`, GC reconcile: an owner with thousands of repos fires thousands of concurrent GETs per call; `paginate` slices the already-materialised list.
Location: `/Users/karthik/kloudlite/crates/storage/src/index.rs:171-221`; `/Users/karthik/kloudlite/crates/registry/src/lib.rs:117-130`; callers `/Users/karthik/kloudlite/crates/api/src/repos.rs:36-53`, `teams.rs:438,494`, `feed.rs:137`.
Fix: `buffer_unordered(32)`; cache the decoded listing per owner in Redis (short TTL, dropped on create/delete/flip — the api tier is the writer); longer term one per-owner listing object rewritten by the owning node.
Effort: M
Measure: GET count per `/api/{owner}/images` vs marker count; `/{owner}/repos` latency at 500 repos.

### [P-9] GC sweep lists every blob of every owner every pass and reads every manifest body twice; reconcile fans out one LIST per image unbounded
Impact: high at scale — 10k manifests / 5k images per owner = 10k GETs + a full blob LIST per ~65 s pass plus 5k concurrent `manifest_stat` LISTs.
Location: `/Users/karthik/kloudlite/crates/registry/src/gc.rs:37-84` (`referenced`), `:278-285`, `:313`, `:169,190` (`join_all`); `/Users/karthik/kloudlite/bins/worker/src/main.rs:434-435,482-523`.
Fix: `buffered(16)` in `reconcile_owner`; skip `manifest_stat` when the marker is fresher than the image prefix; make the second `referenced()` incremental (only manifests newer than the first listing); raise `GC_PASS_GAP` to 10–15 min once owners > ~500.
Effort: M
Measure: per-owner sweep duration and GET/LIST counts (log them); Azure `Transactions` by `ListBlobs`.

### [P-10] Web: `AutoRefresh` re-renders every layout and page of every open tab every 10 s
Impact: high — api-tier baseline load is `tabs × 3–6 calls / 10 s` with nobody touching anything; 100 idle repo tabs ≈ 50 req/s of `getRepo` + `listTeams` + `refs`.
Location: `/Users/karthik/kloudlite/web/apps/web/src/components/app/auto-refresh.tsx:27-44` mounted at `/Users/karthik/kloudlite/web/apps/web/src/app/(shell)/layout.tsx:26`; per-tick cost `components/app/app-shell.tsx:64`, `app/(shell)/[owner]/[repo]/guard.ts:34`, `components/repo/code.tsx:84`.
What: `router.refresh()` re-runs every server component; `staleTimes.dynamic` does nothing here. Per-row `FastRefresh` already covers transitional rows.
Fix: mount `AutoRefresh` only on pages with server-changing state (workspaces, environments, PR merge state); 30–60 s if kept global; SSE from the `events` stream later.
Effort: S
Measure: api request rate per web replica with N idle tabs.

### [P-11] Workspaces: flat 120 s download deadline turns a large layer fetch into a PERMANENT restore failure
Impact: high — any stream layer bigger than ~120 s × link bandwidth (≈2.4 GB at 20 MB/s) settles the Volume as `Error/FetchFailed` and is never retried.
Location: `/Users/karthik/kloudlite/crates/workspaces/src/engine/blob.rs:53,130-142` (`get_bytes`), `/Users/karthik/kloudlite/crates/workspaces/src/engine/ops.rs:523-536`, `/Users/karthik/kloudlite/bins/agent/src/controller.rs:636-644` (`FETCH_FAILED` → permanent).
Fix: stream the body with a per-chunk inactivity deadline (as the block path does at `ops.rs:461-476`); classify timeouts `Transient`, only 403/404 permanent.
Effort: S
Measure: restore a ≥3 GB workspace on a `tc`-throttled node; watch `.status.conditions` for `FetchFailed`.

### [P-12] Stream-layer restore buffers every missing layer fully in RAM, all at once
Impact: high — chain of up to 50 layers × 256 MB fetched cold = tens of GB resident; OOM-kills the agent DaemonSet pod (which has no memory limit, P-17).
Location: `/Users/karthik/kloudlite/crates/workspaces/src/engine/ops.rs:523-546`, `/Users/karthik/kloudlite/crates/workspaces/src/engine/blob.rs:362-383` (`receive_into(&[u8])`).
Fix: stream each layer into `btrfs receive` stdin, prefetch ≤ 2 ahead with `buffered(2)`; hash while streaming.
Effort: M
Measure: agent container `memory.current` during a 40-layer restore.

### [P-13] Staged-layer upload reads the whole file into memory and single-request PUTs it
Impact: high — first push of a big workspace fails outright above the 5 GiB single-PUT limit, and holds the blob in RAM meanwhile.
Location: `/Users/karthik/kloudlite/crates/workspaces/src/engine/blob.rs:344-347` (`upload_file` → `std::fs::read` → `put_bytes`), called from `ops.rs:316`; `upload_stream` (`blob.rs:185-254`) already streams multipart but is only used by squash.
Fix: route `upload_file` through the existing `WriteMultipart` path.
Effort: S
Measure: push a workspace whose first layer compresses to > 5 GiB; agent RSS during push.

### [P-14] Api tier: every browse request pays two Mongo round trips BEFORE the Redis cache is consulted
Impact: high — Cosmos RU and latency on the critical path of every cache hit; private repos are never cached at all for members.
Location: `/Users/karthik/kloudlite/crates/api/src/browse.rs:240-257` (order: identity → `may_act_under` → visibility → cache), `/Users/karthik/kloudlite/crates/api/src/repos.rs:92-106` (`users.find_one` + `teams.find_one`), `browse.rs:293-303`.
Fix: visibility first; public + cache hit → serve without membership; cache `(email, owner) → bool` 30–60 s; allow caching private bodies for authenticated owners (only reachable after a fresh membership check).
Effort: M
Measure: Cosmos RU/min vs api RPS; `/api/{o}/{n}/tree/…` p50 on a warm cache.

### [P-15] `users.username` has no index — `user_by_handle` is a collection scan on request paths
Impact: high as users grow — workspace create, key add/remove, `list_ws` with a missing Secret each scan `users`.
Location: query `/Users/karthik/kloudlite/crates/pulls/src/directory/mod.rs:548-553`; `ensure_indexes` `mod.rs:566-616` has no `users` entry; callers `crates/api/src/credentials.rs:290-292` ← `bins/api/src/main.rs:54` ← `crates/workspaces/src/api.rs:744,633,788,710`.
Fix: `IndexModel` on `{username: 1}` unique + sparse in `ensure_indexes`.
Effort: S
Measure: `db.users.find({username:"x"}).explain("executionStats").totalDocsExamined`.

### [P-16] Gateway: two uncached kube API GETs per connect, before the limit check, with no global connection cap on a 128 Mi pod
Impact: high — a reconnect storm after a `Recreate` roll drives API-server load; per-owner limit (100) × owners is unbounded, ~100 KiB per tunnel fills 128 Mi at ~1.2k tunnels and drops every SSH session on the node.
Location: `/Users/karthik/kloudlite/bins/gateway/src/resolve.rs:24-47`, `/Users/karthik/kloudlite/bins/gateway/src/tunnel.rs:144-157` (resolve at 144, reserve at 155), `:191,217` (64 KiB buffer + `to_vec` per frame); `/Users/karthik/kloudlite/deploy/k3s/gateway.yaml:126-131`.
Fix: reserve before resolve; one `Semaphore(~1000)` around the pump; limit 256–512 Mi; `Bytes::copy_from_slice`; `set_nodelay(true)`.
Effort: S
Measure: `apiserver_request_total{resource="workspaces"|"pods",verb="get"}` vs `tunnel closed` lines; gateway working-set vs tunnel count.

### [P-17] Merge worker's bare-clone cache is age-pruned only on a 5 Gi emptyDir; rebase checks out a full worktree there
Impact: high — one large monorepo merged within a week exceeds `sizeLimit` → kubelet evicts the pod, killing every in-flight merge and the GC lane.
Location: `/Users/karthik/kloudlite/crates/pulls/src/merge_worker.rs:137-158` (`prune`, ponytail "no size accounting"), `:643-691` (rebase worktree), `:289-295` (full-history fetch), `/Users/karthik/kloudlite/bins/worker/src/main.rs:468` (7-day keep), `/Users/karthik/kloudlite/deploy/kloudlite.yaml:852-854`.
Fix: byte budget in `prune` (LRU by `.last-used` until < 60 % of `sizeLimit`); raise `sizeLimit` to 20 Gi meanwhile; try `--filter=blob:none` fetches (measure).
Effort: M
Measure: `du -sh /var/cache/kloudlite/merge` in the worker; `kubelet_evictions{eviction_signal="ephemeralstorage"}`.

---

## Medium

### [P-18] Chunked blob upload fallback re-streams the whole session on every PATCH; a session that starts small never gets the fast path
Impact: medium-high — clients chunking below 5 MiB and all of `file://` dev mode: a 2 GiB layer in 1 MiB chunks moves ~2 TB.
Location: `/Users/karthik/kloudlite/crates/registry/src/uploads.rs:444-449` (gate `have == 0 && announced >= MIN_PART`), `:455-470` (`pour(src.chain(body))`).
Fix: start the multipart on the first PATCH regardless of size; the existing sidecar tail (capped at `MIN_PART`) absorbs sub-part bytes.
Effort: M
Measure: S3 GET bytes on `uploads/` per PATCH with 1 MiB chunks.

### [P-19] Upload completion reads the whole blob back to hash it, then single-op copies (fails > 5 GiB on S3)
Impact: medium — 2× layer bytes egress per chunked push; `max_layer` (10 GiB) cannot be honoured by `CopyObject`.
Location: `/Users/karthik/kloudlite/crates/registry/src/uploads.rs:778-808`, ponytail at `:625-631`; `crates/registry/src/blobs.rs:22-28`.
Fix: persist running SHA state in the sidecar per PATCH; multipart copy for > 5 GiB or write parts straight to the final key (digest is in the PUT URL).
Effort: L
Measure: S3 GET bytes on `uploads/` during `complete`; a 6 GiB push against S3.

### [P-20] Per-warm-repo background lanes: three lanes each open/scan every warm repo
Impact: medium (bounded by `KLOUDLITE_MAX_WARM`=64) — ~256 background S3 GETs/min/node idle plus a full PR-row scan of every warm repo every 15 s.
Location: `/Users/karthik/kloudlite/bins/server/src/lanes.rs:57-59,100-126,142-154,169-208`, `/Users/karthik/kloudlite/crates/pulls/src/pulls/check.rs:156-168`, `crates/pulls/src/pulls/jobs.rs:136-158`.
Fix: a `meta/has_merge_jobs` flag so the 15 s beat is one `get`; share the P-7 reconcile throttle; skip `open_repo` when `open_only` is empty.
Effort: M
Measure: background S3 GET/LIST rate on an idle node; lane pass duration (log it).

### [P-21] `imagetags` does one full manifest GET + 3 DB reads per tag, unpaginated, bypassing the manifest cache
Impact: medium — image page with hundreds of tags = hundreds of manifest downloads per load.
Location: `/Users/karthik/kloudlite/bins/server/src/browse_api/images.rs:110-143`.
Fix: consult `store.manifests()` first; store size/pushed_ms rows at `put_manifest` time; add `n`/`last`.
Effort: M
Measure: `/api/{o}/{n}/imagetags` latency vs tag count.

### [P-22] Manifest cache: clear-on-full at 256 entries, up to 1 GiB worst case
Impact: medium — > 256 hot manifests on a node thrash the whole cache; 256 × 4 MiB = 1 GiB RSS ceiling.
Location: `/Users/karthik/kloudlite/crates/registry/src/manifests.rs:319-327`, `/Users/karthik/kloudlite/crates/storage/src/store.rs:39-40`.
Fix: bound by bytes (64 MiB) with a `VecDeque` of keys for oldest-first eviction.
Effort: S
Measure: hit ratio counter; srv RSS on a node hosting > 300 images.

### [P-23] `vol_agent` authenticates every request by fetching all regions from Cosmos
Impact: medium — every agent push does an uncached Cosmos read; Cosmos blip = vol registry down.
Location: `/Users/karthik/kloudlite/bins/server/src/vol_agent.rs:62-72`.
Fix: `Mutex<(Instant, Vec<Region>)>` with a 30–60 s TTL on `JobsState`.
Effort: S
Measure: Cosmos request count vs agent push rate.

### [P-24] `volumes` listing LISTs every SST/WAL object of every volume DB under the owner
Impact: medium — thousands of objects per Snapshots page load, growing with pushes and compaction.
Location: `/Users/karthik/kloudlite/bins/server/src/browse_api/volumes.rs:64-81`.
Fix: `list_with_delimiter` for names; an `index/` marker per push for `latest_ms` (already noted at `:36-38`).
Effort: S–M
Measure: objects returned per `/api/{owner}/volumes` vs volume count.

### [P-25] `api_pulls` deserialises every PR (with comment bodies) per list page
Impact: medium — thousands of PRs: full scan + JSON decode per list.
Location: `/Users/karthik/kloudlite/bins/server/src/browse_api/pulls.rs:106-133`, `/Users/karthik/kloudlite/crates/pulls/src/pulls/model.rs:227-234`.
Fix: reverse bounded scan; `pull/open/{n}` secondary key; comments out of the summary row.
Effort: M
Measure: `api_pulls` latency vs PR count.

### [P-26] Cold claim path: leader round trip with up to 20 × 1.5 s retries on the request path
Impact: medium — a leader roll pins one axum task per cold repo for up to 30 s each.
Location: `/Users/karthik/kloudlite/crates/app/src/lib.rs:224-306,494-549`, `/Users/karthik/kloudlite/crates/core/src/peer.rs:47-48`.
Fix: by design; add a per-node semaphore (64) on concurrent leader asks so bursts degrade to fast 503s.
Effort: S
Measure: in-flight `ask_leader` gauge; latency during a leader roll.

### [P-27] Feed: `XREVRANGE` capped at 100 global entries, then up to 40 sequential peer round trips
Impact: medium — busy fleets: an owner's events fall out of the last 100; `/v1/activity` p50 = 2 × repos × RTT (~0.5 s at 20 repos in-cluster).
Location: `/Users/karthik/kloudlite/crates/api/src/feed.rs:154-171,185-223`; `crates/storage/src/events.rs:69`.
Fix: `buffer_unordered(8)` over repos; route `refs`/`log` through the browse cache; larger `XREVRANGE` with early exit.
Effort: S
Measure: `/v1/activity` latency vs repo count.

### [P-28] Agent: 3 cluster-wide unfiltered watches per node; Volume(`mine`) watch opened 4×; SnapshotRequest reflector unbounded and linearly scanned
Impact: medium now (2 nodes), high with N nodes — O(N_nodes × cluster objects) API-server watch load and agent memory.
Location: `/Users/karthik/kloudlite/bins/agent/src/controller.rs:254` (all StatefulSets), `:271,324` (all SnapshotRequests), `:304` (all Workspaces), `:217,243,267,334` (Volume mine ×4), `:328,370-379`.
Fix: label-select StatefulSets (`kloudlite.io/kind=environment`), `placed` selector for the bindings watch, a `stop-of` label for env SnapshotRequests, share one Volume reflector via `watches_stream`, index the snapshot store by `spec.volume`.
Effort: S for selectors, M for sharing/indexing
Measure: `apiserver_longrunning_requests{verb="WATCH"}` per resource; agent RSS.

### [P-29] SnapshotRequests are never garbage-collected; every workspace/environment listing scans all of an owner's pushes
Impact: medium → high over time — ~9k objects/year per hourly-pushing workspace in etcd and every agent's memory; `list_ws`/`get_ws`/`list_env`/`get_env` LIST them all.
Location: `/Users/karthik/kloudlite/crates/workspaces/src/api.rs:429-440,779,812,1334,1350,1468-1486`; only `stop-*` deleted (`controller.rs:2390`).
Fix: ownerReference to the Volume + delete `done` requests older than N days; replace `pushed_volumes` with `Volume.status.lineageTip.is_some()` (written at `controller.rs:1155`).
Effort: M
Measure: `kubectl get snapshotrequests | wc -l` over time; `/v1/workspaces` p50.

### [P-30] Agent janitor runs btrfs/losetup/read_dir on the async reactor, O(V²)
Impact: medium — every 10 min; hundreds of volumes × `btrfs subvolume delete` stalls all in-flight reconciles on a 2-vCPU node.
Location: `/Users/karthik/kloudlite/bins/agent/src/lib.rs:158-210,247,260,313-319`.
Fix: one `spawn_blocking` for the sweep; read lineage files once into a map; single `losetup -l -J`.
Effort: S
Measure: reconcile latency spikes aligned to the 600 s interval.

### [P-31] Converged workspace/environment reconciles re-apply every child object on every pass
Impact: medium — ~10 API writes per workspace per pod event; 8 + 4·S per environment.
Location: `/Users/karthik/kloudlite/bins/agent/src/controller.rs:1555-1594,1529,1791,1640-1644,1804-1826,1876-1880,1899-1911`.
Fix: when `observed_generation == gen && phase == Ready` do only liveness reads; periodic `requeue(10 min)` resync keeps self-heal; drop the double pod GET and the legacy Deployment GET.
Effort: M
Measure: `apiserver_request_total{verb="PATCH",resource=~"persistentvolume.*|statefulsets"}` at steady state (≈0).

### [P-32] Agent `error_policy` is a flat 60 s requeue — no backoff
Impact: medium — every failed object on every node retries in lockstep during an outage; misconfigured objects log forever at 1/min.
Location: `/Users/karthik/kloudlite/bins/agent/src/controller.rs:40,383-388`; contrast `build_failed_backoff` `:1342-1350`.
Fix: derive backoff from the condition timestamp (60 s → 1 h, jittered).
Effort: S
Measure: `reconcile failed, requeueing` rate during an induced API-server outage.

### [P-33] `find_snapshot` / `volume_owner` are serial N+1 over HTTP; CommitRecord lineage prefixes make `history` O(n²)
Impact: medium — restore for a member of T teams × V volumes = serial `history` calls each returning H×L lineage entries (≈50k entries, MBs of JSON at 1k pushes); `volume_refs` needs one id but pulls everything; `volume_history` is O(H²).
Location: `/Users/karthik/kloudlite/crates/workspaces/src/api.rs:1032-1051,1758-1768,1821-1848`, `/Users/karthik/kloudlite/crates/workspaces/src/engine/ops.rs:335-346`, `/Users/karthik/kloudlite/crates/workspaces/src/upstream.rs:81-88`.
Fix: `buffered(8)` fan-out; `?limit=`/`tip` on the server-tier history route; `HashMap<chain_hash, id>` in `volume_history`.
Effort: S + M
Measure: `/api/{owner}/{name}/volumehistory` response size vs push count; restore latency.

### [P-34] `POST /v1/workspaces` blocks the request up to 5 s polling for placement
Impact: medium — 0.5–5 s and 1–10 GETs per create; holds api tasks under load.
Location: `/Users/karthik/kloudlite/crates/workspaces/src/api.rs:657-675`.
Fix: `tokio::spawn` the wait-and-install, or move key install into the OwnerBinding reconciler (ponytail at `:683-684`).
Effort: S
Measure: `POST /v1/workspaces` p50/p99.

### [P-35] Mongo: `repos.owner` count scan on team delete; boot-time unanchored regex scan; `signins`/`cli_logins`/`invites` never swept
Impact: medium — `cli_logins` is fed by an anonymous endpoint and grows at whatever rate the internet pokes it.
Location: `/Users/karthik/kloudlite/crates/pulls/src/directory/teams.rs:287`, `mod.rs:634` (`$regex "[A-Z]"`), ponytails at `mod.rs:385,397`, `teams.rs:347`; `credentials.rs:459`.
Fix: drop the stale `repos` gate; one-time marker for the fingerprint repair; periodic `delete_many({expiresAt < now})` on one replica (Cosmos TTL is `_ts`-only).
Effort: S each
Measure: `db.cli_logins.countDocuments()` over a week; `explain().totalDocsExamined`.

### [P-36] Web: expired api token re-minted on every server-component render and thrown away
Impact: medium-high after 12 h sessions — 2–3 `POST /v1/users` per render, per 10 s tick, until re-login.
Location: `/Users/karthik/kloudlite/web/apps/web/src/auth.ts:169-183`; RSC branch drops `Set-Cookie` in vendored `next-auth/lib/index.js:105-107`; `src/lib/api-token.ts:15-23` reads the old token.
Fix: align `session.maxAge` with the 12 h api token and delete the refresh branch (keep `trigger === "update"`).
Effort: S
Measure: `POST /v1/users` per minute vs sessions older than 12 h.

### [P-37] Web: `force-cache` browse reads write to a read-only filesystem on every miss; cache is per-pod memory (50 MB), cold after every roll, keyed per user
Location: `/Users/karthik/kloudlite/web/apps/web/src/lib/browse.ts:44-51`, `/Users/karthik/kloudlite/deploy/kloudlite-web.yaml:114` (no volumes); vendored `next/dist/server/lib/incremental-cache/file-system-cache.js:303-311`.
Fix: `emptyDir` at `/app/apps/web/.next/cache`; fetch oid-keyed objects of PUBLIC repos without the token so entries are shared.
Effort: S / M
Measure: `grep -c 'Failed to update prerender cache'` in web logs; repo-home TTFB first vs second hit after a roll.

### [P-38] Web: serial waterfalls — `/settings` (4 + 3×owners sequential), environments list (sequential lists + `volumeHistory` per archived volume), PR pages refetch the full diff on every tab/tick, `listTeams` uncached and called twice per render
Location: `/Users/karthik/kloudlite/web/apps/web/src/app/(shell)/settings/page.tsx:20-37`, `app/(shell)/[owner]/(org)/environments/page.tsx:20-51`, `app/(shell)/[owner]/[repo]/pulls/[number]/pull-data.ts:18-41`, `lib/owners.ts:14-21`, `lib/api.ts:137`.
Fix: `Promise.all`; `cache()` on `listTeams`; snapshot count in `/v1/volumes`; PR stat summary + oid-keyed diff on the Files tab only.
Effort: S (+M for PR diff)
Measure: `/v1/teams` count per `/settings` render (should be 1); api→web bytes per PR view.

### [P-39] Web: `call()`/`get()` have no timeout
Impact: medium under failure — a hung api pod pins renders; with the 10 s refresh every tab stacks a new hung render; 384 MB heap × 2 replicas runs out.
Location: `/Users/karthik/kloudlite/web/apps/web/src/lib/api.ts:39-57`, `src/lib/browse.ts:48-51`.
Fix: `signal: AbortSignal.timeout(5_000)` (15 s for `commit`/`compare`); map `TimeoutError` to `unavailable`.
Effort: S
Measure: web pod memory during an api stall.

### [P-40] Worker: a merge over 30 s is `XAUTOCLAIM`ed by a sibling lane while still running; local git subprocesses have no timeout
Impact: medium — redelivered merges re-run fan-out checks and fetches; a stuck `merge-tree`/rebase holds a lane for up to 30 min until liveness restarts the whole pod.
Location: `/Users/karthik/kloudlite/bins/worker/src/main.rs:55,193-217,243-244`; `/Users/karthik/kloudlite/crates/pulls/src/merge_worker.rs:168-170,383-386,663-678`.
Fix: XACK before processing (the owner's claim is the record); wall-clock kill on `out()`; touch the heartbeat between fan-out items.
Effort: S
Measure: 409s on `claim?by=` in srv logs; worker restart count.

### [P-41] srv memory limit vs concurrent pushes; termination grace shorter than the ingress timeout; readiness ≠ ownership settled
Location: `/Users/karthik/kloudlite/deploy/kloudlite.yaml:149-156,35,285,174-207`; `/Users/karthik/kloudlite/deploy/kloudlite-web.yaml:149-150` (600 s `proxy-read-timeout`).
What: covered by P-2 for memory; `terminationGracePeriodSeconds: 90` cuts a push > 75 s mid-pack during a roll.
Fix: grace ≥ 300 s; `minReadySeconds: 30` on both StatefulSets.
Effort: S
Measure: 5xx on `git-receive-pack` during `kubectl rollout status sts/kloudlite-srv`.

---

## Low

### [P-42] Registry SHA-256 of layers runs on tokio worker threads
Location: `/Users/karthik/kloudlite/crates/registry/src/uploads.rs:254-256,794-796`. Fix only if profiling shows it: hash 5 MiB parts in `spawn_blocking`. Effort: S. Measure: worker busy % with two 5 GiB pushes in flight.

### [P-43] `last_changes` decodes each commit's tree twice (`at(parent)` this iteration = `at(id)` next), up to 2000 commits per request
Location: `/Users/karthik/kloudlite/crates/git/src/browse.rs:254-303`, budget at `bins/server/src/browse_api/repo.rs:203`. Fix: carry `before` forward as next `now`. Effort: S. Measure: `lastmod` latency on a deep directory.

### [P-44] Redis: one `XACK` round trip per entry on a shared 250 ms-timeout connection
Location: `/Users/karthik/kloudlite/crates/storage/src/cache/disk.rs:96-129`, `cache/mod.rs:31-34`, `bins/worker/src/main.rs:205-217`. Fix: batch acks; separate `ConnectionManager` for stream commands. Effort: S. Measure: `consumer group ack failed` rate.

### [P-45] Api tier: `ssh-keygen` and PGP verification run on the async runtime; no verification cache
Location: `/Users/karthik/kloudlite/crates/api/src/credentials.rs:752-768,861`, `crates/api/src/signatures.rs:188`, `crates/api/src/gpg.rs:139-205`. Fix: `spawn_blocking`; cache `Verification` per `(repo, sha)`. Effort: S.

### [P-46] `teams_for` loads full Team documents (member arrays) to return slugs, per request
Location: `/Users/karthik/kloudlite/bins/api/src/main.rs:25-27`, `crates/pulls/src/directory/teams.rs:1016-1025`. Fix: `projection {_id:1}`. Effort: S. Measure: RU per `for_user`.

### [P-47] Browse cache miss stampede (no single-flight)
Location: `/Users/karthik/kloudlite/crates/api/src/browse.rs:264-308`. Fix: per-key `OnceCell` single-flight. Effort: S. Measure: upstream `refs` rate for one repo at 200 concurrent.

### [P-48] Every agent GETs the Volume for every new SnapshotRequest cluster-wide; `restore_gate` LISTs pods 40× per reconcile
Location: `/Users/karthik/kloudlite/bins/agent/src/snapshot.rs:49-56,109`, `controller.rs:2017-2029,1882-1889,1961-1973`. Fix: read from the shared reflector (P-28); `requeue(2 s)` instead of spinning. Effort: S.

### [P-49] Agent DaemonSet has no `resources`; api/web have no PDB or anti-affinity; worker roll has zero merge capacity and a cold cache
Location: `/Users/karthik/kloudlite/deploy/k3s/agent-daemonset.yaml:79-189`, `deploy/kloudlite.yaml:618-737,773`, `deploy/kloudlite-web.yaml:12-115`. Fix: requests/limits on the agent (measure a push first); copy the srv anti-affinity + `PDB maxUnavailable: 1`; worker `maxSurge: 1, maxUnavailable: 0`. Effort: S.

### [P-50] GC lane re-lists three prefixes every 60 s regardless of activity
Location: `/Users/karthik/kloudlite/bins/worker/src/main.rs:434-435,482-490`. Fix: raise `GC_PASS_GAP` past ~500 owners; skip reconcile for owners with no event since last pass. Effort: S.

### [P-51] Web: repo home fetches the whole blob list per page for the rail; 716 KB TTF heading font; unbounded repo/workspace lists filtered client-side; `/` dynamic though signed-out content is static; `getSession()` not `cache()`d (2–3 decrypts per request)
Location: `/Users/karthik/kloudlite/web/apps/web/src/lib/repo-rail.ts:18-22`, `app/layout.tsx:21-27`, `lib/api.ts:275,614,724,766,892`, `app/(shell)/page.tsx:8-14`, `lib/session.ts:23-41`. Fix: `/languages/{oid}` + capped path list; woff2; `cache()` on `getSession`/`apiToken`; paginate when an owner passes a few hundred repos. Effort: S–M.

### [P-52] kl: three API calls before the SSH handshake (`list` + `ssh_session` ×2)
Location: `/Users/karthik/kloudlite/bins/kl/src/ws.rs:16-32`, `bins/kl/src/proxy.rs:11`. Fix: pass the session via env to the ProxyCommand child. Effort: S. Measure: `time kl ws ssh <ws> true`.

### [P-53] Unbounded `recovery_asked` map; `Gateway::spend` O(n) retain per connect
Location: `/Users/karthik/kloudlite/crates/app/src/lib.rs:51-53`, `bins/gateway/src/tunnel.rs:60-65`. Both bounded in practice; no action.

---

## Verified good

Git server and storage
- Pack building is `TreeAdditionsComparedToAncestor` (incremental fetch is O(added), with the gix#2935 merge workaround) and the traversal runs once per fetch and is shared with `include-tag`: `crates/git/src/protocol/upload/pack.rs:55-75`, `mod.rs:261-282`.
- All protocol work runs on `spawn_blocking`; `block_in_place` inside it is a no-op on a blocking thread: `bins/server/src/router/git.rs:141,231,264`, `crates/git/src/ssh.rs:220`, `browse_api/mod.rs:74`. Client disconnect aborts pack builds: `git.rs:100-107`.
- Pack upload/download stream in 5 MiB parts with 4 in flight; downloads fsync before rename: `crates/storage/src/store.rs:471-504,591-632`. Push indexes locally and uploads only after connectivity passes: `receive.rs:311-324`.
- Connectivity check hides accepted tips so a multi-branch push walks shared history once: `receive.rs:250-291`. Ancestry check for protected branches is budgeted (50k) and off-thread: `crates/gitbase/src/refs.rs:733-761`.
- SSH: channel cap 16, 600 s inactivity timeout, exec-only: `ssh.rs:14-19,49-77`. Peer forwarding streams both directions: `crates/core/src/peer.rs:112-160`.
- SlateDB pool: single-flight open, bounded `max_warm` with LRU eviction, shared 256 MiB block + 64 MiB meta cache: `crates/storage/src/pool/mod.rs:135-220`. No `std::sync::Mutex` guard held across an `.await` in `pool/`, `store.rs`, `auth.rs`, `manifests.rs`, `app/src/lib.rs`.
- Credential lookups cached 60 s with a bounded negative cache; token check is one SHA-256: `crates/storage/src/auth.rs:17-89`. Negative repo cache prevents LIST + leader ask per sprayed name: `crates/app/src/lib.rs:157-188`.
- Routing reads the ownership map locally (no network per request when the entry is live): `crates/storage/src/ownership/mod.rs:337-348`.
- Health probe hysteresis, 5 s timeout: `store.rs:306-332`. Release profile shipped is `lto=thin, codegen-units=1, panic=abort`: `Cargo.toml:168-174`, `Dockerfile:30-39`.

Registry
- Blob download streams from the object store; monolithic upload streams via `WriteMultipart` with incremental hashing and an abort on refusal: `crates/registry/src/blobs.rs:80-88`, `uploads.rs:209-271`. Fast-path chunked upload sends each part once: `uploads.rs:135-176,498-543`.
- Manifest count/newest are DB rows (no LIST per push); referrers are an indexed scan; `image_delete` uses `delete_stream`: `store.rs:328-346,503-507`, `referrers.rs:86-124`.
- GC is keep-biased, bounded `buffered(16)`, paced per owner: `gc.rs:37-84,269-323`.

Events and worker
- `XADD MAXLEN ~ 5000`; non-blocking `XREADGROUP` with 2 s idle; `XAUTOCLAIM` every 60 s: `crates/storage/src/events.rs:69`, `cache/disk.rs:96-110`, `bins/worker/src/main.rs:193-213`. Every consumer has a Redis-free floor.
- Fan-out checks fetch once then run local `merge-tree`; fetch is named-refspec and pruned; networked git has low-speed timeouts and the peer secret never reaches error text: `merge_worker.rs:223-234,261-295`. Merge is idempotent on retry.

Workspaces
- btrfs/nix/ssh-keygen work is on `spawn_blocking` with its own runtime: `bins/agent/src/controller.rs:622-723,1309-1321`, `snapshot.rs:198-208`. Status writes are change-guarded everywhere; `heal_labels` patches only on diff; claims are optimistic with two attempts.
- `/v1` lists use label selectors; clients (kube, Cosmos, reqwest, Mongo pool 16) are created once at boot with timeouts: `crates/workspaces/src/api.rs:384-392`, `bins/api/src/main.rs:78-172`.
- Block-layer restore and squash upload both stream with backpressure: `engine/ops.rs:457-507`, `engine/blob.rs:185-254`. Push batches records into one POST + one ref move.

Api tier and web
- Browse cache is generation-keyed, body-capped (1 MiB), upstream capped (8 MiB), private answers never cached: `crates/api/src/browse.rs:139-144,264`, `forward.rs:5-14`. JWT verified once per request. `describe` resolves members in one `$in`.
- Web: `/api/health` is a 204 with no work; React `cache()` dedupes layout+page reads; oid-keyed reads are `force-cache`; parallel fetches where independent; ⌘K palette is `next/dynamic`; highlighting is server-only with a 200 KB cap; per-icon lucide imports; 1.4 MB total chunks, largest 229 KB; 27 `loading.tsx`; `AutoRefresh` pauses on hidden tabs; standalone Docker image with heap capped under the 512 Mi limit.

Deploy
- No CPU limits (deliberate), `TOKIO_WORKER_THREADS=4`, startup/liveness split prevents WAL-replay crash loops, srv/leader PDBs and anti-affinity, `KLOUDLITE_MAX_WARM=16` × 64 MB bounded under 4 Gi, emptyDir cache (measured Multi-Attach outage removed), ingress `proxy-body-size: 0` / 600 s timeouts / request buffering off, per-IP rate limits: `deploy/kloudlite.yaml:59-79,174-207,224-231,293-301,499-521,919-926`, `deploy/kloudlite-web.yaml:37-38,95-111,147-159`.
