# `bins/agent` — full code review

Date: 2026-09-03. Commit: `4c7e94c9`. Scope: every file under `bins/agent/src` and `bins/agent/tests`,
read against `CLAUDE.md` ("Workspaces and environments"),
`docs/superpowers/specs/2026-09-03-stop-interrupt-decommission-design.md` and
`docs/superpowers/specs/2026-09-03-durable-snapshots-design.md`.

`cargo clippy -p rustic-git-agent-bin -- -D warnings` is **clean** at this commit.

Counts: **3 Critical, 6 Important, 7 Minor, 9 Cleanup.**

---

## Critical

### C1. `retired()` can delete the bytes of a snapshot that was pushed during the pull pass
`bins/agent/src/peer.rs:561-566` (the rule) and `:578-692` (the caller).

`pull_volume` lists the volume's `Snapshot` CRs **first** (`:578`) and reads the local subvolume
names **after** (`:589`), then at the end of the pass deletes every local name not in that
listing (`:686`, `engine.drop_commit`). The `Snapshot` reconciler runs concurrently in the same
process: `/v1` creates a push CR and `reconcile_commit` cuts `snap/<name>` on this same node.
A push whose CR is created after the list and whose btrfs cut lands before line 686 is present on
disk and absent from `existing` — so the pull beat deletes the bytes of a `Ready` snapshot while
its record stays. The window is the whole pull loop, which is a `btrfs receive` of potentially tens
of GiB, not milliseconds.

This is exactly the race `sweep_orphan_snap_bytes` closes with a fresh `get_opt` per candidate
(`peer.rs:588`, "one fresh GET per candidate, exactly as the record sweep does"). `retired()` has
no such guard, and it deletes on the same evidence.

Cost: silent loss of a user's push. Unrecoverable; the CR survives and points at nothing, so the
next `checkout` fails `NO_SUCH_RECORD` and `permanent_reason` makes it terminal.

Fix: give the `retired()` loop the same fresh GET the byte sweep uses — inside the `for name in
retired(...)` loop, `if !matches!(snap_api.get_opt(&name).await, Ok(None)) { continue }` before
`drop_commit`. One GET per candidate, and candidates are rare. (Cheaper still: delete `retired()`
entirely and let `sweep_orphan_snap_bytes` — which already runs every beat over every held volume
and is already guarded — be the only byte reclaimer. That is a net deletion.)

### C2. A crash between the release CAS and the un-place strands every parent on the volume forever
`bins/agent/src/peer.rs:276-382` (`sweep_volumes`), release arm at `:301-329`, parent loop at
`:377-381`.

The release arm clears `spec.nodeName` first (correct — the comment at `:302` says why), then
un-places each parent. If the sweeping agent dies, loses the API server, or its process is
SIGKILLed between the two, the volume has an empty pin and the parents still carry
`status.nodeName = <dead node>`.

Nothing recovers from that state:
- the next beat's `sweep_volumes` skips the volume outright (`:286`, `if owner.is_empty() ...
  continue`), so the parents are never revisited;
- no live node's parent watch matches them (`run.rs:122`, `status.nodeName={me}`);
- the unplaced claim watch (`run.rs:204`, `status.nodeName=`) does not match either, because
  the field is not empty;
- `resolve_volume`'s mismatch self-heal (`controller/volume.rs:~735`) only runs on the node named
  in `status.nodeName` — which is the dead one.

The same window exists in `start_placement` (`controller/stop.rs:192-198`), but there it does
self-heal, because the named node is the live owner. The comment at `stop.rs:190-192` ("a cleared
pin with placed parents self-heals through the mismatch branch") is therefore true for the spread
path and **false** for the dead-node sweep, which is where it also gets applied.

Cost: a workspace or environment that can never be started again without `kubectl patch --subresource=status`.

Fix: make an unowned volume a sweepable case rather than a skipped one. In `sweep_volumes`, when
`owner.is_empty()`, still run the parent loop for any parent on that volume whose `node_name` is
non-empty and unplaceable — i.e. un-place it. Concretely: replace the `owner.is_empty() ||
!owners.contains(&owner)` guard with `!owners.contains(&owner) && !(owner.is_empty() &&
beat.all_parents.iter().any(|p| p.volume == name && owners.contains(&p.node_name)))`, and let the
already-released volume fall straight into the parent loop with `release: true`. One extra arm, no
new listing.

### C3. `cleanup_parent` deletes sync points a seeded clone is still waiting on
`bins/agent/src/controller/workspace.rs:596-638`, deletion at `:615-617`.

`retain` (`snapshot.rs`) is careful: it consults `seeded_from_cuts` and refuses to prune a
transient a not-yet-materialized `SeededFrom` Volume names (`snapshot.rs:~215-250`, with its own
doc explaining that the alternative is a permanent `NO_SUCH_RECORD`). `cleanup_parent` deletes
**every** non-snapshot record of the worktree with no such check.

So: node A dies, someone clones the interrupted workspace (the spec's one way forward —
`SeededFrom{volume, snapshot}`), then deletes the interrupted source. The delete path removes the
exact cut the clone is seeding from, and the clone settles `Permanent/NoSuchSnapshot`.

Cost: the documented recovery path for an interrupted parent is destroyed by an ordinary delete.

Fix: reuse the existing predicate — call `snapshot::seeded_from_cuts(ctx, &volume)` (make it
`pub(crate)`) and skip any name in it, exactly as `retain` does. If the list fails, return an `Err`
so the finalizer retries rather than deleting on a partial view.

---

## Important

### I1. Blocking `btrfs subvolume delete` and directory walks run on the reactor in `retire_pass`
`bins/agent/src/peer.rs:618-621` (`janitor::cleanup_local` for orphan voldirs), `:674`
(`janitor::drop_stale_worktrees`), `:690` (`cleanup_local` for a retired copy); also
`interesting_volumes` `:522` and `should_retire`'s `voldir(&id).exists()` at `:649`.

`janitor::cleanup_local` walks the tree with `std::fs::read_dir` and shells out to
`std::process::Command::new("btrfs").…status()` per subvolume (`janitor.rs:312`). A volume with
many snapshots is seconds to minutes of a reactor thread. `controller/mod.rs`'s own module doc
states the rule ("Long btrfs work runs on `spawn_blocking` … a `LocalSet`/single-reactor-thread
design would let one workspace's lock wait freeze every other in-flight operation"), and
`sweep_orphan_snap_bytes` two functions up obeys it (`peer.rs:593-598`).

Cost: every reconcile and every peer `btrfs send` on the node stalls behind a retire.

Fix: wrap each of the three call sites in `tokio::task::spawn_blocking` with a cloned
`ctx.engine`, matching `sweep_orphan_snap_bytes`'s shape.

### I2. An authenticated peer can drive this node's pull beat continuously
`bins/agent/src/peer.rs:280-286` (`wake`), `controller/run.rs:355-386` (`spawn_pull`),
`peer.rs:351-361` (`after_pass`).

`wake` fires `notify_one` with no rate limit. `after_pass` returns `RunAgain` whenever a permit is
pending, and `spawn_pull` then runs the next pass with **no sleep at all**. A peer POSTing
`/peer/v1/wake` in a loop pins this node in a back-to-back pull beat: ~6 cluster-wide LISTs plus
one field-selected `Snapshot` LIST and one `local_commits` directory walk per interesting volume,
forever.

Cost: one compromised or buggy agent degrades the API server and every other node's beat. The
secret is a shared fleet-wide symmetric token, so "authenticated" is a weak boundary here.

Fix: a floor between wake-driven passes — the same shape `snapshot::wake_worthy` already uses on
the sending side. In `after_pass`, return `RetrySoon(MIN_WAKE_GAP)` rather than `RunAgain` when the
previous pass started less than, say, 5 s ago. Five lines, no new state beyond an `Instant`.

### I3. The `commit` send lock is held for the whole stream, so one stalled puller blocks a volume for an hour
`bins/agent/src/peer.rs:154` (lock acquired), `:174` (moved into `KillOnDrop`), `:265-267`
(`send_timeout`, default 3600).

The per-volume `AsyncMutex` is held for the life of the response body. A puller that opens the
connection and stops reading holds it until its own `send_timeout` fires — and the server has no
timeout of its own (the doc at `:264` says so explicitly: "The receive side has no timeout knob of
its own — the sender's is the only bound"). Every other node's pull of that volume queues behind it.

Cost: replication of one volume stops fleet-wide for up to an hour on a single wedged connection;
`Replicated` never goes true, so nothing on that volume can start elsewhere.

Fix: bound the server side too — wrap the streaming body in a `tokio::time::timeout` or set an
idle-write deadline on the `KillOnDrop` reader, and make it a separate, shorter env
(`WS_PEER_SERVE_TIMEOUT_SECS`) than the client's.

### I4. `pull_one` accepts an unbounded stream into `btrfs receive` with no size ceiling
`bins/agent/src/peer.rs:720-774`, copy at `:56`.

The received subvolume lands under `snap/` on the pulling node with no quota (quotas are set per
`live` worktree and per volume in `volume_work`, not per received snapshot) and no byte cap.
A peer answering the request with an arbitrarily long body fills the pool.

Cost: pool exhaustion takes down every workspace on the node, not just the one volume.

Fix: cap the copy — `tokio::io::copy` on a `reader.take(max_bytes)` with `max_bytes` derived from
the volume's `spec.quotaGb` (times a slack factor) or a plain `WS_PEER_MAX_RECEIVE_BYTES`. A
truncated receive already deletes the partial (`:67-73`), so the failure mode is the existing one.

### I5. `sync_one`'s "newest recorded sync point" comparison is on `Option<u64>`, so a missing annotation wins
`bins/agent/src/sync.rs:115-120`.

```rust
let gen = s.annotations().get(SYNCED_GENERATION).and_then(|g| g.parse::<u64>().ok());
if gen >= recorded { recorded = gen; recorded_state = …; parent = s.name_any(); }
```

`recorded` starts `None`. `Some(_) >= None` is true and `None >= None` is true, so the first Ready
transient always wins regardless of annotation, and a later one with **no** annotation
(`record_post_cut_generation` failed — a documented keep-biased path, `snapshot.rs:~155`) only
loses if `recorded` is already `Some`. Iteration order of a list response therefore decides both
`parent` (the `btrfs send -p` base) and `recorded_state` (the definition-change comparison).

Cost: a redundant full send instead of a delta, and a spurious definition-change cut every beat —
low, but it is exactly the "cut once per interval forever" failure the annotation exists to prevent.

Fix: compare through the same key the rest of the system uses —
`crd::transient_generation_of(s)` (a `u64`, missing = 0), which `peer.rs:703` and
`newest_transient_of` already use. One-line change, and it makes three call sites agree.

### I6. Every node reconciles every `Snapshot` in the cluster, at 1–2 GETs each
`bins/agent/src/controller/run.rs:248-255` (unfiltered `Controller::new` over `Snapshot`),
`bins/agent/src/snapshot.rs:32-48` (`worktree_node`), `:63-72` (the "not mine" arms).

The `Snapshot` watch has no field or label selector, and the first thing `reconcile_commit` does
for a `Working` snapshot is a `Workspace` GET plus (on a miss) an `Environment` GET, purely to
discover it belongs to another node. With `N` nodes and the sync beat cutting one transient per
live worktree per `WS_SYNC_SECS`, that is `N ×` the cluster's cut rate in wasted GETs, plus a
`requeue(TICK)` every 15 s for any snapshot whose worktree cannot be resolved (`:63-68`).

Cost: the largest per-object API cost in the agent, and it grows with cluster size × worktree count.

Fix: the Volume store is already shared and node-scoped (`Ctx::volumes`, `run.rs:85-107`). Read
`s.spec.volume` out of that store first and return `await_change()` when the Volume is not this
node's — no API call at all for the ~(N-1)/N of snapshots that are not ours. The existing
`worktree_node` stays as the second check.

---

## Minor

### M1. `kept_conditions` silently drops `Replicated` and `Decommissioning` on every wait arm
`controller/workspace.rs:413-418`. It keeps only `PackagesReady` and `ATTACHED`. Every
`Resolved::Wait` arm, the namespace gate, `HomeNotReady`, `HeadUnknown` and `CommitPending` go
through `ws_conditions` → `kept_conditions`, so a starting workspace loses the `Replicated`
condition the per-volume sweep reads (`listing.rs:172-174`) and the drain notice
(`controller/mod.rs:with_drain_notice`). The sweep then reads `replicated: false`, which is the
keep-biased direction, so nothing breaks — but the "one place computes it, everywhere reads it"
rule (spec simplification 2) is not actually held. Fix: add `"Replicated"` and `"Decommissioning"`
to the keep list.

### M2. `node_dead_secs()`'s doc says 600, the cluster runs 180, `controller/mod.rs` says 180
`peer.rs:387-392` ("default 600 — how long a node must be observed NotReady"), against
`controller/mod.rs:my_node`'s doc ("The `WS_NODE_DEAD_SECS` floor (180 s)") and commit `5319f67d`
("Declare a node dead after 180 s instead of 600"). The env default was never moved. Fix: make the
code default 180 and delete the deploy-side override, or fix both doc comments — one number, one
place.

### M3. `drain_services` blocks a reconcile for up to 10 s of wall clock
`controller/environment.rs:608-616` — 40 × 250 ms polls, each a namespaced pod LIST. On a stop or
restore of an environment with slow services this is 40 LISTs and 10 s of a reconcile slot. The
doc justifies the wait; the polling rate is the part worth halving (500 ms × 20 is the same
ceiling at half the LISTs).

### M4. `btrfs_delete` panics on a non-UTF-8 path
`janitor.rs:313`, `path.to_str().unwrap()`. Unreachable with today's names (all `valid_segment`),
but this runs inside `cleanup_local`, which the finalizer path depends on. Use `.arg(path)` —
`Command::arg` takes `AsRef<OsStr>`, so the `to_str()` is not needed at all.

### M5. The dead-node sweep runs identically on every live node
`peer.rs:467-481`. Each live node computes the same dead set and calls `sweep_volumes`; only one
wins the release CAS, and the losers `continue` (`:323`) — but every one of them also walks every
volume and, on a `Mark` verdict, issues the parent status writes (idempotent, guarded by the idle
check at `mark_parent_of:433`). It is correct but pays `N ×` the writes and `N ×` the GET-per-parent
at `:415`. The codebase elects a single writer elsewhere (the ownership lease in the server tier).
Not worth a lease here; worth a rendezvous over `live` keyed by volume id — the same
`preferred_node` helper this file already has — if the write volume ever shows up.

### M6. `secret_ok` compares digests with `==` on a `GenericArray`
`peer.rs:113-114`. The comment says the SHA-256 makes the comparison length-independent, which is
right; the `==` itself is still an early-exit `memcmp` over the digests. Against a hashed value
that leaks nothing useful, so this is correct as written — noted only so a future reader does not
"fix" it into a constant-time compare crate and think something changed.

### M7. `agent_pod_addr` trusts a label in `kube-system`
`peer.rs:250-260` — the peer address for a node is whichever pod in `kube-system` carries
`app=rustic-git-agent` and `spec.nodeName={node}`. Anyone who can create a pod in `kube-system`
can redirect a pull. That is already a cluster-admin-adjacent capability, so this is an
observation, not a hole; a `spec.serviceAccountName == "rustic-git-agent"` check on the pod would
close it for one line.

---

## Cleanup

### K1. `compatibleNodes` is dead but still in the CRD and in two status comparisons
Spec simplifications 6 and 11 say it goes. Nothing writes it (`claim.rs:40` documents its removal),
yet `crd.rs:629` and `:736` still declare it and
`controller/workspace.rs:1284` / `controller/environment.rs:641` still compare it in the
status-equality predicate. Delete the field and the two comparison lines; the tolerated-unknown
parse test at `crd.rs:1242` already covers old objects.

### K2. Test fixtures still write `compatibleNodes`
`decommission.rs:234`, `peer.rs:2103`, `:2112`, `:2136`. Dead payload in three files.

### K3. RBAC grants `apps/deployments: get,delete` for code that no longer exists
`deploy/k3s/agent-rbac.yaml` ("legacy migration only"). `grep -n 'Deployment' bins/agent/src`
returns nothing. Drop the rule and its table row — the file's own header says the table *is* the
role.

### K4. The RBAC table names three dead concepts
Same file: `home_commit_beat`, `stop-home-{ws}`, and the `deployments` row. The home moved to
ZeroFS (spec 2026-09-01) and there is no home commit beat. The table is load-bearing documentation;
stale rows in it are worse than none.

### K5. `Placement::has_commits` / `commit_phase` / `has_commits` still speak "commit"
`claim.rs:23`, `:76-110`; `peer.rs` throughout ("Every volume this node must hold a commit-model
replica of", "retired commit", "orphaned commit bytes"). The durable-snapshots vocabulary note is
explicit: read **snapshot** for **commit**. A rename of the four `claim.rs` helpers and the log
strings in `pull_volume`/`retire_pass` is mechanical and makes the code match the words the API,
the web and the CLI now use.

### K6. `binding.rs`'s module doc still explains `spec.nodeName`, which the struct no longer has
`binding.rs:8-13`. True but now archaeology; the `OwnerBinding.spec.nodeName` field is item 11 of
the spec's four dropped fields. Two sentences.

### K7. Stale `// ponytail:` markers
- `peer.rs:198-202`: "`WS_NODE_DEAD_SECS`'s default (600s) swallows ordinary NTP drift" — the
  cluster runs 180 s (see M2), so the ceiling this marker names no longer holds and the upgrade
  it defers (apiserver-relative delta) is now closer to needed.
- `peer.rs:558-560` (`retired`, "all-or-nothing rather than transients-only"): if C1 is fixed by
  deleting `retired()` in favour of the guarded byte sweep, this marker goes with it.
- `binding.rs:15` ("bindings are never deleted; a node-retirement path re-homes them later") —
  the node-retirement path now exists (`decommission.rs`) and does not touch bindings. Either the
  marker should name why that is still fine, or the drain should collect them.

### K8. Four near-identical `NoopNix` + `test_ctx` fixtures
`listing.rs:205-232`, `claim.rs:347-379`, `peer.rs:758-790`, `sync.rs:168-199`, `decommission.rs:137-168`,
`controller/volume.rs` — five copies of the same 30 lines. `rustic_git_workspaces::kube_test`
already exists as the shared test-support module; one `agent_test_ctx(pool, node, routes)` there
deletes ~150 lines.

### K9. Duplicated start-spread block between `workspace.rs` and `environment.rs`
`controller/workspace.rs:117-127` and `controller/environment.rs:110-117` are the same eight lines
(`if prev.phase == Stopped { parents_on_volume → start_placement → await_change }`) with a
different log field. Same for the `migrate_and_seed_baseline` → `latest_transient` →
`effective_head` → `HeadUnknown` → `clone_commit`/`CommitPending`/`NoSuchCommit` → `checkout` +
`set_quota_worktree` → first-graft sequence (`workspace.rs:189-312` vs `environment.rs:249-350`),
which is ~120 lines duplicated with the status struct as the only real difference. The comments
say so themselves ("`apply_workspace`'s twin arm, verbatim in shape"). A shared
`worktree_gate(parent_name, volume, storage, prev_head, ctx) -> Result<WorktreeGate>` returning an
enum the two callers turn into their own status type would remove the largest duplication in the
crate — and the largest place the two kinds can drift.

---

## Per-beat cost table

Costs are per node per interval. "LIST" = one cluster-wide list unless marked field/label-selected.

| Beat / trigger | Interval | Kubernetes cost | Local cost |
|---|---|---|---|
| `spawn_pull` → `pull_beat_with` (`peer.rs:436`) | `WS_REPLICA_SECS`=300 s, plus every wake | 1 Node LIST; `listing::beat` = 1 Volume + 1 VolumeReplica + 1 Workspace + 1 Environment LIST; 1 label-selected Node LIST (`pool_nodes`) | — |
| ↳ `reap_dead_replicas` | same | 1 DELETE per dead row | — |
| ↳ `sweep_dead_nodes`/`sweep_volumes` | same | per marked volume: ≤3 status PUTs; per parent: 1 GET + ≤2 status PUTs; per released volume: 1 JSON-patch | — |
| ↳ `pull_volume`, **per interesting volume** | same | 1 field-selected Snapshot LIST; 1 pod LIST per source node (≤ sources); 1 VolumeReplica GET (+CREATE) + 1 status PUT | 1 `local_commits` readdir; 1 `btrfs receive` per missing snapshot |
| ↳ `retire_pass` | same | 1 **cluster-wide Snapshot LIST**; 1 Volume GET per orphan-snapshot candidate; 1 Snapshot GET per orphan-byte candidate; 1 Volume GET per non-retiring, non-hosted volume; ≤1 VolumeReplica DELETE + ≤1 Volume DELETE per volume | `readdir(vol/)`; `local_commits` per held volume; `btrfs subvolume delete` per orphan (**on the reactor**, see I1) |
| `spawn_sync` → `sync_beat` (`sync.rs:64`) | `WS_SYNC_SECS`=60 s | 1 Workspace + 1 Environment LIST; **per live worktree** 1 field-selected Snapshot LIST + ≤1 Snapshot CREATE | 1 `Engine::generation` per live worktree (spawn_blocking ✓) |
| `spawn_decommission` (`decommission.rs:68`) | `WS_DECOMMISSION_SECS`=30 s | 1 Node LIST **always, on every node**; if labelled: + the 4 `listing::beat` LISTs, `sweep_volumes`' writes, 1 Node PATCH | — |
| `spawn_heartbeat` (`run.rs:336`) | 30 s | 1 Volume LIST `limit=1` | 1 file write |
| `spawn_janitor` (`janitor.rs:25`) | 600 s | none | `readdir` of `attach/`, `vol/`, `profiles/`; recursive `du` of every home **and of `/nix/store`** (spawn_blocking ✓) |
| Workspace reconcile (`apply_workspace`) | `TICK`=15 s while unconverged, else on watch | 1 Node GET (`my_node`); 1 Volume GET; 1 OwnerBinding GET + 1 Namespace GET; 1 Pod GET (`pod_is_ready`) + 1 (`pod_carries_the_attach_mount`); when no worktree: 1 Snapshot LIST (`latest_transient`) + 1 Snapshot LIST (`has_commits`); on a **start** pass: + 2 cluster-wide parent LISTs (`parents_on_volume`) and, inside `start_placement`, 1 VolumeReplica LIST + 1 Node LIST + 1 Snapshot LIST **per parent** | `ensure_shared_home` (may run `mount`/`umount`, ≤65 s, spawn_blocking ✓); `write_resolv_conf`; `checkout` + `set_quota_worktree` |
| Stopped-parent reconcile | `TICK` | + `replicated_condition`: 1 Snapshot LIST + 1 field-selected VolumeReplica LIST | — |
| Environment reconcile | `TICK` | as above, plus 1 StatefulSet GET per service, 1 Pod LIST per drain poll (≤40) | `mkdir_env_mounts` (spawn_blocking ✓) |
| `Snapshot` reconcile | every Snapshot event, **on every node** | 1 Workspace GET (+1 Environment GET on miss) before the "not mine" bail — see I6; if mine: 1 status PATCH, 1 metadata PATCH, 1 Snapshot LIST (`retain`) + 1 Volume LIST (`seeded_from_cuts`), + `wake_peers` = 1 Node LIST + 1 label-selected Node LIST + 1 pod LIST per peer | `commit_worktree`, `generation` (spawn_blocking ✓) |

The single largest recurring cost is `retire_pass`'s cluster-wide `Snapshot` LIST plus a
`local_commits` directory walk per held volume, every 300 s on every node; the largest avoidable
one is I6.

---

## Btrfs-gated tests that never run in CI

CI runs on a container with no loopback btrfs and no root, so these are skipped there and only
ever exercised by hand or by `tests/ws_e2e.sh` on a Linux VM:

- `bins/agent/src/janitor.rs::janitor_tests::cleanup_local_deletes_nested_commit_model_subvolumes`
  — the only explicit gate in the crate (`if !have_btrfs() { eprintln!("skipping…"); return }`,
  `janitor.rs:523`). It is the **only** test of `cleanup_local` against real subvolumes; every
  other `cleanup_local`/`drop_stale_worktrees` test uses `fake_engine()` and exercises
  `btrfs_delete`'s `#[cfg(test)]` `remove_dir_all` fallback (`janitor.rs:322-330`) — i.e. it proves
  the fallback, not the production path.

Implicitly gated (they pass on a Mac only because the code short-circuits before touching btrfs,
which is worth knowing when editing the engine):

- `snapshot.rs::cut_on_my_node_sets_ready_and_advances_head_preserving_other_status_fields` — passes
  only because `snap/{name}` is pre-created so `commit_worktree`'s `dst.exists()` returns early
  (the test says so).
- `reconcile.rs::commit_model_checkout_converges_on_an_existing_worktree`,
  `commit_model_environment_bootstrap_materializes_its_worktree`,
  `commit_model_clone_checks_out_its_graft_commit_and_records_it_as_head`,
  `the_sync_beat_cuts_a_transient_only_when_the_worktree_generation_moved` — same shape:
  pre-created directories stand in for subvolumes, so no `btrfs` binary is invoked.
- `bins/agent/tests/peer.rs` — the whole file drives the router with a fake `btrfs send` shell
  script. Good coverage of auth, `valid_segment` and streaming; **zero** coverage of the receive
  half (`pull_one`) against a real `btrfs receive`.

Tests asserting mere existence (weak assertions worth strengthening): `sync.rs::
a_failed_parent_listing_cuts_no_sync_points` asserts only "no POST"; `decommission.rs::
a_drain_leaves_a_running_parent_completely_alone` asserts "no DELETE" and absence of a path
substring. Both are absence-assertions against a mock whose route list would 404 anyway — they
pass for the wrong reason if the code under test changes which API it calls.

---

## What is good, and should not be touched

1. **`listing::Beat` and the `None`-means-partial rule** (`listing.rs`). One query, four LISTs,
   threaded through every sweep, with `None` (not an empty list) on any failure. It is the reason
   the sweeps cannot disagree, and the module doc that states "a consumer that finds an object
   present in one list and absent from another must SKIP it this beat" is the single most valuable
   comment in the crate.
2. **`volume_decision` + `sweep_volumes` as one function for both sweeps** (`peer.rs:236-382`).
   The spec asked for exactly this (simplification 9) and the code delivers it, including the
   `mark_running` flag that keeps a drain from libelling a healthy workspace. The pin-before-unplace
   ordering and its comment are correct and load-bearing.
3. **The claim's optimistic `replace_status` with a re-read-and-re-decide on 409**
   (`claim.rs:200-290`). The F1 status-merge (start from the object's own status, overlay only
   phase/nodeName/conditions) and the refusal to use a forced apply are both exactly right, and
   the "a second, subtly different copy of the 409 arms is how a loser talks itself into
   overwriting a winner" comment should survive any refactor.
4. **`up_to_date` by NAME, never by clock** (`peer.rs:87-93`) and the single `newest_transient_of`
   ordering key shared with `crd`. This is what makes the whole placement story skew-proof.
5. **`my_node`'s dead-guard above every write** (`controller/mod.rs`) with `node_is_dead` rather
   than `unplaceable`, and the matching `unplaceable`/`node_is_dead` split in `peer.rs:169-196`.
   The distinction (a decommissioning node is alive and must keep converging) is subtle, correct,
   and has a test in `controller/volume.rs` pinning it.
6. **The mount hygiene in `lib.rs`** — `mount_answers` doing a READDIR rather than `statfs`,
   `timeout -s KILL`, `nsenter -t 1 -n` without `-m`, the repair-before-`create_dir_all` ordering,
   and the reasons written down for each. Every line of it is a production incident.
7. **`write_resolv_conf`'s in-place write** (`workspace.rs:380-395`) and its "do not fix this into
   an atomic write" warning, backed by an inode test in the integration suite.
8. **`wake_worthy`'s compare-exchange coalescing** (`snapshot.rs:135-142`) — person-initiated cuts
   always wake and never consume the window; sync cuts wake at most once per interval per node.
   Small, pure, and tested including the "a person-initiated wake never moves the sync window"
   property.
9. **`mkdir_env_mounts` calling `validate_mount` before `create_dir_all`** with a test asserting
   the traversal makes no directory (`environment.rs:620-635`). The comment names the escape
   correctly: `create_dir_all` on an unvalidated folder *is* the escape.
10. **`secret_ok`'s empty-secret guard at the trust boundary** rather than at the one caller that
    enforces it today, with a test for it (`tests/peer.rs`).
11. **`KillOnDrop` + the concurrent stderr drain** on the send path (`peer.rs:178-223`) — a
    disconnected puller does not leak a root `btrfs send`, and an unread 64 K of stderr does not
    wedge one.
12. **`janitor_sweep_profiles`' asymmetric strictness** — bailing on any error while its sibling
    sweeps `.flatten()` past them, with the reason written down (an unlinked GC root gets a live
    store path collected). That is the keep-bias applied with judgement rather than by rote.
