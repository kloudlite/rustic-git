# Repo-local data: metadata, pulls, and the listing index

**Date:** 2026-08-22
**Status:** Approved design, pending implementation plan

## First principle

> **Truth is always isolated. Central things are always disposable.**

Every piece of data is one of two kinds:

- **Truth** — authoritative state. Lives in the repo's (or image's) own SlateDB,
  owned by exactly one node, moving and scaling with the repo. Never centralized.
- **View** — derived state serving reads: listings, feeds, caches. May be
  centralized (object-store markers, Redis) **only if deleting all of it loses
  nothing** — it must be rebuildable from truth. The test: "if we FLUSHALL Redis
  and delete every marker, is anything lost?" If yes, it is truth in disguise and
  is in the wrong place.

Cosmos keeps only what is *inherently* global — identity that is not "in" any
repo: `users`, `teams`, `handles`, `credentials`, `passkeys`. The `repos`,
`pulls`, and `counters` collections are removed.

## Goals (from the product owner)

1. Isolated stale repos cost nothing at scale.
2. A repo is easy to shard and move: one prefix in the object store.
3. The merge worker is fed by a persistent queue instead of polling a central DB.
4. Container images gain the metadata scope repos have (visibility, listing
   fields) — which they currently cannot have because reading their DB on a
   non-owning node fences the owner.

Out of scope for this design: backfill/migration of existing Cosmos rows and
existing images (explicitly deferred by the owner); team-membership credential
revocation; any change to git object storage.

## 1. Truth: what moves into the repo's SlateDB

New keys in each **code repo's** database (single writer = the owning node):

| Key | Value | Replaces |
|---|---|---|
| `meta` | description, public, created_by, created_at (same line-based encoding style as `ownership::Entry`) | Mongo `repos` row |
| `pull:{number:08}` | the full `PullRequest` document (serde_json bytes — it nests too deep for line encoding) | Mongo `pulls` row |
| `meta:next_pull` | next PR number | Mongo `counters` row |

Zero-padded pull keys make `scan_prefix("pull:")` return numeric order.

Writes that were Mongo calls become owning-node DB writes, serialized by the
node's existing per-repo single-writer position plus the `keyed_lock` for
read-modify-write sequences (PR numbering: lock `pulls/{owner}/{name}` →
read `meta:next_pull` → write PR + counter):

- `open_pull`, `comment_on_pull`, `request_merge`, `record_mergeability`,
  `claim_merge`, `finish_merge`, `clear_merge` — all become handlers on the
  owning node, reached through the existing routing middleware (`repo_of` →
  `route_inner`). Each gets a `BROWSE_TAILS` route;
  `every_browse_route_is_routable` keeps the router and middleware in step.
- Repo create: routes to the owning node; check-then-create of `meta` is safe
  there because a single writer cannot race itself. Mongo's duplicate-key
  uniqueness is no longer needed.
- The api tier (`src/api.rs`) stops calling `directory` for repos/pulls and
  forwards to the owning node the same way image writes already do.

The image's SlateDB gains one key:

| Key | Value |
|---|---|
| `meta` | public, description, created_by, created_at |

Visibility for images today lives in the image DB already (`image_is_public`);
`meta` consolidates it with the new fields.

## 2. View: the path-encoded listing index

Small **marker objects** in the object store, beside (never inside) the SlateDB
prefix. The database itself never moves on a visibility flip — only the marker.

```
index/public/repo/{owner}/{name}    code repos
index/private/repo/{owner}/{name}
index/public/img/{owner}/{name}     container images
index/private/img/{owner}/{name}
```

- **Path carries the security-relevant fields**: existence and visibility come
  from the listing itself, zero reads. A private name is structurally unable to
  appear under `index/public/`.
- **Body carries the cosmetic fields**: description, created_by, created_at;
  for images additionally manifest count and newest-manifest ms (retiring the
  per-image `manifest_stat` full listing — the N+1).
- Markers are **views**: only ever consulted for listings, never authorization.
  The owning node's DB remains what authorizes (same rule as the old
  `repos.public` mirror, now stated as policy).

**Visibility flip ordering (fail closed).** A flip is two object operations; a
crash between them must never leak. Always remove the more permissive marker
first: public→private = delete public, then write private; private→public =
delete private, then write public. Worst case in either direction is a repo
temporarily missing from listings. A reader that somehow sees both markers
treats the repo as private.

**Who writes markers:** the owning node, immediately after the corresponding DB
write (create, description edit, visibility flip, image push, image delete).
Delete-image becomes: remove marker (instantly gone from listings), then clean
storage at leisure — the ghost-image problem and its heavyweight delete go away,
and the public API stops depending on SlateDB's internal file layout.

**Drift repair:** markers are rebuildable from truth. The GC worker's existing
per-owner sweep gains a reconcile step: for each repo/image it visits, compare
the marker against the DB's `meta` (it is on the owning path or can ask the
owner) and rewrite/remove stale markers. Losing every marker is recoverable.

## 3. Listing reads: layered so the common case is one call

For `/{owner}` (repos) and `/{owner}` images:

1. **Warm (deferred to sub-project 2):** one Redis GET of the assembled
   listing, keyed per owner. This needs a **per-owner generation** ("something
   of alice's changed"), which does not exist yet — today's invalidation
   generation is per repo. That counter is a small mechanism to design
   deliberately with the truth move, not to bolt on; until then every listing
   read takes the cold path, which is already strictly cheaper than today's
   per-image `manifest_stat` walk.
2. **Cold:** one listing per visibility prefix the caller may see (public only
   for strangers; both for the owner/members), then N parallel marker-body
   GETs, assemble. No pagination: an owner's marker set is one prefix scan;
   revisit if an owner ever holds thousands of entries.

No peer fan-out anywhere on the read path: a rolling restart cannot break
listings. Anonymous callers only ever list `index/public/…`.

The `/api/{owner}/images` and `_catalog` handlers keep their any-node property —
they now read markers instead of SlateDB directory presence, which is strictly
safer coupling.

## 4. Events: Redis Streams replace the polling queue

The existing Azure Managed Redis (already a dependency, `redis` crate already in
tree) gains a stream, e.g. `events:{owner}/{name}`-partitioned or a single
`events` stream — final shape chosen at plan time by what XAUTOCLAIM tuning
needs. Semantics:

- **Producers:** owning nodes. On PR open / comment / merge-request / merge /
  close, and on push to a PR's head branch, `XADD` an event after the DB write.
- **Merge worker:** a consumer group (`XREADGROUP` / `XACK` / `XAUTOCLAIM` for
  crashed-consumer redelivery) replaces `pull_to_check`'s findAndModify polling
  claim. The claim semantics move from Mongo's atomic update to the consumer
  group's pending-entries list.
- **Activity feed** (`api.rs` events endpoint, today built from `repos_for` +
  `pulls_across`): becomes a consumer/reader of the same stream — the events are
  literally what the feed renders. Feed history is bounded by stream retention
  (MAXLEN ~ a few thousand per stream); older history is not a goal.

**Queue durability is a prerequisite:** the Redis instance (or a dedicated
logical DB) must run `noeviction`. An eviction policy that can drop stream
entries silently drops queued merge work. Verified at deploy time, not assumed.

**Degradation:** if Redis is down, DB writes still succeed; the event is lost
from the stream. For the worker this parallels today's behavior (a check is
delayed, discovered on the next event or a periodic reconcile scan by the worker
over open PRs it knows); for the feed it is a gap in a disposable view. Redis
outage never blocks a PR operation.

## 5. What each cross-repo consumer becomes

| Today (Mongo) | After |
|---|---|
| `repos_for(owner)` | marker listing (§3) |
| `pulls_for(repo)` | owning node scans `pull:` prefix (routed, node-safe) |
| `pull(repo, n)` | owning node reads `pull:{n}` |
| `pulls_across(repos)` — feed only | stream consumer (§4) |
| `pull_to_check()` — worker queue | consumer group (§4) |
| `counters` | `meta:next_pull` per repo |
| `repos` uniqueness via `_id` | single-writer check-then-create on owning node |

## 6. Consistency model

Truth and views are not kept "in sync" bidirectionally; sync is a one-way,
self-healing flow. Four layers:

1. **One direction, one writer.** Only the owning node writes a repo's marker
   and bumps its Redis generation, immediately after its own DB write. Nothing
   reads a view and writes it back into a database. The DB is always right; the
   only possible defect in a view is staleness, never authority.
2. **Ordering chooses the failure direction.** DB write first, view second — a
   crash leaves a stale listing, not wrong truth. Flips remove the permissive
   marker first, so the crash window hides a repo rather than exposing one.
   A reader seeing both markers treats the repo as private. The worst
   observable inconsistency is "missing from a list for a while."
3. **Detection.** Redis listings are keyed to the existing per-repo generation,
   bumped on every mutating operation, so a stale cached listing dies on the
   first read after any change.
4. **Repair — split by who may safely read which truth.** Two repair loops,
   because the GC worker must never open a repo's or image's database (opening
   one on a non-owning node fences the owner):
   - **Structural repair (GC worker, object-store reads only):** a repo/image
     directory with no marker gains one (created private — fail closed); a
     marker whose directory is gone is removed; stale stats in a marker body
     (manifest count, updated-ms) are rewritten from object-store listings.
   - **Visibility repair (owning node, its own databases only):** each node
     reconciles the markers of repos and images it owns — on open, and on a
     low-frequency lane of the existing renewal loop for what it holds warm —
     comparing the DB's visibility with the marker and rewriting on mismatch.
     The owner is the one party allowed to read that truth, so this closes the
     drift the worker cannot: a crashed flip that left DB and marker
     disagreeing.
   Together the two loops are total: every drift — crashed flip, failed marker
   write, wiped Redis, deleted markers — converges once the owner next touches
   the repo or the sweep next visits it. The repair loops are total precisely
   because views must pass the disposability test (§ first principle): there is
   no view state they cannot rebuild.

5. **Flip serialization.** Visibility flips route to the single owning node,
   so racing flips are two requests on one process: the flip's
   remove-then-write sequence runs under the repo's `keyed_lock`
   (`index/{owner}/{name}`), making interleaved card-swaps impossible rather
   than merely survivable.

Deliberately not provided: transactions across SlateDB + object store + Redis.
Synchronous consistency would couple every repo write to three systems'
availability — a Redis blip must not block opening a PR. Eventual-with-bounded-
repair is safe here because views never authorize; a stale view can mislead a
listing, never grant access.

The sweep period is the drift ceiling and becomes a documented operational
number at plan time ("a listing is at most X minutes stale after a failure"),
not an accident of GC cadence.

## 7. Error handling summary

- DB write succeeds, marker write fails → listing is stale until the reconcile
  sweep; truth is intact. Never the reverse order.
- DB write succeeds, XADD fails → worker misses one nudge; periodic reconcile
  catches it; feed has a gap. Never blocks the user's operation.
- Visibility-flip crash → repo temporarily invisible (fail closed), repaired by
  reconcile.
- Redis FLUSHALL / all markers deleted → listings and feed empty until
  reconcile rebuilds them; no data loss (the disposability test, by design).

## 8. Testing strategy

- Unit: marker path encode/decode; flip ordering (assert the permissive marker
  is never present alongside the new one mid-sequence in a step-driven test);
  pull key ordering; PR number allocation under concurrent open_pull on one
  node (keyed_lock test, like `concurrent_pulls_count_every_hit`).
- Integration: create → listed; flip → moves prefix; delete image → marker gone
  before storage cleanup; anonymous listing never contains a private name (the
  leak test); PR lifecycle end-to-end through the routed handlers; worker
  consumes an event and records mergeability; XAUTOCLAIM redelivery after a
  killed consumer.
- The routing test `every_browse_route_is_routable` extended to the new pull
  routes.

## 9. Sequencing (three sub-projects, in order)

**Order revised 2026-08-22 (ruling):** events move BEFORE the truth move. `pull_to_check` and
`claim_merge` are global Mongo operations the merge worker depends on; moving pulls per-repo first
would leave the worker unable to find work without opening every repo's database — the exact
fencing hazard this design exists to avoid. Streams can be built while pulls are still in Mongo,
so each step stays independently shippable.

1. **Listing index + image metadata scope** — markers for repos and images,
   listing reads switched, image delete simplified. No behavior change for
   PRs. Independently shippable; images gain visibility.
2. **Redis Streams events** (was 3) — worker off polling, feed off `pulls_across`, while pulls
   still live in Mongo. Prerequisite for step 3.
3. **Repo metadata + pulls into SlateDB** (was 2) — the truth move; Mongo `repos`,
   `pulls`, `counters` retired from the write path (reads can dual-run behind a
   flag until cutover).
   - split `api.rs` and `directory.rs` as their Mongo repo/pull halves are deleted
   - close the delete-path lock gap and add structural repair for REPO markers before
     repo listings switch to markers (final-review findings 5 and §6.4)

Backfill of existing Cosmos rows and unmarked images: **deferred** by the
owner; until then, sub-project 2's cutover applies to newly written data and
the reconcile sweep can populate markers for pre-existing repos/images as it
visits them.

**Amended 2026-08-22 at sub-project 3 plan time — three rulings:**

1. **Repo `meta` is discrete `meta/*` keys, not one line-encoded blob.** The repo DB already has a
   `meta/` namespace (`meta/public`, `meta/protect/`); extending it beats adding a second convention
   beside it, and discrete keys make a description edit one `put` instead of a read-modify-write a
   concurrent flip could clobber. Consequence: `meta/public` is ALREADY the authorizing truth, so
   visibility does not move at all — only description, created_by, created_at and the PRs do.

2. **Backfill is no longer deferred for PULLS; it becomes lazy per-repo migration.** Deferring it is
   unsafe once reads cut over: existing PRs would vanish from every repo, and `meta/next_pull`
   starting at 1 against existing PRs 1..n would COLLIDE, overwriting real rows. Instead the owning
   node runs `ensure_migrated` on first touch — idempotent, under the repo lock, marker written
   last. This honours the "no big-bang backfill" intent without losing data. Markers for repos/images
   remain lazily repaired by the sweep as originally written.

3. **The merge worker's safety floor moves to the owning node.** `pull_to_check` is a global Mongo
   scan; once pulls are per-repo, no worker can scan them without opening databases it does not own —
   the fencing hazard this design exists to prevent. §5 named the consumers but did not resolve this.
   Discovery moves to the owner (the only party allowed to read that truth); one shared check
   function serves both the owner's periodic lane (the floor, needing no Redis) and a routed
   endpoint the worker calls on a stream nudge (low latency). The floor gets STRONGER: it no longer
   depends on any central system being reachable.
