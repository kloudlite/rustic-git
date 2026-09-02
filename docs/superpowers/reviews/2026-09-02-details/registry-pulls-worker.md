# Review — `crates/registry`, `crates/pulls`, `bins/worker`, `browse_api/pulls.rs`

Read-only. Files read whole: registry `lib/auth/blobs/store/uploads/manifests/gc/routes/referrers`,
pulls `lib/merge_worker/pulls{mod,model,jobs,check}`, `bins/worker/src/main.rs`,
`bins/server/src/browse_api/pulls.rs`, plus `bins/server/src/router/route.rs:355-395` for the
`/v2` routing refusal.

## Load-bearing rules: verified, no violations found

- **Only two blob deletion paths.** Grep of every `.delete(` in the two crates confirms exactly
  two touch `blobs/`: `blobs.rs:218` (client DELETE) and `gc.rs:332` (sweep). `delete_manifest`
  (`manifests.rs:443`) deletes only the manifest object and DB rows; `delete_image`
  (`store.rs:634`) deletes only the image's `repo/img/…` DB prefix.
- **Manifest bytes verbatim.** `put_manifest` stores `body` unmodified (`manifests.rs:236`); the
  parsed `v` is pruned only for the presence walk and never re-serialized. `manifest_response`
  returns the stored bytes (or the cached copy of them).
- **`Digest::parse` is the only path segment → object key.** Every `blob_path`/`manifest_path`
  call site takes a parsed `Digest`; `store.rs:28-42` rejects uppercase, wrong length, unknown
  algo, and anything with a second colon or a `.`. `valid_uuid` (`uploads.rs:232`) forbids `.`,
  so a session id can never name its own sidecar.
- **OCI envelope everywhere.** `routes.rs:238` `oci_envelope` re-wraps axum's own plain-text
  client errors (413 from `DefaultBodyLimit`, 405, the 404 in `route.rs:372`), and it is the
  outermost layer so it sees the body-limit rejection.
- **Anonymous ≠ invalid.** `RegistryToken::{Owner,Anonymous,Invalid}` (`routes.rs:271`) and
  `auth::caller` (`auth.rs:78-91`) keep them apart, with a unit test at `routes.rs:298`.
- **Body limits.** Three distinct caps, correctly separated: `max_layer` on blob routes
  (`routes.rs:200`, plus `pour`'s own streaming count — the layer only matters if a `Bytes`
  extractor reappears), `MAX_MANIFEST` on the manifest route (`routes.rs:213`), git's `max_body`
  elsewhere.
- **Peer secret containment.** `local()`/`networked()` split (`merge_worker.rs:301`/`324`) holds:
  no `format!` of a networked argv anywhere; `fetch` at line 446 names only the URL. Asserted by
  `a_failed_networked_call_never_names_the_secret` (line 1068).
- **No command injection into git.** Every user-controlled string reaches git as a *prefixed*
  argv element (`refs/heads/{b}`, `+refs/heads/{b}:refs/heads/{b}`,
  `--force-with-lease=refs/heads/{b}:{oid}`), never as a bare arg that could be read as an option,
  and never through a shell. `out()` uses `Command`, not `sh`.
- **Unbounded memory on layers.** `pour` bounds at `(1+IN_FLIGHT)*5 MiB`; hashing is off the
  runtime via `BlockingHasher`; `get_blob` streams (`blobs.rs:100`) rather than buffering.
- **GC keep-bias.** `referenced()` aborts on an unreadable *or* unparseable manifest
  (`gc.rs:58,73`), the double-read closes the mount race (`gc.rs:328`), and `sweep_stale_uploads`
  skips entries it cannot read.

## Findings

### Medium

**M1 — `max_layer` default (10 GiB) exceeds the S3 CopyObject cap the fast path relies on.**
Category: correctness. `crates/registry/src/uploads.rs:872-876` (copy), `blobs.rs:24-27` (default).
On the multipart fast path the assembled blob is verified on the staging key and then `copy`d into
`blobs/`, a server-side CopyObject S3 caps at 5 GiB — but `max_layer` accepts 10 GiB, so a 6 GiB
layer passes every size check, is uploaded, is re-read and hashed (O(N)), and then dies with a 500
`UNKNOWN` at the very last step. The ponytail comment at `uploads.rs:691-697` names the ceiling but
the shipped default sits above it. Fix: default `max_layer` to `5 GiB` (one line at `blobs.rs:26`),
or clamp it to 5 GiB when `store.mp.is_some()`.

**M2 — the blob-row backfill runs inline on a stranger's pull, unbounded and 500-on-error.**
Category: performance / availability. `crates/registry/src/store.rs:210-242`.
The first stranger pull of a pre-rows image LISTs and GETs *every* manifest of that image inside
the blob request, serially (no `buffered` bound, unlike `gc::referenced` and `gc::stats_of` which
both cap at 16), with no lock — so N concurrent first pulls each do the whole walk N times before
any of them writes `BLOB_ROWS_BACKFILLED`. Worse, `store.os.get(&loc).await?` (line 224) propagates
a transient object-store error, turning a pull into a 500 where the honest answer is a 404. Fix:
take `store.keyed_lock(&format!("blobrows/{owner}/{name}"))` around the walk, use
`futures::StreamExt::buffered(16)` as `referenced()` does, and treat a failed GET as "not held".

**M3 — `base`/`head` are never validated as branch names when a change is opened.**
Category: correctness. `bins/server/src/browse_api/pulls.rs:160-192`.
`title` is capped at 200 chars and a comment body at 10 000, but `base`/`head` are only `trim()`ed
and stored. They flow to the worker's git argv (`merge_worker.rs:420`, `551`, `654`, `657`) and to
`store.get_ref(repo, &format!("refs/heads/{}", pr.base))` in `check.rs`. No injection results (see
above), but a change opened with a name git will never accept is permanently unmergeable while
still burning a claim, a fetch and a full worker job on every merge request, and re-announcing
every 30 s (`jobs.rs:ANNOUNCE_EVERY`). Fix: at open time reject anything failing the
`check-ref-format` basics — empty, leading `-`, `..`, ASCII control/space, any of `~^:?*[\`, a
trailing `.lock`, or >255 bytes — with the 400 the other field checks already use.

**M4 — unbounded fan-out where the same code elsewhere is deliberately bounded.**
Category: performance. `crates/registry/src/routes.rs:52` (`join_all` of `manifest_stat`, one LIST
per unmarked image) and `crates/registry/src/manifests.rs:189-193` (`join_all` of up to two HEADs
per declared digest — an index can name thousands). `gc.rs:216-223` already has `stats_of` with
`STAT_CONCURRENCY = 16` and its comment says exactly why. Fix: route both through a bounded
`buffered(16)`; `stats_of` is reusable for the first one verbatim.

**M5 — `api_pull_mergeability` accepts a verdict from any peer with no claim check.**
Category: correctness. `bins/server/src/browse_api/pulls.rs:521-554`.
`api_pull_outcome` guards against a lapsed-lease worker's late report by matching `?by=` against
`claimed_by` (line 463) and documents why. The verdict route has no equivalent, so a slow lane's
stale `Dirty`/`Clean` can overwrite a newer lane's answer on the same change. Self-healing (the
next `check` rewrites it) and peer-only, hence Medium not High. Fix: stamp the verdict with the
`base_oid`/`head_oid` it was computed from and drop it if `pr.mergeability`'s tips have moved —
cheaper than a claim token and it is the invalidation rule the row already uses (`model.rs:70-74`).

### Low

**L1 — `delete_blob` leaves the image's hold rows behind.** `crates/registry/src/blobs.rs:206-226`.
The object goes; the `image/blob/{digest}/{via}` rows in the image DB stay, so `image_holds_blob`
keeps answering `true` for bytes that are gone. Harmless today (the store answers 404 and the
handler maps it to `BLOB_UNKNOWN`), but it is the mirror image of `forget_manifest_blobs`, which
exists for exactly this reason on the manifest path. Fix: call the same prefix sweep for
`image/blob/{d}/` after a successful delete.

**L2 — `?n=0` looks like the end of the catalog.** `crates/registry/src/lib.rs:118-127`.
`paginate` returns an empty page, and `truncated` is `page.last()` — `None` — so no `Link` header
is emitted even though the list is not exhausted. A paging client stops. Fix: compute `truncated`
from `rest`, not from `page` (`(page.len() < rest.len()).then(|| page.last().or(q_last))`), or
treat `n=0` as absent like the non-numeric case already is.

**L3 — a pull is counted before the manifest is known to exist.**
`crates/registry/src/manifests.rs:326-334`. `bump_pulls` fires on tag resolution; if the manifest
object is gone the request 404s at line 354 and the pull has still been counted. Display-only
number, hence Low. Fix: move the bump after the bytes are fetched.

**L4 — two implementations of "delete every row whose key ends `/{digest}`".**
`crates/registry/src/store.rs:182-195` (`forget_manifest_blobs`) and
`crates/registry/src/referrers.rs:63-74` (`unindex`) are the same scan-prefix / suffix-match /
delete loop over two different prefixes, and `delete_manifest` calls both back to back
(`manifests.rs:422,436`). Fix: one `delete_suffixed(db, prefix, &format!("/{d}"))` helper.

**L5 — the merge cache lock is held across the whole job.** `bins/worker/src/main.rs:255-256`.
`keyed_lock("merge/{owner}/{name}")` is taken before the branch and held through a merge that
`job_timeout` allows to run 25 minutes, so every other event for that repo — including cheap
mergeability nudges that never touch the cache — queues behind it in whichever lane picks them up.
Deliberate (the comment says the lock is "really about" the claim), so noting rather than
recommending: if it bites, take the lock only around the git work and let the claim's lease do the
mutual exclusion it already does across pods.

**L6 — `catalog`'s `last` is passed through unstripped when it carries no `{who}/` prefix.**
`crates/registry/src/routes.rs:157-161`. A client that pages with a bare name (or another owner's
prefixed name) gets it used verbatim as the marker name. Cosmetic; the answer is still owner-scoped.

**L7 — test gap: nothing asserts a manifest DELETE leaves its blobs alone.**
`tests/registry_manifests.rs` covers the media-type row (line 134), the tags (line 98) and the
cache (line 752); `tests/registry_gc.rs:50-67` covers a *sibling* still needing a shared layer. The
rule the crate doc puts first — a manifest path never deletes a blob — has no direct test. One
assertion in `deleting_a_manifest_by_digest_drops_its_media_type_row`: `head(blob_path(...))` still
`is_ok()` after the DELETE. Same shape of gap for the `?n=0` case in L2.

## Architecture notes

- The registry's split is clean and the comments are load-bearing rather than decorative: nearly
  every non-obvious branch names the bug it exists for, and the ones I chased (`declared_chunk`'s
  `/total` suffix, `RegistryToken::Anonymous`, `landed_anyway`'s squash arm) all check out.
- One real pattern to unify: bounded concurrency. `gc.rs` gets it right twice and states the
  reason; `routes.rs` and `manifests.rs` use `join_all` for the same shape of work (M4).
- `ImageExt`/`UploadsExt` are orphan-rule workarounds, not speculative abstractions — each has one
  impl because `Store` is foreign, and the doc comments say so. No dead `pub` items found: every
  exported item in both crates has a caller in `bins/` or `tests/`.
- The `merge_worker` `local()`/`networked()` split is the strongest thing here; it is one rule, it
  is testable, and it is tested. Worth copying anywhere else a credential enters an argv.
- Config that nobody sets: `RUSTIC_GIT_MERGE_CMD_TIMEOUT`/`_JOB_TIMEOUT`,
  `RUSTIC_GIT_UPLOAD_GRACE_SECS`, `RUSTIC_GIT_MAX_LAYER` are all absent from `deploy/`. That is
  fine for escape hatches — but `max_layer`'s default is the one that is actually wrong (M1).
