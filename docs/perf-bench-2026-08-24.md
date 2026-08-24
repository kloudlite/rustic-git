# Post-deploy performance bench — 2026-08-24

Measured against `kolomi-cluster` / `rustic-git` after the perf-fix roll (master `7ef3e60`, both
images repinned). Method matches the 2026-08-23 baseline: an in-cluster `curlimages/curl` pod
against `http://rustic-git-http.rustic-git.svc:80` for registry work, laptop-side `git` and the
api tier (port-forwarded) for the rest. Random `/dev/urandom` payloads, never a dedupe hit.

## Registry — in cluster

| what | result | baseline (2026-08-23) |
|---|---|---|
| `/v2/` RTT | 6.0–10.5 ms | 6.7 ms |
| 16 MiB push, monolithic | 201, **0.376 s → 42.5 MiB/s** | 0.351 s → 45.6 MiB/s |
| 16 MiB push, chunked (POST+4×PATCH+PUT) | 201, **1.650 s → 9.7 MiB/s** | (laptop-only before) |
| 16 MiB blob GET | 200, **0.234 / 0.169 / 0.154 s** (≈ 100 MiB/s warm) | not measured |
| 16 MiB blob HEAD | 200, 12.7 / 12.3 ms | not measured |
| manifest PUT | 201, 0.316 s | — |
| manifest GET by digest | 12.7 ms cold, then **6.1 / 8.5 / 6.4 ms** | — |
| manifest GET by tag | 13.5 / 97.5 ms | — |
| `tags/list` | 9.4 / 6.3 ms | ~100 ms over Cloudflare |
| `_catalog` | 32.1 / 28.8 ms | — |

Monolithic push is level with the baseline (0.376 s vs 0.351 s — inside run-to-run noise for a
single sample; nothing regressed). The interesting new rows are the reads: **the digest-keyed
manifest cache shows up exactly as designed** — first read 12.7 ms, subsequent reads 6.1–8.5 ms,
i.e. down to bare RTT once cached. The by-tag path is not cached (tag→digest is mutable), which
is why its second sample is slower; that is the intended split.

Chunked at 9.7 MiB/s vs 42.5 monolithic is the known `ponytail:` ceiling — each PATCH re-streams
the staging object, so chunked is O(N×chunks) by construction. Unchanged by this work.

## Git

| what | result |
|---|---|
| push 200 commits (20 files churned) | **1.62 s** |
| fresh clone, 200 objects | **1.31 s** |

## Worker merges — click to landed, end to end

| strategy | outcome | wall |
|---|---|---|
| merge (diverged) | merged | **2.26 s** |
| squash | merged | **1.07 s** |
| rebase | merged | **1.25 s** |

Comparable to the 2026-08-23 figures (1.9 / 1.0 / 1.0 s) — the worker's narrower fetch (base+head
refspecs instead of a full mirror) did not cost latency on a small repo, and its benefit only
shows on repos with many branches, which this synthetic one does not have.

## Compression (new)

| endpoint | identity | gzip | ratio |
|---|---|---|---|
| `/v1/repos?owner=bench` | 581 B | 188 B | **3.1×** |
| `/v1/activity?owner=bench&limit=50` | 6392 B | 1334 B | **4.8×** |
| `/v1/repos/bench/b1/pulls` (empty list) | 2 B | 2 B | not compressed (below threshold) |

`content-encoding: gzip` confirmed live on the public app origin too. Byte-route exclusion holds:
blob/manifest and pack responses are untouched.

## Web

Landing page 0.117 / 0.123 s. Repo pages 307 to sign-in for an anonymous caller, so authed page
TTFB was not measured here — the immutable-caching win (oid-keyed browse fetches) needs a signed-in
session to observe and is best judged from real traffic.

## Reading these numbers

Most of this branch's wins are **round-trip eliminations**, not throughput gains, so they show up
in the read rows (manifest cache, blob single-GET) and in work that no longer happens at all (the
mergeability sweep's discarded diff, the per-PR repo open, the second commit traversal per clone).
A single-sample synthetic bench on a small repo cannot show the sweep or big-repo clone wins; those
need production traffic or a large fixture repo. Nothing regressed against the baseline.
