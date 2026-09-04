# kloudlite-git — Rust code quality, correctness and best-practices audit

Scope: `crates/{core,storage,gitbase,pulls,app,git,registry,api,workspaces}`, `bins/{server,api,worker,agent,gateway,kl}`, `tests/`. Date: 2026-08-29, HEAD `8c0b4e0`.
Method: every `.rs` in each slice read by one of five parallel readers; the five highest-severity claims were re-verified by hand against the source before ranking. Mechanical checks run directly: `cargo clippy --workspace -- -D warnings` (clean), `cargo clippy --all-targets` (6 test-target warnings), `cargo tree -d`, grep passes for `unwrap/expect/panic`, `std::sync::Mutex`, unbounded channels, `println!`, `ponytail:` (89 markers), `TODO/FIXME` (0).

Counts: **bug 5 · high 13 · medium 34 · low 49** (101 findings).

---

## Bugs

### [Q-1] Forwarded `/vol-agent/*` writes skip authentication on any multi-node fleet
Severity: bug
Location: /Users/karthik/kloudlite-git/bins/server/src/router/mod.rs:55-57; /Users/karthik/kloudlite-git/bins/server/src/vol_agent.rs:142,165,185
What: `route_public` runs before any handler and forwards a `/vol-agent/{owner}/{name}/commits|ref|history` request to the owner's PEER listener, adding the peer secret. `peer_router` mounts `vol_agent_routes()` under `Extension(PeerVouched)` with `JobsState::new(None)`, and the handlers do `if vouched.is_none() && !authorized_for(..)`. Nothing on the forwarding node checked the agent token, region scope or source CIDR first (auth lives in the handler, which never ran there). Net: an unauthenticated POST to `/vol-agent/alice/ws/ref` rewrites volume history whenever the LB lands on a non-owner — (N-1)/N of requests. The comment "the peer secret is strictly stronger" is only true for requests that originated on the peer network. `tests/vol_agent.rs` is single-node and never forwards. Verified by reading `router/mod.rs`.
Fix: Drop `PeerVouched`. Build `JobsState` once in `serve()` and hand the same `Arc` to both routers; the owner re-runs `authorized_for` on the forwarded `Authorization`/`WS_AGENT_HEADER` headers, which `Forwarder::forward` already passes verbatim. Add a two-node test in `tests/routing.rs`: POST without a token to the non-owner, expect 401.
Effort: S

### [Q-2] `POST /v1/repos` deletes an existing repository when the upstream create times out
Severity: bug
Location: /Users/karthik/kloudlite-git/crates/api/src/repos.rs:187-213
What: A send error (including the 15 s `UPSTREAM_TIMEOUT`) maps to `status = 0`, which falls into `other =>` and fires `/api/{owner}/{name}/delete` on the owning node. If the name already existed and the 409 was simply lost or slow, the rollback deletes the live repo. The arm's comment assumes "the create got far enough to claim the name" and never distinguishes "existed before this request".
Fix: Issue the rollback delete only on a definite non-201/non-409 status; on `Err(_)` return 502 and delete nothing. Optionally make the node's create idempotent by `created_at_ms` and have delete refuse a mismatch.
Effort: S

### [Q-3] Gateway per-owner limit frees a live tunnel's slot on every refused connect
Severity: bug
Location: /Users/karthik/kloudlite-git/bins/gateway/src/tunnel.rs:105-112 (`reserve`), :75-80 (`Drop for Slot`), :83-93
What: `reserve` builds `Slot` after the per-ws `take` but before the per-owner `take`. When the owner is at `MAX_PER_OWNER`, `take` returns false without incrementing, `then_some` drops the `Slot`, and `Drop` calls `release(per_owner)` — decrementing a count this slot never held. Hammering the limit erases the accounting for real tunnels.
Fix: Do both `take`s first; construct `Slot` only if both succeed, calling `release(per_ws)` explicitly on the second failure (or carry `owner_taken: bool` in `Slot`).
Effort: S

### [Q-4] Thin-pack push can lift a sibling fork's private objects into this repo
Severity: bug
Location: /Users/karthik/kloudlite-git/crates/git/src/protocol/receive.rs:400-418
What: `write_pack` passes the fork-network pool odb as `thin_pack_base_object_lookup`. `upload/mod.rs:176-178,205-211` exist precisely because "existence in the pool says nothing about whether THIS repo may see the object", but the receive side has no such check. `git push` is `--thin` by default; a pusher can send a ref-delta whose base is any oid in a sibling's history, gix materialises the base into the new pack, its id lands in `pushed` (:229), connectivity passes, and the next fetch serves the sibling's bytes. `cannot_claim_sibling_object_as_tip` covers only the tip case.
Fix: Wrap the lookup in a `gix_object::Find` that records every id it served; after indexing, require each recorded id ∈ `reachable_set(&odb, &old_tips)` (already computed lazily at :282), else reject with "missing necessary objects". Add a thin-pack test whose base is a victim-only blob.
Effort: M

### [Q-5] GPG "verified" trusts the key's self-asserted UID, not the registrant
Severity: bug
Location: /Users/karthik/kloudlite-git/crates/api/src/gpg.rs:199-204; /Users/karthik/kloudlite-git/crates/api/src/signatures.rs:188-206
What: `judge_pgp` calls `gpg::verify(.., &signed.author_email)` which checks `verified_emails(&key).contains(author)` — UIDs the key holder signed themselves. It never compares `known.created_by`; the SSH path does (`judge_ssh`, :231). `add_key` (`credentials.rs:333,346`) does not pin UIDs to the registrant either. Bob registers a key with UID `alice@…`, signs commits authored as Alice, and the badge reads `verified` with `signer: bob`.
Fix: In `judge_pgp`, require `known.created_by.eq_ignore_ascii_case(author_email)` exactly as `judge_ssh` does (keep the UID check as an additional condition if desired).
Effort: S

---

## High

### [Q-6] `open()` releases the lease on a failed open while the SlateDB handle is still warm
Severity: high
Location: /Users/karthik/kloudlite-git/bins/server/src/router/git.rs:70-82; contract at /Users/karthik/kloudlite-git/crates/app/src/lib.rs:454-457
What: On a non-fence `open_repo` error (e.g. pack download failing at `store.rs:392-397`) the handler calls `app.release(&repo)`, but `open_repo` already opened the DB via `repo_exists → db_for` (`store.rs:349`) and the pool keeps it. `App::release` requires "the caller must already have CLOSED the database". The map says unowned, another node claims and opens, and this node's live handle is fenced — the two-writer window CLAUDE.md's first invariant forbids. The renewal beat then re-grants it, so ownership flaps.
Fix: `pool.evict(&owner,&name).await` before `app.release`, or drop the release and let the renewal loop handle it.
Effort: S

### [Q-7] Marker/pull lanes treat warm `vol/…` databases as git repos owned by `vol`
Severity: high
Location: /Users/karthik/kloudlite-git/bins/server/src/lanes.rs:101-106, 143-148, 170-174
What: `warm_repos()` yields `img/` and `vol/` keys; the lanes strip only `img/`. A warm volume becomes `Kind::Repo, owner="vol", name="alice/ws-1"`; `is_public` opens the same DB and `reconcile_marker` writes `index/private/repo/vol/alice/ws-1` every 30 s; `check_owned_pulls`/`announce_stranded_merges` scan volume DBs for `pull/` rows. Lanes are only tested with git repos warm.
Fix: One `fn kind_of(key) -> Repo|Img|Vol` used by all three lanes; test that warming a volume then running `reconcile_owned_markers` writes no `index/*/repo/vol/` object.
Effort: S

### [Q-8] Advertised `atomic` push is not atomic across connectivity failures
Severity: high
Location: /Users/karthik/kloudlite-git/crates/git/src/protocol/receive.rs:14-15, 256-301
What: `CAPS` advertises `atomic`, but a ref failing the connectivity walk is just dropped from `owned` and the survivors are applied. `atomic_push_rejects_whole_batch` (tests/protocol.rs:501) passes only because a third ref also fails inside `update_refs_txn`.
Fix: After the connectivity loop, if any `results[i].is_some()`, mark every other update "atomic push failed", delete the pack, skip `update_refs`. Extend the test to two refs.
Effort: S

### [Q-9] `check_repo` never recomputes mergeability past the 25 lowest-numbered open PRs
Severity: high
Location: /Users/karthik/kloudlite-git/crates/pulls/src/pulls/check.rs:14,162; /Users/karthik/kloudlite-git/crates/pulls/src/pulls/model.rs:259-270
What: `open_only(&db, CHECK_LIMIT)` returns the same 25 rows every pass; `Unchanged` rows consume slots and there is no cursor, so the ponytail's "leaves the tail to the next pass" never happens. PR #26+ keeps a stale merge verdict.
Fix: Iterate all open rows and cap only those whose tips moved, or persist `meta/check_cursor`.
Effort: S

### [Q-10] Registry multipart fast path is dead on the production backend (Azure)
Severity: high
Location: /Users/karthik/kloudlite-git/crates/storage/src/config.rs:82-87; /Users/karthik/kloudlite-git/crates/registry/src/uploads.rs:14-17, 455-470
What: `object_store_views` sets `mp = Some(..)` only for `mem://` and `s3://`; `az://` leaves `mp = None`, so every PATCH takes the fallback: re-streaming the whole staged session through a fresh multipart per chunk (O(N·K)), and `complete` re-reads it again. All fast-path tests run on `InMemory`.
Fix: Build the Azure client via `MicrosoftAzureBuilder` and set `mp = Some(s.clone())` like the S3 branch; log at `open_store` which backend lacks `MultipartStore`; fix the uploads.rs module doc.
Effort: S

### [Q-11] Workspace push reads the whole staged delta into RAM; pull buffers every missing layer concurrently
Severity: high
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/engine/blob.rs:344-347 (from ops.rs:316); /Users/karthik/kloudlite-git/crates/workspaces/src/engine/blob.rs:130; /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:524-537
What: `upload_file` is `std::fs::read` + one `put`; the squash threshold is evaluated after upload, so the first push of a large seeded workspace stages and then loads the full compressed delta. `pull_core` fetches each missing stream layer as `Vec<u8>` and spawns them all at once.
Fix: Stream the stage file through the existing `upload_stream` (`blob.rs:185`); in `pull_core` fetch sequentially and pipe into `btrfs receive` stdin.
Effort: M

### [Q-12] A stream layer slower than the flat 120 s `GET_TIMEOUT` is settled `FETCH_FAILED` permanently
Severity: high
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/engine/blob.rs:53,130-142; /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:572,636-644
What: `get_bytes` wraps the whole `.bytes()` collect in one deadline (the block path bounds per chunk); `permanent_reason` maps `FETCH_FAILED` to `settle(Permanent)` → `await_change()`, so the volume is never retried.
Fix: Per-chunk deadline for stream layers; treat timeouts as transient, keep only 404/403 permanent.
Effort: S

### [Q-13] Agent restart mid-stop wedges the environment with no user-facing way out
Severity: high
Location: /Users/karthik/kloudlite-git/bins/agent/src/snapshot.rs:134-145; /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:2087-2103, 1789
What: `apply_snapshot` marks a `working` request with no in-flight handle as `Error/AgentRestarted` and never re-runs it. `await_stop_push` sees `Error` on the fixed-name `stop-{env}` request and returns `await_change()`. Only a `Done` deletes the request, `/v1` has no delete route for SnapshotRequests; only `kubectl delete` unwedges it.
Fix: In the `Error` arm of `await_stop_push`, delete the failed `stop-{env}` request so the next pass creates a fresh one (or key it by generation).
Effort: S

### [Q-14] CLAUDE.md's "RBAC — not convention — stops a controller editing desired state" is false
Severity: high
Location: /Users/karthik/kloudlite-git/deploy/k3s/agent-rbac.yaml:30-43; /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:463-479, 2034-2038; /Users/karthik/kloudlite-git/CLAUDE.md "Workspaces and environments"
What: `heal_labels` needs `patch` on the parent resources, so the agent can merge-patch any spec field; `restore_gate` genuinely writes `Volume.spec.restoreTo`; the agent has full verbs on `volumes`. The yaml's own ponytail admits it. The doc states a security property the deployment does not have.
Fix: Add the ValidatingAdmissionPolicy the ponytail names, or correct CLAUDE.md and `crd.rs:11-12` to "the /status split protects against ACCIDENTAL spec writes".
Effort: S (doc) / M (VAP)

### [Q-15] Blocking process/crypto work inside async `/v1` handlers
Severity: high
Location: /Users/karthik/kloudlite-git/crates/api/src/credentials.rs:752-768 (called :861), :318,:333; /Users/karthik/kloudlite-git/crates/api/src/gpg.rs:139-205 (from signatures.rs:159,188)
What: `generate_ed25519` runs `ssh-keygen` via `std::process::Command::output()` plus sync `fs::read_to_string` on `GET/POST /v1/platform-key`; `gpg::verify`/`fingerprints_of`/`emails_of` do armour parsing and RSA math synchronously; `add_key` parses the armour twice. There is no `spawn_blocking` anywhere in `crates/api`.
Fix: `tokio::process::Command` (or `spawn_blocking`) for keygen; `spawn_blocking` around `gpg::*`; parse the armour once.
Effort: S

### [Q-16] Feed fans out to the fleet serially with a ~10-minute worst case per request
Severity: high
Location: /Users/karthik/kloudlite-git/crates/api/src/feed.rs:90-92, 185-223
What: `activity` performs up to `feed_repos`(20) × 2 sequential upstream GETs, each bounded only by the 15 s client timeout; any member can trigger it and there is no overall budget.
Fix: `stream::iter(..).buffer_unordered(4)` plus an overall `timeout` (≈5 s) returning partial results.
Effort: M

### [Q-17] No timeout on local git subprocesses in the merge worker; a wedged merge takes the pod down
Severity: high
Location: /Users/karthik/kloudlite-git/crates/pulls/src/merge_worker.rs:168-170, 183-198, 663-678; /Users/karthik/kloudlite-git/bins/worker/src/main.rs:182-218
What: `networked` has the lowSpeed guard, but `local`/`must`/`commit_tree`/`rebase` use plain `Command::output()`. A pathological `merge-tree`/`rebase` blocks the lane and the `merge/{owner}/{name}` keyed lock; the heartbeat is written only at the loop top, so the liveness probe restarts the pod and kills every other lane's in-flight merge. A lane waiting on the keyed lock also stops heartbeating.
Fix: `tokio::process` + `timeout` + `kill_on_drop` with a per-command ceiling (≈15 min); on expiry return `Err` so the lease re-announces.
Effort: M

### [Q-18] Environment stop snapshots live databases, then tears them down
Severity: high
Location: /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:1776-1785
What: `await_stop_push` pushes while every StatefulSet is still at 1 replica; they are deleted only after `Done`. The pushed record (what a restore on another node reads) is crash-consistent, not the final state, despite the comment promising the "last state". `restore_gate` already has the correct scale-to-zero + `writing_pods` drain shape.
Fix: Scale services to 0 and drain before creating the stop request; delete after `Done`.
Effort: M

---

## Medium

### [Q-19] `api_create` opens a repo's database with no lease
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/app/src/lib.rs:258-266; /Users/karthik/kloudlite-git/bins/server/src/browse_api/admin.rs:139-141
What: `App::route` answers `Local` for a non-existent DB; `create_repo → db_for` then opens it on whichever node received the request, unleased. The next request from any node claims and fences the creator's handle. Self-healing but exactly the pattern the middleware exists to prevent.
Fix: `app.claim(&repo)` before `create_repo`; 503 unless `Granted` names this node.
Effort: S

### [Q-20] `delete_repo` races a concurrent open into a resurrected ghost repo
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/storage/src/refmeta.rs:191-218; /Users/karthik/kloudlite-git/crates/storage/src/store.rs:561-570
What: `delete_repo_db` evicts then deletes object-store files serially; the lease still names this node, so a concurrent request reopens (fresh manifest/WAL) and those objects survive. `admin purge-ghost-repo` (boot.rs:179) exists to clean this up by hand — the race is being hit.
Fix: `deleting: Mutex<HashSet<String>>` on `Pool` checked in `get_once` until the prefix is empty.
Effort: M

### [Q-21] First-write region stamp comes from the client body, not the authenticated token
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/server/src/vol_agent.rs:114-120, 142-145; /Users/karthik/kloudlite-git/crates/workspaces/src/registry.rs:126-130
What: `authorized_for` returns true for an unstamped volume and `append_commits` stamps `REGION_KEY` from `records[0].region`. A region-A token can stamp a fresh volume as region B, locking A out. The ponytail at :92 covers TOFU, not this. The test always sets record.region == token region.
Fix: `authorized_for` returns the presented region; refuse (400) records whose `region` differs; stamp from the token.
Effort: S

### [Q-22] `X-Forwarded-For` fallback makes agent source-binding client-forgeable off-ingress
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/server/src/vol_agent.rs:246-252; context /Users/karthik/kloudlite-git/crates/core/src/peer.rs:11-13 (`networkPolicy: none`)
What: When `X-Real-IP` is absent the first XFF entry — the one a client sets — is trusted. Any pod, port-forward or non-ingress path can present a leaked token plus a forged XFF.
Fix: Drop the XFF fallback; missing `X-Real-IP` with a bound region → refuse.
Effort: S

### [Q-23] `is_client_fault` turns local disk failures into 400s
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/server/src/router/git.rs:318-325; /Users/karthik/kloudlite-git/crates/gitbase/src/objects.rs:132
What: `create_dir_all(&repo.pack_dir)?` and pack writes surface a full/read-only disk as a bare `io::Error`, which is answered `400` with the OS message.
Fix: Wrap server-side io errors at source, or match specific `ErrorKind`s (`InvalidData`, `UnexpectedEof`).
Effort: S

### [Q-24] `evict` inside the renewal loop is unbounded; one hung close stalls every lease renewal
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/app/src/lib.rs:425-434; /Users/karthik/kloudlite-git/crates/storage/src/pool/lease.rs:133-135
What: `renew_once` awaits `h.close()` (an S3 flush) on the same task that renews. `CHECKPOINT_TIMEOUT` was added for exactly this failure; the lost-lease close has no bound.
Fix: `timeout(FLUSH_PATIENCE, h.close())` in `evict`/`evict_if_same`, or spawn the close.
Effort: S

### [Q-25] Per-request full Cosmos region scan for agent auth
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/server/src/vol_agent.rs:62-72
What: `presented_region` calls `store.regions()` on every `/vol-agent` request with no cache or timeout; a slow Cosmos stalls every agent push.
Fix: 60 s `(token → region)` cache in `JobsState`, bounded miss path.
Effort: S

### [Q-26] Blocking filesystem calls on the runtime in open/delete paths
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/storage/src/store.rs:165-209 (via :401), :547, :586
What: `prune_stale_packs` (`read_dir`/`remove_file`/`fs::write`) runs inline in `open_repo` on every request; `delete_objects` does `remove_dir_all` inline.
Fix: `spawn_blocking` / `tokio::fs`.
Effort: S

### [Q-27] `index::list` fans out one GET per marker with no bound
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/storage/src/index.rs:207-214
What: `join_all` over every marker body; thousands of concurrent GETs from one listing request (compare `.buffered(8)` at images.rs:140).
Fix: `buffer_unordered(16)`.
Effort: S

### [Q-28] `route_inner` is a 225-line nested recovery state machine that cannot be unit-tested
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/server/src/router/route.rs:300-524
What: The forward-failed path (404-520) nests `match`/`if let`/`match` with three `forward()` calls and early returns from arm guards; the most safety-critical code in the repo has no fleet-free test.
Fix: Extract a pure `enum Recovery { ServeHere, ForwardTo(node), Force, GiveUp }` decision over `(Grant, peer, self)`, unit-test it, and an `async fn recover_after_failed_forward`.
Effort: M

### [Q-29] Key-layout strings hand-formatted in several places that must agree
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/server/src/browse_api/admin.rs:51,139,224 vs /Users/karthik/kloudlite-git/crates/storage/src/store.rs:104 vs /Users/karthik/kloudlite-git/crates/registry/src/store.rs:413 (index lock key ×4); /Users/karthik/kloudlite-git/bins/server/src/browse_api/images.rs:210 vs `registry::store::manifest_path`; /Users/karthik/kloudlite-git/bins/server/src/browse_api/volumes.rs:64,75 vs `pool::path`; `MAX_REPLY` (boot.rs:111) and `ssh_fingerprint` (boot.rs:68) mirror `crates/api`
Fix: `index_lock_key(kind, owner, name)` in index.rs; `manifest_prefix()` in registry; move `MAX_REPLY`/`read_bounded`/`ssh_fingerprint` into `kloudlite_git_core`.
Effort: S

### [Q-30] Budget-exhausted `merge_base` is recorded as Dirty ("share no history"), not Unknown
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/pulls/src/pulls/check.rs:74-77,95-99; /Users/karthik/kloudlite-git/crates/gitbase/src/merge_base.rs:22-33
What: `None` is returned both for unrelated histories and for an ancestor deeper than the 50 000-commit budget; the arm maps it to `Dirty`, hiding the merge button on long-lived branches of big repos.
Fix: Tri-state return; map exhaustion to `Unknown` with `deep = true`; same in `browse::compare`.
Effort: S

### [Q-31] Every incremental fetch with `have`s (and every push) enumerates every object in the repo
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/git/src/protocol/upload/mod.rs:180-185, 340-378; /Users/karthik/kloudlite-git/crates/git/src/protocol/receive.rs:273-286
What: `common` is computed via `ours()` = full commit traversal + `TreeContents` expansion; the push side expands whole trees of new commits so unchanged blobs are always "unexplained". Both marked ponytail, but the ceiling is "any repo of size".
Fix: Commit-only walk for haves; `TreeAdditionsComparedToAncestor` expansion for push so full enumeration is the exceptional path.
Effort: M

### [Q-32] `blob_at` inflates the whole blob before applying `cap`
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/git/src/browse.rs:171-184 (contrast :541-555)
Fix: `try_header` first; if `size > cap` return truncated without inflating.
Effort: S

### [Q-33] Marker `updated_ms` flip-flops between owning node and GC worker on every pass
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/registry/src/store.rs:355-364; /Users/karthik/kloudlite-git/crates/registry/src/gc.rs:190-204
What: `note_manifest_put` records `now_ms()` (node clock); `reconcile_owner` case (c) recomputes from object-store `last_modified` (1 s granular) and compares for equality — every image is "repaired" each pass and flipped back on the next push.
Fix: Store the object's `last_modified` in `note_manifest_put`, or compare only `manifests` and tolerate 1 s.
Effort: S

### [Q-34] In-flight fast-path upload older than `upload_grace` is swept mid-upload; sidecar multiparts never aborted
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/registry/src/uploads.rs:362, 831-845
What: The staging object is written empty at open and untouched until complete; the sweep judges by its `last_modified`, so a push longer than grace 404s and restarts from zero. Swept sidecars' multiparts are never aborted (parts leak on S3 forever without a bucket rule — see Q-35).
Fix: Skip the staging object when `{uuid}.parts` is fresher than cutoff; `abort_multipart` when deleting a sidecar.
Effort: S

### [Q-35] "Bucket incomplete-multipart lifecycle rule" is referenced three times but exists nowhere
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/registry/src/uploads.rs:223-227, 494-497, 612-613
What: Comments defer cleanup to a README rule; README and `deploy/` contain no such rule. Ponytail upgrade path points at documentation never written.
Fix: Document it (S3 `AbortIncompleteMultipartUpload` 1 day; Azure 7-day built-in) or rely on Q-34's abort.
Effort: S

### [Q-36] Claim ignores `OwnerBinding`; team workspaces can land on a node whose namespace is never built
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/agent/src/claim.rs:24-29,95-103; /Users/karthik/kloudlite-git/bins/agent/src/binding.rs:41-56; /Users/karthik/kloudlite-git/crates/workspaces/src/crd.rs:139-141
What: `decide` reads only `compatibleNodes`; a fresh workspace is claimable by any session node. `teams_in_use` lists only workspaces on this node, so a team namespace for a workspace claimed elsewhere is never ensured; `namespace_ready` returns true from the blanket condition; pod create 404s and requeues forever. `crd.rs:139` still says nodeName is "written ONCE by /v1 from the OwnerBinding", no longer true.
Fix: If an `OwnerBinding {region, owner}` exists, claim only when `spec.nodeName == me`; fix the comment.
Effort: S

### [Q-37] Failed/crashed `commit_core` leaks RO btrfs snapshots that nothing reclaims
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:247-274; /Users/karthik/kloudlite-git/bins/agent/src/lib.rs:238-264
What: Snapshot created at :247 before the send; on failure only the stage file is removed. The janitor iterates lineage entries only, so an unnamed `recv/{uuid}` pins extents forever.
Fix: Delete the snapshot on the error path; janitor sweeps `recv/*` not in any lineage and older than `SWEEP_MIN_AGE`.
Effort: S

### [Q-38] Detached squash children are never reaped — one zombie per auto-squash
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:386-392
What: `Command::spawn()` and the `Child` is dropped; the agent is PID 1 with no init.
Fix: Spawn a task that `wait()`s the child, or run squash in-process on `spawn_blocking`.
Effort: S

### [Q-39] `create_ws`/`create_env` accept an unknown region and an unbounded/zero quota
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/api.rs:571-635, 994-1006, 1185-1217
What: `body.region` is never checked against active regions (object is never claimed, page stuck on "not placed"); `quota_gb = 0` becomes a `0Gi` PVC; no upper bound.
Fix: Validate region ∈ active regions (400) and clamp `quota_gb` in both handlers.
Effort: S

### [Q-40] Environment service names/ports/env keys are unvalidated → permanently erroring reconcile
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/api.rs:1168-1170,1194; /Users/karthik/kloudlite-git/crates/workspaces/src/k8s.rs:904-1036
What: `svc.name` becomes a StatefulSet/Service name and label value (DNS-1035), ports may be 0, env keys may be invalid, duplicate names overwrite each other. Each 422s on every pass → 60 s requeue forever with the raw kube error in `Ready=False`.
Fix: Validate name (`valid_segment` + lowercase + ≤63 + unique), ports 1..=65535, env keys `[A-Za-z_][A-Za-z0-9_]*` in `create_env`/`restore_env`, re-check in `service_statefulset`.
Effort: S

### [Q-41] Long btrfs/upload work occupies the blocking pool with no concurrency cap
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:622-623, 1309; /Users/karthik/kloudlite-git/bins/agent/src/snapshot.rs:198
What: Every materialize/push/nix build is an hours-long `spawn_blocking` sharing the 512-thread pool with the short blocking calls the reconcilers depend on; nothing bounds how many start.
Fix: `Semaphore(8)` around long operations, or `Controller::concurrency(n)`.
Effort: S

### [Q-42] Membership rule exists twice and differs
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/api/src/repos.rs:92-106 (`may_act_under`) vs /Users/karthik/kloudlite-git/crates/workspaces/src/api.rs:163-165 (`may_act_on`)
Fix: Export one from the directory crate; workspaces calls it.
Effort: M

### [Q-43] `update_team` writes name/description before validating pins
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/api/src/teams.rs:481-504
Fix: Compute `names`/`check_pins` before `db.update_team`.
Effort: S

### [Q-44] `kl` rewrites `~/.ssh/config` and `known_hosts` non-atomically
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/kl/src/sshconfig.rs:60-65; /Users/karthik/kloudlite-git/bins/kl/src/config.rs:87-98
What: Truncate-then-write of the user's whole ssh config; a crash or full disk empties it.
Fix: Write `config.tmp` in the same dir, then `rename`.
Effort: S

### [Q-45] 400 vs 502 decided by substring match on `Box<dyn Error>` strings
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/api/src/teams.rs:38,123,171,484
What: `msg.contains("handle")` at :171 matches unrelated directory errors and echoes Mongo's message as a 400. Root cause: `core::Error = Box<dyn Error>` with no variants.
Fix: `enum DirectoryError { Invalid(String), Db(..) }` (thiserror) and match on it.
Effort: M

### [Q-46] Malformed Basic credential degrades to anonymous on browse
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/core/src/httpx.rs:35-40; /Users/karthik/kloudlite-git/crates/api/src/browse.rs:197-199
What: Undecodable base64 / non-UTF-8 / no colon → `None` → anonymous. "anonymous ≠ invalid credential" is upheld by the registry but not here.
Fix: Distinguish absent header from undecodable; 401 the latter.
Effort: S

### [Q-47] CLAUDE.md and README name a `pulls_across` feed fallback that does not exist
Severity: medium
Location: /Users/karthik/kloudlite-git/CLAUDE.md:96; /Users/karthik/kloudlite-git/README.md:110; /Users/karthik/kloudlite-git/crates/api/src/feed.rs:174-180
What: The "load-bearing rule" says every stream consumer keeps a Redis-independent fallback, naming `pulls_across`; `feed.rs` says "No fallback here on purpose … no Mongo fallback any more"; the symbol appears only in a comment in `events.rs`.
Fix: State that the PR half of the feed goes quiet without Redis; only `repo_created` survives.
Effort: S

### [Q-48] Stale ponytail doc block on `Api.on_keys_changed` describes a field that no longer exists
Severity: medium
Location: /Users/karthik/kloudlite-git/crates/api/src/lib.rs:114-121
What: Device codes moved to the directory (`credentials.rs:456-461`); the ceiling is reached and the marker now misleads.
Fix: Delete lines 114-118.
Effort: S

### [Q-49] Three TLS stacks and two `reqwest` majors in the dependency graph, contradicting Cargo.toml's stated intent
Severity: medium
Location: /Users/karthik/kloudlite-git/Cargo.toml (comments on `rustls`, `reqwest`, `axum-server`, `kube`); Cargo.lock
What: `cargo tree -i` shows `aws-lc-rs` pulled by `object_store 0.14` (via slatedb and the direct dep), and `native-tls`/`openssl-sys` pulled by `azure_data_cosmos → azure_core → reqwest 0.12 → hyper-tls`. The workspace also carries `reqwest 0.12` and `0.13`, `thiserror 1/2`, `sha2 0.10/0.11`, `syn 2/3`, `rand 0.8/0.9/0.10`, `rsa 0.9/0.10-rc`. Four separate comments say the second stack is kept out; it is not. Every binary does install a ring provider (`storage/config.rs:25`, `gateway/main.rs:20`, `kl/main.rs:69`), so behaviour is correct, but the graph is larger and slower to build than documented, and `deny.toml`'s `multiple-versions = "warn"` hides it. `cargo audit` is not installed locally (CI runs `rustsec/audit-check`).
Fix: `object_store = { default-features = false, features = ["aws","azure"] }` and check whether `azure_core` offers a `rustls`/no-`native-tls` feature (azure_core 0.31 does: disable `reqwest_native_tls`-style defaults); update the comments to describe reality; consider `multiple-versions = "deny"` with the known skips.
Effort: S

### [Q-50] `env`-mutating tests without a shared lock
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/server/src/vol_agent.rs:346-373; /Users/karthik/kloudlite-git/bins/server/src/boot.rs:366 (`ENV_LOCK`); /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:1092-1094
What: `set_var`/`remove_var` in parallel unit tests (`KLOUDLITE_GIT_VOL_AGENT_TOKENS`, `WSSNAP_SQUASH_LATCH_SECS`); UB in edition 2024 and racy today.
Fix: Make `break_glass_matches(tok, configured)` and `latch_is_stale_with(ttl)` pure and test those.
Effort: S

### [Q-51] Agent still connects to Cosmos for an `Engine.meta` that is never read
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/agent/src/lib.rs:93-94; /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:224-228
What: `run`/`squash` call `meta_store_from_env` to build `meta`; nothing reads it. The agent holds a Cosmos key for nothing, contradicting "bins/api is its only writer".
Fix: Delete the field and drop `COSMOS_*` from the agent Secret.
Effort: S

### [Q-52] Restore on a stopped environment silently pushes an extra snapshot
Severity: medium
Location: /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:1764-1780
What: `restore_gate` bumps `generation`; the Stopped arm's `observed_generation == gen` guard then fails and a fresh `stop-{env}` pushes the just-restored subvolume as an unrequested commit.
Fix: Short-circuit when `prev.phase == Stopped` and only the restore changed.
Effort: S

---

## Low

### [Q-53] Shutdown: sequential release loop and second `pool.close()` are unbounded
Severity: low
Location: /Users/karthik/kloudlite-git/bins/server/src/main.rs:211-214, 268; /Users/karthik/kloudlite-git/crates/storage/src/pool/evict.rs:183-187
What: With the leader down each release is up to ~21 s; the second close has no bound and only `HARD_EXIT` (exit 1) ends it, so every pod exits abnormally on a leader-down roll.
Fix: `join_all` the releases; bound the second close with `RELEASE_DEADLINE`.
Effort: S

### [Q-54] `grant_renew` does one durable put per repo under `leader_lock`
Severity: low
Location: /Users/karthik/kloudlite-git/crates/app/src/lib.rs:589-606
Fix: Read all, decide, one `WriteBatch` under one lock acquisition.
Effort: S

### [Q-55] JSON extractors run before authorization on the vol-agent routes
Severity: low
Location: /Users/karthik/kloudlite-git/bins/server/src/vol_agent.rs:140,163 (contrast pulls.rs:468-471)
Fix: Take `Bytes`, authorize, then `from_slice`.
Effort: S

### [Q-56] Errors logged twice
Severity: low
Location: /Users/karthik/kloudlite-git/bins/server/src/browse_api/images.rs:304-307; /Users/karthik/kloudlite-git/bins/server/src/browse_api/pulls.rs:46-48 (then `internal()` logs at limits.rs:12)
Fix: Drop the handler-side log.
Effort: S

### [Q-57] Swallowed errors that hide data problems
Severity: low
Location: /Users/karthik/kloudlite-git/bins/server/src/browse_api/images.rs:121,136 (`tag().unwrap_or(None)?`); /Users/karthik/kloudlite-git/bins/server/src/browse_api/pulls.rs:117 (`unwrap_or(0)`, `to_value().unwrap_or_default()` renders a PR as `null`); /Users/karthik/kloudlite-git/crates/storage/src/store.rs:442 (`let _ = self.record_pack`)
Fix: `tracing::warn!` at each site; return the error from `pack_index`.
Effort: S

### [Q-58] Stale doc comments in load-bearing places
Severity: low
Location: /Users/karthik/kloudlite-git/bins/server/src/vol_agent.rs:194-196 ("PUBLIC router only" — false, see Q-1), :205-216 (describes register/work/jobs routes that no longer exist), :214 (`route::vol_agent_job_shape` missing), :221-228 (`JobsState` ponytail "rename to AgentAuth when something else touches this file" — ceiling reached), :236-243/:284-301 (~25 blank lines); /Users/karthik/kloudlite-git/bins/server/src/browse_api/merge.rs:79-84 (`App::announce_stranded_merges` no longer a caller); five files still say `http.rs` for `router/route.rs`; /Users/karthik/kloudlite-git/bins/server/src/router/route.rs:25 ("no serde_json" — false); /Users/karthik/kloudlite-git/CLAUDE.md:37 ("four deployed binaries" — six bins; gateway is built by image.yml, kl by kl.yml), :103 (`App::announce_stranded_merges` is a free fn in lanes.rs), :138-139 (services are StatefulSets, stop is a delete — controller.rs:966-999,1784), :163-164 (`Engine::clone_running` is never called; running sources use `clone_local_snapshot`, ops.rs:736); /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:11-28,209-210,705,732,765 and /Users/karthik/kloudlite-git/crates/workspaces/src/model.rs:28,69,196 (job-era `WsClone`/`EnvUp` arms that no longer exist); /Users/karthik/kloudlite-git/tests/pulls.rs:764 ("merge with libgit2"); /Users/karthik/kloudlite-git/crates/api/src/feed.rs:3-5,320; /Users/karthik/kloudlite-git/crates/api/src/credentials.rs:234; /Users/karthik/kloudlite-git/crates/registry/src/gc.rs:304-306 (overstates the HEAD-probe guard that manifests.rs:113 correctly marks ponytail)
Fix: Correct or delete each; rename `JobsState` → `AgentAuth`.
Effort: S

### [Q-59] `every_browse_route_is_routable` checks only one direction
Severity: low
Location: /Users/karthik/kloudlite-git/bins/server/src/router/route.rs:571-615
Fix: Also assert `BROWSE_TAILS ⊆ scraped ∪ owner_scoped`.
Effort: S

### [Q-60] Not every `/v2` error is the OCI envelope
Severity: low
Location: /Users/karthik/kloudlite-git/crates/registry/src/routes.rs:183,206; /Users/karthik/kloudlite-git/crates/registry/src/manifests.rs:57; /Users/karthik/kloudlite-git/tests/registry_limits.rs:81-83
What: axum's `DefaultBodyLimit` 413, extractor rejections and 405s are plain text; CLAUDE.md states the rule without the exception.
Fix: One fallback/`HandleErrorLayer` on `v2_routes()` mapping rejections to `oci_err`, or note the exception.
Effort: S

### [Q-61] `put_manifest` skips unparseable digests, so GC treats their blobs as unreferenced
Severity: low
Location: /Users/karthik/kloudlite-git/crates/registry/src/manifests.rs:140; /Users/karthik/kloudlite-git/crates/registry/src/gc.rs:100-107, 290-293
Fix: Reject non-parseable `layers[]`/`config`/`manifests[]` digests with `MANIFEST_INVALID`.
Effort: S

### [Q-62] Duplicated status/refusal constructors in the registry
Severity: low
Location: uploads.rs:203,691,787; blobs.rs:172; manifests.rs:63 (`StatusCode::from_u16(413).unwrap()` ×5); uploads.rs `BLOB_UPLOAD_UNKNOWN` ×9
Fix: `StatusCode::PAYLOAD_TOO_LARGE`; `fn upload_unknown()`.
Effort: S

### [Q-63] `challenge()` panics on a malformed `KLOUDLITE_GIT_EXTERNAL_URL` at first request, not boot
Severity: low
Location: /Users/karthik/kloudlite-git/crates/registry/src/auth.rs:22-27
Fix: Validate once via `OnceLock`, fail closed at startup.
Effort: S

### [Q-64] `Content-Range` with a `/total` suffix silently disables the length check
Severity: low
Location: /Users/karthik/kloudlite-git/crates/registry/src/uploads.rs:316-322
Fix: `split_once('/')` or reject with `BLOB_UPLOAD_INVALID`.
Effort: S

### [Q-65] Worker `sync` and rev-parse run before the strategy is validated
Severity: low
Location: /Users/karthik/kloudlite-git/crates/pulls/src/merge_worker.rs:412, 501-505
Fix: Match strategy first; `Outcome::refused` before `sync`.
Effort: S

### [Q-66] Rebase worktree named per pid, comment claims per lane
Severity: low
Location: /Users/karthik/kloudlite-git/crates/pulls/src/merge_worker.rs:644-647
Fix: Add a nonce, or fix the comment to credit the keyed lock.
Effort: S

### [Q-67] Any rebase failure is reported as Conflicts
Severity: low
Location: /Users/karthik/kloudlite-git/crates/pulls/src/merge_worker.rs:679-684
Fix: Detect the conflict shape; return `Err` for the rest so the lease retries.
Effort: S

### [Q-68] Connectivity-walk I/O errors are reported to the pusher as "missing necessary objects"
Severity: low
Location: /Users/karthik/kloudlite-git/crates/git/src/protocol/receive.rs:266-271
Fix: `NotFound` → per-ref rejection; anything else → propagate.
Effort: S

### [Q-69] Local pack file leaks on two early error paths in `apply`
Severity: low
Location: /Users/karthik/kloudlite-git/crates/git/src/protocol/receive.rs:228-233, 282
Fix: Scope guard removing `this_push_pack` on any `Err`.
Effort: S

### [Q-70] `browse::compare` silently truncates the commit list on traversal error
Severity: low
Location: /Users/karthik/kloudlite-git/crates/git/src/browse.rs:468-474
Fix: Collect as `Result`; surface or log.
Effort: S

### [Q-71] Filtered incremental fetch re-sends every tree/blob of each new commit
Severity: low
Location: /Users/karthik/kloudlite-git/crates/git/src/protocol/upload/mod.rs:295-309; walk.rs:53-103
Fix: Diff each commit's tree against its parent before filtering.
Effort: M

### [Q-72] `is_ancestor` walk lacks `.hide(old)`
Severity: low
Location: /Users/karthik/kloudlite-git/crates/gitbase/src/refs.rs:58-63
Fix: `.hide(Some(old))`.
Effort: S

### [Q-73] `stranded_merges` scans every `pull/` row of every owned repo on the 15 s beat
Severity: low
Location: /Users/karthik/kloudlite-git/crates/pulls/src/pulls/model.rs:242-255; /Users/karthik/kloudlite-git/crates/pulls/src/pulls/jobs.rs:136-158
Fix: `meta/merge_jobs` index key, or scan only every `ANNOUNCE_EVERY`.
Effort: S

### [Q-74] Git-dependent tests pass green when `git`/`ssh` is absent
Severity: low
Location: /Users/karthik/kloudlite-git/tests/protocol.rs:87-90 (+20 siblings); /Users/karthik/kloudlite-git/tests/ssh_e2e.rs:5-13; /Users/karthik/kloudlite-git/tests/pulls.rs:820-823; /Users/karthik/kloudlite-git/tests/pack_cap.rs:26-29; /Users/karthik/kloudlite-git/tests/cache_invalidation.rs:11-13,24-26 (silent `return`)
Fix: `KLOUDLITE_GIT_REQUIRE_GIT=1` in CI → panic instead of return.
Effort: S

### [Q-75] One credential id per fingerprint, globally
Severity: low
Location: /Users/karthik/kloudlite-git/crates/api/src/credentials.rs:343, 354-356
What: Same key cannot be personal + team; the 409 tells user B that user A owns it.
Fix: Key as `{owner}:{fingerprint}` or neutral 400.
Effort: M

### [Q-76] Removing a user-added key equal to the platform key breaks workspace pushes
Severity: low
Location: /Users/karthik/kloudlite-git/crates/api/src/credentials.rs:208-210, 367-373
Fix: Refuse a fingerprint matching `user_key(owner)` in `add_key`.
Effort: S

### [Q-77] Any team member can rotate the team's platform key
Severity: low
Location: /Users/karthik/kloudlite-git/crates/api/src/credentials.rs:823-839
Fix: Require Admin for team owners, or document.
Effort: S

### [Q-78] Gateway spends the token before the dial
Severity: low
Location: /Users/karthik/kloudlite-git/bins/gateway/src/tunnel.rs:162-172
Fix: `spend` after a successful connect.
Effort: S

### [Q-79] Gateway parses `Authorization` case-sensitively, duplicating `httpx::scheme`
Severity: low
Location: /Users/karthik/kloudlite-git/bins/gateway/src/tunnel.rs:130-134
Fix: `kloudlite_git_core::httpx::bearer_token(&headers)`.
Effort: S

### [Q-80] `list_cli_tokens` verifies the token and hits the directory twice
Severity: low
Location: /Users/karthik/kloudlite-git/crates/api/src/credentials.rs:683-696
Effort: S

### [Q-81] Key name from the pasted comment is uncapped
Severity: low
Location: /Users/karthik/kloudlite-git/crates/api/src/credentials.rs:337 (tokens capped at 60, :83-85)
Fix: `.chars().take(60)`.
Effort: S

### [Q-82] `kl ws ssh` interpolates the workspace id into a shell-parsed `ProxyCommand` unchecked
Severity: low
Location: /Users/karthik/kloudlite-git/bins/kl/src/ws.rs:41
Fix: `sshconfig::safe_name(&id)` first.
Effort: S

### [Q-83] `hex` re-implemented in `jwt::new_jti`
Severity: low
Location: /Users/karthik/kloudlite-git/crates/core/src/jwt.rs:72-74 vs err.rs:38-47
Effort: S

### [Q-84] Weak assertions in the integration suite
Severity: low
Location: /Users/karthik/kloudlite-git/tests/api_server.rs:586 (`404 || 503`), :293 (`2xx || 404`)
What: Either outcome passes because there is no directory fixture; every team/credential/invite test stops at 503.
Fix: A Mongo (or MemStore-backed directory) fixture so 404 is the only accepted answer.
Effort: M

### [Q-85] `CommitRecord.region` re-read from the environment per record instead of `Engine.region`
Severity: low
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:343 vs :183
Fix: `region: self.region.clone()`.
Effort: S

### [Q-86] Duplicate label constants
Severity: low
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/api.rs:375-379 and /Users/karthik/kloudlite-git/crates/workspaces/src/k8s.rs:56-60; /Users/karthik/kloudlite-git/crates/workspaces/src/crd.rs:579-580 (literal)
Fix: `pub use k8s::{OWNER_LABEL,KIND_LABEL,TEAM_LABEL}`.
Effort: S

### [Q-87] Dead engine surface still shipped
Severity: low
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:566-583, 796-916 (`clone_running*`, `init`, `pull`, `pull_env`, `pull_raw`); /Users/karthik/kloudlite-git/crates/workspaces/src/k8s.rs:1224-1295 (`attach_policy`, `attach_egress_policy`)
What: `volume_work` calls only `create_subvol`, `clone_local_ids`, `restore`; nothing else calls these outside tests.
Fix: Delete.
Effort: M

### [Q-88] `kube_err`/`store_err` leak raw API-server and Cosmos error text to `/v1` callers
Severity: low
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/api.rs:256, 367-373
Fix: `tracing::error!` and a fixed body.
Effort: S

### [Q-89] `create_ws` blocks the request up to 5 s polling for placement
Severity: low
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/api.rs:657-675 (duplicates the `list_ws` retry at 784-790)
Fix: Drop the wait.
Effort: S

### [Q-90] Timing assertion in an integration test
Severity: low
Location: /Users/karthik/kloudlite-git/crates/workspaces/tests/engine_ops.rs:622 (`out.locked < 2s`)
Fix: Assert ordering, not duration.
Effort: S

### [Q-91] `restore_ws` searches every team's volumes serially on every restore
Severity: low
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/api.rs:1032-1051
Fix: Optional `volume` in `RestoreBody`.
Effort: S

### [Q-92] Oversized functions
Severity: low
Location: /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:1714 (`apply_environment` 225 lines), :1462 (`apply_workspace` 215), :1175 (`ensure_profile` 162), :207 (`run` 158), :503 (`apply_volume` 130); /Users/karthik/kloudlite-git/crates/workspaces/src/engine/ops.rs:439 (`pull_core` 123); /Users/karthik/kloudlite-git/bins/server/src/router/route.rs:300 (`route_inner` 225, see Q-28). Files >1 900 lines: `bins/agent/tests/reconcile.rs` 2446, `bins/agent/src/controller.rs` 2232, `crates/workspaces/src/api.rs` 1951, `crates/workspaces/src/k8s.rs` 1926.
What: The PV/PVC/nix-PV blocks (controller.rs:1555-1594 and 1833-1857) and the four `*_status`/`status_eq` writers are near-duplicates.
Fix: `ensure_storage(..)` shared by both parents; split `apply_environment` into `stop_environment`/`run_environment`.
Effort: M

### [Q-93] Clippy `--all-targets` carries six pre-existing test-target lints
Severity: low
Location: `cargo clippy --workspace --all-targets`: `items_after_test_module` ×3 (api lib test), `useless_vec` (routing), `contains` vs `iter().any()` (browse_http), `unnecessary clone` (storage), `len_zero` (server), `set_readonly(false)` (registry)
What: CI gates only `--workspace` (lib+bin), so these accumulate; CLAUDE.md sets the bar as "no NEW warnings in files you touch" but nothing enforces it.
Fix: Fix the six (all one-liners) and add `--all-targets` to the CI clippy line.
Effort: S

### [Q-94] `#![allow(clippy::result_large_err)]` crate-wide in seven crates
Severity: low
Location: /Users/karthik/kloudlite-git/crates/{core,storage,registry,api,git}/src/lib.rs:1, /Users/karthik/kloudlite-git/crates/workspaces/src/api.rs:17, /Users/karthik/kloudlite-git/bins/server/src/lib.rs:4
What: The lint fires because `core::Error` is a large `Box<dyn Error>`-wrapping type; silencing it crate-wide hides every future large-error `Result`. Same root cause as Q-45.
Fix: Box the payload once in `core::Error` and remove the allows.
Effort: M

### [Q-95] Three unbounded wake channels in the controller
Severity: low
Location: /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:123-125
What: `unbounded_channel` for `reconcile_on` wakeups; senders are the agent's own finished operations so growth is bounded in practice, but nothing documents that.
Fix: `channel(256)` with `try_send`, or a one-line comment stating why unbounded is safe.
Effort: S

### [Q-96] `println!`/`eprintln!` mixed with `tracing` in admin/boot paths
Severity: low
Location: /Users/karthik/kloudlite-git/bins/server/src/boot.rs:97,167,189,224-266,299,321; /Users/karthik/kloudlite-git/bins/server/src/main.rs:287,296; /Users/karthik/kloudlite-git/bins/agent/src/main.rs:33,63; /Users/karthik/kloudlite-git/bins/agent/src/lib.rs:594
What: Admin subcommands print to stdout while everything else goes through `tracing` to stderr; acceptable for a CLI subcommand, but `boot.rs:97,299,321` are startup warnings that bypass `RUST_LOG` filtering.
Fix: `tracing::warn!` for the three startup paths; leave subcommand output.
Effort: S

### [Q-97] `Query<HashMap>` on `_catalog`/`tags/list` falls back silently on non-numeric `n`
Severity: low
Location: /Users/karthik/kloudlite-git/crates/registry/src/routes.rs (catalog/tags handlers)
Fix: 400 `UNSUPPORTED` or pin the fallback with a test.
Effort: S

### [Q-98] `heal_labels` has no test that writes an object with wrong/missing labels
Severity: low
Location: /Users/karthik/kloudlite-git/bins/agent/src/controller.rs:463-479; /Users/karthik/kloudlite-git/bins/agent/tests/reconcile.rs
What: CLAUDE.md calls it load-bearing; nothing pins it.
Fix: One reconcile test seeding a Workspace with a wrong owner label and asserting the re-stamp.
Effort: S

### [Q-99] CosmosStore has no test at all
Severity: low
Location: /Users/karthik/kloudlite-git/crates/workspaces/src/cosmos.rs
What: Only `tests/ws_e2e.sh` (not in CI) touches it; `MemStore` parity is asserted by nothing.
Fix: A shared trait-level test module run against both, gated on `COSMOS_*`.
Effort: M

### [Q-100] `kl` `#![allow(dead_code)]` on the test stub and `api_volumes.rs` `#[allow(dead_code)]`
Severity: low
Location: /Users/karthik/kloudlite-git/bins/kl/tests/stub.rs:3; /Users/karthik/kloudlite-git/crates/workspaces/tests/api_volumes.rs:24
Fix: Delete the unused items instead.
Effort: S

### [Q-101] Ownership tests poll and drive a mock clock with a real-time task
Severity: low
Location: /Users/karthik/kloudlite-git/crates/storage/src/ownership/tests.rs:284-290 (40×100 ms poll), :397-405 (mock clock + wall-time driver, asserts on SST ids)
Fix: Use `tokio::time::pause()`/`advance`; assert on presence, not ids.
Effort: S

---

## Verified good

Invariants from CLAUDE.md confirmed enforced in code:

- **Single-writer routing.** Routing precedes auth on both listeners (`bins/server/src/router/mod.rs:31-32,63-64`); `/api/` 404s on the public listener (`route.rs:312-314`); `BROWSE_TAILS` gates `api_route` (`route.rs:203-211`) and `every_browse_route_is_routable` scrapes `browse_api/mod.rs`; volume/image tails route by their own keys (`route.rs:242-251`); hop bound: absent = 0, unparseable = exhausted (`route.rs:354-361`); `App::route` never serves on a failed claim except when the expired entry names this node and the DB is warm (`crates/app/src/lib.rs:267-287`); leader is string equality (`:214-216`), followers 421 `/own/*` (`route.rs:126-137`), `OwnershipStore::Reader` put/delete error (`ownership/mod.rs:352-374`); all four leader RMWs under `leader_lock` (`app/lib.rs:444,558,598,609`); retire/close drain→close→release (`pool/evict.rs:68-101,144-189`); `may_release` refuses stale (`ownership/mod.rs:536-538`); `Fenced` is never reopened (`pool/lease.rs:17-36`), orphans adopted-and-closed (`:91-98`); `open_repo_after_fence` re-routes before one retry (`app/lib.rs:637-644`).
- **Blob deletion.** Every `os.delete`/`delete_stream` in the workspace enumerated: blob-path deletes are exactly `crates/registry/src/blobs.rs:205` (client DELETE) and `gc.rs:317` (sweep); pinned by `deleting_an_image_leaves_its_blobs_on_disk`.
- **Verbatim manifests.** `manifests.rs:174` stores `body.clone()`, `:299-334` returns fetched bytes; byte identity tested.
- **`Digest::parse` is the only path→key.** `store.rs:120-126` take `&Digest`; every handler parses (blobs.rs:60,116,163,202; manifests.rs:41; referrers.rs:96; uploads.rs:644); uuids via `valid_uuid` on all four routes; prefix listing is segment-wise (tested).
- **OCI auth.** All handlers call `auth::allow` first; anonymous token → public pulls (`auth.rs:60-65`); `image_is_public` errors read as private.
- **GC keep-bias.** Listing error / unreadable / unparseable manifest all abort (`gc.rs:43,55-77`); double `referenced()` read closes the mount race; sha512 paths handled.
- **Body limits.** Blob routes via `pour`'s byte count and `put_parts` `room`; manifest layer sized off `MAX_MANIFEST`; git `max_body` only on git routes (`router/git.rs:343`).
- **Redis is a nudge.** `events::publish` never errors (`disk.rs:13-46`); `xreadgroup` returns empty without a connection (`disk.rs:86`); lanes need only the repo DB; every Redis command bounded by `CMD_TIMEOUT`/`MAINTENANCE_TIMEOUT` (`cache/mod.rs:254-278`); worker idles when disconnected (`worker/main.rs:100-106,205-212`). (Exception: feed's PR half — Q-47.)
- **Merge worker.** No networked argv is ever formatted (`merge_worker.rs:168-170,315,538`; `worker/main.rs:321-329`; pinned by `a_failed_networked_call_never_names_the_secret`); `GIT_AUTHOR_*`/`GIT_COMMITTER_*` on `commit_tree` (:625-630) and `rebase` (:663-678); `--force-with-lease` from the rev-parse oid (:415-434,519); squash tree==base guard (:477-483); worktree removed on every exit (:687-689); one `merge/{o}/{n}` mutex across claim+merge+report; server refuses an outcome whose `by` ≠ `claimed_by` (`browse_api/pulls.rs:491-493`); lane heartbeats on the `KLOUDLITE_GIT_CACHE_DIR` mount (deploy yaml 805-847); a lane panic exits the process (`worker/main.rs:140-149`, tested).
- **gix#2935 workaround still required**: vendored `gix-pack-0.73.0/src/data/output/count/objects/mod.rs:254-267` still clears the delegate per parent; keep `upload/pack.rs:59-75`. gix partial packs are auto-removed tempfiles (`bundle/write/mod.rs:82`).
- **Protocol.** pkt-line length parsing and caps (`core/pktline.rs:9-27,76-108`); pack cap on both transports (`receive.rs:357-392,410-411`), 1 GiB per-object alloc limit; want/have isolation (`upload/mod.rs:176-185,205-220`); ref updates are a serialisable CAS on a blocking thread (`gitbase/refs.rs:75-88`); SSH publickey-only, 16 channels, v2 required (`ssh.rs:15-19,51,105-118,176`); blocking gix work is on `spawn_blocking`/`block_in_place` throughout git/gitbase/pulls.
- **Workspaces.** `may_act_on` and every owner check read `spec.owner` (`workspaces/api.rs:163-165,799,1159`; `snapshot.rs:194-197`); `heal_labels` re-stamps on every parent reconcile (`controller.rs:463-479`, called 1463/1715); the claim is a real CAS (`replace_status` with `resourceVersion`, `controller.rs:844-856`; 409 → re-read, `claim.rs:112-161`; tested); `compatibleNodes` is a union; env stop gated on `Phase::Done` only (`controller.rs:2085-2086`), terminating `done` counts as absent; push crash seam: stage/`unpushed` cleared only after `post_commits` AND `move_ref` (`ops.rs:353-366`), uploads idempotent by blob id, `set_lineage` tmp+rename; `validate_mount` at the API (`api.rs:1168,1194,1254`), in `service_statefulset` (`k8s.rs:912`) and `mkdir_env_mounts` (`controller.rs:2146`); no hostPath emitted by any builder (tested); init-container `repo`/`branch` re-validated at the argv boundary (`k8s.rs:741-750`); in-place restore staged, `before-restore-*` kept, parent scaled to zero and drained first (`ops.rs:658-698`; `controller.rs:2005-2038`); finalizers wait for in-flight handles; status writes no-op-guarded; Cosmos holds only `Region` (`store.rs:18-21`, `cosmos.rs:84-96`); secrets never enter error strings (`registry_client.rs:35-45`, `upstream.rs:51-73`); blob digests verified on every fetch; nix expression is one argv element with a deadline and process-group kill.
- **Credentials/JWT.** HS256 pinned, `exp`/`typ` validated, `alg:none` refused (`core/jwt.rs:119-127,161-171`); secret ≥ 32 bytes; gateway and api fail closed without the secret; git tokens 128-bit stored as SHA-256 (`storage/auth.rs:17-20,184-192`); invite/sign-in tokens 256-bit hashed; peer secret constant-time and non-empty; peer-only routes ignore Bearer; CLI login exactly-once via `find_one_and_delete`; revocation on every `user_identity`; token create/revoke ordering with unwind; browse path validation, cache read only after authz; teams 404 for non-member and non-existent alike; `may_grant` single source; GPG revocation/subkey binding/expiry handled (modulo Q-5); SSH sigs under `git` namespace; gateway token bound to ws+region, frame caps, 30-min idle timeout; `kl` config 0600, token scrubbed from errors, ssh-config keyword injection guarded.
- **Async hygiene (positive).** No `std::sync::Mutex` guard is held across an `.await` anywhere in the workspace (all sites are lock-compute-drop; awaited sections use `Arc<tokio::Mutex>` from `keyed_lock`).
- **Tooling.** `cargo clippy --workspace -- -D warnings` is clean; `deny.toml` covers advisories (one documented ignore), licences and unknown sources; CI runs `rustsec/audit-check` + `cargo-deny`; `panic = "abort"` in release is documented; zero `TODO`/`FIXME` in `src`.

---

## Untested paths (consolidated, most important first)

1. Forwarded `/vol-agent/*` on a two-node fleet (Q-1) — `tests/vol_agent.rs` is single-node.
2. Thin-pack push with a ref-delta against an existing object (Q-4) — none.
3. Everything under `/v1` with a real directory — `tests/api_server.rs` has no Mongo/MemStore directory fixture, so `may_act_under`, role gating, invites, `cli_approve` double-approve, `revoke`, platform-key rotation and the GPG `add_key` HTTP path all stop at 503 (Q-84).
4. Engine push/clone/restore/squash and the janitor — gated on root+btrfs, not in any CI workflow; `CosmosStore` has no test (Q-99).
5. `create_repo` rollback arm (Q-2), gateway `reserve`/`release` at the limits and `spend` replay (Q-3).
6. `route_inner` recovery branches (HeldBy third node, force-claim lost, `may_ask_to_recover` throttle); `App::route` leader-unreachable-but-warm; `git.rs open()` failed-open release (Q-6); lanes with `vol/`/`img/` keys warm (Q-7).
7. Two-ref push with one connectivity failure (Q-8); >`CHECK_LIMIT` open PRs (Q-9); `merge_base` budget exhaustion (Q-30).
8. Registry fast path on any real backend; push then `gc::reconcile_owner` (Q-33); sweep of a sidecar-bearing session (Q-34); fast-path `complete` with a lying digest; `vol_agent` record.region ≠ token region.
9. `delete_repo` vs concurrent open (Q-20); `api_create` without lease (Q-19); `Pool::close` timeout then second close; `prune_stale_packs` (no test at all).
10. Worker `handle_event` end-to-end, heartbeat/probe, rebase conflict + retry, `landed_anyway` with a real URL.
11. Controller: `heal_labels` (Q-98), `apply_volume` permanent-settle paths other than `REGION_UNREACHABLE`, `AgentRestarted` on `stop-{env}` (Q-13), restore on a stopped env (Q-52), legacy Deployment migration.
12. `feed::activity` handler; `browse_caller` session path; `httpx::basic_creds` edge cases; `kl` login/logout/proxy/pin_host_key.

## Dependency hygiene summary

734 lock packages. Duplicates of note (`cargo tree -d`): `rand` 0.8/0.9/0.10, `rsa` 0.9/0.10-rc (both documented), `reqwest` 0.12/0.13, `thiserror` 1/2, `sha2` 0.10/0.11, `sha1` 0.10/0.11, `sha3` ×3, `syn` 2/3, `spin`, `socket2`, `webpki-roots` 0.26/1.0, `getrandom` ×2. TLS: `ring` (intended) + `aws-lc-rs` (via `object_store`) + `native-tls`/`openssl-sys` (via `azure_data_cosmos → reqwest 0.12`) — see Q-49. No `git2`/`libgit2` (correct per CLAUDE.md). `cargo audit` not installed locally; CI covers it.
