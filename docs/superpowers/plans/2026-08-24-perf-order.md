# Performance Fixes — Execution Order

**Spec:** docs/perf-review-2026-08-24.md
**Plans:** 2026-08-24-perf-{server,registry,git-core,web-ops}.md — 40 live tasks
(registry Task 9 is a stub; the same fix lives in server Task 6).

Ordered by effort vs payoff, dependencies respected. Waves are sequential; tasks inside a
wave touch disjoint files and may interleave. Rust plans gate on `cargo test` +
`cargo clippy --lib -- -D warnings`; web on `bun run lint` + `bunx tsc --noEmit` + `bun test`.

## Wave 0 — availability + two-line wins (all small, ship first)
- server 1 — lease lanes off the renewal task (availability bug)
- web-ops 1 — TOKIO_WORKER_THREADS + NODE_OPTIONS (yaml-only, rolls without a repin)
- web-ops 2 — panic = "abort"
- registry 1 — blob pull single GET (hottest registry path)

## Wave 1 — the HEAD+GET family and background-sweep costs
- registry 2 — concurrent manifest-push blob probes
- registry 3 — upload PATCH/complete single read
- server 4 — mergeability drops the discarded diff + per-PR open
- server 5 — merge-job raw-bytes prefilter
- server 6 — imagetags single GET (supersedes registry 9)
- server 8 — join the ref pairs

## Wave 2 — hot-path request costs
- server 2 — index::read joined GETs
- server 3 — visibility check opens the DB once
- registry 5 — drop image_exists tax / put_tag double resolve / tags() sort
- registry 4 — digest-keyed manifest cache
- web-ops 3 — gzip on the JSON hops (new dep: tower-http compression-gzip)
- web-ops 6 — immutable browse caching (security gate inside the task)

## Wave 3 — git core
- git-core 1 — single traversal + merges from the walk (biggest clone win)
- git-core 2 — slice params, no per-ref clones
- git-core 3 — unpin pack writer threads
- git-core 4 — upload.rs micro-batch
- git-core 5 — streamed pack upload
- git-core 6 — prune gate
- git-core 7 — in-memory indexing + by-value patches

## Wave 4 — worker + registry background
- git-core 8 — fetch only the job's branches (mirror fallback for gone heads)
- git-core 9 — batched rev-parses
- registry 7 — GC concurrent reads, single blob listing
- registry 8 — manifest_stat fan-out
- registry 6 — delete-by-digest single scan
- registry 10 + server 7 — delete_stream batching

## Wave 5 — web experience + P2 batches
- web-ops 4 then 8 — get-one repo endpoint, then guardRepo uses it
- web-ops 5 then 9 — commentCount/state/limit, then pulls page uses it
- web-ops 7 — lazy ⌘K
- web-ops 10 — About rail off the critical path
- web-ops 11 — go-to-file cap + README prefetch
- web-ops 12 — plain overflow scroll
- server 9, web-ops 13 — P2 micro batches

## Deploy checkpoints
Wave 0's yaml half rolls immediately (env-only). After Waves 2, 3 and 5: push, wait for CI,
repin SHAs, `kubectl apply`, and re-run the layer bench + a clone/merge timing to measure.
Numbers to watch: registry pull RTTs (halved), clone wall-clock on merge-heavy branches,
mergeability sweep CPU, web TTFB on repo pages.
